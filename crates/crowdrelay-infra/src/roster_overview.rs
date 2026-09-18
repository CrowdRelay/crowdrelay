//! The measured half of the roster view (5.5): one organisation's acts, each
//! with its attention spend, its pipeline and the raw fields the gaps read.
//!
//! Everything here is composed rather than re-derived:
//!
//! * **Pending** is the same `awaiting_approval`-with-an-open-window
//!   predicate the weekly brief and the attention queue use, so the three
//!   surfaces can never disagree about an act's queue depth.
//! * **Attention** reads the governor and the touch ledger 5.17 added —
//!   `contacts_on_hold` counts rows still inside `next_contact_after`,
//!   `do_not_contact` counts doors closed, and `touches_30d` is the act's
//!   trailing-30-day spend of the org-wide budget.
//! * **Pipeline** counts published shows ahead of `now` and names the
//!   nearest — the calendar a manager can act on, not a history.
//! * **Reachable fans** is the send-path count: `active` fans whose latest
//!   `marketing` consent is a grant — the same `latest_marketing` discipline
//!   the measurement claims use, so a churned or revoked fan does not inflate
//!   the audience the page reports.
//!
//! Gap naming and row order live in the domain's
//! ([`crowdrelay_domain::roster_overview::compose`]); this module measures,
//! it does not judge.

use std::collections::HashMap;

use crowdrelay_application::RepositoryError;
use crowdrelay_domain::roster_overview::{ActAttention, ActOverview, ActPipeline, RosterOverview};
use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

/// A roster wider than this is not read in one pass — the same bound every
/// sibling read carries, for the same reason.
const MAX_ACTS: i64 = 60;

/// The trailing window the attention spend counts, in days — the same month
/// the org budget enforces over.
const TOUCH_WINDOW_DAYS: i64 = 30;

#[derive(Debug, sqlx::FromRow)]
struct MemberRow {
    id: Uuid,
    name: String,
}

