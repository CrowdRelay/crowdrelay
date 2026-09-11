//! Recording that the Signal app exists on a device, before it is anyone's.
//!
//! Nineteen people have the app installed. CrowdRelay knew about two, because
//! every Signal number it collects is measured below a fan session:
//! `/v1/me/push/endpoints` requires one, and the app's registration path
//! returns silently when there is none. An install that never signs in, or
//! whose owner declines the notification prompt, left no trace anywhere.
//!
//! That inversion is the bug. The system could not tell "the app is not being
//! installed" from "the app is installed and not converting", and those two
//! readings call for opposite work. It also could not count what it was
//! missing, which is how seventeen people stay invisible for months.
//!
//! So this endpoint is public on purpose. Recording an install must never
//! depend on the identity the install is supposed to produce.
//!
//! # What it is not
//!
//! `installation_id` is the app's own random, device-scoped identifier,
//! generated locally with no account involved. It identifies a copy of the
//! app, not a person, and carries nothing about one. A row becomes personal
//! data only when `fan_id` is set, which happens after that fan has
//! identified themselves through the ordinary consented path.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{Problem, request_id};

/// The platforms the migration's CHECK constraint accepts. Kept beside the
/// parse so a new platform fails here, with a 422 naming the field, rather
/// than as a constraint violation five layers down.
const PLATFORMS: [&str; 4] = ["android", "ios", "desktop", "web"];

/// Bounds on what a device may write into a public table. Long enough for a
/// UUID or a store identifier, short enough that the row cannot be used as
/// storage.
const MAX_INSTALLATION_ID: usize = 128;
const MAX_APP_VERSION: usize = 32;

#[derive(Debug, Deserialize)]
pub struct RecordInstallationRequest {
    installation_id: String,
    platform: String,
    #[serde(default)]
    app_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct RecordInstallationResponse {
    recorded: bool,
}

/// Upserts the installation and bumps `last_seen_at`.
///
/// Idempotent by primary key, so the app may call it on every launch: that is
/// what keeps `last_seen_at` meaningful, and a still-installed app is a
/// different fact from one installed once in March.
pub async fn record_installation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RecordInstallationRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };

    let installation_id = payload.installation_id.trim();
    let platform = payload.platform.trim().to_lowercase();
    if installation_id.is_empty()
        || installation_id.len() > MAX_INSTALLATION_ID
        || !PLATFORMS.contains(&platform.as_str())
    {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let app_version = payload
        .app_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= MAX_APP_VERSION)
        .map(str::to_owned);

    let result = crowdrelay_infra::signal_installations::record_installation(
        state.ticketing.pool(),
        state.ticketing.workspace_id(),
        installation_id,
        &platform,
        app_version.as_deref(),
    )
    .await;

    match result {
        Ok(_) => (
            StatusCode::OK,
            Json(RecordInstallationResponse { recorded: true }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "could not record a Signal installation");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
