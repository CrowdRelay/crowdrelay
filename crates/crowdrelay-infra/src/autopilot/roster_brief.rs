//! The roster weekly brief's delivery half: one organisation, one brief,
//! issued once a week into the member workspaces' own handoff rail.
//!
//! `crate::roster_weekly_brief::act_briefs` measures the week; the admin
//! endpoint renders it on demand. What was missing is that nobody was ever
//! told it exists — a manager who does not curl the admin surface never saw
//! the page. This sweep is the cadence: on the issuing workspace's local
//! Monday morning it composes the same measured read into a durable
//! `roster_briefs` artifact, hands every owner/admin member of the
//! measured member workspaces a `roster_weekly_brief` assignment, and
//! queues the proven `team.assignment.email` action for each recipient —
//! the identical rail the daily briefing rides.
//!
//! Three decisions worth recording:
//!
//! * **Who issues.** Workers are per-workspace, so every member workspace's
//!   worker attempts the same org brief on its own local Monday. ISO weeks
//!   make the date label identical in every zone, and
//!   `UNIQUE (organization_id, local_date)` plus `ON CONFLICT DO NOTHING`
//!   makes exactly one attempt win — the earliest member's Monday morning.
//!   A recipient in a later zone still gets their email in their own
//!   morning: `queue_team_email_action` computes the quiet window per
//!   workspace, so delivery time follows the recipient's clock even though
//!   issue time followed the earliest member's.
//! * **Who receives.** Owner/admin members only. The brief names sibling
//!   acts' pending decisions and slips — that is label-management
//!   information, and `staff` members (local crew, not roster management)
//!   do not get it. One person holding admin in several workspaces gets
//!   one email — deduped on `normalized_email`, first email-capable
//!   workspace in member order sends it — but an assignment per workspace,
//!   because each workspace's task list is its own surface.
//! * **Who must be able to hear.** Same rule the daily briefing applies: a
//!   workspace without a live `team.email` capability cannot be told, so
//!   its members get no assignment — a row nobody is told about is a row
//!   nobody reads. An organisation where no member workspace can email
//!   gets no artifact at all, which is the honest answer rather than a
//!   brief written for nobody.

use std::collections::{HashMap, HashSet};

use super::team::{crew_locale_in_tx, crew_timezone_in_tx, queue_team_email_action};
use super::*;
use crowdrelay_application::autopilot::BriefingLocale;
use crowdrelay_domain::roster_weekly_brief::{ActBrief, RosterWeeklyBrief};

/// The local hour the roster brief issues at — the same morning boundary
/// the daily briefing uses, so the weekly page lands with the daily one
/// rather than at a second, competing hour.
const ROSTER_BRIEF_LOCAL_HOUR: i32 = 8;

/// The body rides the n8n mail workflow, which slices at 1800 chars —
/// the daily briefing keeps 1650 for subject and footer, and this keeps
/// a little more room because a roster line is longer than a briefing's.
/// One body serves both the artifact and the email: the row records what
/// was actually mailed, and the "+N more" trailer names the endpoint
/// that holds the unclipped list.
const BODY_BUDGET: usize = 1500;

/// An organisation wider than this is not issued in one pass — the same
/// bound `act_briefs` measures with, so the artifact and the measured
/// page can never disagree about who is on the roster.
const MAX_MEMBERS: i64 = 60;

/// The copy for one locale. Kept terse in the daily briefing's voice:
/// labels and counts, verdicts verbatim, no encouragement.
#[derive(Clone, Copy)]
struct RosterBriefFrame {
    title: &'static str,
    /// "{week}" — the issuer-local Monday the brief speaks for.
    week_label: &'static str,
    /// "{name}" and "{delta}" — the roster's North Star line: the weakest
    /// measured act's window growth, stated as the headline it is.
    weakest: &'static str,
    /// "{total}" — every measured act's delta summed.
    total: &'static str,
    /// "{n}" — acts without two North Star readings, named not folded.
    no_signal_count: &'static str,
    /// The whole-headline fallback when nothing is measurable.
    no_signal_headline: &'static str,
    pending: &'static str,
    slipped: &'static str,
    /// "{posture}" — the brain's verdict carried verbatim.
    posture_attention: &'static str,
    fans_delta: &'static str,
    no_briefing: &'static str,
    no_signal: &'static str,
    quiet: &'static str,
    /// "{n}" — acts dropped from the body by the budget; the full list is
    /// the admin endpoint the trailer names.
    more: &'static str,
}

