//! The daily briefing — the cadence is something the system issues.
//!
//! The suggestion engine already expires stale asks and caps how many are
//! outstanding; this module is the third leg of §4i's cadence: one readable
//! briefing per workspace per tenant-local day, instead of a notification
//! per thing that changed.
//!
//! Composition is deterministic and runs inside the team-handoff sweep's
//! transaction: the active arc, the asks awaiting a decision, the open
//! handoffs, the coming production days and what landed in the last 24
//! hours. Delivery rides the proven assignment path — each active member
//! gets a `daily_briefing` assignment pointing at the briefing row, so the
//! same email + trace machinery that carries every other handoff carries
//! this one, and no new delivery channel exists to break silently.
//!
//! Two deliberate absences: briefing assignments take no reminders (a
//! briefing nagged about is two notifications, not one), and yesterday's
//! briefing cancels itself when today's issues — a stale "read me" is
//! noise, and the briefing exists to be the opposite of noise. The day the
//! system has nothing to say, the briefing says so.

use super::{team::queue_team_email_action, *};
use crowdrelay_application::autopilot::{AutopilotActionPayload, BriefingLocale};

/// The tenant-local hour at which the briefing may first issue. A sweep
/// running before it waits; a sweep running after it issues for the whole
/// day — catch-up semantics, so a workspace whose cycles run sparse still
/// gets its briefing rather than missing the window.
const BRIEFING_LOCAL_HOUR: i32 = 8;

/// The n8n mail workflow slices bodies at 1800 chars; stay under it with
/// room for the subject line and the truncation footer.
const BODY_LIMIT: usize = 1650;

/// Locale-keyed frame for the briefing's fixed strings, the same shape
/// `DetailFrame` gives the task detail path.
struct BriefingFrame {
    title: &'static str,
    arc: &'static str,
    no_arc: &'static str,
    pending: &'static str,
    tasks: &'static str,
    production: &'static str,
    changed: &'static str,
    changed_counts: &'static str,
    nothing: &'static str,
    more_in_panel: &'static str,
    due_label: &'static str,
    no_plan: &'static str,
    yield_label: &'static str,
    unmeasured: &'static str,
    fans_label: &'static str,
    fans_flat: &'static str,
    fans_format_unrecorded: &'static str,
    awaiting_report: &'static str,
    beat_label: &'static str,
    /// §4i-6: asks handed out per member this week, against the tenant's own
    /// ceiling — the load the router enforces, made visible instead of
    /// silently accumulating on whoever answers fastest.
    capacity: &'static str,
    /// Asks that died waiting — approvals that expired without anybody ever
    /// being assigned, or deferred asks whose deadline lapsed unresolved.
    /// The drop is stated, not silently absorbed.
    capacity_dropped: &'static str,
    /// Asks the router refused (no skill match, or everyone eligible at the
    /// weekly ceiling) that remain unassigned inside their window.
    capacity_waiting: &'static str,
    ceiling_label: &'static str,
}