/// One act's row, measured. Returned in member order (name, then id);
/// sequencing into read order is the domain compose's job.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn roster_overview(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<RosterOverview, RepositoryError> {
    let members = sqlx::query_as::<_, MemberRow>(
        r#"
        SELECT workspace.id, workspace.name
        FROM workspaces AS workspace
        WHERE workspace.organization_id = $1
        ORDER BY workspace.name, workspace.id
        LIMIT $2
        "#,
    )
    .bind(organization_id)
    .bind(MAX_ACTS)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    if members.is_empty() {
        return Ok(crowdrelay_domain::roster_overview::compose(
            organization_id,
            now,
            0,
            Vec::new(),
        ));
    }
    let member_ids: Vec<Uuid> = members.iter().map(|member| member.id).collect();

    // Attention: each act's trailing-month spend of the shared budget, plus
    // the org total the per-act numbers are shares of.
    let touch_rows = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT touch.workspace_id, count(*) AS touches
        FROM viryaos_contact_touches AS touch
        WHERE touch.workspace_id = ANY($1)
          AND touch.touched_at >= $2 - ($3::int * INTERVAL '1 day')
        GROUP BY touch.workspace_id
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .bind(TOUCH_WINDOW_DAYS)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Attention: governor rows still cooling down, and doors closed.
    let governor_rows = sqlx::query_as::<_, (Uuid, i64, i64)>(
        r#"
        SELECT governor.workspace_id,
               count(*) FILTER (WHERE governor.next_contact_after > $2) AS on_hold,
               count(*) FILTER (WHERE governor.do_not_contact) AS blocked
        FROM viryaos_contact_governor AS governor
        WHERE governor.workspace_id = ANY($1)
        GROUP BY governor.workspace_id
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Pipeline: approvals waiting on a human, window still open — the same
    // predicate the brief and the attention queue share.
    let pending_rows = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT action.workspace_id, count(*) AS pending
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = ANY($1)
          AND action.status = 'awaiting_approval'
          AND (action.approval_expires_at IS NULL OR action.approval_expires_at > $2)
        GROUP BY action.workspace_id
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Pipeline: the published shows ahead, and the nearest one's date.
    let show_rows = sqlx::query_as::<_, (Uuid, i64, Option<OffsetDateTime>)>(
        r#"
        SELECT event.workspace_id, count(*) AS upcoming,
               min(event.starts_at) AS next_show_at
        FROM events AS event
        WHERE event.workspace_id = ANY($1)
          AND event.status = 'published'
          AND event.starts_at > $2
        GROUP BY event.workspace_id
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // The newest day each act's briefing spoke for — absent is `None`, the
    // honest "nothing yet" the `no_briefing` gap reads.
    let briefing_rows = sqlx::query_as::<_, (Uuid, Date)>(
        r#"
        SELECT briefing.workspace_id, max(briefing.local_date) AS latest_briefing_date
        FROM viryaos_daily_briefings AS briefing
        WHERE briefing.workspace_id = ANY($1)
        GROUP BY briefing.workspace_id
        "#,
    )
    .bind(&member_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // The send-path count per act: active fans whose latest `marketing`
    // consent is a grant — the same latest-grant discipline the measurement
    // claims apply, scoped per workspace across the member set.
    let fan_rows = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        WITH latest_marketing AS (
            SELECT DISTINCT ON (consent.workspace_id, consent.fan_id)
                   consent.workspace_id, consent.fan_id, consent.granted
            FROM fan_consents AS consent
            WHERE consent.workspace_id = ANY($1)
              AND consent.purpose = 'marketing'
            ORDER BY consent.workspace_id, consent.fan_id,
                     consent.recorded_at DESC, consent.id DESC
        )
        SELECT fan.workspace_id, count(*) AS reachable
        FROM fans AS fan
        JOIN latest_marketing AS lm
          ON lm.workspace_id = fan.workspace_id
         AND lm.fan_id = fan.id
         AND lm.granted
        WHERE fan.workspace_id = ANY($1)
          AND fan.status = 'active'
        GROUP BY fan.workspace_id
        "#,
    )
    .bind(&member_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let touches_by_act: HashMap<Uuid, i64> = touch_rows.into_iter().collect();
    let governor_by_act: HashMap<Uuid, (i64, i64)> = governor_rows
        .into_iter()
        .map(|(id, on_hold, blocked)| (id, (on_hold, blocked)))
        .collect();
    let pending_by_act: HashMap<Uuid, i64> = pending_rows.into_iter().collect();
    let shows_by_act: HashMap<Uuid, (i64, Option<OffsetDateTime>)> = show_rows
        .into_iter()
        .map(|(id, upcoming, next_show_at)| (id, (upcoming, next_show_at)))
        .collect();
    let briefing_by_act: HashMap<Uuid, Date> = briefing_rows.into_iter().collect();
    let fans_by_act: HashMap<Uuid, i64> = fan_rows.into_iter().collect();

    let org_touches: i64 = touches_by_act.values().sum();

    let acts = members
        .into_iter()
        .map(|member| {
            let (on_hold, blocked) = governor_by_act.get(&member.id).copied().unwrap_or((0, 0));
            let (upcoming, next_show_at) =
                shows_by_act.get(&member.id).copied().unwrap_or((0, None));
            ActOverview {
                workspace_id: crowdrelay_domain::WorkspaceId::from_uuid(member.id),
                name: member.name,
                attention: ActAttention {
                    touches_30d: clamp(touches_by_act.get(&member.id).copied().unwrap_or(0)),
                    contacts_on_hold: clamp(on_hold),
                    do_not_contact: clamp(blocked),
                },
                pipeline: ActPipeline {
                    pending_decisions: clamp(pending_by_act.get(&member.id).copied().unwrap_or(0)),
                    upcoming_shows: clamp(upcoming),
                    next_show_at,
                },
                latest_briefing_date: briefing_by_act.get(&member.id).copied(),
                reachable_fans: clamp(fans_by_act.get(&member.id).copied().unwrap_or(0)),
                gaps: Vec::new(),
            }
        })
        .collect();

    Ok(crowdrelay_domain::roster_overview::compose(
        organization_id,
        now,
        clamp(org_touches),
        acts,
    ))
}

/// A count from Postgres is an `i64` and cannot be negative here; saturating
/// rather than erroring keeps one impossible row from failing the whole read.
fn clamp(count: i64) -> u32 {
    u32::try_from(count.max(0)).unwrap_or(u32::MAX)
}

/// Narrows a database failure to the repository's error vocabulary — the same
/// mapping every sibling module applies, so the read fails the way the rest
/// of the surface fails.
fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}
