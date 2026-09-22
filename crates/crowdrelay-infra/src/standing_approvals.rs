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
