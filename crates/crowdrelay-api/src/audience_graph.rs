//! Operator surface for the Audience Graph.
//!
//! HTTP owns protocol mapping only: every statement lives in
//! `crowdrelay-infra::audience_graph` (api-sql ratchet) and pipeline policy
//! lives in `crowdrelay-domain::audience_graph`. All routes sit under
//! `/v1/admin/`, so authentication and rate limiting are handled by the
//! central privileged-namespace middleware.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use crowdrelay_domain::audience_graph::{OutreachStage, PlaceKind};
use crowdrelay_infra::audience_graph::{
    AudienceGraphError, EvidenceInput, PlaceRulesInput, PostgresAudienceGraphRepository,
    UpsertPlaceInput,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_IMPORT_PLACES: usize = 500;
const MAX_LIST_LIMIT: i64 = 200;

#[derive(Clone, Copy, Debug)]
struct WorkspaceScope(Uuid);

impl WorkspaceScope {
    fn from_state(state: &crate::AppState) -> Self {
        Self(state.ticketing.workspace_id().into_uuid())
    }
}

fn repository(state: &crate::AppState) -> PostgresAudienceGraphRepository {
    PostgresAudienceGraphRepository::new(state.database.clone())
}

/// The workspace boundary is enforced by every repository statement through
/// this scope; handlers never pass raw ids around it.
fn error_response(error: AudienceGraphError, request_id_value: Option<String>) -> Response {
    match error {
        AudienceGraphError::NotFound => Problem::not_found(request_id_value).into_response(),
        // Dynamic detail would leak pipeline internals; the operator reads the
        // stage from GET /places/{id} instead.
        AudienceGraphError::InvalidTransition { .. }
        | AudienceGraphError::CooldownActive { .. } => {
            Problem::conflict(request_id_value).into_response()
        }
        AudienceGraphError::Database(_) => {
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ListPlacesQuery {
    kind: Option<String>,
    status: Option<String>,
    stage: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaceResponse {
    id: Uuid,
    place_kind: String,
    platform: String,
    name: String,
    url: String,
    country_code: Option<String>,
    language: Option<String>,
    genres: Vec<String>,
    member_count: Option<i32>,
    activity_bp: Option<i32>,
    status: String,
    notes: Option<String>,
    rules: Option<PlaceRulesResponse>,
    outreach: Option<OutreachResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaceRulesResponse {
    self_promo_ratio_percent: Option<i16>,
    contact_channel: Option<String>,
    contact_target: Option<String>,
    requires_approval: bool,
    cooldown_days: Option<i16>,
    rules_summary: Option<String>,
    verified_at: Option<time::OffsetDateTime>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutreachResponse {
    stage: String,
    next_eligible_at: Option<time::OffsetDateTime>,
    last_action_at: Option<time::OffsetDateTime>,
}

fn place_response(row: crowdrelay_infra::audience_graph::PlaceRow) -> PlaceResponse {
    PlaceResponse {
        id: row.id,
        place_kind: row.place_kind,
        platform: row.platform,
        name: row.name,
        url: row.url,
        country_code: row.country_code,
        language: row.language,
        genres: row.genres,
        member_count: row.member_count,
        activity_bp: row.activity_bp,
        status: row.status,
        notes: row.notes,
        rules: if row.cooldown_days.is_some() || row.rules_summary.is_some() {
            Some(PlaceRulesResponse {
                self_promo_ratio_percent: row.self_promo_ratio_percent,
                contact_channel: row.contact_channel,
                contact_target: row.contact_target,
                requires_approval: row.requires_approval.unwrap_or(false),
                cooldown_days: row.cooldown_days,
                rules_summary: row.rules_summary,
                verified_at: row.rules_verified_at,
            })
        } else {
            None
        },
        outreach: row
            .stage
            .as_deref()
            .and_then(OutreachStage::from_storage)
            .map(|stage| OutreachResponse {
                stage: stage.as_str().to_owned(),
                next_eligible_at: row.next_eligible_at,
                last_action_at: row.last_action_at,
            }),
    }
}

pub async fn list_places(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListPlacesQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let kind = query.kind.as_deref().and_then(PlaceKind::from_storage);
    let stage = query.stage.as_deref().and_then(OutreachStage::from_storage);
    if (query.kind.is_some() && kind.is_none()) || (query.stage.is_some() && stage.is_none()) {
        return Problem::bad_request(request_id_value).into_response();
    }
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_LIST_LIMIT);
    match repository(&state)
        .list_places(
            WorkspaceScope::from_state(&state).0,
            kind,
            query.status.as_deref(),
            stage,
            limit,
        )
        .await
    {
        Ok(rows) => {
            let places: Vec<PlaceResponse> = rows.into_iter().map(place_response).collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(serde_json::json!({ "places": places })),
            )
                .into_response()
        }
        Err(error) => error_response(error, request_id_value),
    }
}

pub async fn place_detail(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(place_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state)
        .place_detail(WorkspaceScope::from_state(&state).0, place_id)
        .await
    {
        Ok(row) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(place_response(row)),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpsertPlaceRequest {
    place_kind: String,
    platform: String,
    name: String,
    url: String,
    country_code: Option<String>,
    language: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    member_count: Option<i32>,
    /// 0..=10000, mirroring confidence basis points used elsewhere.
    activity_bp: Option<i32>,
    notes: Option<String>,
}

impl UpsertPlaceRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if PlaceKind::from_storage(&self.place_kind).is_none() {
            return Err("unknown placeKind");
        }
        if self.platform.trim().is_empty() || self.platform.len() > 64 {
            return Err("platform must be 1..64 characters");
        }
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err("name must be 1..200 characters");
        }
        if self.url.is_empty() || self.url.len() > 512 {
            return Err("url must be 1..512 characters");
        }
        if self
            .activity_bp
            .is_some_and(|value| !(0..=10_000).contains(&value))
        {
            return Err("activityBp must be within 0..10000");
        }
        Ok(())
    }

    fn to_input(&self, workspace_id: Uuid) -> UpsertPlaceInput<'_> {
        UpsertPlaceInput {
            workspace_id,
            place_kind: PlaceKind::from_storage(&self.place_kind).unwrap_or(PlaceKind::Other),
            platform: self.platform.trim(),
            name: self.name.trim(),
            url: &self.url,
            country_code: self.country_code.as_deref(),
            language: self.language.as_deref(),
            genres: &self.genres,
            member_count: self.member_count,
            activity_bp: self.activity_bp,
            notes: self.notes.as_deref(),
        }
    }
}

pub async fn upsert_place(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpsertPlaceRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if let Some(reason) = request.validate().err() {
        tracing::debug!(reason, "audience graph place rejected");
        return Problem::unprocessable(request_id_value).into_response();
    }
    let workspace_id = WorkspaceScope::from_state(&state).0;
    match repository(&state)
        .upsert_place(&request.to_input(workspace_id))
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "placeId": id })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
pub struct ImportScanRequest {
    places: Vec<ScanPlaceEntry>,
    method: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScanPlaceEntry {
    place: UpsertPlaceRequest,
    evidence_kind: Option<String>,
    confidence_bp: Option<i32>,
    payload: Option<serde_json::Value>,
}

pub async fn import_scan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ImportScanRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if request.places.is_empty() || request.places.len() > MAX_IMPORT_PLACES {
        return Problem::unprocessable(request_id_value).into_response();
    }
    for entry in &request.places {
        if entry.place.validate().is_err() {
            return Problem::unprocessable(request_id_value).into_response();
        }
    }
    let default_method: &str = request.method.as_deref().unwrap_or("manual_import");
    let workspace_id = WorkspaceScope::from_state(&state).0;
    let inputs: Vec<UpsertPlaceInput<'_>> = request
        .places
        .iter()
        .map(|entry| entry.place.to_input(workspace_id))
        .collect();
    // Evidence inputs borrow from the request body, so the map is built over
    // the same lifetime as `inputs`.
    let mut evidence_by_index: HashMap<usize, Vec<EvidenceInput<'_>>> = HashMap::new();
    for (index, entry) in request.places.iter().enumerate() {
        if entry.evidence_kind.is_none() && entry.payload.is_none() {
            continue;
        }
        let kind = entry.evidence_kind.as_deref().unwrap_or("scan");
        if !matches!(
            kind,
            "scan" | "mention" | "sample_post" | "mod_contact" | "manual_note"
        ) {
            return Problem::unprocessable(request_id_value).into_response();
        }
        let confidence = entry.confidence_bp.unwrap_or(5_000);
        if !(0..=10_000).contains(&confidence) {
            return Problem::unprocessable(request_id_value).into_response();
        }
        evidence_by_index.insert(
            index,
            vec![EvidenceInput {
                evidence_kind: kind,
                method: default_method,
                confidence_bp: confidence,
                payload: entry.payload.as_ref().unwrap_or(&serde_json::Value::Null),
            }],
        );
    }
    match repository(&state)
        .import_scan_batch(&inputs, &evidence_by_index)
        .await
    {
        Ok(ids) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "imported": ids.len(),
                "placeIds": ids,
            })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachRulesRequest {
    self_promo_ratio_percent: Option<i16>,
    contact_channel: Option<String>,
    contact_target: Option<String>,
    #[serde(default)]
    requires_approval: bool,
    #[serde(default = "default_cooldown_days")]
    cooldown_days: i16,
    rules_summary: Option<String>,
    #[serde(default)]
    verified: bool,
}

fn default_cooldown_days() -> i16 {
    14
}

pub async fn attach_rules(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(place_id): Path<Uuid>,
    payload: Result<Json<AttachRulesRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if request
        .self_promo_ratio_percent
        .is_some_and(|value| !(0..=100).contains(&value))
        || !(1..=365).contains(&request.cooldown_days)
    {
        return Problem::unprocessable(request_id_value).into_response();
    }
    let rules = PlaceRulesInput {
        self_promo_ratio_percent: request.self_promo_ratio_percent,
        contact_channel: request.contact_channel.as_deref(),
        contact_target: request.contact_target.as_deref(),
        requires_approval: request.requires_approval,
        cooldown_days: request.cooldown_days,
        rules_summary: request.rules_summary.as_deref(),
    };
    match repository(&state)
        .attach_rules(
            WorkspaceScope::from_state(&state).0,
            place_id,
            &rules,
            request.verified,
        )
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "attached": true })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppendEvidenceRequest {
    evidence_kind: String,
    method: String,
    #[serde(default = "default_confidence")]
    confidence_bp: i32,
    #[serde(default)]
    payload: serde_json::Value,
}

fn default_confidence() -> i32 {
    5_000
}

pub async fn append_evidence(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(place_id): Path<Uuid>,
    payload: Result<Json<AppendEvidenceRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !matches!(
        request.evidence_kind.as_str(),
        "scan" | "mention" | "sample_post" | "mod_contact" | "manual_note"
    ) || !(0..=10_000).contains(&request.confidence_bp)
    {
        return Problem::unprocessable(request_id_value).into_response();
    }
    let input = EvidenceInput {
        evidence_kind: &request.evidence_kind,
        method: &request.method,
        confidence_bp: request.confidence_bp,
        payload: &request.payload,
    };
    match repository(&state)
        .append_evidence(WorkspaceScope::from_state(&state).0, place_id, &input)
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "recorded": true })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdvanceOutreachRequest {
    from_stage: String,
    to_stage: String,
    outcome_notes: Option<String>,
}

