//! Operator surface for fanbases: addressable audience blocks fed by
//! swappable providers. Protocol mapping only — statements live in
//! `crowdrelay-infra::fanbase`, policy in `crowdrelay-domain::fanbase`.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::fanbase::SourceKind;
use crowdrelay_infra::fanbase::{FanbaseEntry, FanbaseError, PostgresFanbaseRepository};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_INGEST_ENTRIES: usize = 500;

fn repository(state: &crate::AppState) -> PostgresFanbaseRepository {
    PostgresFanbaseRepository::new(state.database.clone())
}

fn workspace(state: &crate::AppState) -> Uuid {
    state.ticketing.workspace_id().into_uuid()
}

fn error_response(error: FanbaseError, request_id_value: Option<String>) -> Response {
    match error {
        FanbaseError::NotFound => Problem::not_found(request_id_value).into_response(),
        FanbaseError::NameTaken | FanbaseError::ConnectionExists => {
            Problem::conflict(request_id_value).into_response()
        }
        FanbaseError::Database(_) => Problem::service_unavailable(request_id_value).into_response(),
    }
}

#[derive(Serialize)]
pub struct FanbaseResponse {
    id: Uuid,
    name: String,
    source_kind: String,
    fetch_url: Option<String>,
    consent_attested_by: Option<String>,
    enabled: bool,
    members: Option<i64>,
    last_status: Option<String>,
    last_imported_pending: Option<i32>,
}

