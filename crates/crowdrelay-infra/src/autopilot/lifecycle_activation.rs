//! Deliver one first-party resource, using the immutable signup promise.
use super::*;
use crate::tenant_settings::TenantBrandSettings;
use crowdrelay_domain::acquisition::{CaptureOffer, FanCaptureContext};

pub(super) struct WelcomeRequest<'a> {
    pub workspace_id: WorkspaceId,
    pub action_id: AutopilotActionId,
    pub fan_id: crowdrelay_domain::FanId,
    pub locale: &'a str,
    pub now: OffsetDateTime,
}

#[derive(serde::Serialize)]
pub(super) struct WelcomeActivation {
    pub kind: &'static str,
    pub title: String,
    pub url: String,
}

pub(super) async fn prepare(
    tx: &mut Transaction<'_, Postgres>,
    brand: &TenantBrandSettings,
    request: WelcomeRequest<'_>,
) -> Result<Option<WelcomeActivation>, RepositoryError> {
    let workspace = request.workspace_id.into_uuid();
    let raw = sqlx::query_scalar::<_, Value>(
        "SELECT context FROM fan_capture_contexts WHERE workspace_id=$1 AND fan_id=$2",
    )
    .bind(workspace)
    .bind(request.fan_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let context = raw
        .and_then(|value| serde_json::from_value::<FanCaptureContext>(value).ok())
        .filter(FanCaptureContext::is_valid)
        .unwrap_or_default();
    let wants_show = context.event_slug.is_some() || context.offer == Some(CaptureOffer::Shows);
    let selected = if wants_show {
        match event(tx, workspace, context.event_slug.as_deref(), request.now).await? {
            Some(show) => Some(("event", show)),
            None => event(tx, workspace, None, request.now)
                .await?
                .map(|show| ("event", show)),
        }
    } else {
        match video(tx, workspace, context.video_id.as_deref(), request.now).await? {
            Some(video) => Some(("video", video)),
            None => video(tx, workspace, None, request.now)
                .await?
                .map(|video| ("video", video)),
        }
    };
    // A cancelled/removed promise never becomes a fabricated reward or dead URL.
    // A tenant with no suitable public resource gets an honest, linkless welcome.
    let Some((kind, (resource, title))) = selected else {
        return Ok(None);
    };
    let destination = if kind == "event" {
        brand.event_page_url(request.locale, &resource)
    } else {
        brand.watch_page_url(request.locale, &resource)
    };
    let Some(destination) = destination else {
        return Ok(None);
    };
    let slug = format!("welcome-{}", request.action_id.into_uuid().simple());
    let link = crate::tracked_links::ensure_smart_link_in_tx(
        tx,
        workspace,
        &slug,
        &destination,
        brand.site_root(),
        Some("email"),
        Some("welcome-v2"),
    )
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::ConflictBecause(
        "welcome resource is not a printable URL",
    ))?;
    let bound = sqlx::query("UPDATE smart_links SET action_id=$3 WHERE workspace_id=$1 AND slug=$2 AND (action_id IS NULL OR action_id=$3)")
        .bind(workspace).bind(&slug).bind(request.action_id.into_uuid())
        .execute(&mut **tx).await.map_err(map_sqlx)?;
    if bound.rows_affected() != 1 {
        return Err(RepositoryError::ConflictBecause(
            "welcome link belongs to another action",
        ));
    }
    Ok(Some(WelcomeActivation {
        kind,
        title,
        url: link.as_str().to_owned(),
    }))
}

async fn event(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    slug: Option<&str>,
    now: OffsetDateTime,
) -> Result<Option<(String, String)>, RepositoryError> {
    sqlx::query_as(EVENT_SQL)
        .bind(workspace)
        .bind(slug)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)
}

async fn video(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    video: Option<&str>,
    now: OffsetDateTime,
) -> Result<Option<(String, String)>, RepositoryError> {
    sqlx::query_as(VIDEO_SQL)
        .bind(workspace)
        .bind(video)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)
}

const EVENT_SQL: &str = r#"
SELECT slug,title FROM events
WHERE workspace_id=$1 AND status='published' AND starts_at>$3
  AND ($2::text IS NULL OR slug=$2)
ORDER BY starts_at,id LIMIT 1
"#;

// Match the public-owned-video read: active tenant video sources. Bound the
// publish time and resource syntax before constructing a first-party route.
const VIDEO_SQL: &str = r#"
SELECT substring(source_key FROM 9),title FROM content_sources
WHERE workspace_id=$1 AND source_kind='video' AND active AND occurred_at<=$3
  AND source_key ~ '^youtube:[A-Za-z0-9_-]{11}$'
  AND ($2::text IS NULL OR source_key='youtube:'||$2)
ORDER BY occurred_at DESC,id LIMIT 1
"#;