const fn roster_frame(locale: BriefingLocale) -> RosterBriefFrame {
    match locale {
        BriefingLocale::Pl => RosterBriefFrame {
            title: "CrowdRelay — tygodniowy przegląd rostera",
            week_label: "Tydzień rostera od {week}: {acts} zespołów.",
            weakest: "Najsłabszy mierzalny: {name} ({delta} fanów, okno 60 dni).",
            total: "Suma mierzonych: {total}.",
            no_signal_count: "Bez sygnału: {n}.",
            no_signal_headline: "Za mało danych, żeby wskazać najsłabszy — roster jest młody.",
            pending: "{n} czeka na decyzję",
            slipped: "{n} przepadło",
            posture_attention: "postać {posture} — do sprawdzenia",
            fans_delta: "fani 60d: {delta}",
            no_briefing: "brak podsumowań",
            no_signal: "brak sygnału",
            quiet: "spokojnie",
            more: "+{n} więcej — pełna lista: /v1/admin/roster-plan/weekly-brief",
        },
        BriefingLocale::En => RosterBriefFrame {
            title: "CrowdRelay — weekly roster brief",
            week_label: "Roster week of {week}: {acts} acts.",
            weakest: "Weakest measured act: {name} ({delta} fans, 60d window).",
            total: "Measured total: {total}.",
            no_signal_count: "No signal yet: {n}.",
            no_signal_headline: "Too young to name a weakest act — not enough signal yet.",
            pending: "{n} waiting on a decision",
            slipped: "{n} slipped",
            posture_attention: "posture {posture} — needs a look",
            fans_delta: "fans 60d: {delta}",
            no_briefing: "no briefing yet",
            no_signal: "no signal",
            quiet: "quiet",
            more: "+{n} more — full list: /v1/admin/roster-plan/weekly-brief",
        },
    }
}

#[derive(Debug, FromRow)]
struct OrgMemberRow {
    id: Uuid,
}

#[derive(Debug, FromRow)]
struct RecipientRow {
    member_id: Uuid,
    workspace_id: Uuid,
    display_name: String,
    normalized_email: String,
}

