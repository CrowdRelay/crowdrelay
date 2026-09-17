//! Operator surface for per-organisation settings (4G.2b).
//!
//! The roster's own numbers, which belong to the organisation rather than to
//! any one act. Admin rather than control-plane on purpose, and for exactly the
//! reason the roster plan is: the control-plane surface is scoped to one tenant
//! by construction, so accepting a workspace token here would let one act read
//! or change what its labelmates' plans are sized by.
//!
//! GET returns what has been set, and says nothing more. There is no shipped
//! default to merge over: a roster that has never stated its capacity has not
//! got one, and the planner says so rather than planning against a guess.
//!
//! PUT accepts only keys from `EDITABLE_KEYS`, with the vocabulary's own
//! validator deciding what a valid value is — the bounds live next to the key
//! they bound, so the two cannot drift. Every write lands in
//! `crowdrelay-infra::organization_settings` (api-sql ratchet).

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::organization_settings::{
    EDITABLE_KEYS, OrganizationSettingsRepository, is_valid_value,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn repository(state: &crate::AppState) -> OrganizationSettingsRepository {
    OrganizationSettingsRepository::new(state.database.clone())
}

#[derive(Serialize)]
pub struct OrganizationSettingsResponse {
    /// Only what has actually been set. An absent key is a question nobody has
    /// answered, which reads differently from a value.
    pub settings: HashMap<String, String>,
    pub editable_keys: Vec<&'static str>,
}

/// `GET /v1/admin/organizations/{organization_id}/settings`
pub async fn get_organization_settings(
    State(state): State<crate::AppState>,
    Path(organization_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match repository(&state).list(organization_id).await {
        Ok(rows) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(OrganizationSettingsResponse {
                settings: rows.into_iter().collect(),
                editable_keys: EDITABLE_KEYS.to_vec(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "organization settings lookup failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateOrganizationSettingRequest {
    value: String,
}

/// `PUT /v1/admin/organizations/{organization_id}/settings/{key}`
pub async fn upsert_organization_setting(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path((organization_id, key)): Path<(Uuid, String)>,
    payload: Result<Json<UpdateOrganizationSettingRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // A value the reader would discard must never be stored. Storing one means
    // the manager believes the roster is sized and the planner keeps asking,
    // with nothing on either surface explaining the disagreement.
    if !EDITABLE_KEYS.contains(&key.as_str()) || !is_valid_value(&key, &request.value) {
        return Problem::unprocessable(request_id_value).into_response();
    }
    match repository(&state)
        .set(organization_id, &key, request.value.trim())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "key": key, "value": request.value.trim() })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "organization setting update failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
