//! HTTP endpoints for email confirmation and unsubscribe actions.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, SET_COOKIE},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    ConfirmFan, ConfirmFanCommand, FanLifecycleError, IdempotencyKey, RequestId, UnsubscribeFan,
};
use crowdrelay_domain::{FanActionToken, FanStatus, WorkspaceId};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, request_id};

const FAN_SESSION_COOKIE: &str = "crowdrelay_fan";
const FAN_SESSION_COOKIE_MAX_AGE_SECONDS: u32 = 90 * 24 * 60 * 60;
const PRIVATE_NO_STORE: &str = "private, no-store";

/// Dependencies for fan confirmation and unsubscribe routes.
#[derive(Clone)]
pub struct FanLifecycleState {
    workspace_id: WorkspaceId,
    confirm_fan: ConfirmFan,
    unsubscribe_fan: UnsubscribeFan,
    public_site_base_url: Url,
    secure_cookies: bool,
}

impl FanLifecycleState {
    /// Creates the lifecycle route state for one trusted workspace.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        confirm_fan: ConfirmFan,
        unsubscribe_fan: UnsubscribeFan,
        public_site_base_url: Url,
        secure_cookies: bool,
    ) -> Self {
        Self {
            workspace_id,
            confirm_fan,
            unsubscribe_fan,
            public_site_base_url,
            secure_cookies,
        }
    }
}

/// JSON body containing a one-time confirmation or unsubscribe token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanActionRequest {
    token: String,
}

#[derive(Serialize)]
struct FanConfirmationResponse {
    fan_id: crowdrelay_domain::FanId,
    status: FanStatus,
    referral_url: String,
}

#[derive(Serialize)]
struct FanUnsubscribeResponse {
    fan_id: crowdrelay_domain::FanId,
    status: FanStatus,
}

/// Confirms ownership of a fan email address and creates a browser session.
pub async fn confirm_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FanActionRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let token = match FanActionToken::parse(payload.token) {
        Ok(token) => token,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let idempotency_key = match headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(IdempotencyKey::parse)
    {
        Some(Ok(key)) => key,
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(raw_request_id) = headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::error!("server request ID middleware did not populate the request");
        return Problem::internal(None).private().into_response();
    };
    let Ok(command_request_id) = RequestId::parse(raw_request_id) else {
        tracing::error!("server request ID did not pass application validation");
        return Problem::internal(None).private().into_response();
    };
    let command = ConfirmFanCommand {
        workspace_id: state.fan_lifecycle.workspace_id,
        token,
        idempotency_key,
        request_id: command_request_id,
    };
    let result = match state.fan_lifecycle.confirm_fan.execute(&command).await {
        Ok(result) => result,
        Err(error) => return lifecycle_problem(error, request_id_value).into_response(),
    };
    let referral_url = match state
        .fan_lifecycle
        .public_site_base_url
        .join(&format!("r/{}", result.referral_code.as_str()))
    {
        Ok(url) => url.to_string(),
        Err(error) => {
            tracing::error!(%error, "could not build referral URL after confirmation");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
    };
    let cookie = match HeaderValue::from_str(&fan_session_cookie(
        result.fan_session_token.as_str(),
        state.fan_lifecycle.secure_cookies,
    )) {
        Ok(cookie) => cookie,
        Err(error) => {
            tracing::error!(%error, "could not encode fan session cookie");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
    };

    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (SET_COOKIE, cookie),
        ],
        Json(FanConfirmationResponse {
            fan_id: result.fan_id,
            status: result.status,
            referral_url,
        }),
    )
        .into_response()
}

/// Revokes marketing consent and active browser sessions for a fan.
pub async fn unsubscribe_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FanActionRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let token = match FanActionToken::parse(payload.token) {
        Ok(token) => token,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let result = match state
        .fan_lifecycle
        .unsubscribe_fan
        .execute(state.fan_lifecycle.workspace_id, &token)
        .await
    {
        Ok(result) => result,
        Err(error) => return lifecycle_problem(error, request_id_value).into_response(),
    };
    let clear_cookie = match HeaderValue::from_str(&clear_fan_session_cookie(
        state.fan_lifecycle.secure_cookies,
    )) {
        Ok(cookie) => cookie,
        Err(error) => {
            tracing::error!(%error, "could not encode fan session removal cookie");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
    };
    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (SET_COOKIE, clear_cookie),
        ],
        Json(FanUnsubscribeResponse {
            fan_id: result.fan_id,
            status: result.status,
        }),
    )
        .into_response()
}

fn fan_session_cookie(token: &str, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!(
        "{FAN_SESSION_COOKIE}={token}; Max-Age={FAN_SESSION_COOKIE_MAX_AGE_SECONDS}; Path=/; HttpOnly; SameSite=Lax{secure_attribute}"
    )
}

fn clear_fan_session_cookie(secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!("{FAN_SESSION_COOKIE}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax{secure_attribute}")
}

fn lifecycle_problem(error: FanLifecycleError, request_id: Option<String>) -> Problem {
    match error {
        FanLifecycleError::Repository(crowdrelay_application::RepositoryError::Unavailable) => {
            Problem::service_unavailable(request_id)
        }
        FanLifecycleError::Repository(crowdrelay_application::RepositoryError::NotFound) => {
            Problem::not_found(request_id)
        }
        FanLifecycleError::Repository(crowdrelay_application::RepositoryError::Conflict) => {
            Problem::conflict(request_id)
        }
        FanLifecycleError::Repository(crowdrelay_application::RepositoryError::Unexpected) => {
            Problem::internal(request_id)
        }
    }
    .private()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_cookie_is_http_only_and_secure() {
        let cookie = fan_session_cookie(&"a".repeat(64), true);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
        assert!(clear_fan_session_cookie(true).contains("Secure"));
        assert!(!clear_fan_session_cookie(false).contains("; Secure"));
    }
}
