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
use uuid::Uuid;

use crate::{Problem, request_id};

/// The platforms the migration's CHECK constraint accepts. Kept beside the
/// parse so a new platform fails here, with a 422 naming the field, rather
/// than as a constraint violation five layers down.
const PLATFORMS: [&str; 4] = ["android", "ios", "desktop", "web"];

/// Bound on the version string a device may write into a public table.
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
    if !crate::push::valid_installation_id(installation_id)
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

/// The body the identified half takes — the same fields the public endpoint
/// accepts. The platform is the caller's claim about itself; a web Signal
/// session reports `web`, and that is what the funnel counts it as.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkInstallationRequest {
    installation_id: String,
    platform: String,
    #[serde(default)]
    app_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct LinkInstallationResponse {
    recorded: bool,
    /// False only when the row was already linked to a fan — the first
    /// identification is the conversion the funnel measures; later calls are
    /// still-alive heartbeats.
    linked: bool,
}

/// Records an install *and* names its fan — the identified half of the
/// funnel, for Signal surfaces that never carry a push transport.
///
/// `/v1/me/push/endpoints` links an install to its fan, but only for a client
/// that holds a push subscription. Web Signal has none: a browser session is
/// the install, and without this route a fan could open Signal every day and
/// the funnel would still read zero. Three rows of `signal_installations`,
/// all Android, is what a web-only funnel looked like.
///
/// Unlike the public endpoint this one answers 401 without a fan session —
/// naming a fan is the whole point of the call.
pub async fn link_installation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LinkInstallationRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let fan_id = match current_fan_id(&state, &headers).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
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
    if !crate::push::valid_installation_id(installation_id)
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

    let workspace_id = state.ticketing.workspace_id();
    if let Err(error) = crowdrelay_infra::signal_installations::record_installation(
        state.ticketing.pool(),
        workspace_id,
        installation_id,
        &platform,
        app_version.as_deref(),
    )
    .await
    {
        tracing::warn!(%error, "could not record an identified Signal installation");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    match crowdrelay_infra::signal_installations::link_installation_to_fan(
        state.ticketing.pool(),
        workspace_id,
        installation_id,
        fan_id,
    )
    .await
    {
        Ok(linked) => (
            StatusCode::OK,
            Json(LinkInstallationResponse {
                recorded: true,
                linked,
            }),
        )
            .into_response(),
        Err(error) => {
            // Here the link is the job, not a datapoint beside it — a caller
            // that retried on this 503 finds the row recorded and links it.
            tracing::warn!(%error, %fan_id, "could not link a Signal installation to its fan");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// Resolves the fan behind the session cookie. The same four predicates every
/// fan-scoped route applies — the session is live, unrevoked, unexpired, and
/// the fan behind it is still active.
async fn current_fan_id(state: &crate::AppState, headers: &HeaderMap) -> Result<Uuid, Problem> {
    let Some(session) = crate::acquisition::fan_session_from_headers(headers) else {
        return Err(Problem::unauthorized(request_id(headers)).private());
    };
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT session.fan_id
        FROM fan_sessions session
        JOIN fans fan ON fan.workspace_id = session.workspace_id AND fan.id = session.fan_id
        WHERE session.workspace_id = $1
          AND session.session_token_hash = digest($2, 'sha256')
          AND session.revoked_at IS NULL
          AND session.expires_at > now()
          AND fan.status = 'active'
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(session.as_str())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(|error| {
        tracing::warn!(%error, "signal-install fan-session lookup failed");
        Problem::service_unavailable(request_id(headers)).private()
    })?
    .ok_or_else(|| Problem::unauthorized(request_id(headers)).private())
}