const fn briefing_frame(locale: BriefingLocale) -> BriefingFrame {
    match locale {
        BriefingLocale::Pl => BriefingFrame {
            title: "ViryaOS — poranne podsumowanie",
            arc: "Łuk treści",
            no_arc: "brak aktywnego łuku",
            pending: "Czeka na decyzję",
            tasks: "Otwarte zadania",
            production: "Dni produkcyjne",
            changed: "Co się zmieniło (24h)",
            changed_counts: "materiał: {}, obserwacje: {}, wygasłe: {}",
            nothing: "Dziś nic nie wymaga uwagi — system pracuje.",
            more_in_panel: "Więcej w panelu: /staff",
            due_label: "termin",
            no_plan: "brak planu",
            yield_label: "zbiór",
            unmeasured: "brak pomiaru",
            fans_label: "fani (30 dni)",
            fans_flat: "brak konwersji — nic jeszcze nie działa, i to też jest wiedza",
            fans_format_unrecorded: "format niezapisany",
            awaiting_report: "Czeka na raport",
            beat_label: "beat",
            capacity: "Prośby w tym tygodniu",
            capacity_dropped: "przepadło bez przypisania",
            capacity_waiting: "czeka — nikt wolny lub bez umiejętności",
            ceiling_label: "limit",
        },
        BriefingLocale::En => BriefingFrame {
            title: "ViryaOS — morning briefing",
            arc: "Content arc",
            no_arc: "no active arc",
            pending: "Awaiting your decision",
            tasks: "Open tasks",
            production: "Production days",
            changed: "What changed (24h)",
            changed_counts: "material: {}, observations: {}, expired: {}",
            nothing: "Nothing needs you today — the system is working.",
            more_in_panel: "More in the panel: /staff",
            due_label: "due",
            no_plan: "no plan",
            yield_label: "harvest",
            unmeasured: "unmeasured",
            fans_label: "fans (30d)",
            fans_flat: "no conversions — nothing is working yet, worth knowing",
            fans_format_unrecorded: "format unrecorded",
            awaiting_report: "Awaiting your report",
            beat_label: "beat",
            capacity: "Asks this week",
            capacity_dropped: "dropped unassigned",
            capacity_waiting: "waiting — nobody free or no skill match",
            ceiling_label: "ceiling",
        },
    }
}

#[derive(Debug, FromRow)]
struct BriefingMemberRow {
    member_id: Uuid,
    display_name: String,
    normalized_email: String,
}

#[derive(Debug, FromRow)]
struct PendingAskRow {
    action_kind: String,
    expires_local: Option<time::Date>,
    payload: serde_json::Value,
}

#[derive(Debug, FromRow)]
struct AwaitingReportRow {
    concept: String,
    suggested_before: Option<time::Date>,
}

#[derive(Debug, FromRow)]
struct OpenTaskRow {
    display_name: String,
    open_count: i64,
    nearest_due_local: Option<time::Date>,
}

#[derive(Debug, FromRow)]
struct ProductionDayRow {
    title: String,
    scheduled_for: time::Date,
    plan_status: Option<String>,
}

#[derive(Debug, FromRow)]
struct RecentDayRow {
    title: String,
    scheduled_for: time::Date,
    plan_status: Option<String>,
    sources_landed: Option<i32>,
    planned: Option<i32>,
}

#[derive(Debug, FromRow)]
struct TopShareRow {
    label: String,
    fans: i64,
}

