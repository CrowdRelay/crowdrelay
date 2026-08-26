//! Label portfolio operator surface.
//!
//! One tenant = a label or festival operating a roster; amplification edges
//! route consented audience between roster members. Protocol mapping only:
//! statements live in `crowdrelay-infra::portfolio`, lifecycle policy in
//! `crowdrelay-domain::portfolio`.

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crowdrelay_domain::portfolio::{AmplificationPurpose, ConsentStatus};
use crowdrelay_infra::portfolio::{PortfolioError, PostgresPortfolioRepository};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_BATCH_FANS: i64 = 5_000;

fn repository(state: &crate::AppState) -> PostgresPortfolioRepository {
    PostgresPortfolioRepository::new(state.database.clone())
}

fn workspace_id(state: &crate::AppState) -> Uuid {
    state.ticketing.workspace_id().into_uuid()
}

fn error_response(error: PortfolioError, request_id_value: Option<String>) -> Response {
    match error {
        PortfolioError::NotFound => Problem::not_found(request_id_value).into_response(),
        PortfolioError::InvalidDecision | PortfolioError::NotInSameOrganization => {
            Problem::conflict(request_id_value).into_response()
        }
        PortfolioError::CapReached => Problem::conflict(request_id_value).into_response(),
        PortfolioError::Database(_) => {
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}

#[derive(Serialize)]
struct ConsentResponse {
    id: Uuid,
    from_workspace_id: Uuid,
    to_workspace_id: Uuid,
    purpose: String,
    scope: String,
    status: String,
    max_campaigns_per_month: i16,
    cooldown_days: i16,
    campaigns_this_month: i64,
    approved_by: Option<String>,
    approved_at: Option<time::OffsetDateTime>,
    revoked_at: Option<time::OffsetDateTime>,
}

fn consent_response(row: crowdrelay_infra::portfolio::ConsentRow) -> ConsentResponse {
    ConsentResponse {
        id: row.id,
        from_workspace_id: row.from_workspace_id,
        to_workspace_id: row.to_workspace_id,
        purpose: row.purpose,
        scope: row.scope,
        status: row.status,
        max_campaigns_per_month: row.max_campaigns_per_month,
        cooldown_days: row.cooldown_days,
        campaigns_this_month: row.campaigns_this_month,
        approved_by: row.approved_by,
        approved_at: row.approved_at,
        revoked_at: row.revoked_at,
    }
}

#[derive(Deserialize)]
pub struct CreateOrganizationRequest {
    slug: String,
    name: String,
}

pub async fn create_organization(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateOrganizationRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let slug_ok = !request.slug.trim().is_empty()
        && request.slug.len() <= 128
        && request.slug.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    if !slug_ok || request.name.trim().is_empty() || request.name.len() > 200 {
        return Problem::unprocessable(request_id_value).into_response();
    }
    match repository(&state)
        .create_organization_for_workspace(
            workspace_id(&state),
            request.slug.trim(),
            request.name.trim(),
        )
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "organizationId": id })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProposeRequest {
    to_workspace_id: Uuid,
    purpose: String,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default = "default_cap")]
    max_campaigns_per_month: i16,
    #[serde(default = "default_cooldown")]
    cooldown_days: i16,
}

fn default_scope() -> String {
    "all_active".to_owned()
}
fn default_cap() -> i16 {
    2
}
fn default_cooldown() -> i16 {
    21
}

pub async fn propose_amplification(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ProposeRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(purpose) = AmplificationPurpose::from_storage(&request.purpose) else {
        return Problem::unprocessable(request_id_value).into_response();
    };
    if request.scope != "all_active" && request.scope != "double_opt_in" {
        return Problem::unprocessable(request_id_value).into_response();
    }
    match repository(&state)
        .propose_amplification(
            workspace_id(&state),
            request.to_workspace_id,
            purpose,
            &request.scope,
            request.max_campaigns_per_month,
            request.cooldown_days,
        )
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "consentId": id, "status": "proposed" })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
pub struct ListConsentsQuery {
    status: Option<String>,
}