pub async fn advance_outreach(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(place_id): Path<Uuid>,
    payload: Result<Json<AdvanceOutreachRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let (Some(from), Some(to)) = (
        OutreachStage::from_storage(&request.from_stage),
        OutreachStage::from_storage(&request.to_stage),
    ) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // The optimistic from_stage makes concurrent operator moves fail loudly
    // instead of silently double-applying one transition.
    match repository(&state)
        .advance_outreach(
            WorkspaceScope::from_state(&state).0,
            place_id,
            from,
            to,
            request.outcome_notes.as_deref(),
        )
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "stage": to.as_str(),
                "advanced": true,
            })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

/// All graph surfaces live under the privileged admin namespace; auth and
/// rate limiting come from the central middleware, not from here.
pub(super) fn admin_routes() -> Router<crate::AppState> {
    Router::new()
        .route(
            "/v1/admin/audience-graph/places",
            get(list_places).post(upsert_place),
        )
        .route("/v1/admin/audience-graph/places/import", post(import_scan))
        .route(
            "/v1/admin/audience-graph/places/{place_id}",
            get(place_detail),
        )
        .route(
            "/v1/admin/audience-graph/places/{place_id}/rules",
            put(attach_rules),
        )
        .route(
            "/v1/admin/audience-graph/places/{place_id}/evidence",
            post(append_evidence),
        )
        .route(
            "/v1/admin/audience-graph/places/{place_id}/outreach/advance",
            post(advance_outreach),
        )
}
