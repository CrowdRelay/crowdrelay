//! The audit row every operator control writes before it changes anything.
//!
//! Lives apart from the controls themselves because it is the one piece of this
//! all of them share: an operator action is recorded first, keyed on the
//! caller's idempotency key, so a retried request replays its own answer
//! instead of doing the thing twice.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn insert_operator_action(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    operation_id: Uuid,
    action: &'static str,
    target_type: &'static str,
    target_id: Uuid,
    actor_type: &str,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
    details: &Value,
) -> Result<Option<Uuid>, RepositoryError> {
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO operator_actions (
            id, workspace_id, action, target_type, target_id, actor_type,
            idempotency_key, request_id, details
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
        ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(operation_id)
    .bind(workspace_id.into_uuid())
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(actor_type)
    .bind(idempotency_key.as_str())
    .bind(request_id.map(RequestId::as_str))
    .bind(details)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if inserted.is_some() {
        return Ok(None);
    }

    let existing = sqlx::query_as::<_, ExistingOperatorActionRow>(
        r#"
        SELECT id, action, target_type, target_id, details
        FROM operator_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(idempotency_key.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if existing.action != action
        || existing.target_type != target_type
        || existing.target_id != target_id
        || existing.details != *details
    {
        return Err(RepositoryError::Conflict);
    }
    Ok(Some(existing.id))
}
