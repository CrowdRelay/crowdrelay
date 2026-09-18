//! The content engine's operator surface — the peer write path.
//!
//! Peers are the artists the band watches: the peer-act-graph scanner
//! proposes candidates, the operator confirms or refuses them here, and
//! only a `confirmed` row is ever observed. Admin rather than
//! control-plane — this is operator tooling, not the band's console.
//!
//! A proposal arrives without handles and is never observable until the
//! operator adds them, so the resolve route's confirm carries an optional
//! patch applied in the same guarded `UPDATE`. A reject carries none — the
//! row is terminal, and its reason is the suppression record that stops the
//! scanner asking again.

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_domain::content_engine::{Peer, PeerStatus, PeerTier};
use crowdrelay_infra::content_engine::{ContentEngineError, PostgresContentEngineRepository};
use crowdrelay_infra::content_peers::{NewPeer, PeerOutcome, PeerPatch};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

/// The peer write path's admin surface. Auth and rate limiting come from the
/// central middleware — the `/v1/admin/` prefix is the boundary, not a
/// per-route capability.
pub(super) fn admin_routes() -> Router<crate::AppState> {
    Router::new()
        // Operator-entered peers land confirmed, scanner-proposed candidates
        // land proposed, and a resolve confirms or refuses them (confirm
        // carries the handles a proposal lacks, or the sweep can never
        // observe it).
        .route(
            "/v1/admin/content-engine/peers",
            get(list_peers).post(create_peer),
        )
        .route(
            "/v1/admin/content-engine/peers/{peer_id}/resolve",
            post(resolve_peer),
        )
}