impl PostgresAutopilotRepository {
    /// Issues the organisation's weekly roster brief on the first local
    /// weekday-morning cycle of this workspace's week — a worker down on
    /// Monday still catches up before Sunday closes the window. Returns the
    /// number of assignments created — `0` covers every skip (not an org,
    /// outside the window, already issued, nobody to tell) and the lost race.
    ///
    /// Two transactions, deliberately: the gate reads and the org-wide
    /// compose run before the write transaction opens, so the measured read
    /// (up to sixty workspaces' queues and North Star series) never holds a
    /// connection inside a transaction it does not need.
    pub async fn issue_roster_weekly_briefs(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let ws = workspace_id.into_uuid();
            let mut gate = self.pool.begin().await.map_err(map_sqlx)?;

            // A workspace without an organisation has no roster — the
            // cheapest possible exit, one indexed read per sweep.
            let organization_id: Option<Uuid> =
                sqlx::query_scalar("SELECT organization_id FROM workspaces WHERE id = $1")
                    .bind(ws)
                    .fetch_optional(&mut *gate)
                    .await
                    .map_err(map_sqlx)?
                    .flatten();
            let Some(organization_id) = organization_id else {
                gate.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            };

            // The issuing workspace's own clock decides the morning, the
            // same convention `issue_daily_briefings` follows — the brief
            // rides beside the daily page, not on a foreign timezone.
            let zone = crew_timezone_in_tx(&mut gate, workspace_id).await?;
            let (local_date, issue_date, local_hour, isodow): (time::Date, time::Date, i32, i32) =
                sqlx::query_as(
                    "SELECT ($1::timestamptz AT TIME ZONE $2)::date
                            - (EXTRACT(ISODOW FROM $1::timestamptz AT TIME ZONE $2)::int - 1),
                            ($1::timestamptz AT TIME ZONE $2)::date,
                            EXTRACT(HOUR FROM $1::timestamptz AT TIME ZONE $2)::int,
                            EXTRACT(ISODOW FROM $1::timestamptz AT TIME ZONE $2)::int",
                )
                .bind(now)
                .bind(&zone)
                .fetch_one(&mut *gate)
                .await
                .map_err(map_sqlx)?;
            if isodow == 7 || (isodow == 1 && local_hour < ROSTER_BRIEF_LOCAL_HOUR) {
                gate.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            // The week is already spoken for — the common path once one
            // member's worker has won, so it stays cheap.
            let already_issued: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM roster_briefs
                     WHERE organization_id = $1 AND local_date = $2)",
            )
            .bind(organization_id)
            .bind(local_date)
            .fetch_one(&mut *gate)
            .await
            .map_err(map_sqlx)?;
            if already_issued {
                gate.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            // The membership this brief speaks for — identical query and
            // bound to `act_briefs`, so the recipients and the measured
            // page are the same roster.
            let members = sqlx::query_as::<_, OrgMemberRow>(
                "SELECT id FROM workspaces
                 WHERE organization_id = $1
                 ORDER BY name, id
                 LIMIT $2",
            )
            .bind(organization_id)
            .bind(MAX_MEMBERS)
            .fetch_all(&mut *gate)
            .await
            .map_err(map_sqlx)?;

            // Capability is per workspace: the org brief can only ride the
            // workspaces that can actually send. None anywhere means no
            // artifact — same reasoning as the daily briefing's gate.
            let mut can_email: HashMap<Uuid, bool> = HashMap::with_capacity(members.len());
            for member in &members {
                let member_id = WorkspaceId::from_uuid(member.id);
                let available =
                    executor_capability_available(&mut gate, member_id, "team.email").await?;
                can_email.insert(member.id, available);
            }
            if !can_email.values().any(|available| *available) {
                gate.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            let issuer_locale = crew_locale_in_tx(&mut gate, workspace_id).await;
            gate.commit().await.map_err(map_sqlx)?;

            // The measured read, outside any transaction — sixty acts of
            // queues and series must not sit on a held connection.
            let acts =
                crate::roster_weekly_brief::act_briefs(&self.pool, organization_id, now).await?;
            let brief = crowdrelay_domain::roster_weekly_brief::compose(organization_id, now, acts);
            if brief.acts.is_empty() {
                // The organisation emptied between the gate and the read —
                // a brief about nobody is not issued.
                return Ok(0);
            }

            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

            // The artifact is the final arbiter of the member race: whoever
            // inserts owns this week's delivery.
            let brief_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO roster_briefs
                    (organization_id, local_date, title, body, sections)
                VALUES ($1,$2,$3,$4,$5)
                ON CONFLICT (organization_id, local_date) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(organization_id)
            .bind(local_date)
            .bind(compose_title(roster_frame(issuer_locale)))
            .bind(compose_body(
                &brief,
                local_date,
                roster_frame(issuer_locale),
            ))
            .bind(sections_json(&brief))
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            let Some(brief_id) = brief_id else {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            };

            let member_ids: Vec<Uuid> = members.iter().map(|member| member.id).collect();

            // A new week's brief supersedes the open ones — the manager's
            // task list shows this week's page, not last week's leftovers.
            // 'cancelled' carries no completed_at (the CHECK equates done
            // with a completion stamp), and the update stays inside the
            // member workspaces the brief speaks for.
            sqlx::query(
                "UPDATE team_assignments
                 SET status = 'cancelled', updated_at = now()
                 WHERE workspace_id = ANY($1)
                   AND source_kind = 'roster_weekly_brief'
                   AND status = 'open'
                   AND source_id <> $2",
            )
            .bind(&member_ids)
            .bind(brief_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            // Recipients: owner/admin members of the workspaces the brief
            // measured — intersected against live membership, so a workspace
            // that left the organisation mid-sweep does not hand its people
            // a brief about a roster they are no longer on.
            let recipients = sqlx::query_as::<_, RecipientRow>(
                r#"
                SELECT member.id AS member_id, member.workspace_id,
                       COALESCE(member.display_name, member.normalized_email) AS display_name,
                       member.normalized_email
                FROM workspace_members AS member
                WHERE member.workspace_id = ANY($1)
                  AND member.status = 'active'
                  AND member.role IN ('owner','admin')
                  AND EXISTS (
                      SELECT 1 FROM workspaces AS owned
                      WHERE owned.id = member.workspace_id
                        AND owned.organization_id = $2
                  )
                ORDER BY member.workspace_id, member.normalized_email
                "#,
            )
            .bind(&member_ids)
            .bind(organization_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            // Per-recipient-workspace copies: the artifact stays in the
            // issuer's locale; each emailed copy is composed in the
            // workspace the recipient manages, so a Polish act's owner is
            // not mailed in English because an English member's worker won
            // the race. Two locales at most — cache by tag.
            let mut copies: HashMap<&'static str, (String, String)> = HashMap::new();
            let mut emailed: HashSet<String> = HashSet::new();
            let mut issued = 0u32;
            for member in &members {
                if !can_email.get(&member.id).copied().unwrap_or(false) {
                    continue;
                }
                let member_ws = WorkspaceId::from_uuid(member.id);
                let member_zone = crew_timezone_in_tx(&mut tx, member_ws).await?;
                let member_locale = crew_locale_in_tx(&mut tx, member_ws).await;
                let copy_key = match member_locale {
                    BriefingLocale::Pl => "pl",
                    BriefingLocale::En => "en",
                };
                let (title, body) = copies
                    .entry(copy_key)
                    .or_insert_with(|| {
                        let frame = roster_frame(member_locale);
                        (
                            compose_title(frame),
                            compose_body(&brief, local_date, frame),
                        )
                    })
                    .clone();

                // Due at the end of the issue day in the recipient's own
                // zone — a read-today item even on a late catch-up,
                // matching the daily briefing's due semantics.
                let due_at: Option<OffsetDateTime> =
                    sqlx::query_scalar("SELECT (($1::date + 1)::timestamp AT TIME ZONE $2)")
                        .bind(issue_date)
                        .bind(&member_zone)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(map_sqlx)?;

                for recipient in recipients
                    .iter()
                    .filter(|recipient| recipient.workspace_id == member.id)
                {
                    let assignment_id = Uuid::now_v7();
                    let inserted = sqlx::query_scalar::<_, Uuid>(
                        r#"
                        INSERT INTO team_assignments (
                            id, workspace_id, action_id, source_kind, source_id, source_ref,
                            assignee_member_id, required_skill, due_at, next_reminder_at
                        ) VALUES ($1,$2,NULL,'roster_weekly_brief',$3,NULL,$4,'briefing',$5,NULL)
                        ON CONFLICT DO NOTHING
                        RETURNING id
                        "#,
                    )
                    .bind(assignment_id)
                    .bind(member.id)
                    .bind(brief_id)
                    .bind(recipient.member_id)
                    .bind(due_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(map_sqlx)?;
                    if inserted.is_none() {
                        continue;
                    }
                    issued = issued.saturating_add(1);
                    // One email per person across the whole organisation —
                    // an admin of three member workspaces reads the same
                    // brief once; the assignments keep every workspace's
                    // task list honest.
                    if emailed.insert(recipient.normalized_email.clone()) {
                        queue_team_email_action(
                            &mut tx,
                            member_ws,
                            assignment_id,
                            "roster",
                            &recipient.normalized_email,
                            &recipient.display_name,
                            title.clone(),
                            body.clone(),
                            due_at,
                            0,
                            None,
                            now,
                        )
                        .await?;
                    }
                }
            }

            tx.commit().await.map_err(map_sqlx)?;
            Ok(issued)
        })
        .await
    }
}

fn compose_title(frame: RosterBriefFrame) -> String {
    // The title names the artifact; the week is the first line of the body.
    // Kept under the column's 200 bound by construction.
    frame.title.to_owned()
}

/// The body: headline line, then one line per act in the domain's read
/// order, until the budget says stop — at which point the trailer says
/// where the rest lives rather than silently clipping.
fn compose_body(brief: &RosterWeeklyBrief, week: time::Date, frame: RosterBriefFrame) -> String {
    let mut body = frame
        .week_label
        .replace("{week}", &week.to_string())
        .replace("{acts}", &brief.acts.len().to_string());
    body.push(' ');

    match (
        brief.headline.weakest_act_name.as_deref(),
        brief.headline.weakest_act_growth,
        brief.headline.total_growth,
    ) {
        (Some(name), Some(delta), total) => {
            body.push_str(
                &frame
                    .weakest
                    .replace("{name}", name)
                    .replace("{delta}", &signed(delta)),
            );
            body.push(' ');
            if let Some(total) = total {
                body.push_str(&frame.total.replace("{total}", &signed(total)));
                body.push(' ');
            }
        }
        _ => {
            body.push_str(frame.no_signal_headline);
            body.push(' ');
        }
    }
    if brief.headline.acts_without_signal > 0 {
        body.push_str(
            &frame
                .no_signal_count
                .replace("{n}", &brief.headline.acts_without_signal.to_string()),
        );
        body.push(' ');
    }
    body.push('\n');

    for (index, act) in brief.acts.iter().enumerate() {
        let line = act_line(act, &frame);
        let remaining = brief.acts.len() - index;
        // The trailer costs its own room; reserve it so the last line plus
        // the trailer still fit the budget.
        let trailer = frame.more.replace("{n}", &remaining.to_string());
        if body.len() + line.len() + trailer.len() + 2 > BODY_BUDGET {
            body.push_str(&trailer);
            body.push('\n');
            break;
        }
        body.push_str(&line);
        body.push('\n');
    }
    body.trim_end().to_owned()
}

/// One act's clauses, joined into a single line. An act with nothing
/// measured is "quiet", not blank — the line exists so the manager sees
/// the act was looked at.
fn act_line(act: &ActBrief, frame: &RosterBriefFrame) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if act.pending_decisions > 0 {
        let mut clause = frame
            .pending
            .replace("{n}", &act.pending_decisions.to_string());
        if !act.pending.is_empty() {
            let kinds: Vec<&str> = act
                .pending
                .iter()
                .map(|item| item.action_kind.as_str())
                .collect();
            clause.push_str(&format!(" ({})", kinds.join(", ")));
        }
        clauses.push(clause);
    }
    if act.slipped > 0 {
        let mut clause = frame.slipped.replace("{n}", &act.slipped.to_string());
        // Distinct resolutions, first-seen order — `dedup` alone would only
        // collapse adjacent repeats, and the items arrive ordered by time.
        let mut seen = HashSet::new();
        let resolutions: Vec<&str> = act
            .slipped_items
            .iter()
            .map(|item| item.resolution.as_str())
            .filter(|resolution| seen.insert(*resolution))
            .collect();
        if !resolutions.is_empty() {
            clause.push_str(&format!(" ({})", resolutions.join(", ")));
        }
        clauses.push(clause);
    }
    if act.posture_needs_attention {
        clauses.push(frame.posture_attention.replace("{posture}", &act.posture));
    }
    match act.north_star_delta {
        Some(delta) => clauses.push(frame.fans_delta.replace("{delta}", &signed(delta))),
        None => clauses.push(frame.no_signal.to_owned()),
    }
    if act.latest_briefing_date.is_none() {
        clauses.push(frame.no_briefing.to_owned());
    }
    if clauses.is_empty() {
        clauses.push(frame.quiet.to_owned());
    }
    format!("{}: {}", act.name, clauses.join("; "))
}

/// The operator-facing counts behind the prose — same contract as the
/// daily briefing's `sections` jsonb.
fn sections_json(brief: &RosterWeeklyBrief) -> serde_json::Value {
    serde_json::json!({
        "acts": brief.acts.len(),
        "acts_with_pending": brief.acts.iter().filter(|act| act.pending_decisions > 0).count(),
        "pending_total": brief.acts.iter().map(|act| act.pending_decisions).sum::<u32>(),
        "slipped_total": brief.acts.iter().map(|act| act.slipped).sum::<u32>(),
        "acts_needing_attention": brief
            .acts
            .iter()
            .filter(|act| act.posture_needs_attention)
            .count(),
        "acts_without_signal": brief.headline.acts_without_signal,
        "weakest_act_growth": brief.headline.weakest_act_growth,
        "total_growth": brief.headline.total_growth,
    })
}

/// `+N` for a positive delta — the sign is information, and a growth of
/// zero reads `0`, not `+0`.
fn signed(value: i64) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
    }
}