/// Issues today's briefing once per tenant-local day, to every active
/// workspace member. Returns the number of assignments created.
pub(in crate::autopilot) async fn issue_daily_briefings(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    locale: BriefingLocale,
) -> Result<u32, RepositoryError> {
    let ws = workspace_id.into_uuid();

    // The day boundary is the tenant's, not the session clock's. Absent a
    // configured zone the shipped default is UTC — the same convention
    // tenant_settings documents for every per-workspace override. The read
    // itself is shared with the reminder sweep's quiet window so both clocks
    // resolve one row the same way.
    let zone = super::team::crew_timezone_in_tx(tx, workspace_id).await?;

    // Local date + hour in one statement so the gate and the artifact can
    // never disagree about which day it is.
    let (local_date, local_hour): (time::Date, i32) = sqlx::query_as(
        "SELECT ($1::timestamptz AT TIME ZONE $2)::date,
                EXTRACT(HOUR FROM $1::timestamptz AT TIME ZONE $2)::int",
    )
    .bind(now)
    .bind(&zone)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if local_hour < BRIEFING_LOCAL_HOUR {
        return Ok(0);
    }

    // Yesterday's briefing stops asking to be read the moment today's
    // exists — cancelled, not done, because nobody read it.
    sqlx::query(
        r#"
        UPDATE viryaos_team_assignments assignment
        SET status = 'cancelled', updated_at = now()
        FROM viryaos_daily_briefings briefing
        WHERE assignment.workspace_id = $1
          AND assignment.source_kind = 'daily_briefing'
          AND assignment.status = 'open'
          AND briefing.workspace_id = assignment.workspace_id
          AND briefing.id = assignment.source_id
          AND briefing.local_date < $2
        "#,
    )
    .bind(ws)
    .bind(local_date)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // The day is already spoken for — skip the composition cost; the
    // INSERT's ON CONFLICT still guards the two-sweeps-racing case.
    let already_issued: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM viryaos_daily_briefings
             WHERE workspace_id = $1 AND local_date = $2)",
    )
    .bind(ws)
    .bind(local_date)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if already_issued {
        return Ok(0);
    }

    let frame = briefing_frame(locale);
    let (title, body, sections) =
        compose_briefing(tx, workspace_id, local_date, now, &frame, locale, &zone).await?;

    let briefing_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_daily_briefings
            (workspace_id, local_date, title, body, sections)
        VALUES ($1,$2,$3,$4,$5)
        ON CONFLICT (workspace_id, local_date) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(ws)
    .bind(local_date)
    .bind(&title)
    .bind(&body)
    .bind(&sections)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let Some(briefing_id) = briefing_id else {
        return Ok(0);
    };

    // The roster is every active member, not the routing roster — a member
    // without a team profile, or one already at capacity, still gets the
    // briefing. Reading it is not work that needs a slot.
    let members = sqlx::query_as::<_, BriefingMemberRow>(
        r#"
        SELECT id AS member_id, COALESCE(display_name, normalized_email) AS display_name,
               normalized_email
        FROM workspace_members
        WHERE workspace_id = $1 AND status = 'active'
        ORDER BY display_name
        "#,
    )
    .bind(ws)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // Due at end of the local day — a briefing is a read-today item, and
    // the due time is what the assignment UI sorts by.
    let due_at: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT (($1::date + 1)::timestamp AT TIME ZONE $2)")
            .bind(local_date)
            .bind(&zone)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_sqlx)?;

    let mut issued = 0u32;
    for member in &members {
        let assignment_id = Uuid::now_v7();
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_team_assignments (
                id, workspace_id, action_id, source_kind, source_id, source_ref,
                assignee_member_id, required_skill, due_at, next_reminder_at
            ) VALUES ($1,$2,NULL,'daily_briefing',$3,NULL,$4,'briefing',$5,NULL)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(assignment_id)
        .bind(ws)
        .bind(briefing_id)
        .bind(member.member_id)
        .bind(due_at)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        if inserted.is_none() {
            continue;
        }
        // No reminder is scheduled (next_reminder_at stays NULL, which the
        // reminder sweep reads as "never remind") — the briefing is the
        // cadence, a reminder about it would be a second notification.
        queue_team_email_action(
            tx,
            workspace_id,
            assignment_id,
            "show_operations",
            &member.normalized_email,
            &member.display_name,
            title.clone(),
            body.clone(),
            due_at,
            0,
            None,
            now,
        )
        .await?;
        issued = issued.saturating_add(1);
    }
    Ok(issued)
}