/// A peer as the operator reads it — the domain row flattened to the wire
/// shape, ids and enums as their string forms.
#[derive(Debug, Serialize)]
struct PeerView {
    id: Uuid,
    name: String,
    tier: &'static str,
    handles: serde_json::Value,
    watch_for: Vec<String>,
    why: String,
    proposed_by: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejection_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl From<&Peer> for PeerView {
    fn from(peer: &Peer) -> Self {
        Self {
            id: peer.id.into_uuid(),
            name: peer.name.clone(),
            tier: peer.tier.as_str(),
            handles: peer.handles.clone(),
            watch_for: peer.watch_for.clone(),
            why: peer.why.clone(),
            proposed_by: peer.proposed_by.clone(),
            status: peer.status.as_str(),
            rejection_reason: peer.rejection_reason.clone(),
            confirmed_at: peer.confirmed_at,
            created_at: peer.created_at,
            updated_at: peer.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct PeerListResponse {
    peers: Vec<PeerView>,
}

fn repository(state: &crate::AppState) -> PostgresContentEngineRepository {
    PostgresContentEngineRepository::new(state.database.clone())
}

fn parsed_request_id(headers: &HeaderMap) -> Option<RequestId> {
    request_id(headers).and_then(|value| RequestId::parse(value).ok())
}

/// `GET /v1/admin/content-engine/peers` — the watch list as it stands,
/// proposals included, optionally narrowed to one status.
pub async fn list_peers(
    State(state): State<crate::AppState>,
    Query(params): Query<PeerListParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let status = match params.status.as_deref().map(str::trim) {
        Some(value) => match PeerStatus::parse(value) {
            Some(status) => Some(status),
            None => {
                return Problem::bad_request(request_id_value)
                    .private()
                    .into_response();
            }
        },
        None => None,
    };
    match repository(&state)
        .list_peers(state.ticketing.workspace_id(), status)
        .await
    {
        Ok(peers) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PeerListResponse {
                peers: peers.iter().map(PeerView::from).collect(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "peer list read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PeerListParams {
    /// One of `proposed` | `confirmed` | `rejected`; absent lists all.
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePeerRequest {
    /// The artist's name — the dedup identity. A live peer already carrying
    /// it conflicts rather than silently deduplicating.
    name: String,
    /// `aspirational` | `near_peer` | `lateral` — which questions watching
    /// them answers.
    tier: String,
    /// `{"youtube": "@handle", "rss": "https://..."}` — the observation
    /// sweep reads whichever platforms a peer uses.
    #[serde(default)]
    handles: Option<serde_json::Value>,
    /// Which dimensions are worth observing for this peer.
    #[serde(default)]
    watch_for: Vec<String>,
    /// Why they are on the list, in the operator's words — the audit.
    why: String,
}

/// `POST /v1/admin/content-engine/peers` — an operator-entered peer lands
/// `confirmed`: typed in by a person, it is already the decision.
pub async fn create_peer(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreatePeerRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let tier = PeerTier::parse(request.tier.trim());
    if request.name.trim().is_empty()
        || request.why.trim().is_empty()
        || tier.is_none()
        || request.handles.as_ref().is_some_and(|h| !h.is_object())
    {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let Some(idempotency_key) = headers
        .get(&crate::IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        // Required rather than generated: a retried click must not become a
        // second audit row, and only the caller knows which click this is.
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let peer = NewPeer {
        name: request.name,
        handles: request.handles.unwrap_or_else(|| serde_json::json!({})),
        tier: tier.unwrap_or(PeerTier::NearPeer),
        watch_for: request.watch_for,
        why: request.why,
        proposed_by: "operator".to_owned(),
        confirmed: true,
    };
    match repository(&state)
        .create_operator_peer(
            state.ticketing.workspace_id(),
            &peer,
            &idempotency_key,
            parsed_request_id(&headers).as_ref(),
        )
        .await
    {
        Ok(PeerOutcome::Applied(peer)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PeerView::from(&peer)),
        )
            .into_response(),
        Ok(PeerOutcome::Replayed(peer)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PeerView::from(&peer)),
        )
            .into_response(),
        Err(ContentEngineError::PeerNameTaken | ContentEngineError::KeyConflict) => {
            Problem::conflict(request_id_value)
                .private()
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "peer create failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvePeerRequest {
    /// `confirmed` | `rejected` — the only statuses a proposal may resolve to.
    status: String,
    /// Required on `rejected` — the record that stops the same wrong name
    /// being proposed twice. Must be absent on `confirmed`.
    #[serde(default)]
    rejection_reason: Option<String>,
    /// Confirm-time edits: a scanner proposal arrives without handles and
    /// is never observable until the operator adds them.
    #[serde(default)]
    handles: Option<serde_json::Value>,
    #[serde(default)]
    watch_for: Option<Vec<String>>,
    #[serde(default)]
    tier: Option<String>,
}

/// `POST /v1/admin/content-engine/peers/{peer_id}/resolve` — confirm or
/// refuse a `proposed` peer. The guard lives in the `UPDATE`'s `WHERE`
/// clause, so a stale second decision conflicts rather than rewriting the
/// first.
pub async fn resolve_peer(
    State(state): State<crate::AppState>,
    Path(peer_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ResolvePeerRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(peer_uuid) = Uuid::parse_str(peer_id.trim()) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(next) = PeerStatus::parse(request.status.trim())
        .filter(|status| matches!(status, PeerStatus::Confirmed | PeerStatus::Rejected))
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let tier = match request.tier.as_deref().map(str::trim) {
        Some(value) => match PeerTier::parse(value) {
            Some(tier) => Some(tier),
            None => {
                return Problem::bad_request(request_id_value)
                    .private()
                    .into_response();
            }
        },
        None => None,
    };
    let has_patch = request.handles.is_some() || request.watch_for.is_some() || tier.is_some();
    // A rejection is terminal — there is no row left to edit. Fields on a
    // reject are a malformed request, not a smaller confirm; a reason on a
    // confirm is the same class of bug.
    if (next == PeerStatus::Rejected && has_patch)
        || (next == PeerStatus::Confirmed && request.rejection_reason.is_some())
        || request.handles.as_ref().is_some_and(|h| !h.is_object())
    {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let patch = has_patch.then_some(PeerPatch {
        handles: request.handles,
        watch_for: request.watch_for,
        tier,
    });
    let Some(idempotency_key) = headers
        .get(&crate::IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match repository(&state)
        .resolve_peer_operator(
            state.ticketing.workspace_id(),
            crowdrelay_domain::PeerId::from_uuid(peer_uuid),
            next,
            request.rejection_reason.as_deref(),
            patch.as_ref(),
            &idempotency_key,
            parsed_request_id(&headers).as_ref(),
        )
        .await
    {
        Ok(PeerOutcome::Applied(peer)) | Ok(PeerOutcome::Replayed(peer)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PeerView::from(&peer)),
        )
            .into_response(),
        Err(ContentEngineError::MissingReason) => Problem::unprocessable(request_id_value)
            .private()
            .into_response(),
        Err(ContentEngineError::InvalidTransition | ContentEngineError::KeyConflict) => {
            Problem::conflict(request_id_value)
                .private()
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "peer resolve failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
