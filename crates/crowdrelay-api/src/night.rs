//! The shared night — §12-9's rendezvous, on the control-plane surface.
//!
//! `GET /v1/control-plane/nights/{place_event_id}` answers "who else is on
//! this night, and what did each side choose to share" — one endpoint whose
//! payload's shape is the caller's lens, resolved inside the repository
//! from the caller's relationship, never from a parameter. A caller with no
//! relationship to the night gets the same 404 as a night that does not
//! exist.
//!
//! The writes are small and symmetric: contribute, revoke, mint the
//! organiser link, revoke it, and the billed act's own confirmation. The
//! link-holder's read is `/v1/public/nights/{token}` — the organiser lens
//! forced, the token the whole credential — registered in `routing.rs`.
//!
//! Everything here is thin over `PostgresNightRepository`: the SQL and the
//! boundary both live in infra, because "the organiser sees the sum, never
//! the parts" is a property of the query, not of this file's good manners.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::night::{ContributionKind, validate_contribution};
use crowdrelay_infra::night::{NightError, PostgresNightRepository};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Problem, request_id};

fn night_repo(state: &crate::AppState) -> PostgresNightRepository {
    PostgresNightRepository::new(state.database.clone())
}

fn night_problem(error: NightError, request_id: Option<String>) -> Response {
    match error {
        NightError::NotFound => Problem::not_found(request_id).private().into_response(),
        NightError::Database(error) => {
            tracing::warn!(%error, "night query failed");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
    }
}

/// GET — the night, projected for the caller's resolved lens.
pub async fn get_night(
    State(state): State<crate::AppState>,
    Path(place_event_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match night_repo(&state).load(workspace_id, place_event_id).await {
        Ok(view) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(view),
        )
            .into_response(),
        Err(error) => night_problem(error, request_id_value),
    }
}

/// The contribution write — `{kind, value}`. `kind` is parsed, not
/// deserialized: an unknown kind is a 400 naming the field, not an axum
/// rejection naming the syntax.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributionBody {
    kind: String,
    value: serde_json::Value,
}

/// POST — publish or replace one contribution. The domain validates the
/// value against the kind's shape before the write; the refusal's sentence
/// reaches the caller as the detail, same as the listing's review does.
pub async fn upsert_night_contribution(
    State(state): State<crate::AppState>,
    Path(place_event_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ContributionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(kind) = ContributionKind::parse(&body.kind) else {
        return Problem::bad_request_because(
            "kind must be one of draw_estimate, announce_status, asks, terms",
            request_id_value,
        )
        .private()
        .into_response();
    };
    if let Err(refusal) = validate_contribution(kind, &body.value) {
        return Problem::conflict_owned(refusal.message().into(), request_id_value)
            .private()
            .into_response();
    }
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = night_repo(&state);
    match repo
        .upsert_contribution(workspace_id, place_event_id, kind, &body.value)
        .await
    {
        Ok(()) => get_night(State(state), Path(place_event_id), headers).await,
        Err(error) => night_problem(error, request_id_value),
    }
}

/// DELETE — withdraw a contribution. The row stays as `revoked` — the
/// record that the workspace once chose to share is the audit, not a hole.
pub async fn revoke_night_contribution(
    State(state): State<crate::AppState>,
    Path((place_event_id, kind)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(kind) = ContributionKind::parse(&kind) else {
        return Problem::bad_request_because(
            "kind must be one of draw_estimate, announce_status, asks, terms",
            request_id_value,
        )
        .private()
        .into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = night_repo(&state);
    match repo
        .revoke_contribution(workspace_id, place_event_id, kind)
        .await
    {
        Ok(()) => get_night(State(state), Path(place_event_id), headers).await,
        Err(error) => night_problem(error, request_id_value),
    }
}

/// POST — mint the night's organiser link. Revoke-then-mint means every
/// link already sent dies on this call; the token returns once, here.
pub async fn mint_night_organiser_link(
    State(state): State<crate::AppState>,
    Path(place_event_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match night_repo(&state)
        .mint_organiser_link(workspace_id, place_event_id)
        .await
    {
        Ok(link) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(link),
        )
            .into_response(),
        Err(error) => night_problem(error, request_id_value),
    }
}

/// DELETE — kill the live organiser link. The minter or an event owner may
/// revoke; a sent link goes to 404 at once.
pub async fn revoke_night_organiser_link(
    State(state): State<crate::AppState>,
    Path(place_event_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = night_repo(&state);
    match repo
        .revoke_organiser_link(workspace_id, place_event_id)
        .await
    {
        Ok(()) => get_night(State(state), Path(place_event_id), headers).await,
        Err(error) => night_problem(error, request_id_value),
    }
}

/// POST — the named act confirms it is on this bill. The repository's
/// `act_workspace_id = caller` clause is the whole rule: a band confirms
/// itself only, and another workspace's claim about it cannot be confirmed
/// by anyone but the act.
pub async fn confirm_night_act(
    State(state): State<crate::AppState>,
    Path((place_event_id, act_slug)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = night_repo(&state);
    match repo
        .confirm_act(workspace_id, place_event_id, &act_slug)
        .await
    {
        Ok(()) => get_night(State(state), Path(place_event_id), headers).await,
        Err(error) => night_problem(error, request_id_value),
    }
}

// ── The public read ──────────────────────────────────────────────────

/// GET `/v1/public/nights/{token}` — what a link-holder receives.
///
/// The token is the whole credential: a live organiser link returns the
/// organiser lens; anything else — wrong token, revoked, expired — is the
/// same 404, because "this link used to work" is the band's business, not
/// the reader's. Cacheable for a minute — a revocation shows up as a 404
/// fast, not as a stale copy living forever.
pub async fn public_night(
    State(state): State<crate::AppState>,
    Path(token): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    match night_repo(&state).load_organiser_by_token(token).await {
        Ok(view) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(view),
        )
            .into_response(),
        Err(NightError::NotFound) => Problem::not_found(request_id(&headers)).into_response(),
        Err(NightError::Database(error)) => {
            tracing::warn!(%error, "public night read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