/// Builds the briefing from the rows the band already owns. Every section
/// is a count first and a list second — the body is for a reader, the
/// sections map is for the operator asking whether the briefing had
/// anything in it.
async fn compose_briefing(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    local_date: time::Date,
    now: OffsetDateTime,
    frame: &BriefingFrame,
    locale: BriefingLocale,
    zone: &str,
) -> Result<(String, String, serde_json::Value), RepositoryError> {
    let ws = workspace_id.into_uuid();

    // ── The active arc ────────────────────────────────────────────────
    let arc = sqlx::query_as::<_, (String, serde_json::Value, Option<time::Date>)>(
        r#"
        SELECT title, spine, horizon_end
        FROM viryaos_arcs
        WHERE workspace_id = $1 AND status = 'active'
        ORDER BY created_at DESC LIMIT 1
        "#,
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let arc_line = arc.as_ref().map(|(title, spine, horizon_end)| {
        let beats = spine.as_array().map_or(0, Vec::len);
        match horizon_end {
            Some(end) => format!("{title} — {beats} beats, runs to {end}"),
            None => format!("{title} — {beats} beats"),
        }
    });

    // ── Asks awaiting a decision ──────────────────────────────────────
    // The same lapsed-approval filter the routing query uses: an ask whose
    // window already closed is dead work, not a decision to report. Dates
    // render tenant-local — a UTC date is off by one near midnight for a
    // non-UTC crew.
    let asks = sqlx::query_as::<_, PendingAskRow>(
        r#"
        SELECT action_kind,
               (approval_expires_at AT TIME ZONE $3)::date AS expires_local,
               payload
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1 AND status = 'awaiting_approval'
          AND (approval_expires_at IS NULL OR approval_expires_at > $2)
        ORDER BY approval_expires_at NULLS LAST, created_at, id
        LIMIT 4
        "#,
    )
    .bind(ws)
    .bind(now)
    .bind(zone)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let pending_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND status = 'awaiting_approval'
           AND (approval_expires_at IS NULL OR approval_expires_at > $2)",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Approved work awaiting the band's report ─────────────────────
    // An approved suggestion stays open until somebody reports done /
    // done_differently — and only until its beat day, when the sweep
    // resolves it `expired` whether the work happened or not. Naming the
    // count and the nearest day is what makes the report path get used
    // before the window closes and the stale rule scores it as ignored.
    let awaiting_report = sqlx::query_as::<_, AwaitingReportRow>(
        r#"
        SELECT concept, suggested_before
        FROM viryaos_content_suggestions
        WHERE workspace_id = $1 AND status = 'approved'
        ORDER BY suggested_before NULLS LAST, created_at
        LIMIT 3
        "#,
    )
    .bind(ws)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let awaiting_report_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_content_suggestions
         WHERE workspace_id = $1 AND status = 'approved'",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Open handoffs per member (the briefing itself does not count) ──
    let open_tasks = sqlx::query_as::<_, OpenTaskRow>(
        r#"
        SELECT COALESCE(member.display_name, member.normalized_email) AS display_name,
               COUNT(*) AS open_count,
               (MIN(assignment.due_at) AT TIME ZONE $2)::date AS nearest_due_local
        FROM viryaos_team_assignments assignment
        JOIN workspace_members member
          ON member.workspace_id = assignment.workspace_id
         AND member.id = assignment.assignee_member_id
        WHERE assignment.workspace_id = $1
          AND assignment.status = 'open'
          AND assignment.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
        GROUP BY member.id, member.display_name, member.normalized_email
        ORDER BY MIN(assignment.due_at) NULLS LAST, 1
        LIMIT 5
        "#,
    )
    .bind(ws)
    .bind(zone)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let open_tasks_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_team_assignments
         WHERE workspace_id = $1 AND status = 'open'
           AND source_kind NOT IN ('daily_briefing','roster_weekly_brief')",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Coming production days ────────────────────────────────────────
    // The briefing composes before this sweep's capture-plan issue (it
    // must run ahead of the roster gate, and plan routing needs the
    // roster), so a plan issuing this cycle shows as "no plan" until
    // tomorrow's briefing — one sweep of staleness, never a wrong verdict.
    let production_days = sqlx::query_as::<_, ProductionDayRow>(
        r#"
        SELECT day.title, day.scheduled_for,
               (SELECT plan.status FROM viryaos_capture_plans plan
                WHERE plan.workspace_id = day.workspace_id
                  AND plan.production_event_id = day.id
                ORDER BY (plan.status IN ('draft','issued')) DESC,
                         plan.issued_at DESC NULLS LAST LIMIT 1) AS plan_status
        FROM viryaos_production_events day
        WHERE day.workspace_id = $1
          AND day.status IN ('scheduled','in_progress')
          AND day.scheduled_for >= $2
        ORDER BY day.scheduled_for
        LIMIT 3
        "#,
    )
    .bind(ws)
    .bind(local_date)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // Days just gone, with what they produced — the leverage number the
    // operator reads: landed pieces against the plan's shot list. A day
    // with no plan is silent about yield, not a zero.
    let recent_days = sqlx::query_as::<_, RecentDayRow>(
        r#"
        SELECT day.title, day.scheduled_for, plan.status AS plan_status,
               plan.sources_landed, plan.planned
        FROM viryaos_production_events day
        JOIN LATERAL (
            SELECT p.status, p.sources_landed,
                   jsonb_array_length(p.items)::int AS planned, p.issued_at
            FROM viryaos_capture_plans p
            WHERE p.workspace_id = day.workspace_id
              AND p.production_event_id = day.id
              AND p.status IN ('done','abandoned')
            ORDER BY p.issued_at DESC NULLS LAST
            LIMIT 1
        ) plan ON true
        WHERE day.workspace_id = $1
          AND day.scheduled_for < $2
          AND day.scheduled_for >= $2 - INTERVAL '7 days'
        ORDER BY day.scheduled_for DESC
        LIMIT 2
        "#,
    )
    .bind(ws)
    .bind(local_date)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── What changed in the last 24h ──────────────────────────────────
    let material_landed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_content_sources
         WHERE workspace_id = $1
           AND source_kind IN ('video','story','release','social_post')
           AND created_at > $2 - INTERVAL '24 hours'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let observations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_peer_observations
         WHERE workspace_id = $1 AND created_at > $2 - INTERVAL '24 hours'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let expired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_suggestion_outcomes
         WHERE workspace_id = $1 AND outcome = 'expired'
           AND resolved_at > $2 - INTERVAL '24 hours'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Concentration (§4b-4) ─────────────────────────────────────────
    // Share of new fans carried by the top channel, city and content
    // format. A flat spread — or zero conversions — is the honest answer
    // that nothing is compounding yet; the line renders either way rather
    // than only celebrating when a leader exists. The format share only
    // counts conversions whose promoted source was filed with a declared
    // format — the rest report "unrecorded", not a guessed bucket.
    let fans_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT fan_id) FROM fan_provenance_events
         WHERE workspace_id = $1 AND event_kind = 'conversion'
           AND occurred_at > $2 - INTERVAL '30 days'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let top_channel = sqlx::query_as::<_, TopShareRow>(
        "SELECT channel AS label, COUNT(DISTINCT fan_id) AS fans
         FROM fan_provenance_events
         WHERE workspace_id = $1 AND event_kind = 'conversion'
           AND occurred_at > $2 - INTERVAL '30 days'
         GROUP BY channel ORDER BY fans DESC, label LIMIT 1",
    )
    .bind(ws)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let top_city = sqlx::query_as::<_, TopShareRow>(
        "SELECT c.name AS label, COUNT(DISTINCT e.fan_id) AS fans
         FROM fan_provenance_events e
         JOIN fan_city_interests i
           ON i.workspace_id = e.workspace_id AND i.fan_id = e.fan_id
         JOIN cities c ON c.id = i.city_id
         WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
           AND e.occurred_at > $2 - INTERVAL '30 days'
         GROUP BY c.name ORDER BY fans DESC, label LIMIT 1",
    )
    .bind(ws)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let top_format = sqlx::query_as::<_, TopShareRow>(
        "SELECT format_key AS label, COUNT(DISTINCT fan_id) AS fans
         FROM fan_provenance_events
         WHERE workspace_id = $1 AND event_kind = 'conversion'
           AND occurred_at > $2 - INTERVAL '30 days'
           AND format_key IS NOT NULL
         GROUP BY format_key ORDER BY fans DESC, label LIMIT 1",
    )
    .bind(ws)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Assemble ──────────────────────────────────────────────────────
    let mut sections_map = serde_json::Map::new();
    sections_map.insert("arc_active".to_owned(), arc_line.is_some().into());
    let mut body = String::new();

    body.push_str(&format!(
        "{}: {}\n",
        frame.arc,
        arc_line.as_deref().unwrap_or(frame.no_arc)
    ));

    sections_map.insert("fans_30d".to_owned(), fans_30d.into());
    if fans_30d == 0 {
        body.push_str(&format!("{}: {}\n", frame.fans_label, frame.fans_flat));
    } else {
        // `channel` is NOT NULL, so a conversion always has a channel
        // share; the city share only appears when the converting fan
        // declared one — unattributed fans are absent, not zero.
        let mut shares: Vec<String> = Vec::new();
        for top in [&top_channel, &top_city, &top_format].into_iter().flatten() {
            shares.push(format!(
                "{} {}%",
                top.label,
                top.fans * 100 / fans_30d.max(1)
            ));
        }
        if top_format.is_none() {
            shares.push(frame.fans_format_unrecorded.to_owned());
        }
        body.push_str(&format!(
            "{}: {} — {}\n",
            frame.fans_label,
            fans_30d,
            shares.join(" · ")
        ));
    }

    if pending_total > 0 {
        sections_map.insert("pending_asks".to_owned(), pending_total.into());
        body.push_str(&format!("\n{} ({}):", frame.pending, pending_total));
        for ask in &asks {
            let summary = serde_json::from_value::<AutopilotActionPayload>(ask.payload.clone())
                .map(|payload| payload.briefing().localized(locale).summary)
                .unwrap_or_else(|_| super::team::friendly_action_title(&ask.action_kind, locale));
            let summary = if summary.chars().count() > 90 {
                format!("{}…", summary.chars().take(89).collect::<String>())
            } else {
                summary
            };
            match ask.expires_local {
                Some(expires) => {
                    body.push_str(&format!("\n- {summary} — {} {}", frame.due_label, expires))
                }
                None => body.push_str(&format!("\n- {summary}")),
            }
        }
        if pending_total > asks.len() as i64 {
            body.push_str(&format!("\n- … +{}", pending_total - asks.len() as i64));
        }
        body.push('\n');
    }

    if awaiting_report_total > 0 {
        sections_map.insert("awaiting_report".to_owned(), awaiting_report_total.into());
        body.push_str(&format!(
            "\n{} ({}):",
            frame.awaiting_report, awaiting_report_total
        ));
        for row in &awaiting_report {
            let concept = if row.concept.chars().count() > 90 {
                format!("{}…", row.concept.chars().take(89).collect::<String>())
            } else {
                row.concept.clone()
            };
            match row.suggested_before {
                Some(beat) => {
                    body.push_str(&format!("\n- {concept} — {} {beat}", frame.beat_label))
                }
                None => body.push_str(&format!("\n- {concept}")),
            }
        }
        if awaiting_report_total > awaiting_report.len() as i64 {
            body.push_str(&format!(
                "\n- … +{}",
                awaiting_report_total - awaiting_report.len() as i64
            ));
        }
        body.push('\n');
    }

    if !open_tasks.is_empty() {
        sections_map.insert("open_tasks".to_owned(), open_tasks_total.into());
        body.push_str(&format!("\n{} ({}):", frame.tasks, open_tasks_total));
        for row in &open_tasks {
            match row.nearest_due_local {
                Some(due) => body.push_str(&format!(
                    "\n- {}: {} — {} {}",
                    row.display_name, row.open_count, frame.due_label, due
                )),
                None => body.push_str(&format!("\n- {}: {}", row.display_name, row.open_count)),
            }
        }
        if open_tasks_total > open_tasks.iter().map(|row| row.open_count).sum::<i64>() {
            body.push_str("\n- …");
        }
        body.push('\n');
    }

    // ── Capacity (§4i-6) ────────────────────────────────────────────────
    // Asks handed out per member over the last seven days beside the
    // tenant's own weekly ceiling, plus the asks that expired never having
    // been assigned — the load and the dropped work, stated together.
    let weekly_ceiling: Option<i64> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'team_weekly_ask_ceiling'",
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .and_then(|value| value.trim().parse::<i64>().ok());
    let weekly_asks = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT COALESCE(member.display_name, member.normalized_email) AS display_name,
               COUNT(assignment.id) AS asks
        FROM viryaos_team_profiles profile
        JOIN workspace_members member
          ON member.workspace_id = profile.workspace_id
         AND member.id = profile.member_id
        LEFT JOIN viryaos_team_assignments assignment
          ON assignment.workspace_id = profile.workspace_id
         AND assignment.assignee_member_id = profile.member_id
         AND assignment.assigned_at >= $2 - INTERVAL '7 days'
         AND assignment.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
        WHERE profile.workspace_id = $1
          AND profile.active
          AND member.status = 'active'
        GROUP BY member.id, member.display_name, member.normalized_email
        ORDER BY asks DESC, 1
        LIMIT 6
        "#,
    )
    .bind(ws)
    .bind(now)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let dropped_asks: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM viryaos_autopilot_actions action
           WHERE action.workspace_id = $1
             AND action.status = 'cancelled'
             AND action.last_error_kind = 'approval_expired'
             AND action.finished_at >= $2 - INTERVAL '7 days'
             AND NOT EXISTS (
                 SELECT 1 FROM viryaos_team_assignments assignment
                 WHERE assignment.workspace_id = action.workspace_id
                   AND assignment.action_id = action.id)"#,
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    // §4i-6 "waiting": asks the router refused and nothing has picked up
    // since — the deferral audit row exists, no matching assignment landed,
    // and for action-backed asks the action still waits on approval. A past
    // deadline or a cancelled action moves the count to dropped, not here.
    let waiting_asks: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM (
             SELECT DISTINCT audit.target_type, audit.target_id
             FROM audit_events audit
             WHERE audit.workspace_id = $1
               AND audit.action = 'team.ask_deferred'
               AND audit.occurred_at >= $2 - INTERVAL '7 days'
               AND NOT EXISTS (
                   SELECT 1 FROM viryaos_team_assignments assignment
                   WHERE assignment.workspace_id = audit.workspace_id
                     AND (
                         (assignment.action_id IS NOT NULL
                          AND assignment.action_id::text = audit.metadata->>'action_id')
                         OR (assignment.source_kind = audit.metadata->>'source_kind'
                             AND assignment.source_id::text = audit.metadata->>'source_id'
                             AND assignment.source_ref IS NOT DISTINCT FROM audit.metadata->>'source_ref')
                     ))
               AND (audit.metadata->>'action_id' IS NULL OR EXISTS (
                   SELECT 1 FROM viryaos_autopilot_actions act
                   WHERE act.workspace_id = audit.workspace_id
                     AND act.id::text = audit.metadata->>'action_id'
                     AND act.status = 'awaiting_approval'))
               AND (audit.metadata->>'deadline_at' IS NULL
                    OR (audit.metadata->>'deadline_at')::timestamptz > $2)
           ) waiting"#,
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    // Non-action asks have no approval expiry to count them: a deferred ask
    // whose deadline passed unresolved is the "dropped and says why" case.
    let dropped_unassigned: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM (
             SELECT DISTINCT audit.target_type, audit.target_id
             FROM audit_events audit
             WHERE audit.workspace_id = $1
               AND audit.action = 'team.ask_deferred'
               AND audit.occurred_at >= $2 - INTERVAL '30 days'
               AND audit.metadata->>'action_id' IS NULL
               AND audit.metadata->>'deadline_at' IS NOT NULL
               AND (audit.metadata->>'deadline_at')::timestamptz <= $2
               AND NOT EXISTS (
                   SELECT 1 FROM viryaos_team_assignments assignment
                   WHERE assignment.workspace_id = audit.workspace_id
                     AND assignment.source_kind = audit.metadata->>'source_kind'
                     AND assignment.source_id::text = audit.metadata->>'source_id'
                     AND assignment.source_ref IS NOT DISTINCT FROM audit.metadata->>'source_ref')
           ) dropped"#,
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    if !weekly_asks.is_empty() || dropped_asks + dropped_unassigned > 0 || waiting_asks > 0 {
        sections_map.insert(
            "capacity_asks_7d".to_owned(),
            weekly_asks
                .iter()
                .map(|(_, asks)| *asks)
                .sum::<i64>()
                .into(),
        );
        if waiting_asks > 0 {
            sections_map.insert("capacity_waiting".to_owned(), waiting_asks.into());
        }
        let dropped_total = dropped_asks + dropped_unassigned;
        if dropped_total > 0 {
            sections_map.insert("capacity_dropped_7d".to_owned(), dropped_total.into());
        }
        body.push_str(&format!("\n{}:", frame.capacity));
        for (name, asks) in &weekly_asks {
            match weekly_ceiling {
                Some(ceiling) => body.push_str(&format!(
                    "\n- {name}: {asks} ({} {ceiling})",
                    frame.ceiling_label
                )),
                None => body.push_str(&format!("\n- {name}: {asks}")),
            }
        }
        if waiting_asks > 0 {
            body.push_str(&format!("\n- {waiting_asks} {}", frame.capacity_waiting));
        }
        if dropped_total > 0 {
            body.push_str(&format!("\n- {dropped_total} {}", frame.capacity_dropped));
        }
        body.push('\n');
    }

    if !production_days.is_empty() || !recent_days.is_empty() {
        sections_map.insert(
            "production_days".to_owned(),
            ((production_days.len() + recent_days.len()) as i64).into(),
        );
        body.push_str(&format!("\n{}:", frame.production));
        for day in &production_days {
            body.push_str(&format!(
                "\n- {} {} — {}",
                day.scheduled_for,
                day.title,
                day.plan_status.as_deref().unwrap_or(frame.no_plan)
            ));
        }
        for day in &recent_days {
            body.push_str(&format!(
                "\n- {} {} — {}",
                day.scheduled_for,
                day.title,
                day.plan_status.as_deref().unwrap_or(frame.no_plan)
            ));
            // A zero shot list is no denominator — "harvest 4/0" reads as
            // a bug, not as a plan that asked for nothing.
            match (day.sources_landed, day.planned) {
                (Some(landed), Some(planned)) if planned > 0 => {
                    body.push_str(&format!(" · {} {}/{}", frame.yield_label, landed, planned))
                }
                (Some(landed), _) => body.push_str(&format!(" · {} {}", frame.yield_label, landed)),
                _ => body.push_str(&format!(" · {}", frame.unmeasured)),
            }
        }
        body.push('\n');
    }

    if material_landed + observations + expired > 0 {
        sections_map.insert("material_landed_24h".to_owned(), material_landed.into());
        sections_map.insert("observations_24h".to_owned(), observations.into());
        sections_map.insert("expired_24h".to_owned(), expired.into());
        body.push_str(&format!(
            "\n{}: {}",
            frame.changed,
            frame
                .changed_counts
                .replacen("{}", &material_landed.to_string(), 1)
                .replacen("{}", &observations.to_string(), 1)
                .replacen("{}", &expired.to_string(), 1)
        ));
    }

    // The arc and fans lines are always present — the scoreboard is not
    // content. A quiet day is one where nothing else landed; the briefing
    // says so under the scoreboard rather than sending an empty page.
    if sections_map
        .keys()
        .all(|key| key == "arc_active" || key == "fans_30d")
    {
        body.push_str(&format!("\n{}", frame.nothing));
    }

    // The mail workflow slices bodies at 1800 chars; cut on a line
    // boundary so the briefing never ends mid-word. `match_indices` on an
    // ASCII newline always lands on a char boundary.
    if body.len() > BODY_LIMIT {
        let cut = body
            .match_indices('\n')
            .map(|(index, _)| index)
            .take_while(|index| *index <= BODY_LIMIT)
            .last()
            .unwrap_or(0);
        body.truncate(cut);
        body.push_str(&format!("\n…\n{}", frame.more_in_panel));
    }

    let title = format!("{} — {}", frame.title, local_date);
    Ok((title, body, serde_json::Value::Object(sections_map)))
}
