//! The measured half of the roster weekly brief: one organisation's acts,
//! each with its open decisions, its asks that died waiting, the brain's own
//! verdict and the day its last briefing spoke for.
//!
//! Everything here is composed rather than re-derived:
//!
//! * **Pending** is the same predicate the daily briefing and the attention
//!   queue use — `awaiting_approval` with a window still open, so an ask
//!   whose deadline lapsed is dead work, not a decision. Each act's rows are
//!   counted in SQL (`count(*) OVER`), because a displayed "top few" beside
//!   a truncated count would be two different truths.
//! * **Slipped** is the pair of resolutions the claim path writes when an
//!   ask dies unanswered — `approval_expired` for a window nobody answered,
//!   `insufficient_evidence` for a proposal cleared because it could not be
//!   answered — kept distinct verbatim rather than merged into "dropped".
//! * **Posture** is the brain's own self-assessment, computed by the same
//!   `daily_north_star` series + `self_assessment::assess` the attention
//!   read runs per workspace — the page reports the verdict, it does not
//!   grow a second drift detector.
//! * **Briefing date** is the newest `local_date` the act issued. Absent is
//!   `None` — "nothing yet" — which the page renders as such.
//!
//! The order the acts are read in is the domain's
//! ([`crowdrelay_domain::roster_weekly_brief::compose`]); this module
//! measures, it does not sequence.

use std::collections::HashMap;

use crowdrelay_application::RepositoryError;
use crowdrelay_domain::roster_weekly_brief::{
    ActBrief, MAX_ITEMS_SHOWN, PendingItem, SlippedItem, WINDOW_DAYS,
};
use sqlx::{FromRow, PgPool};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

/// A roster wider than this is not read in one pass — the same bound the
/// roster plan and the pooled channel read carry, for the same reason: one
/// request holding a connection open across an unbounded number of acts is
/// how a read becomes an outage.
const MAX_ACTS: i64 = 60;

#[derive(Debug, FromRow)]
struct MemberRow {
    id: Uuid,
    name: String,
}

#[derive(Debug, FromRow)]
struct PendingRow {
    workspace_id: Uuid,
    action_id: Uuid,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    approval_expires_at: Option<OffsetDateTime>,
    pending_total: i64,
}

#[derive(Debug, FromRow)]
struct SlippedRow {
    workspace_id: Uuid,
    action_id: Uuid,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    resolution: String,
    finished_at: OffsetDateTime,
    slipped_total: i64,
}

