//! Staff endpoints for the fan identity spine (§4e-5): review merge
//! candidates, execute an explicit merge, reverse it, dismiss a candidate.
//!
//! Everything here is Operator-authorized by the `/v1/staff/` prefix and
//! answered `private, no-store` — fan identity data never caches.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    DismissMergeCandidateCommand, FanIdentity, FanIdentityError, MergeFansCommand,
    UnmergeFanCommand,
};
use crowdrelay_domain::fan_identity::{CandidateStatus, MergeError};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Problem, request_id, security::bearer_sha256};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_CANDIDATE_LIMIT: u32 = 200;
const MAX_REASON_CHARS: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeCandidatesQuery {
    /// `pending` (default), `merged` or `dismissed`.
    status: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeFansRequest {
    survivor_fan_id: Uuid,
    merged_fan_id: Uuid,
    reason: Option<String>,
}

/// The operator label for `merged_by`: the staff device session's display
/// name when the request authenticated with one, else the credential kind.
async fn staff_actor(state: &crate::AppState, headers: &HeaderMap) -> String {
    if let Some(token_hash) = bearer_sha256(headers)
        && let Ok(Some(name)) = sqlx::query_scalar::<_, String>(
            "SELECT display_name FROM staff_device_sessions \
             WHERE workspace_id = $1 AND token_hash = $2 AND revoked_at IS NULL",
        )
        .bind(state.ops.workspace_id().into_uuid())
        .bind(token_hash.to_vec())
        .fetch_optional(&state.database)
        .await
    {
        return name;
    }
    "operator".to_string()
}

fn identity_error_response(error: &FanIdentityError, request_id: Option<String>) -> Response {
    match error {
        FanIdentityError::NotFound => Problem::not_found(request_id).private().into_response(),
        FanIdentityError::InvalidMerge(reason) => match reason {
            MergeError::SameFan => Problem::bad_request(request_id).private().into_response(),
            MergeError::FanNotFound => Problem::not_found(request_id).private().into_response(),
            MergeError::AlreadyMerged | MergeError::SurvivorMerged => {
                Problem::conflict_because("One of the fans is already part of a merge.", request_id)
                    .private()
                    .into_response()
            }
            MergeError::MergedFanIsSurvivor => Problem::conflict_because(
                "The fan being merged is itself the survivor of an open merge — \
                 merge the newer identity into the root survivor instead.",
                request_id,
            )
            .private()
            .into_response(),
        },
        FanIdentityError::NothingToUnmerge => {
            Problem::conflict_because("The fan has no open merge to reverse.", request_id)
                .private()
                .into_response()
        }
        FanIdentityError::AlreadyResolved => {
            Problem::conflict_because("The candidate is already resolved.", request_id)
                .private()
                .into_response()
        }
        FanIdentityError::Unavailable => Problem::service_unavailable(request_id)
            .private()
            .into_response(),
    }
}

/// `GET /v1/staff/fan-merges/candidates` — the pending (or asked-for status)
/// merge candidates, newest first.
pub async fn list_merge_candidates(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<MergeCandidatesQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let status = match query.status.as_deref() {
        None | Some("pending") => Some(CandidateStatus::Pending),
        Some("merged") => Some(CandidateStatus::Merged),
        Some("dismissed") => Some(CandidateStatus::Dismissed),
        Some(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_CANDIDATE_LIMIT);
    let use_case = FanIdentity::new(std::sync::Arc::new(
        crowdrelay_infra::fan_identity::PgFanIdentityRepository::new(state.database.clone()),
    ));
    match use_case
        .list_merge_candidates(state.ops.workspace_id(), status, limit)
        .await
    {
        Ok(candidates) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "candidates": candidates })),
        )
            .into_response(),
        Err(error) => identity_error_response(&error, request_id_value),
    }
}

/// `POST /v1/staff/fan-merges/candidates/{candidate_id}/dismiss` — a human
/// decided the pair are two people.
pub async fn dismiss_merge_candidate(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(candidate_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    let use_case = FanIdentity::new(std::sync::Arc::new(
        crowdrelay_infra::fan_identity::PgFanIdentityRepository::new(state.database.clone()),
    ));
    match use_case
        .dismiss_merge_candidate(&DismissMergeCandidateCommand {
            workspace_id: state.ops.workspace_id(),
            candidate_id,
        })
        .await
    {
        Ok(()) => (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response(),
        Err(error) => identity_error_response(&error, request_id_value),
    }
}

/// `POST /v1/staff/fan-merges` — the explicit merge. `merged_fan_id` is
/// tombstoned into `survivor_fan_id`; every moved row is recorded so the
/// `unmerge` route can restore exactly.
pub async fn merge_fans(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<MergeFansRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    if payload
        .reason
        .as_deref()
        .is_some_and(|r| r.chars().count() > MAX_REASON_CHARS)
    {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let actor = staff_actor(&state, &headers).await;
    let use_case = FanIdentity::new(std::sync::Arc::new(
        crowdrelay_infra::fan_identity::PgFanIdentityRepository::new(state.database.clone()),
    ));
    match use_case
        .merge_fans(&MergeFansCommand {
            workspace_id: state.ops.workspace_id(),
            survivor_fan_id: payload.survivor_fan_id,
            merged_fan_id: payload.merged_fan_id,
            reason: payload.reason,
            merged_by: actor,
            request_id: request_id_value.clone().unwrap_or_default(),
        })
        .await
    {
        Ok(merge) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(merge),
        )
            .into_response(),
        Err(error) => identity_error_response(&error, request_id_value),
    }
}

/// `POST /v1/staff/fans/{fan_id}/unmerge` — reverses the fan's latest open
/// merge: recorded moved rows re-point back and the prior status returns.
pub async fn unmerge_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(fan_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    let actor = staff_actor(&state, &headers).await;
    let use_case = FanIdentity::new(std::sync::Arc::new(
        crowdrelay_infra::fan_identity::PgFanIdentityRepository::new(state.database.clone()),
    ));
    match use_case
        .unmerge_fan(&UnmergeFanCommand {
            workspace_id: state.ops.workspace_id(),
            merged_fan_id: fan_id,
            unmerged_by: actor,
            request_id: request_id_value.clone().unwrap_or_default(),
        })
        .await
    {
        Ok(merge) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(merge),
        )
            .into_response(),
        Err(error) => identity_error_response(&error, request_id_value),
    }
}

/// `GET /v1/staff/fans/{fan_id}/identity` — the fan's verified identifiers
/// and merge history: the review surface for a merge decision.
pub async fn fan_identity_detail(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(fan_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id();
    let use_case = FanIdentity::new(std::sync::Arc::new(
        crowdrelay_infra::fan_identity::PgFanIdentityRepository::new(state.database.clone()),
    ));
    let exists = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM fans WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(&state.database)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "fan identity detail lookup failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    if !exists {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }
    let identifiers = match use_case.list_fan_identifiers(workspace_id, fan_id).await {
        Ok(value) => value,
        Err(error) => return identity_error_response(&error, request_id_value),
    };
    let merges = match use_case.list_fan_merges(workspace_id, fan_id).await {
        Ok(value) => value,
        Err(error) => return identity_error_response(&error, request_id_value),
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(serde_json::json!({
            "fanId": fan_id,
            "identifiers": identifiers,
            "merges": merges,
        })),
    )
        .into_response()
}
