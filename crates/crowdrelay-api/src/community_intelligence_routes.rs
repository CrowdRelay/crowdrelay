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
    routing::{get, post},
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
        .route(
            "/v1/control-plane/community-intelligence/communities/{place_id}/membership",
            post(set_membership),
        )
        .route(
            "/v1/control-plane/community-intelligence/communities/{place_id}/intro-draft",
            get(intro_draft),
        )
}

/// The states an operator can record for a community.
///
/// `not_a_fit` is separate from `rejected` on purpose: one is our judgement,
/// the other is theirs, and collapsing them loses the reason a place should
/// not be revisited.
const MEMBERSHIP_STATES: [&str; 5] = ["not_joined", "joining", "joined", "rejected", "not_a_fit"];

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetMembershipRequest {
    state: String,
    /// Why — the rule that got us rejected, the channel we may post in.
    note: Option<String>,
}

/// Records where we stand with a community.
async fn set_membership(
    State(state): State<crate::AppState>,
    Path(place_id): Path<Uuid>,
    Json(body): Json<SetMembershipRequest>,
) -> Response {
    let workspace_id = WorkspaceScope::from_state(&state).0;
    let requested = body.state.trim();
    if !MEMBERSHIP_STATES.contains(&requested) {
        return Problem::bad_request(None).into_response();
    }
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty());
    if note.is_some_and(|note| note.chars().count() > 1000) {
        return Problem::bad_request(None).into_response();
    }
    match repository(&state)
        .set_place_membership(workspace_id, place_id, requested, note, "control-plane")
        .await
    {
        Ok(true) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "placeId": place_id, "membershipState": requested })),
        )
            .into_response(),
        Ok(false) => Problem::not_found(None).into_response(),
        Err(error) => {
            tracing::warn!(%error, "community membership update failed");
            Problem::service_unavailable(None).into_response()
        }
    }
}

