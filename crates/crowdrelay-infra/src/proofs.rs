//! PostgreSQL adapter for external-proof persistence helpers.

use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Seeds one eligible audit-ledger leaf for a deploy Rekor canary.
///
/// The caller deliberately owns the surrounding transaction and advisory lock:
/// seeding and selecting the proof batch therefore remain atomic with respect
/// to the normal audit-batch scheduler.
pub async fn seed_rekor_canary_audit_event(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let canary_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type,
            target_id, request_id, metadata
        ) VALUES (
            $1, 'service', 'rekor.canary.seeded', 'external_proof_canary',
            $2, $3, $4
        )
        "#,
    )
    .bind(workspace_id)
    .bind(canary_id.to_string())
    .bind(request_id)
    .bind(json!({"purpose": "deploy_e2e"}))
    .execute(&mut **tx)
    .await?;
    Ok(())
}