pub async fn list_amplification(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListConsentsQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let status_filter = query
        .status
        .as_deref()
        .and_then(ConsentStatus::from_storage);
    if query.status.is_some() && status_filter.is_none() {
        return Problem::bad_request(request_id_value).into_response();
    }
    match repository(&state).list_consents(workspace_id(&state)).await {
        Ok(rows) => {
            let consents: Vec<ConsentResponse> = rows
                .into_iter()
                .filter(|row| {
                    status_filter
                        .map(|wanted| ConsentStatus::from_storage(&row.status) == Some(wanted))
                        .unwrap_or(true)
                })
                .map(consent_response)
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(serde_json::json!({ "consents": consents })),
            )
                .into_response()
        }
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DecideRequest {
    action: String,
    actor: Option<String>,
    revoke_reason: Option<String>,
}

pub async fn decide_amplification(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(consent_id): Path<Uuid>,
    payload: Result<Json<DecideRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // Approve activates; pause/resume/revoke map onto their targets. The
    // domain transition table is the authority on what is legal from where.
    let target = match request.action.as_str() {
        "approve" => ConsentStatus::Active,
        "pause" => ConsentStatus::Paused,
        "resume" => ConsentStatus::Active,
        "revoke" => ConsentStatus::Revoked,
        _ => return Problem::bad_request(request_id_value).into_response(),
    };
    if request.action == "approve" && request.actor.as_deref().unwrap_or("").trim().is_empty() {
        // An activation without a named approver has no paper trail.
        return Problem::unprocessable(request_id_value).into_response();
    }
    if request.action == "revoke"
        && request
            .revoke_reason
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        return Problem::unprocessable(request_id_value).into_response();
    }
    match repository(&state)
        .decide_amplification(
            workspace_id(&state),
            consent_id,
            target,
            request.actor.as_deref(),
            request.revoke_reason.as_deref(),
        )
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "status": target.as_str() })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn preview_audience(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(consent_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state)
        .preview_audience(workspace_id(&state), consent_id)
        .await
    {
        Ok(count) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "reachableFans": count })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunCampaignRequest {
    campaign_reference: String,
    subject: String,
    text: String,
    limit: Option<i64>,
}

pub async fn run_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(consent_id): Path<Uuid>,
    payload: Result<Json<RunCampaignRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let reference_ok =
        !request.campaign_reference.trim().is_empty() && request.campaign_reference.len() <= 200;
    if !reference_ok
        || request.subject.trim().is_empty()
        || request.text.trim().is_empty()
        || request
            .limit
            .is_some_and(|limit| !(1..=MAX_BATCH_FANS).contains(&limit))
    {
        return Problem::unprocessable(request_id_value).into_response();
    }
    match repository(&state)
        .run_amplification_campaign(
            workspace_id(&state),
            consent_id,
            request.campaign_reference.trim(),
            request.subject.trim(),
            request.text.trim(),
            request.limit.unwrap_or(2_000),
        )
        .await
    {
        Ok(reached) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "queued": reached,
                "campaignReference": request.campaign_reference.trim(),
            })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn portfolio_overview(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state).org_overview(workspace_id(&state)).await {
        Ok(row) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "workspaceCount": row.workspace_count,
                "activeFans": row.active_fans,
                "fansLast30d": row.fans_last_30d,
                "activeEdges": row.active_edges,
                "deliveriesLast30d": row.deliveries_last_30d,
            })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

/// All portfolio surfaces sit under the privileged admin namespace; auth and
/// rate limiting come from the central middleware.
pub(super) fn admin_routes() -> Router<crate::AppState> {
    Router::new()
        .route(
            "/v1/admin/portfolio/organization",
            post(create_organization),
        )
        .route("/v1/admin/portfolio/overview", get(portfolio_overview))
        .route(
            "/v1/admin/portfolio/amplification",
            get(list_amplification).post(propose_amplification),
        )
        .route(
            "/v1/admin/portfolio/amplification/{consent_id}/decide",
            post(decide_amplification),
        )
        .route(
            "/v1/admin/portfolio/amplification/{consent_id}/audience-preview",
            get(preview_audience),
        )
        .route(
            "/v1/admin/portfolio/amplification/{consent_id}/campaign",
            post(run_campaign),
        )
}
