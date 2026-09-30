//! Live source-owned restrictions, read again before an external send.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn source_platform_allowed(
    pool: &PgPool,
    workspace_id: Uuid,
    source_id: Uuid,
    platform: &str,
) -> Result<bool, sqlx::Error> {
    let source: Option<(String, Value)> = sqlx::query_as(
        "SELECT source_kind, metadata FROM content_sources WHERE workspace_id=$1 AND id=$2 AND active AND expires_at>now()",
    ).bind(workspace_id).bind(source_id).fetch_optional(pool).await?;
    Ok(source.is_some_and(|(kind, metadata)| {
        crowdrelay_domain::video_promotion::platform_allowed(&kind, &metadata, platform)
    }))
}

pub async fn action_platform_allowed(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    platform: &str,
) -> Result<bool, sqlx::Error> {
    let payload: Option<Value> =
        sqlx::query_scalar("SELECT payload FROM autopilot_actions WHERE workspace_id=$1 AND id=$2")
            .bind(workspace_id)
            .bind(action_id)
            .fetch_optional(pool)
            .await?;
    let Some(payload) = payload else {
        return Ok(false);
    };
    let source = payload
        .get("source_id")
        .filter(|id| !id.is_null())
        .or_else(|| {
            payload
                .get("draft")?
                .get("source_id")
                .filter(|id| !id.is_null())
        });
    let Some(source) = source else {
        return Ok(true);
    };
    let Some(source_id) = source.as_str().and_then(|id| Uuid::parse_str(id).ok()) else {
        return Ok(false);
    };
    source_platform_allowed(pool, workspace_id, source_id, platform).await
}