fn fanbase_response(row: crowdrelay_infra::fanbase::FanbaseRow) -> FanbaseResponse {
    FanbaseResponse {
        id: row.id,
        name: row.name,
        source_kind: row.source_kind,
        fetch_url: row.fetch_url,
        consent_attested_by: row.consent_attested_by,
        enabled: row.enabled,
        members: row.members,
        last_status: row.last_status,
        last_imported_pending: row.last_imported_pending,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateFanbaseRequest {
    name: String,
    source_kind: String,
    fetch_url: Option<String>,
    /// Required for origins that cannot carry per-candidate consent evidence
    /// (manual imports and lists bought or scraped elsewhere are refused).
    consent_attested_by: Option<String>,
}

pub async fn create_fanbase(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateFanbaseRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(source_kind) = SourceKind::from_storage(&request.source_kind) else {
        return Problem::unprocessable(request_id_value).into_response();
    };
    if request.name.trim().is_empty() || request.name.len() > 200 {
        return Problem::unprocessable(request_id_value).into_response();
    }
    // Origins without their own consent evidence need a named attestation:
    // someone vouches the list was collected with permission.
    let needs_attestation = !matches!(source_kind, SourceKind::HttpJsonPull)
        && request
            .consent_attested_by
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty);
    if needs_attestation || request.fetch_url.as_deref().map(str::len).unwrap_or(0) > 512 {
        return Problem::unprocessable(request_id_value).into_response();
    }

    match repository(&state)
        .create_fanbase(
            workspace(&state),
            request.name.trim(),
            source_kind,
            request.fetch_url.as_deref(),
            request.consent_attested_by.as_deref(),
        )
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "fanbaseId": id })),
        )
            .into_response(),
        Err(FanbaseError::NameTaken) => Problem::conflict(request_id_value).into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn list_fanbases(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state).list_fanbases(workspace(&state)).await {
        Ok(rows) => {
            let items: Vec<FanbaseResponse> = rows.into_iter().map(fanbase_response).collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(serde_json::json!({ "fanbases": items })),
            )
                .into_response()
        }
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn delete_fanbase(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(fanbase_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state)
        .delete_fanbase(workspace(&state), fanbase_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(FanbaseError::NotFound) => Problem::not_found(request_id_value).into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IngestFanbaseRequest {
    entries: Vec<IngestEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IngestEntry {
    external_id: String,
    email: Option<String>,
    display_name: Option<String>,
    locale: Option<String>,
}

pub async fn ingest_fanbase(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(fanbase_id): Path<Uuid>,
    payload: Result<Json<IngestFanbaseRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if request.entries.is_empty() || request.entries.len() > MAX_INGEST_ENTRIES {
        return Problem::unprocessable(request_id_value).into_response();
    }
    for entry in &request.entries {
        if entry.external_id.trim().is_empty() || entry.external_id.len() > 200 {
            return Problem::unprocessable(request_id_value).into_response();
        }
    }
    let entries: Vec<FanbaseEntry> = request
        .entries
        .iter()
        .map(|entry| FanbaseEntry {
            external_id: entry.external_id.trim().to_owned(),
            email: entry
                .email
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
            display_name: entry.display_name.clone(),
            locale: entry.locale.clone(),
        })
        .collect();

    match repository(&state)
        .ingest_candidates(
            workspace(&state),
            fanbase_id,
            &entries,
            ACCESS_TOKEN_TTL_DAYS,
            ACCESS_RESEND_COOLDOWN_SECONDS,
        )
        .await
    {
        Ok(counts) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "received": counts.received,
                "importedPending": counts.imported_pending,
                "confirmationResent": counts.confirmation_resent,
                "alreadyActive": counts.already_active,
                "skippedSuppressed": counts.skipped_suppressed,
                "cooldownSkipped": counts.cooldown_skipped,
                "invalid": counts.invalid,
            })),
        )
            .into_response(),
        Err(FanbaseError::NotFound) => Problem::not_found(request_id_value).into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

// Re-exported constants live in the interactive flow module; importing keeps
// token TTL and resend cooldown identical across acquisition surfaces.
use crate::fan_lifecycle::{ACCESS_RESEND_COOLDOWN_SECONDS, ACCESS_TOKEN_TTL_DAYS};

// ---------------------------------------------------------------------------
// Fanbase OAuth connections — first-class platform linking.
// ---------------------------------------------------------------------------

use crowdrelay_domain::fanbase::Platform;
use crowdrelay_infra::fanbase_oauth::{
    FanbaseOauthConfig, FanbaseOauthError, FanbaseOauthRepository,
};

fn oauth_repository(state: &crate::AppState) -> &FanbaseOauthRepository {
    &state.fanbase_oauth_repository
}

fn encryption_key(
    state: &crate::AppState,
) -> &crowdrelay_infra::sensitive_response::SensitiveResponseKey {
    &state.response_encryption_key
}

#[allow(clippy::result_large_err)]
fn oauth_config_for(
    platform: Platform,
    state: &crate::AppState,
) -> Result<FanbaseOauthConfig, Response> {
    let configs = state
        .fanbase_oauth_configs
        .iter()
        .find(|c| c.platform == platform);
    let config = configs.ok_or_else(|| Problem::service_unavailable(None).into_response())?;
    Ok(FanbaseOauthConfig {
        platform: config.platform,
        client_id: config.client_id.clone(),
        client_secret: config.client_secret.clone(),
        authorize_url: config.authorize_url.clone(),
        token_url: config.token_url.clone(),
        scopes: config.scopes.clone(),
    })
}

fn oauth_error_response(error: FanbaseOauthError, request_id_value: Option<String>) -> Response {
    match error {
        FanbaseOauthError::StateNotFound => Problem::not_found(request_id_value).into_response(),
        FanbaseOauthError::UnsupportedPlatform(_) | FanbaseOauthError::TokenExchange(_) => {
            Problem::bad_request(request_id_value).into_response()
        }
        FanbaseOauthError::Database(_) | FanbaseOauthError::Http(_) => {
            Problem::service_unavailable(request_id_value).into_response()
        }
        FanbaseOauthError::Encryption(_) => {
            Problem::service_unavailable(request_id_value).into_response()
        }
        FanbaseOauthError::ProfileFetchFailed => {
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartOauthRequest {
    redirect_uri: String,
}

pub async fn start_fanbase_oauth(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(platform_str): Path<String>,
    Json(body): Json<StartOauthRequest>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(platform) = Platform::from_storage(&platform_str) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !platform.supports_oauth() {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Ok(config) = oauth_config_for(platform, &state) else {
        return Problem::service_unavailable(request_id_value).into_response();
    };
    if body.redirect_uri.is_empty() || body.redirect_uri.len() > 512 {
        return Problem::bad_request(request_id_value).into_response();
    }
    match oauth_repository(&state)
        .start_oauth(workspace(&state), platform, &config, &body.redirect_uri)
        .await
    {
        Ok(auth_url) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "url": auth_url.url, "state": auth_url.state })),
        )
            .into_response(),
        Err(error) => oauth_error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthCallbackRequest {
    state: String,
    code: String,
}

pub async fn fanbase_oauth_callback(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(platform_str): Path<String>,
    Json(body): Json<OauthCallbackRequest>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(platform) = Platform::from_storage(&platform_str) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !platform.supports_oauth() {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Ok(config) = oauth_config_for(platform, &state) else {
        return Problem::service_unavailable(request_id_value).into_response();
    };
    match oauth_repository(&state)
        .exchange_code(
            workspace(&state),
            platform,
            &config,
            &body.state,
            &body.code,
            encryption_key(&state),
        )
        .await
    {
        Ok(connection_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "success": true, "connectionId": connection_id })),
        )
            .into_response(),
        Err(error) => oauth_error_response(error, request_id_value),
    }
}

pub async fn list_fanbase_connections(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let repo = crowdrelay_infra::fanbase::PostgresFanbaseRepository::new(state.database.clone());
    match repo.list_connections(workspace(&state)).await {
        Ok(connections) => {
            let items: Vec<serde_json::Value> = connections
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "platform": c.platform,
                        "external_account_ref": c.external_account_ref,
                        "label": c.label,
                        "status": c.status,
                        "last_sync_at": c.last_sync_at,
                        "created_at": c.created_at,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(serde_json::json!({ "connections": items })),
            )
                .into_response()
        }
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn delete_fanbase_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(connection_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    let repo = crowdrelay_infra::fanbase::PostgresFanbaseRepository::new(state.database.clone());
    match repo
        .delete_connection(workspace(&state), connection_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(FanbaseError::NotFound) => Problem::not_found(request_id_value).into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterManualPostRequest {
    reddit_post_url: String,
}

/// Registers a manually-posted Reddit URL for a community post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the metrics poller can track
/// its performance via Reddit's public JSON endpoint.
pub async fn register_manual_community_post(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(community_post_id): Path<Uuid>,
    payload: Result<Json<RegisterManualPostRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    match crowdrelay_infra::fanbase_oauth::register_manual_reddit_post(
        &state.database,
        workspace(&state),
        community_post_id,
        &body.reddit_post_url,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to register manual community post");
            Problem::bad_request(request_id_value).into_response()
        }
    }
}
