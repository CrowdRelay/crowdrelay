//! Reading and writing the operator's standing approvals.
//!
//! The rule about what a grant may do lives in
//! `crowdrelay_domain::standing_approval`; this file only stores and returns
//! rows. Two things are enforced here rather than there because they are
//! facts about storage:
//!
//! - A grant is refused for a class that may never carry one. The migration's
//!   CHECK would refuse it too, but a constraint violation reaches the
//!   operator as a 500 and tells them nothing; this reaches them as the
//!   sentence the domain already wrote.
//! - A re-grant for a target that already has a live row updates it rather
//!   than failing. An operator extending a grant is not making a mistake, and
//!   an error there would push them towards revoking first — which leaves a
//!   window where the agent silently stops.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crowdrelay_domain::action_class::ActionClass;
use crowdrelay_domain::standing_approval::{GrantError, expiry_for};

#[derive(Debug, thiserror::Error)]
pub enum StandingApprovalError {
    #[error("{0}")]
    Refused(#[from] GrantError),
    #[error("no standing approval for that action kind and target")]
    NotFound,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// One grant as an operator reads it back.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct StandingApprovalView {
    pub action_kind: String,
    pub target_key: String,
    pub action_class: String,
    pub granted_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    pub revoked_by: Option<String>,
    pub note: Option<String>,
}

/// A community for which a standing approval is worth asking about.
///
/// This is evidence for a human decision, never authority. The read is
/// intentionally conservative: a target appears only after repeated explicit
/// operator approvals produced real posts and at least one fan survived past
/// the D30 retention boundary. Any historical standing-grant row suppresses
/// the suggestion, including a revoked one — "we revoked this" is stronger
/// operator evidence than the current yield.
#[derive(Debug, serde::Serialize)]
pub struct StandingApprovalCandidateView {
    pub action_kind: &'static str,
    pub target_key: String,
    pub action_class: &'static str,
    pub suggested_days: i64,
    pub display_name: String,
    pub platform: String,
    pub community: String,
    pub operator_approved_posts_90d: u32,
    pub converted_fans_90d: u32,
    pub retained_fans_d30: u32,
    pub reason: String,
}

#[derive(Debug, sqlx::FromRow)]
struct StandingApprovalCandidateRow {
    target_id: Uuid,
    display_name: String,
    platform: String,
    community: String,
    operator_approved_posts_90d: i64,
    converted_fans_90d: i64,
    retained_fans_d30: i64,
}

const MIN_OPERATOR_APPROVED_POSTS: u32 = 3;
const MIN_RETAINED_FANS_D30: u32 = 1;
const MAX_STANDING_APPROVAL_CANDIDATES: usize = 20;
const MAX_STANDING_APPROVAL_CANDIDATE_ROWS: i64 = 100;

fn standing_candidate_from_row(
    row: StandingApprovalCandidateRow,
) -> Option<StandingApprovalCandidateView> {
    let approved = u32::try_from(row.operator_approved_posts_90d.max(0)).unwrap_or(u32::MAX);
    let converted = u32::try_from(row.converted_fans_90d.max(0)).unwrap_or(u32::MAX);
    let retained = u32::try_from(row.retained_fans_d30.max(0)).unwrap_or(u32::MAX);
    if approved < MIN_OPERATOR_APPROVED_POSTS || retained < MIN_RETAINED_FANS_D30 {
        return None;
    }
    Some(StandingApprovalCandidateView {
        action_kind: "community.engage.request",
        target_key: row.target_id.to_string(),
        action_class: ActionClass::ThirdParty.as_str(),
        suggested_days: crowdrelay_domain::standing_approval::DEFAULT_GRANT_DAYS,
        display_name: row.display_name,
        platform: row.platform,
        community: row.community,
        operator_approved_posts_90d: approved,
        converted_fans_90d: converted,
        retained_fans_d30: retained,
        reason: format!(
            "{approved} operator-approved posts in 90d produced {converted} attributed fan(s), with {retained} retained past D30"
        ),
    })
}

/// Communities where the product should ask the operator whether repeated
/// approval has become ceremony.
///
/// This does NOT create or extend a grant. It only returns a ready-to-display
/// recommendation beside the existing grant API. The evidence is target
/// scoped by the exact community action IDs, so one good subreddit cannot
/// lend its fan conversions to another.
///
/// # Errors
/// Database failure is returned as StandingApprovalError::Database.
pub async fn candidates(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<StandingApprovalCandidateView>, StandingApprovalError> {
    let rows = sqlx::query_as::<_, StandingApprovalCandidateRow>(
        r#"
        SELECT
            target.id AS target_id,
            target.display_name,
            COALESCE(NULLIF(target.platform, ''), 'reddit') AS platform,
            COALESCE(target.subreddit, target.display_name) AS community,
            COUNT(DISTINCT post.id) FILTER (
                WHERE action.approved_by LIKE 'operator:%'
                  AND action.approved_by <> 'operator:standing_grant'
            )::bigint AS operator_approved_posts_90d,
            COUNT(DISTINCT provenance.fan_id)::bigint AS converted_fans_90d,
            COUNT(DISTINCT provenance.fan_id) FILTER (
                WHERE provenance.occurred_at <= $2 - INTERVAL '30 days'
                  AND fan.status = 'active'
                  AND latest_consent.granted
                  AND fan_has_meaningful_action_between(
                      fan.workspace_id,
                      fan.id,
                      fan.normalized_email,
                      GREATEST(
                          $2 - INTERVAL '30 days',
                          provenance.occurred_at + INTERVAL '30 days'
                      ),
                      $2
                  )
            )::bigint AS retained_fans_d30
        FROM agent_outreach_targets AS target
        JOIN community_posts AS post
          ON post.workspace_id = target.workspace_id
         AND post.target_id = target.id
         AND post.status = 'posted'
         AND post.posted_at >= $2 - INTERVAL '90 days'
         AND post.posted_at <= $2
        JOIN autopilot_actions AS action
          ON action.workspace_id = post.workspace_id
         AND action.id = post.action_id
        LEFT JOIN fan_provenance_events AS provenance
          ON provenance.workspace_id = post.workspace_id
         AND provenance.action_id = post.action_id
         -- Trust evidence must come from the same class of deliveries the
         -- organiser actually approved. Historical policy:auto sends may
         -- still have honest acquisition rows, but they are not evidence that
         -- this person repeatedly trusted the target.
         AND action.approved_by LIKE 'operator:%'
         AND action.approved_by <> 'operator:standing_grant'
         AND provenance.event_kind = 'conversion'
         AND provenance.fan_id IS NOT NULL
         AND provenance.occurred_at >= $2 - INTERVAL '90 days'
         AND provenance.occurred_at <= $2
        LEFT JOIN fans AS fan
          ON fan.workspace_id = provenance.workspace_id
         AND fan.id = provenance.fan_id
        LEFT JOIN LATERAL (
            SELECT consent.granted
            FROM fan_consents AS consent
            WHERE consent.workspace_id = fan.workspace_id
              AND consent.fan_id = fan.id
              AND consent.purpose = 'marketing'
              AND consent.recorded_at <= $2
            ORDER BY consent.recorded_at DESC, consent.id DESC
            LIMIT 1
        ) AS latest_consent ON true
        WHERE target.workspace_id = $1
          AND target.target_kind = 'community'
          AND target.status = 'promoted'
          AND target.screening_verdict IS DISTINCT FROM 'refused'
          AND NOT EXISTS (
              SELECT 1
              FROM standing_approvals AS standing
              WHERE standing.workspace_id = target.workspace_id
                AND standing.action_kind = 'community.engage.request'
                AND standing.target_key = target.id::text
          )
        GROUP BY target.id, target.display_name, target.platform, target.subreddit
        ORDER BY retained_fans_d30 DESC,
                 converted_fans_90d DESC,
                 operator_approved_posts_90d DESC,
                 target.display_name,
                 target.id
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(MAX_STANDING_APPROVAL_CANDIDATE_ROWS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(standing_candidate_from_row)
        .take(MAX_STANDING_APPROVAL_CANDIDATES)
        .collect())
}

/// One grant as an operator asks for it.
///
/// A struct rather than six positional arguments because four of them are
/// strings: `grant(.., action_kind, target_key, .., granted_by, ..)` is three
/// chances to swap a pair silently, and swapping the first two would write a
/// grant nobody asked for against a target nobody named.
#[derive(Debug, Clone, Copy)]
pub struct GrantRequest<'a> {
    pub action_kind: &'a str,
    pub target_key: &'a str,
    pub class: ActionClass,
    pub granted_by: &'a str,
    pub days: i64,
    pub note: Option<&'a str>,
}

/// Records that this target may act without a per-action approval.
///
/// Re-granting an existing target extends it and clears any revocation: the
/// operator is answering the same question again, and the answer they just
/// gave is the current one.
///
/// # Errors
/// [`StandingApprovalError::Refused`] when the class may never carry a grant,
/// or the requested duration is not positive or is longer than the maximum.
pub async fn grant(
    pool: &PgPool,
    workspace_id: Uuid,
    request: GrantRequest<'_>,
    now: OffsetDateTime,
) -> Result<StandingApprovalView, StandingApprovalError> {
    let GrantRequest {
        action_kind,
        target_key,
        class,
        granted_by,
        days,
        note,
    } = request;
    if !class.may_carry_standing_approval() {
        return Err(GrantError::ClassNotGrantable.into());
    }
    let expires_at = expiry_for(now, days)?;
    let view = sqlx::query_as::<_, StandingApprovalView>(
        r#"
        INSERT INTO standing_approvals (
            workspace_id, action_kind, target_key, action_class,
            granted_by, granted_at, expires_at, note
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
        ON CONFLICT (workspace_id, action_kind, target_key) DO UPDATE
        SET action_class = EXCLUDED.action_class,
            granted_by = EXCLUDED.granted_by,
            granted_at = EXCLUDED.granted_at,
            expires_at = EXCLUDED.expires_at,
            note = EXCLUDED.note,
            revoked_at = NULL,
            revoked_by = NULL
        RETURNING action_kind, target_key, action_class, granted_by,
                  granted_at, expires_at, revoked_at, revoked_by, note
        "#,
    )
    .bind(workspace_id)
    .bind(action_kind)
    .bind(target_key)
    .bind(class.as_str())
    .bind(granted_by)
    .bind(now)
    .bind(expires_at)
    .bind(note)
    .fetch_one(pool)
    .await?;
    Ok(view)
}

/// Withdraws a grant.
///
/// The row stays. What was trusted, by whom and until when is the history an
/// operator needs when the same community comes back, and a deleted row
/// answers none of it.
///
/// # Errors
/// [`StandingApprovalError::NotFound`] when no live grant matches.
pub async fn revoke(
    pool: &PgPool,
    workspace_id: Uuid,
    action_kind: &str,
    target_key: &str,
    revoked_by: &str,
    now: OffsetDateTime,
) -> Result<(), StandingApprovalError> {
    let result = sqlx::query(
        r#"
        UPDATE standing_approvals
        SET revoked_at = $5, revoked_by = $4
        WHERE workspace_id = $1
          AND action_kind = $2
          AND target_key = $3
          AND revoked_at IS NULL
        "#,
    )
    .bind(workspace_id)
    .bind(action_kind)
    .bind(target_key)
    .bind(revoked_by)
    .bind(now)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(StandingApprovalError::NotFound);
    }
    Ok(())
}

/// What a standing approval for this action would be recorded against.
///
/// Reads the action's own payload rather than trusting anything the caller
/// supplies, so "approve this and stop asking about it" cannot become a grant
/// over some other target. `None` means this action has no target a standing
/// approval could sensibly cover — see
/// `AutopilotActionPayload::standing_approval_target`.
///
/// # Errors
/// [`StandingApprovalError::NotFound`] when no such action exists in this
/// workspace; [`StandingApprovalError::Database`] when the read fails.
pub async fn grant_target_for_action(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
) -> Result<Option<(String, String, ActionClass)>, StandingApprovalError> {
    let row: Option<(String, serde_json::Value)> = sqlx::query_as(
        r#"
        SELECT action_kind, payload
        FROM autopilot_actions
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_optional(pool)
    .await?;
    let Some((action_kind, payload)) = row else {
        return Err(StandingApprovalError::NotFound);
    };
    // A payload this build cannot parse has no target it can name. Guessing
    // one would be the opposite of what reading the payload is for.
    let Ok(parsed) = serde_json::from_value::<
        crowdrelay_application::autopilot::AutopilotActionPayload,
    >(payload) else {
        return Ok(None);
    };
    let Some(target_key) = parsed.standing_approval_target() else {
        return Ok(None);
    };
    Ok(Some((action_kind, target_key, parsed.action_class())))
}

/// Every grant the workspace has ever written, newest first.
///
/// Revoked and expired rows are included on purpose. "Which of these did we
/// turn off, and when" is the question an operator asks after a community
/// goes quiet, and a list that hides them cannot answer it.
///
/// # Errors
/// [`StandingApprovalError::Database`] when the read fails.
pub async fn list(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<StandingApprovalView>, StandingApprovalError> {
    let rows = sqlx::query_as::<_, StandingApprovalView>(
        r#"
        SELECT action_kind, target_key, action_class, granted_by,
               granted_at, expires_at, revoked_at, revoked_by, note
        FROM standing_approvals
        WHERE workspace_id = $1
        ORDER BY granted_at DESC, action_kind, target_key
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    fn row(approved: i64, converted: i64, retained: i64) -> StandingApprovalCandidateRow {
        StandingApprovalCandidateRow {
            target_id: Uuid::nil(),
            display_name: "r/proven".to_owned(),
            platform: "reddit".to_owned(),
            community: "r/proven".to_owned(),
            operator_approved_posts_90d: approved,
            converted_fans_90d: converted,
            retained_fans_d30: retained,
        }
    }

    #[test]
    fn standing_suggestion_requires_repeated_human_yes_and_retained_fan() {
        assert!(standing_candidate_from_row(row(2, 10, 10)).is_none());
        assert!(standing_candidate_from_row(row(10, 10, 0)).is_none());
        let candidate = standing_candidate_from_row(row(3, 1, 1)).expect("proven target");
        assert_eq!(candidate.action_kind, "community.engage.request");
        assert_eq!(candidate.action_class, "third_party");
        assert_eq!(candidate.operator_approved_posts_90d, 3);
        assert_eq!(candidate.retained_fans_d30, 1);
    }
}
