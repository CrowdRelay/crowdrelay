//! One source-owned acquisition campaign across owned and community lanes.
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub async fn ensure_source_campaign(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    source_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    let source: Option<(String, String, Value)> = sqlx::query_as(
        "SELECT source_kind,title,metadata FROM content_sources WHERE workspace_id=$1 AND id=$2 AND active FOR UPDATE",
    ).bind(workspace_id).bind(source_id).fetch_optional(&mut **tx).await?;
    let Some((kind, title, metadata)) = source else {
        return Ok(None);
    };
    if !matches!(kind.as_str(), "video" | "release") {
        return Ok(None);
    }
    if let Some(id) = metadata
        .get("promotion_campaign_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM campaigns WHERE workspace_id=$1 AND id=$2 AND active)",
        )
        .bind(workspace_id)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        if exists {
            reconcile_source_links(tx, workspace_id, source_id, id, &metadata).await?;
            return Ok(Some(id));
        }
    }
    let video_id = metadata
        .get("video_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let url = metadata
        .get("url")
        .or_else(|| metadata.get("listen_url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let plan: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM release_plans WHERE workspace_id=$1 AND active AND (id=$2 OR ($3 <> '' AND listen_url=$3) OR ($4 <> '' AND listen_url IN ('https://youtu.be/' || $4,'https://www.youtube.com/watch?v=' || $4))) ORDER BY (id=$2) DESC,created_at LIMIT 1 FOR UPDATE",
    ).bind(workspace_id).bind(source_id).bind(url).bind(video_id).fetch_optional(&mut **tx).await?;
    let existing: Option<Uuid> = match plan {
        Some(plan_id) => sqlx::query_scalar("SELECT id FROM campaigns WHERE workspace_id=$1 AND release_plan_id=$2 AND active ORDER BY created_at LIMIT 1")
            .bind(workspace_id).bind(plan_id).fetch_optional(&mut **tx).await?,
        None => None,
    };
    let campaign_id = match existing {
        Some(id) => id,
        None => sqlx::query_scalar("INSERT INTO campaigns(workspace_id,name,release_plan_id,active) VALUES($1,$2,$3,true) RETURNING id")
            .bind(workspace_id).bind(format!("{title} · promotion")).bind(plan).fetch_one(&mut **tx).await?,
    };
    sqlx::query("UPDATE content_sources SET metadata=jsonb_set(metadata,'{promotion_campaign_id}',to_jsonb($3::text)),updated_at=now() WHERE workspace_id=$1 AND id=$2")
        .bind(workspace_id).bind(source_id).bind(campaign_id.to_string()).execute(&mut **tx).await?;
    reconcile_source_links(tx, workspace_id, source_id, campaign_id, &metadata).await?;
    Ok(Some(campaign_id))
}

async fn reconcile_source_links(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    source_id: Uuid,
    campaign_id: Uuid,
    metadata: &Value,
) -> Result<(), sqlx::Error> {
    let canonical = metadata
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
    // Repair only links whose action proves exact source ownership. Keep
    // public slugs and historical events; do not fabricate past fans.
    sqlx::query(
        r#"UPDATE smart_links link SET campaign_id=$3,
               destination_url=COALESCE($4,link.destination_url),version=link.version+1,updated_at=now()
           WHERE link.workspace_id=$1 AND link.campaign_id IS NULL
             AND EXISTS(SELECT 1 FROM autopilot_actions action
                 WHERE action.workspace_id=link.workspace_id
                   AND action.action_kind='community.engage.request'
                   AND action.payload->>'source_id'=$2::uuid::text
                   AND action.payload->>'smart_link'='/l/' || link.slug)"#,
    ).bind(workspace_id).bind(source_id).bind(campaign_id).bind(canonical).execute(&mut **tx).await?;
    Ok(())
}
