//! HTTP transport for referral sharing, private fan progress, and commerce redemption.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, LOCATION, REFERRER_POLICY, SET_COOKIE},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    IdempotencyKey, LoadReferralProgress, RedeemCoupon, RedeemCouponCommand, RepositoryError,
    RequestId, ResolveReferralCode,
};
use crowdrelay_domain::{CouponCode, ReferralCode, WorkspaceId};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, acquisition::fan_session_from_headers, request_id,
    security::bearer_sha256_matches,
};

const REFERRAL_COOKIE: &str = "crowdrelay_referral";
const REFERRAL_COOKIE_MAX_AGE_SECONDS: u32 = 30 * 24 * 60 * 60;
const PRIVATE_NO_STORE: &str = "private, no-store";

/// Dependencies used by referral, fan-progress, and commerce routes.
#[derive(Clone)]
pub struct ReferralState {
    workspace_id: WorkspaceId,
    resolve_referral_code: ResolveReferralCode,
    load_referral_progress: LoadReferralProgress,
    redeem_coupon: RedeemCoupon,
    public_site_base_url: Url,
    secure_cookies: bool,
    commerce_api_key_sha256: Option<[u8; 32]>,
    staff_api_key_sha256: Option<[u8; 32]>,
    admin_api_key_sha256: Option<[u8; 32]>,
}

impl ReferralState {
    /// Creates referral route state for one trusted workspace.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_id: WorkspaceId,
        resolve_referral_code: ResolveReferralCode,
        load_referral_progress: LoadReferralProgress,
        redeem_coupon: RedeemCoupon,
        public_site_base_url: Url,
        secure_cookies: bool,
        commerce_api_key_sha256: Option<[u8; 32]>,
        staff_api_key_sha256: Option<[u8; 32]>,
        admin_api_key_sha256: Option<[u8; 32]>,
    ) -> Self {
        Self {
            workspace_id,
            resolve_referral_code,
            load_referral_progress,
            redeem_coupon,
            public_site_base_url,
            secure_cookies,
            commerce_api_key_sha256,
            staff_api_key_sha256,
            admin_api_key_sha256,
        }
    }
}

/// Validates a referral code, stores first-party attribution, and redirects to signup.
pub async fn redirect_referral(
    State(state): State<crate::AppState>,
    Path(raw_code): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(code) = ReferralCode::parse(raw_code) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    match state
        .referrals
        .resolve_referral_code
        .execute(state.referrals.workspace_id, &code)
        .await
    {
        Ok(true) => {}
        Ok(false) | Err(RepositoryError::NotFound) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => return repository_problem(error, request_id_value).into_response(),
    }

    let location = match state.referrals.public_site_base_url.join("join") {
        Ok(url) => url,
        Err(_) => {
            tracing::error!("configured public site URL could not form the join URL");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
    };
    let Ok(location) = HeaderValue::from_str(location.as_str()) else {
        tracing::error!("referral landing URL could not be encoded as a response header");
        return Problem::internal(request_id_value)
            .private()
            .into_response();
    };
    let Ok(cookie) = HeaderValue::from_str(&referral_cookie(&code, state.referrals.secure_cookies))
    else {
        tracing::error!("referral cookie could not be encoded as a response header");
        return Problem::internal(request_id_value)
            .private()
            .into_response();
    };

    (
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (SET_COOKIE, cookie),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ],
    )
        .into_response()
}

/// Returns private referral and reward progress for the current fan session.
pub async fn referral_progress(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session_token) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    match state
        .referrals
        .load_referral_progress
        .execute(state.referrals.workspace_id, &session_token)
        .await
    {
        Ok(progress) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(progress),
        )
            .into_response(),
        Err(RepositoryError::NotFound) => Problem::unauthorized(request_id_value)
            .private()
            .into_response(),
        Err(error) => repository_problem(error, request_id_value).into_response(),
    }
}

/// JSON body accepted by the coupon redemption endpoint.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemCouponRequest {
    code: String,
    order_reference: String,
}

#[derive(Serialize)]
struct RedeemCouponResponse {
    result: crowdrelay_domain::CouponRedemptionResult,
}

/// Atomically redeems a merch coupon for an authenticated commerce service or operator.
pub async fn redeem_coupon(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RedeemCouponRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let authorized = bearer_sha256_matches(&headers, state.referrals.commerce_api_key_sha256)
        || bearer_sha256_matches(&headers, state.referrals.staff_api_key_sha256)
        || bearer_sha256_matches(&headers, state.referrals.admin_api_key_sha256);
    if !authorized {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            let problem = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                Problem::payload_too_large(request_id_value)
            } else {
                Problem::bad_request(request_id_value)
            };
            return problem.private().into_response();
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
    let Ok(code) = CouponCode::parse(payload.code) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };
    let command = match RedeemCouponCommand::new(
        state.referrals.workspace_id,
        idempotency_key,
        command_request_id,
        code,
        payload.order_reference,
    ) {
        Ok(command) => command,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };

    match state.referrals.redeem_coupon.execute(&command).await {
        Ok(result) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(RedeemCouponResponse { result }),
        )
            .into_response(),
        Err(error) => repository_problem(error, request_id_value).into_response(),
    }
}

fn referral_cookie(code: &ReferralCode, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!(
        "{REFERRAL_COOKIE}={}; Max-Age={REFERRAL_COOKIE_MAX_AGE_SECONDS}; \
         Path=/; HttpOnly; SameSite=Lax{secure_attribute}",
        code.as_str()
    )
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Problem {
    match error {
        RepositoryError::Unavailable => {
            tracing::warn!("referral repository is temporarily unavailable");
            Problem::service_unavailable(request_id)
        }
        RepositoryError::NotFound => Problem::not_found(request_id),
        RepositoryError::Conflict => Problem::conflict(request_id),
        RepositoryError::Unexpected => {
            tracing::error!("referral repository failed unexpectedly");
            Problem::internal(request_id)
        }
    }
    .private()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referral_cookie_is_first_party_and_http_only() -> Result<(), Box<dyn std::error::Error>> {
        let code = ReferralCode::parse("Fan_Code-123")?;
        let cookie = referral_cookie(&code, true);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
        assert!(!cookie.contains("Domain="));
        Ok(())
    }
}
