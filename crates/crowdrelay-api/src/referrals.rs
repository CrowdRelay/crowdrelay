//! HTTP transport for referral sharing, private fan progress, and commerce redemption.

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{CACHE_CONTROL, LOCATION, REFERRER_POLICY, SET_COOKIE},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    IdempotencyKey, LoadReferralProgress, RedeemCoupon, RedeemCouponCommand, RepositoryError,
    RequestId, ResolveReferralCode,
};
use crowdrelay_domain::{CouponCode, EventSlug, ReferralCode, WorkspaceId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID,
    acquisition::{
        attribution_cookie, attribution_visitor, automated_fetch, fan_session_from_headers,
        record_dropped,
    },
    request_id,
};

const REFERRAL_COOKIE: &str = "crowdrelay_referral";
const REFERRAL_COOKIE_MAX_AGE_SECONDS: u32 = 30 * 24 * 60 * 60;
const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Debug, Default, Deserialize)]
struct ReferralDestinationQuery {
    event: Option<String>,
    release: Option<String>,
    lang: Option<String>,
}

/// Dependencies used by referral, fan-progress, and commerce routes.
#[derive(Clone)]
pub struct ReferralState {
    workspace_id: WorkspaceId,
    resolve_referral_code: ResolveReferralCode,
    load_referral_progress: LoadReferralProgress,
    redeem_coupon: RedeemCoupon,
    public_site_base_url: Url,
    live_page_path: String,
    member_area_path: String,
    secure_cookies: bool,
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
        live_page_path: String,
        member_area_path: String,
        secure_cookies: bool,
    ) -> Self {
        Self {
            workspace_id,
            resolve_referral_code,
            load_referral_progress,
            redeem_coupon,
            public_site_base_url,
            live_page_path,
            member_area_path,
            secure_cookies,
        }
    }

    fn localized_path(&self, configured: &str, language: Option<&str>) -> Option<String> {
        let path = configured.trim_matches('/');
        if path.is_empty() {
            return None;
        }
        let path = if language.is_some_and(|lang| lang.starts_with("pl")) {
            path
        } else {
            path.strip_prefix("pl/").unwrap_or(path)
        };
        (!path.is_empty()).then(|| path.to_owned())
    }
}

/// Validates a referral code, stores first-party attribution, and redirects to signup.
pub async fn redirect_referral(
    State(state): State<crate::AppState>,
    Path(raw_code): Path<String>,
    query: Result<Query<ReferralDestinationQuery>, QueryRejection>,
    method: Method,
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

    let requested_context = query.ok().map(|Query(query)| query);
    let destination = if let Some(query) = requested_context.as_ref() {
        if let Some(slug) = query
            .event
            .as_ref()
            .and_then(|raw| EventSlug::parse(raw.clone()).ok())
            .filter(|slug| state.events.has_public_event(slug))
        {
            state
                .referrals
                .localized_path(&state.referrals.live_page_path, query.lang.as_deref())
                .map(|prefix| format!("{prefix}/{}/", slug.as_str()))
        } else if let Some(source_id) = query
            .release
            .as_deref()
            .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM content_sources
                    WHERE workspace_id = $1 AND id = $2
                      AND source_kind IN ('release', 'video') AND active
                )",
            )
            .bind(state.referrals.workspace_id.into_uuid())
            .bind(source_id)
            .fetch_one(&state.database)
            .await
            .unwrap_or(false);
            if exists {
                state
                    .referrals
                    .localized_path(&state.referrals.member_area_path, query.lang.as_deref())
                    .map(|path| format!("{path}/#wydania"))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let location = match state
        .referrals
        .public_site_base_url
        .join(destination.as_deref().unwrap_or("join"))
    {
        Ok(url) => url,
        Err(_) => {
            tracing::error!("configured public site URL could not form the referral landing URL");
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
    // A preview bot following a pasted referral is not a person. Serve the
    // destination so unfurls still work, but do not mint attribution state or
    // teach the growth loop from that fetch.
    let automated = automated_fetch(&method, &headers);
    let visitor_id = attribution_visitor(&headers).unwrap_or_default();
    if let Some(reason) = automated {
        record_dropped(reason);
        tracing::debug!("referral fetch by an automated agent not recorded as an interaction");
    } else {
        let pool = state.database.clone();
        let workspace_id = state.referrals.workspace_id;
        let code_for_receipt = code.clone();
        let timeout = state.ticketing.operation_timeout();
        tokio::spawn(async move {
            let write = crowdrelay_infra::referrals::record_referral_interaction(
                &pool,
                workspace_id,
                &code_for_receipt,
                visitor_id,
                OffsetDateTime::now_utc(),
            );
            match tokio::time::timeout(timeout, write).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    tracing::debug!("active referral disappeared before interaction receipt");
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "failed to persist referral interaction receipt");
                }
                Err(_) => {
                    tracing::warn!("timed out persisting referral interaction receipt");
                }
            }
        });
    }

    let mut response = (
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ],
    )
        .into_response();

    if automated.is_none() {
        let Ok(referral_cookie) =
            HeaderValue::from_str(&referral_cookie(&code, state.referrals.secure_cookies))
        else {
            tracing::error!("referral cookie could not be encoded as a response header");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        };
        let Ok(visitor_cookie) = HeaderValue::from_str(&attribution_cookie(
            visitor_id,
            state.referrals.secure_cookies,
        )) else {
            tracing::error!("attribution cookie could not be encoded as a response header");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        };
        response.headers_mut().append(SET_COOKIE, referral_cookie);
        response.headers_mut().append(SET_COOKIE, visitor_cookie);
    }
    response
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
        RepositoryError::ConflictBecause(detail) => Problem::conflict_because(detail, request_id),
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
