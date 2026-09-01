//! Community Intelligence read endpoints.
//!
//! Control-plane-authed read endpoints for the community observation layer.
//! These endpoints expose the structured observations and extracted entities
//! that the Brain can reason over. No sentiment, no affinity, no "relevance
//! score" — just facts.
//!
//! They live in the `/v1/control-plane/` namespace because the only caller is
//! the platform plane, which holds the derived ControlPlane management token
//! and never the tenant's admin bearer. Under `/v1/admin/` the authority check
//! demanded `PrivilegedAuthorization::Admin`, so the panel could not reach them
//! at all — and the AREA tunnel, which allowlists `/v1/control-plane/` paths
//! only, answered 404 before the request ever arrived.

use axum::http::StatusCode;
use axum::{
    Json,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use uuid::Uuid;

use crate::Problem;
use crowdrelay_infra::community_intelligence::PostgresCommunityIntelligenceRepository;

const PRIVATE_NO_STORE: &str = "private, no-store";
const CACHE_CONTROL: &str = "cache-control";
const MAX_LIST_LIMIT: i64 = 100;

#[derive(Clone, Copy, Debug)]
struct WorkspaceScope(Uuid);

impl WorkspaceScope {
    fn from_state(state: &crate::AppState) -> Self {
        Self(state.ticketing.workspace_id().into_uuid())
    }
}

fn repository(state: &crate::AppState) -> PostgresCommunityIntelligenceRepository {
    PostgresCommunityIntelligenceRepository::new(state.database.clone())
}

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/v1/control-plane/community-intelligence/communities",
            get(list_communities),
        )
        .route(
            "/v1/control-plane/community-intelligence/communities/{place_id}/observations",
            get(list_observations),
        )
        .route(
            "/v1/control-plane/community-intelligence/communities/{place_id}/entities",
            get(list_entities),
        )
}

/// Lists all tracked communities (discovery_places) with their latest
/// observation for the workspace.
async fn list_communities(State(state): State<crate::AppState>) -> Response {
    let workspace_id = WorkspaceScope::from_state(&state).0;
    match repository(&state)
        .places_with_latest_observations(workspace_id)
        .await
    {
        Ok(places) => {
            let items: Vec<serde_json::Value> = places
                .iter()
                .map(|p| {
                    json!({
                        "placeId": p.place_id,
                        "placeKind": p.place_kind,
                        "platform": p.platform,
                        "name": p.name,
                        "url": p.url,
                        "countryCode": p.country_code,
                        "language": p.language,
                        "genres": p.genres,
                        "memberCount": p.member_count,
                        "latestObservation": p.latest_observation_id.map(|id| {
                            json!({
                                "id": id,
                                "observedAt": p.latest_observed_at,
                                "source": p.latest_source,
                                "quality": p.latest_observation_quality,
                                "rawActivityMetrics": p.latest_raw_activity_metrics,
                            })
                        }),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({ "items": items })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "community intelligence list failed");
            Problem::internal(None).into_response()
        }
    }
}

/// Lists the observation time series for one community.
async fn list_observations(
    State(state): State<crate::AppState>,
    Path(place_id): Path<Uuid>,
) -> Response {
    let workspace_id = WorkspaceScope::from_state(&state).0;
    match repository(&state)
        .observations_for_place(workspace_id, place_id, MAX_LIST_LIMIT)
        .await
    {
        Ok(observations) => {
            let items: Vec<serde_json::Value> = observations
                .iter()
                .map(|o| {
                    json!({
                        "id": o.id,
                        "placeId": o.place_id,
                        "observedAt": o.observed_at,
                        "source": o.source,
                        "sourceUrl": o.source_url,
                        "collectorVersion": o.collector_version,
                        "rawActivityMetrics": o.raw_activity_metrics,
                        "observationQuality": o.observation_quality,
                        "createdAt": o.created_at,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({ "items": items })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "community intelligence observations failed");
            Problem::internal(None).into_response()
        }
    }
}

/// Lists the extracted entities for the latest observation of a community.
async fn list_entities(
    State(state): State<crate::AppState>,
    Path(place_id): Path<Uuid>,
) -> Response {
    let workspace_id = WorkspaceScope::from_state(&state).0;
    let repo = repository(&state);

    // Get the latest observation for this place.
    let observations = match repo.observations_for_place(workspace_id, place_id, 1).await {
        Ok(o) => o,
        Err(error) => {
            tracing::error!(error = %error, "community intelligence entities failed");
            return Problem::internal(None).into_response();
        }
    };

    let Some(latest) = observations.first() else {
        return (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "items": [] })),
        )
            .into_response();
    };

    match repo.entities_for_observation(workspace_id, latest.id).await {
        Ok(entities) => {
            let items: Vec<serde_json::Value> = entities
                .iter()
                .map(|e| {
                    json!({
                        "id": e.id,
                        "observationId": e.observation_id,
                        "entityType": e.entity_type,
                        "entityRef": e.entity_ref,
                        "strength": e.strength,
                        "observedAt": e.observed_at,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({ "items": items, "observationId": latest.id })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "community intelligence entities failed");
            Problem::internal(None).into_response()
        }
    }
}
