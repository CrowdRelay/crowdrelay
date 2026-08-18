//! Narrow Control Plane management transport for tenant AREA.
//! Exact coordinates are returned only by single-drop editor responses.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use crowdrelay_application::{AreaAdminError, CreateAreaCityCommand, CreateAreaDropCommand};
use crowdrelay_domain::AreaDropDraft;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppState, X_CROWDRELAY_CORRELATION_ID};

const PRIVATE_NO_STORE: &str = "private, no-store";

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/control-plane/area", get(overview))
        .route("/v1/control-plane/area/settings", patch(settings))
        .route(
            "/v1/control-plane/area/cities",
            get(cities).post(create_city),
        )
        .route(
            "/v1/control-plane/area/drops",
            get(list_drops).post(create_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}",
            get(get_drop).delete(delete_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/draft",
            patch(save_draft).delete(discard_draft),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/validate",
            post(validate_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/publish",
            post(publish_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/pause",
            post(pause_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/resume",
            post(resume_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/archive",
            post(archive_drop),
        )
        .route(
            "/v1/control-plane/area/drops/{drop_id}/duplicate",
            post(duplicate_drop),
        )
        .layer(DefaultBodyLimit::max(crate::MAX_PUBLIC_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagementError {
    error: &'static str,
    code: &'static str,
    issues: Option<Vec<crowdrelay_domain::AreaValidationIssue>>,
}

fn error(error: AreaAdminError) -> Response {
    let (status, code, message, issues) = match error {
        AreaAdminError::NotFound => (
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "AREA record not found.",
            None,
        ),
        AreaAdminError::Conflict(code) => (
            StatusCode::CONFLICT,
            code,
            "AREA command conflicts with current state.",
            None,
        ),
        AreaAdminError::Invalid(issues) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_FAILED",
            "AREA draft failed validation.",
            Some(issues),
        ),
        AreaAdminError::Repository(detail) => {
            tracing::error!(error=%detail,"AREA management repository failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "TEMPORARY",
                "AREA management is temporarily unavailable.",
                None,
            )
        }
    };
    (
        status,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ManagementError {
            error: message,
            code,
            issues,
        }),
    )
        .into_response()
}

fn request_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(&X_CROWDRELAY_CORRELATION_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
fn workspace(state: &AppState) -> crowdrelay_domain::WorkspaceId {
    state.ticketing.workspace_id()
}

async fn overview(State(state): State<AppState>) -> Response {
    match state.area_admin.overview(workspace(&state)).await {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsRequest {
    enabled: bool,
}
async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<SettingsRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = payload else {
        return (
            StatusCode::BAD_REQUEST,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(ManagementError {
                error: "Invalid request.",
                code: "INVALID_REQUEST",
                issues: None,
            }),
        )
            .into_response();
    };
    match state
        .area_admin
        .set_enabled(
            workspace(&state),
            payload.enabled,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(enabled) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({"enabled":enabled})),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
struct CityQuery {
    q: Option<String>,
    limit: Option<i64>,
}
async fn cities(State(state): State<AppState>, Query(query): Query<CityQuery>) -> Response {
    match state
        .area_admin
        .list_cities(query.q.as_deref(), query.limit.unwrap_or(30))
        .await
    {
        Ok(items) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({"items":items})),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn create_city(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateAreaCityCommand>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = payload else {
        return error(AreaAdminError::Conflict("INVALID_REQUEST"));
    };
    match state
        .area_admin
        .create_city(
            workspace(&state),
            payload,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(city) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(city),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

async fn list_drops(State(state): State<AppState>) -> Response {
    match state.area_admin.list_drops(workspace(&state)).await {
        Ok(items) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({"items":items})),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateDropRequest {
    drop_id: String,
    draft: AreaDropDraft,
}
async fn create_drop(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateDropRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = payload else {
        return error(AreaAdminError::Conflict("INVALID_REQUEST"));
    };
    match state
        .area_admin
        .create_draft(
            workspace(&state),
            CreateAreaDropCommand {
                drop_id: payload.drop_id,
                draft: payload.draft,
            },
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(item) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(item),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn get_drop(State(state): State<AppState>, Path(drop_id): Path<String>) -> Response {
    match state.area_admin.get_drop(workspace(&state), &drop_id).await {
        Ok(item) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(item),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveDraftRequest {
    base_revision: i64,
    draft: AreaDropDraft,
}
async fn save_draft(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<SaveDraftRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = payload else {
        return error(AreaAdminError::Conflict("INVALID_REQUEST"));
    };
    match state
        .area_admin
        .save_draft(
            workspace(&state),
            &drop_id,
            payload.base_revision,
            payload.draft,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(item) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(item),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn discard_draft(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state
        .area_admin
        .discard_draft(
            workspace(&state),
            &drop_id,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(e),
    }
}
async fn validate_drop(State(state): State<AppState>, Path(drop_id): Path<String>) -> Response {
    match state
        .area_admin
        .validate_draft(workspace(&state), &drop_id)
        .await
    {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishRequest {
    #[serde(default)]
    confirmations: Vec<String>,
}
async fn publish_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<PublishRequest>, JsonRejection>,
) -> Response {
    let confirmations = match payload {
        Ok(Json(payload)) => payload.confirmations,
        Err(_) => return error(AreaAdminError::Conflict("INVALID_REQUEST")),
    };
    match state
        .area_admin
        .publish(
            workspace(&state),
            &drop_id,
            &confirmations,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn pause_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    lifecycle(state, drop_id, headers, false).await
}
async fn resume_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    lifecycle(state, drop_id, headers, true).await
}
async fn lifecycle(state: AppState, drop_id: String, headers: HeaderMap, active: bool) -> Response {
    match state
        .area_admin
        .set_active(
            workspace(&state),
            &drop_id,
            active,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn archive_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state
        .area_admin
        .archive(
            workspace(&state),
            &drop_id,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DuplicateRequest {
    new_drop_id: String,
    city_id: Uuid,
}
async fn duplicate_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<DuplicateRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = payload else {
        return error(AreaAdminError::Conflict("INVALID_REQUEST"));
    };
    match state
        .area_admin
        .duplicate(
            workspace(&state),
            &drop_id,
            &payload.new_drop_id,
            payload.city_id,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(value) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(e) => error(e),
    }
}
async fn delete_drop(
    State(state): State<AppState>,
    Path(drop_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state
        .area_admin
        .delete_unpublished(
            workspace(&state),
            &drop_id,
            "control-plane",
            request_id(&headers),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(e),
    }
}