/// Drafts an introduction post for a community, from what was observed of it.
///
/// Grounded, not invented. The draft names the genres this community actually
/// discusses — taken from `community_entities`, which the source adapters
/// extracted from its own posts — and pairs them against the band's. Where
/// they overlap, that overlap is the reason to be there and the draft says so.
/// Where they do not, the draft says that too rather than papering over it,
/// because posting into a community you do not fit is how a band gets banned
/// and it is better to learn that before writing than after.
///
/// It is a draft. Nothing sends it: the operator reads it, edits it in their
/// own voice, and posts it as themselves.
async fn intro_draft(State(state): State<crate::AppState>, Path(place_id): Path<Uuid>) -> Response {
    let workspace_id = WorkspaceScope::from_state(&state).0;
    let repo = repository(&state);

    let places = match repo.places_with_latest_observations(workspace_id).await {
        Ok(places) => places,
        Err(error) => {
            tracing::warn!(%error, "community lookup for intro draft failed");
            return Problem::service_unavailable(None).into_response();
        }
    };
    let Some(place) = places.into_iter().find(|p| p.place_id == place_id) else {
        return Problem::not_found(None).into_response();
    };

    // Entities come from the community's own posts, so they describe what it
    // talks about rather than what we hope it talks about.
    let observed_genres: Vec<String> = match place.latest_observation_id {
        Some(observation_id) => repo
            .entities_for_observation(workspace_id, observation_id)
            .await
            .map(|rows| {
                rows.into_iter()
                    .filter(|row| row.entity_type == "genre")
                    .map(|row| row.entity_ref)
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let shared: Vec<String> = observed_genres
        .iter()
        .filter(|genre| {
            place
                .genres
                .iter()
                .any(|seeded| seeded.eq_ignore_ascii_case(genre))
        })
        .cloned()
        .collect();

    let draft = compose_intro(&place.name, &place.platform, &observed_genres, &shared);
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(json!({
            "placeId": place_id,
            "name": place.name,
            "url": place.url,
            "observedGenres": observed_genres,
            "sharedGenres": shared,
            "draft": draft,
            "grounded": !observed_genres.is_empty(),
        })),
    )
        .into_response()
}

/// Builds the draft text.
///
/// Separated from the handler so the wording is testable without a database —
/// the thing most likely to go wrong here is tone, not SQL.
fn compose_intro(name: &str, platform: &str, observed: &[String], shared: &[String]) -> String {
    let mut draft = String::new();
    if observed.is_empty() {
        // No observation yet. Saying so beats a confident template that
        // claims a fit nobody has checked.
        draft.push_str(&format!(
            "No observation of {name} yet, so this is a blank rather than a draft.\n\n             Read the rules and the last week of posts first, then write the              introduction yourself — a generic one is worse than none in a              {platform} community that sees them daily.\n\n             Once the community sync has looked at {name}, this draft will              name what they actually discuss."
        ));
        return draft;
    }

    let topics = observed
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");

    draft.push_str("Hey — we're Virya, a metal band from Poland.\n\n");
    if shared.is_empty() {
        draft.push_str(&format!(
            "Worth checking before posting: {name} mostly discusses {topics},              which does not obviously overlap with what we play. If that is              wrong, say so in your own words below. If it is right, this is              probably not our room.\n\n"
        ));
    } else {
        draft.push_str(&format!(
            "Found you while looking for people into {}. That is most of what              we play, so hopefully we are in the right room.\n\n",
            shared.join(" and "),
        ));
    }
    draft.push_str(
        "Not here to drop a link and leave — happy to talk about what          everyone's listening to. If there's a channel or thread where sharing          our own stuff is welcome, point me at it and I'll keep it there.\n\n         — Virya",
    );
    draft
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
                        "membershipState": p.membership_state,
                        "membershipNote": p.membership_note,
                        "membershipChangedAt": p.membership_changed_at
                            .and_then(|at| at.format(&time::format_description::well_known::Rfc3339).ok()),
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

#[cfg(test)]
mod intro_draft_tests {
    use super::compose_intro;

    #[test]
    fn an_unobserved_community_gets_a_blank_not_a_template() {
        // A confident introduction claiming a fit nobody checked is worse than
        // admitting there is nothing to go on — a generic intro is exactly what
        // gets a band ignored or banned.
        let draft = compose_intro("r/Metal", "reddit", &[], &[]);
        assert!(draft.contains("No observation"), "{draft}");
        assert!(
            !draft.contains("hopefully we are in the right room"),
            "an unobserved community must not claim a fit: {draft}",
        );
    }

    #[test]
    fn a_shared_genre_becomes_the_reason_for_being_there() {
        let observed = vec!["Black Metal".to_owned(), "Doom Metal".to_owned()];
        let shared = vec!["Black Metal".to_owned()];
        let draft = compose_intro("r/BlackMetal", "reddit", &observed, &shared);
        assert!(draft.contains("Black Metal"), "{draft}");
        assert!(draft.contains("right room"), "{draft}");
    }

    #[test]
    fn no_overlap_says_so_instead_of_papering_over_it() {
        // Posting into a community you do not fit is how a band gets banned.
        // Better to learn it before writing than after.
        let observed = vec!["Power Metal".to_owned(), "Folk Metal".to_owned()];
        let draft = compose_intro("r/PowerMetal", "reddit", &observed, &[]);
        assert!(draft.contains("does not obviously overlap"), "{draft}");
        assert!(draft.contains("probably not our room"), "{draft}");
    }

    #[test]
    fn the_draft_never_promises_a_link_drop() {
        // Every version has to carry the one line that keeps this from reading
        // as promotion, because that line is the difference between joining a
        // community and spamming it.
        for (observed, shared) in [
            (
                vec!["Death Metal".to_owned()],
                vec!["Death Metal".to_owned()],
            ),
            (vec!["Jazz".to_owned()], vec![]),
        ] {
            let draft = compose_intro("somewhere", "forum", &observed, &shared);
            assert!(
                draft.contains("Not here to drop a link"),
                "draft lost its no-spam line: {draft}",
            );
            assert!(
                draft.contains("point me at it"),
                "draft should ask where self-promo is welcome: {draft}",
            );
        }
    }

    #[test]
    fn only_the_first_few_topics_are_named() {
        // Listing every genre reads as scraped output rather than a person who
        // looked at the group.
        let observed: Vec<String> = (0..9).map(|i| format!("Genre{i}")).collect();
        let draft = compose_intro("big place", "forum", &observed, &[]);
        assert!(draft.contains("Genre0"), "{draft}");
        assert!(!draft.contains("Genre5"), "too many topics named: {draft}");
    }
}