/// Every act in the organisation, measured. Returned in member order
/// (name, then id); sequencing into read order is the domain compose's job.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn act_briefs(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<ActBrief>, RepositoryError> {
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
        return Ok(Vec::new());
    }
    let member_ids: Vec<Uuid> = members.iter().map(|member| member.id).collect();

    // The queue, counted per act in one pass. `row_number` picks the first
    // few to show while `count(*) OVER` keeps the true depth — an act with
    // nine pending reads "9" beside the four it lists, never "4".
    let pending_rows = sqlx::query_as::<_, PendingRow>(
        r#"
        SELECT pending.workspace_id, pending.action_id, pending.action_kind,
               pending.subject_kind, pending.subject_id, pending.approval_expires_at,
               pending.pending_total
        FROM (
            SELECT action.workspace_id, action.id AS action_id, action.action_kind,
                   action.subject_kind, action.subject_id, action.approval_expires_at,
                   count(*) OVER (PARTITION BY action.workspace_id) AS pending_total,
                   row_number() OVER (
                       PARTITION BY action.workspace_id
                       ORDER BY action.approval_expires_at NULLS LAST,
                                action.created_at, action.id
                   ) AS shown
            FROM viryaos_autopilot_actions AS action
            WHERE action.workspace_id = ANY($1)
              AND action.status = 'awaiting_approval'
              AND (action.approval_expires_at IS NULL OR action.approval_expires_at > $2)
              -- Batched relay deliveries ask through the batch card, not here.
              AND NOT (action.action_kind = 'community.engage.request'
                       AND action.payload ->> 'source_id' IS NOT NULL)
        ) AS pending
        WHERE pending.shown <= $3
        ORDER BY pending.workspace_id, pending.shown
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .bind(MAX_ITEMS_SHOWN)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // What died unanswered inside the window. `last_error_kind` is the
    // queue's own verdict on the death; both kinds stay distinct downstream.
    let slipped_rows = sqlx::query_as::<_, SlippedRow>(
        r#"
        SELECT slipped.workspace_id, slipped.action_id, slipped.action_kind,
               slipped.subject_kind, slipped.subject_id, slipped.resolution,
               slipped.finished_at, slipped.slipped_total
        FROM (
            SELECT action.workspace_id, action.id AS action_id, action.action_kind,
                   action.subject_kind, action.subject_id,
                   action.last_error_kind AS resolution, action.finished_at,
                   count(*) OVER (PARTITION BY action.workspace_id) AS slipped_total,
                   row_number() OVER (
                       PARTITION BY action.workspace_id
                       ORDER BY action.finished_at DESC, action.id
                   ) AS shown
            FROM viryaos_autopilot_actions AS action
            WHERE action.workspace_id = ANY($1)
              AND action.status = 'cancelled'
              AND action.last_error_kind IN ('approval_expired', 'insufficient_evidence')
              AND action.finished_at >= $2 - ($3::int * INTERVAL '1 day')
        ) AS slipped
        WHERE slipped.shown <= $4
        ORDER BY slipped.workspace_id, slipped.shown
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .bind(i64::from(WINDOW_DAYS))
    .bind(MAX_ITEMS_SHOWN)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // The newest day each act's briefing spoke for. `local_date` is the
    // tenant-local day — the briefing's own key, so the staleness the page
    // shows is the same staleness the reader experienced.
    let briefing_dates = sqlx::query_as::<_, (Uuid, Date)>(
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
    .map_err(map_sqlx)?
    .into_iter()
    .collect::<HashMap<_, _>>();

    let mut pending_by_act: HashMap<Uuid, (i64, Vec<PendingItem>)> = HashMap::new();
    for row in pending_rows {
        let entry = pending_by_act
            .entry(row.workspace_id)
            .or_insert_with(|| (row.pending_total, Vec::new()));
        entry.1.push(PendingItem {
            action_id: row.action_id,
            action_kind: row.action_kind,
            subject_kind: row.subject_kind,
            subject_id: row.subject_id,
            approval_expires_at: row.approval_expires_at,
        });
    }

    let mut slipped_by_act: HashMap<Uuid, (i64, Vec<SlippedItem>)> = HashMap::new();
    for row in slipped_rows {
        let entry = slipped_by_act
            .entry(row.workspace_id)
            .or_insert_with(|| (row.slipped_total, Vec::new()));
        entry.1.push(SlippedItem {
            action_id: row.action_id,
            action_kind: row.action_kind,
            subject_kind: row.subject_kind,
            subject_id: row.subject_id,
            resolution: row.resolution,
            finished_at: row.finished_at,
        });
    }

    let mut acts = Vec::with_capacity(members.len());
    for member in members {
        // The identical series and verdict the attention read computes for
        // one workspace — per member, so the roster page and the act's own
        // page can never disagree about whether a brain is drifting.
        let samples = crate::autopilot::daily_north_star(
            pool,
            crowdrelay_domain::WorkspaceId::from_uuid(member.id),
            crate::autopilot::NORTH_STAR_WINDOW_DAYS,
        )
        .await?;
        let days_observed = u32::try_from(samples.len()).unwrap_or(u32::MAX);
        // The series arrives newest-first; growth is the newest reading
        // minus the oldest, and under two readings it is unmeasurable —
        // absent, not zero, so a young act is not the roster's "weakest"
        // for want of data.
        let north_star_delta = samples
            .first()
            .zip(samples.last())
            .filter(|_| samples.len() >= 2)
            .map(|(newest, oldest)| (newest.value - oldest.value) as i64);
        let posture = crowdrelay_brain::self_assessment::assess(samples);

        let (pending_total, pending) = pending_by_act
            .remove(&member.id)
            .unwrap_or_else(|| (0, Vec::new()));
        let (slipped_total, slipped_items) = slipped_by_act
            .remove(&member.id)
            .unwrap_or_else(|| (0, Vec::new()));

        acts.push(ActBrief {
            workspace_id: crowdrelay_domain::WorkspaceId::from_uuid(member.id),
            name: member.name,
            posture: posture.as_str().to_owned(),
            posture_needs_attention: posture.needs_attention(),
            days_observed,
            pending_decisions: clamp(pending_total),
            pending,
            slipped: clamp(slipped_total),
            slipped_items,
            latest_briefing_date: briefing_dates.get(&member.id).copied(),
            north_star_delta,
        });
    }
    Ok(acts)
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
