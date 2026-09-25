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

mod artifact_ask;
mod growth_pulse;
mod join_ask_setup;

use super::{team::queue_team_email_action, *};
use artifact_ask::distinguish_artifact_ask;
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
    /// The archive-recovery half of the scoreboard — `fans_label` is
    /// acquisition only, so a bulk-promoted wave of old contacts never
    /// reads as freshly won growth.
    recovered_label: &'static str,
    /// §5: the weekly join-ask scoreboard — clicks the tracked link earned
    /// and the fans those clicks produced, per platform. Absent entirely
    /// when no join-ask post exists: unmeasured is not zero.
    join_ask_label: &'static str,
    /// §5: what the join-ask needs from a person before it can run at all.
    ///
    /// The scoreboard above reports a loop that works. This reports one that
    /// cannot start — and it is the line a workspace nobody has set up is
    /// the *only* line it would otherwise get, because every other section
    /// here measures activity a cold tenant has none of.
    join_ask_setup_label: &'static str,
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
            title: "CrowdRelay — poranne podsumowanie",
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
            fans_label: "nowi fani (30 dni)",
            recovered_label: "odzyskani z archiwum (30 dni)",
            join_ask_label: "dołącz-do-nas (7 dni)",
            join_ask_setup_label: "dołącz-do-nas — do ustawienia",
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
            title: "CrowdRelay — morning briefing",
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
            fans_label: "new fans (30d)",
            recovered_label: "recovered from archive (30d)",
            join_ask_label: "join-ask (7d)",
            join_ask_setup_label: "join-ask — setup needed",
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
    id: Uuid,
    action_kind: String,
    expires_local: Option<time::Date>,
    approval_expires_at: Option<OffsetDateTime>,
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
    // Read outside the transaction on purpose: the readiness line is
    // advisory, it reads settings rows this sweep does not write, and
    // routing it through the same pool-backed assembler the cycle and the
    // attention board use is what keeps all three naming the same blocker.
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    locale: BriefingLocale,
    approval_links: Option<&ApprovalLinkMinter>,
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
        UPDATE team_assignments assignment
        SET status = 'cancelled', updated_at = now()
        FROM daily_briefings briefing
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
             SELECT 1 FROM daily_briefings
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

    let (title, body, sections, pending_links) = compose_briefing(
        tx,
        pool,
        workspace_id,
        local_date,
        now,
        locale,
        &zone,
        approval_links,
    )
    .await?;

    let briefing_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO daily_briefings
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
            INSERT INTO team_assignments (
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
            super::team::EmailApprovalLinks {
                direct: None,
                pending: pending_links.clone(),
            },
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
#[allow(clippy::too_many_arguments)]
async fn compose_briefing(
    tx: &mut Transaction<'_, Postgres>,
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    local_date: time::Date,
    now: OffsetDateTime,
    locale: BriefingLocale,
    zone: &str,
    approval_links: Option<&ApprovalLinkMinter>,
) -> Result<(String, String, serde_json::Value, Vec<PendingApprovalLink>), RepositoryError> {
    let ws = workspace_id.into_uuid();
    let frame = &briefing_frame(locale);

    // ── The active arc ────────────────────────────────────────────────
    let arc = sqlx::query_as::<_, (String, serde_json::Value, Option<time::Date>)>(
        r#"
        SELECT title, spine, horizon_end
        FROM arcs
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
        SELECT id, action_kind,
               (approval_expires_at AT TIME ZONE $3)::date AS expires_local,
               approval_expires_at,
               payload
        FROM autopilot_actions
        WHERE workspace_id = $1 AND status = 'awaiting_approval'
          AND (approval_expires_at IS NULL OR approval_expires_at > $2)
          -- Batched relay deliveries ask through the batch card, not here.
          AND NOT (action_kind = 'community.engage.request'
                   AND payload ->> 'source_id' IS NOT NULL)
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
        "SELECT COUNT(*) FROM autopilot_actions
         WHERE workspace_id = $1 AND status = 'awaiting_approval'
           AND (approval_expires_at IS NULL OR approval_expires_at > $2)
           AND NOT (action_kind = 'community.engage.request'
                    AND payload ->> 'source_id' IS NOT NULL)",
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
        FROM content_suggestions
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
        "SELECT COUNT(*) FROM content_suggestions
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
        FROM team_assignments assignment
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
        "SELECT COUNT(*) FROM team_assignments
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
               (SELECT plan.status FROM capture_plans plan
                WHERE plan.workspace_id = day.workspace_id
                  AND plan.production_event_id = day.id
                ORDER BY (plan.status IN ('draft','issued')) DESC,
                         plan.issued_at DESC NULLS LAST LIMIT 1) AS plan_status
        FROM production_events day
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
        FROM production_events day
        JOIN LATERAL (
            SELECT p.status, p.sources_landed,
                   jsonb_array_length(p.items)::int AS planned, p.issued_at
            FROM capture_plans p
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
        "SELECT COUNT(*) FROM content_sources
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
        "SELECT COUNT(*) FROM peer_observations
         WHERE workspace_id = $1 AND created_at > $2 - INTERVAL '24 hours'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let expired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM suggestion_outcomes
         WHERE workspace_id = $1 AND outcome = 'expired'
           AND resolved_at > $2 - INTERVAL '24 hours'",
    )
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // ── Concentration (§4b-4, §4.5) ────────────────────────────────────
    // Share of new fans carried by the top channel, city and content
    // format. A flat spread — or zero conversions — is the honest answer
    // that nothing is compounding yet; the line renders either way rather
    // than only celebrating when a leader exists. The format share only
    // counts conversions whose promoted source was filed with a declared
    // format — the rest report "unrecorded", not a guessed bucket.
    //
    // §4.5: acquisition and recovery are different statements and never
    // share a line. A fan whose acquisition event carries an archive token
    // — any `+`-separated part of `fan_import:gdrive+gmail` — was recovered
    // from the archive the tenant already had, not won by a channel; the
    // windfall of a bulk promote must not read as growth. Only confirmed
    // fans count (`fans.status = 'active'`) — an imported `pending` row is
    // an address we wrote to, not a fan.
    const ARCHIVE_ARRIVAL: &str = "EXISTS (
        SELECT 1
        FROM fan_acquisition_events acq
        CROSS JOIN LATERAL unnest(string_to_array(
            regexp_replace(acq.source, '^fan_import:', ''), '+')) AS tok
        WHERE acq.workspace_id = e.workspace_id
          AND acq.fan_id = e.fan_id
          AND tok IN ('gdrive','gmail','github','csv','sheet')
    )";
    let fans_30d: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(DISTINCT e.fan_id) FROM fan_provenance_events e
             JOIN fans f ON f.workspace_id = e.workspace_id AND f.id = e.fan_id
             WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
               AND e.occurred_at > $2 - INTERVAL '30 days'
               AND f.status = 'active'
               AND NOT {ARCHIVE_ARRIVAL}"
    ))
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let recovered_30d: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(DISTINCT e.fan_id) FROM fan_provenance_events e
             JOIN fans f ON f.workspace_id = e.workspace_id AND f.id = e.fan_id
             WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
               AND e.occurred_at > $2 - INTERVAL '30 days'
               AND f.status = 'active'
               AND {ARCHIVE_ARRIVAL}"
    ))
    .bind(ws)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    // The shares describe the acquisition line they hang off, so all three
    // tops count the same population — recovered fans belong to neither the
    // count nor its composition.
    let top_channel = sqlx::query_as::<_, TopShareRow>(&format!(
        "SELECT e.channel AS label, COUNT(DISTINCT e.fan_id) AS fans
             FROM fan_provenance_events e
             JOIN fans f ON f.workspace_id = e.workspace_id AND f.id = e.fan_id
             WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
               AND e.occurred_at > $2 - INTERVAL '30 days'
               AND f.status = 'active'
               AND NOT {ARCHIVE_ARRIVAL}
             GROUP BY e.channel ORDER BY fans DESC, label LIMIT 1"
    ))
    .bind(ws)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let top_city = sqlx::query_as::<_, TopShareRow>(&format!(
        "SELECT c.name AS label, COUNT(DISTINCT e.fan_id) AS fans
             FROM fan_provenance_events e
             JOIN fans f ON f.workspace_id = e.workspace_id AND f.id = e.fan_id
             JOIN fan_city_interests i
               ON i.workspace_id = e.workspace_id AND i.fan_id = e.fan_id
             JOIN cities c ON c.id = i.city_id
             WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
               AND e.occurred_at > $2 - INTERVAL '30 days'
               AND f.status = 'active'
               AND NOT {ARCHIVE_ARRIVAL}
             GROUP BY c.name ORDER BY fans DESC, label LIMIT 1"
    ))
    .bind(ws)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let top_format = sqlx::query_as::<_, TopShareRow>(&format!(
        "SELECT e.format_key AS label, COUNT(DISTINCT e.fan_id) AS fans
             FROM fan_provenance_events e
             JOIN fans f ON f.workspace_id = e.workspace_id AND f.id = e.fan_id
             WHERE e.workspace_id = $1 AND e.event_kind = 'conversion'
               AND e.occurred_at > $2 - INTERVAL '30 days'
               AND e.format_key IS NOT NULL
               AND f.status = 'active'
               AND NOT {ARCHIVE_ARRIVAL}
             GROUP BY e.format_key ORDER BY fans DESC, label LIMIT 1"
    ))
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

    // The recovery line renders even at zero — "we recovered nobody this
    // month" is the state the archive button exists to change, and hiding
    // the line would make the split look unmeasured.
    sections_map.insert("recovered_30d".to_owned(), recovered_30d.into());
    body.push_str(&format!("{}: {}\n", frame.recovered_label, recovered_30d));

    // §5: the join-ask's own scoreboard — clicks the tracked link earned in
    // the last 7 days and the fans those clicks produced, per platform.
    // `fan_acquisition_events.anonymous_visitor_id` carries the same visitor
    // id `click_events` records, so the click→signup join is expressible: a
    // fan counts when their acquisition event names a visitor who clicked a
    // join-ask link. The line itself is absent when no join-ask post exists
    // at all — unmeasured is not zero.
    let join_ask_rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT post.platform,
                COUNT(click.id) AS clicks,
                COUNT(DISTINCT acq.fan_id) AS fans
         FROM social_posts post
         JOIN autopilot_actions act
           ON act.workspace_id = post.workspace_id
          AND act.id = post.action_id
          AND act.action_kind = 'social.join_ask.publish'
         JOIN smart_links link
           ON link.workspace_id = post.workspace_id
          AND link.id = post.smart_link_id
         LEFT JOIN click_events click
           ON click.workspace_id = post.workspace_id
          AND click.smart_link_id = link.id
          AND click.occurred_at > $2 - INTERVAL '7 days'
         LEFT JOIN fan_acquisition_events acq
           ON acq.workspace_id = click.workspace_id
          AND click.anonymous_visitor_id IS NOT NULL
          AND acq.anonymous_visitor_id = click.anonymous_visitor_id
          AND acq.occurred_at >= click.occurred_at
         WHERE post.workspace_id = $1
         GROUP BY post.platform
         ORDER BY post.platform",
    )
    .bind(ws)
    .bind(now)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if !join_ask_rows.is_empty() {
        let clicks_total: i64 = join_ask_rows.iter().map(|row| row.1).sum();
        let fans_total: i64 = join_ask_rows.iter().map(|row| row.2).sum();
        sections_map.insert(
            "join_ask_7d".to_owned(),
            serde_json::json!({"clicks": clicks_total, "fans": fans_total}),
        );
        let platform_label = |platform: &str| -> String {
            match platform {
                "facebook" => "FB".to_owned(),
                "instagram" => "IG".to_owned(),
                other => other.to_owned(),
            }
        };
        let clicks_word = match locale {
            BriefingLocale::Pl => "klik.",
            BriefingLocale::En => "clicks",
        };
        let fans_word = match locale {
            BriefingLocale::Pl => "fan",
            BriefingLocale::En => "fans",
        };
        let parts: Vec<String> = join_ask_rows
            .iter()
            .map(|(platform, clicks, fans)| {
                format!(
                    "{} {clicks} {clicks_word} · {fans} {fans_word}",
                    platform_label(platform)
                )
            })
            .collect();
        body.push_str(&format!(
            "{}: {}\n",
            frame.join_ask_label,
            parts.join(" | ")
        ));
    }

    // §5: what the join-ask needs before it can run at all — see the
    // submodule for why this is the line a cold tenant most needs.
    join_ask_setup::append_join_ask_setup(
        pool,
        ws,
        locale,
        frame.join_ask_setup_label,
        &mut sections_map,
        &mut body,
    )
    .await;

    if pending_total > 0 {
        sections_map.insert("pending_asks".to_owned(), pending_total.into());
        body.push_str(&format!("\n{} ({}):", frame.pending, pending_total));
        for ask in &asks {
            let summary = serde_json::from_value::<AutopilotActionPayload>(ask.payload.clone())
                .map(|payload| payload.briefing().localized(locale).summary)
                .unwrap_or_else(|_| super::team::friendly_action_title(&ask.action_kind, locale));
            let summary = distinguish_artifact_ask(&ask.action_kind, &ask.payload, summary);
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

    growth_pulse::growth_pulse_lines(tx, ws, locale, &mut sections_map, &mut body).await?;

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
    //
    // The same fallback the router applies, for the same reason: a briefing
    // that reports "no ceiling" while the router is enforcing one tells the
    // operator the opposite of what is happening to their crew.
    let weekly_ceiling: Option<i64> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'team_weekly_ask_ceiling'",
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .and_then(|value| value.trim().parse::<i64>().ok())
    .filter(|ceiling| (1..=500).contains(ceiling))
    .or(Some(i64::from(
        crowdrelay_domain::team_operations::DEFAULT_WEEKLY_ASK_CEILING,
    )));
    let weekly_asks = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT COALESCE(member.display_name, member.normalized_email) AS display_name,
               COUNT(assignment.id) AS asks
        FROM team_profiles profile
        JOIN workspace_members member
          ON member.workspace_id = profile.workspace_id
         AND member.id = profile.member_id
        LEFT JOIN team_assignments assignment
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
        r#"SELECT COUNT(*) FROM autopilot_actions action
           WHERE action.workspace_id = $1
             AND action.status = 'cancelled'
             AND action.last_error_kind = 'approval_expired'
             AND action.finished_at >= $2 - INTERVAL '7 days'
             AND NOT EXISTS (
                 SELECT 1 FROM team_assignments assignment
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
                   SELECT 1 FROM team_assignments assignment
                   WHERE assignment.workspace_id = audit.workspace_id
                     AND (
                         (assignment.action_id IS NOT NULL
                          AND assignment.action_id::text = audit.metadata->>'action_id')
                         OR (assignment.source_kind = audit.metadata->>'source_kind'
                             AND assignment.source_id::text = audit.metadata->>'source_id'
                             AND assignment.source_ref IS NOT DISTINCT FROM audit.metadata->>'source_ref')
                     ))
               AND (audit.metadata->>'action_id' IS NULL OR EXISTS (
                   SELECT 1 FROM autopilot_actions act
                   WHERE act.workspace_id = audit.workspace_id
                     AND act.id::text = audit.metadata->>'action_id'
                     AND act.status = 'awaiting_approval'))
               AND (audit.metadata->>'deadline_at' IS NULL
                    OR (CASE jsonb_typeof(audit.metadata->'deadline_at') WHEN 'string' THEN (audit.metadata->>'deadline_at')::timestamptz
                      -- pre-2026-09-24 rows hold serde's tuple [y, ordinal day, h, m, s, ns, offset h/m/s]
                      WHEN 'array' THEN make_timestamptz((audit.metadata->'deadline_at'->>0)::int, 1, 1, (audit.metadata->'deadline_at'->>2)::int, (audit.metadata->'deadline_at'->>3)::int, (audit.metadata->'deadline_at'->>4)::double precision, 'UTC')
                          + ((audit.metadata->'deadline_at'->>1)::int - 1) * INTERVAL '1 day' - make_interval(hours => (audit.metadata->'deadline_at'->>6)::int, mins => (audit.metadata->'deadline_at'->>7)::int) END) > $2)
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
               AND (CASE jsonb_typeof(audit.metadata->'deadline_at') WHEN 'string' THEN (audit.metadata->>'deadline_at')::timestamptz
                      -- pre-2026-09-24 rows hold serde's tuple [y, ordinal day, h, m, s, ns, offset h/m/s]
                      WHEN 'array' THEN make_timestamptz((audit.metadata->'deadline_at'->>0)::int, 1, 1, (audit.metadata->'deadline_at'->>2)::int, (audit.metadata->'deadline_at'->>3)::int, (audit.metadata->'deadline_at'->>4)::double precision, 'UTC')
                          + ((audit.metadata->'deadline_at'->>1)::int - 1) * INTERVAL '1 day' - make_interval(hours => (audit.metadata->'deadline_at'->>6)::int, mins => (audit.metadata->'deadline_at'->>7)::int) END) <= $2
               AND NOT EXISTS (
                   SELECT 1 FROM team_assignments assignment
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

    // `weekly_asks` carries a row for every active profile, zero or not, so
    // "non-empty" is true for any tenant with a crew — and a briefing only
    // issues when someone is on the email path. Gating on it made the block
    // (and its `capacity_asks_7d` key) permanent, which kept the quiet-day
    // line below from ever printing. An all-zero scoreboard is not news; the
    // block renders when there is load to show. Once it renders, the zero
    // rows stay in — who is free is the other half of who is loaded.
    let dropped_total = dropped_asks + dropped_unassigned;
    let any_asks = weekly_asks.iter().any(|(_, asks)| *asks > 0);
    if any_asks || dropped_total > 0 || waiting_asks > 0 {
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
        .all(|key| key == "arc_active" || key == "fans_30d" || key == "recovered_30d")
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

    // §6-C: one link pair per listed ask, minted against each action's own
    // expiry. An ask without an expiry gets no link — it is decided in the
    // panel, not from a mail that can never lapse.
    let pending_links: Vec<PendingApprovalLink> = asks
        .iter()
        .filter_map(|ask| {
            approval_links
                .and_then(|minter| minter.mint(ask.id, None, ask.approval_expires_at))
                .map(|(approve_url, skip_url)| PendingApprovalLink {
                    action_id: ask.id,
                    approve_url,
                    skip_url,
                })
        })
        .collect();

    let title = format!("{} — {}", frame.title, local_date);
    Ok((
        title,
        body,
        serde_json::Value::Object(sections_map),
        pending_links,
    ))
}
