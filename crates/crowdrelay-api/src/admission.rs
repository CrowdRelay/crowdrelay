//! HTTP admission-pass endpoints for winners, operators, and gate staff.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, COOKIE, SET_COOKIE},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    AdmissionUseCaseError, ClaimAdmissionPass, ClaimAdmissionPassCommand, IdempotencyKey,
    IssueAdmissionPass, IssueAdmissionPassCommand, LoadAdmissionPass, RedeemAdmissionPass,
    RedeemAdmissionPassCommand, RequestId, RevokeAdmissionPass, RevokeAdmissionPassCommand,
};
use crowdrelay_domain::{
    AdmissionQrClaims, AdmissionQrError, EventSlug, NormalizedEmail, PassClaimToken,
    PassSessionToken, WorkspaceId,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, request_id, security::bearer_sha256_matches,
    ticket_qr::decode_ticket_qr,
};

type HmacSha256 = Hmac<Sha256>;

const PASS_SESSION_COOKIE: &str = "crowdrelay_pass_session";
const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_QR_CLOCK_SKEW_SECONDS: i64 = 5;
const QR_WINDOW_BEFORE_START_HOURS: i64 = 6;
const QR_WINDOW_AFTER_START_HOURS: i64 = 24;

/// Admission dependencies and secret-derived authentication material.
#[derive(Clone)]
pub struct AdmissionState {
    workspace_id: WorkspaceId,
    issue_pass: IssueAdmissionPass,
    claim_pass: ClaimAdmissionPass,
    load_pass: LoadAdmissionPass,
    redeem_pass: RedeemAdmissionPass,
    revoke_pass: RevokeAdmissionPass,
    admin_api_key_sha256: Option<[u8; 32]>,
    staff_api_key_sha256: Option<[u8; 32]>,
    qr_signing_key: Option<[u8; 32]>,
    qr_ttl: Duration,
    secure_cookies: bool,
}

/// Construction parameters for admission HTTP state.
pub struct AdmissionStateArgs {
    pub workspace_id: WorkspaceId,
    pub issue_pass: IssueAdmissionPass,
    pub claim_pass: ClaimAdmissionPass,
    pub load_pass: LoadAdmissionPass,
    pub redeem_pass: RedeemAdmissionPass,
    pub revoke_pass: RevokeAdmissionPass,
    pub admin_api_key_sha256: Option<[u8; 32]>,
    pub staff_api_key_sha256: Option<[u8; 32]>,
    pub qr_signing_key: Option<[u8; 32]>,
    pub qr_ttl: Duration,
    pub secure_cookies: bool,
}

impl AdmissionState {
    /// Creates admission endpoint state.
    #[must_use]
    pub fn new(args: AdmissionStateArgs) -> Self {
        Self {
            workspace_id: args.workspace_id,
            issue_pass: args.issue_pass,
            claim_pass: args.claim_pass,
            load_pass: args.load_pass,
            redeem_pass: args.redeem_pass,
            revoke_pass: args.revoke_pass,
            admin_api_key_sha256: args.admin_api_key_sha256,
            staff_api_key_sha256: args.staff_api_key_sha256,
            qr_signing_key: args.qr_signing_key,
            qr_ttl: args.qr_ttl,
            secure_cookies: args.secure_cookies,
        }
    }
}

/// JSON body accepted by the operator pass-issuance endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuePassRequest {
    event_slug: String,
    pool_slug: String,
    fan_email: String,
    #[serde(default = "default_claim_expiry_hours")]
    claim_expires_hours: u32,
}

fn default_claim_expiry_hours() -> u32 {
    72
}

/// Issues a limited-pool pass to an existing active fan.
pub async fn issue_pass(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<IssuePassRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.admission.admin_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    if !(1..=720).contains(&payload.claim_expires_hours) {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let Some((idempotency_key, command_request_id)) = request_context(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = match (
        EventSlug::parse(payload.event_slug),
        EventSlug::parse(payload.pool_slug),
        NormalizedEmail::parse(payload.fan_email),
    ) {
        (Ok(event_slug), Ok(pool_slug), Ok(fan_email)) => IssueAdmissionPassCommand {
            workspace_id: state.admission.workspace_id,
            event_slug,
            pool_slug,
            fan_email,
            claim_expires_hours: payload.claim_expires_hours,
            idempotency_key,
            request_id: command_request_id,
        },
        _ => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    match state.admission.issue_pass.execute(&command).await {
        Ok(result) => {
            let status = if result.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(result)).into_response()
        }
        Err(error) => admission_problem(error, request_id_value).into_response(),
    }
}

/// JSON body used to exchange a one-time winner claim token.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimPassRequest {
    token: String,
}

/// Exchanges a one-time winner token for a private pass session cookie.
pub async fn claim_pass(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ClaimPassRequest>, JsonRejection>,
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
    let token = match PassClaimToken::parse(payload.token) {
        Ok(token) => token,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some((idempotency_key, command_request_id)) = request_context(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = ClaimAdmissionPassCommand {
        workspace_id: state.admission.workspace_id,
        token,
        idempotency_key,
        request_id: command_request_id,
    };
    match state.admission.claim_pass.execute(&command).await {
        Ok(result) => {
            let cookie = match HeaderValue::from_str(&pass_session_cookie(
                &result.session_token,
                result.pass.session_expires_at,
                OffsetDateTime::now_utc(),
                state.admission.secure_cookies,
            )) {
                Ok(cookie) => cookie,
                Err(_) => {
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
                Json(result.pass),
            )
                .into_response()
        }
        Err(error) => admission_problem(error, request_id_value).into_response(),
    }
}

/// Returns the pass belonging to the current winner session.
pub async fn my_pass(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = pass_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    match state
        .admission
        .load_pass
        .execute(state.admission.workspace_id, &session)
        .await
    {
        Ok(pass) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(pass),
        )
            .into_response(),
        Err(error) => admission_problem(error, request_id_value).into_response(),
    }
}

/// Short-lived signed QR token and its expiry timestamp.
#[derive(Debug, Serialize)]
pub struct AdmissionQrResponse {
    token: String,
    expires_at: OffsetDateTime,
}

/// Creates a short-lived signed QR token for the current claimed pass.
pub async fn my_pass_qr(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = pass_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.admission.qr_signing_key else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let pass = match state
        .admission
        .load_pass
        .execute(state.admission.workspace_id, &session)
        .await
    {
        Ok(pass) => pass,
        Err(error) => return admission_problem(error, request_id_value).into_response(),
    };
    if !matches!(pass.status, crowdrelay_domain::AdmissionPassStatus::Claimed) {
        return Problem::conflict(request_id_value)
            .private()
            .into_response();
    }
    let issued_at = OffsetDateTime::now_utc();
    let qr_window_opens = pass.starts_at - time::Duration::hours(QR_WINDOW_BEFORE_START_HOURS);
    let qr_window_closes = pass.starts_at + time::Duration::hours(QR_WINDOW_AFTER_START_HOURS);
    if issued_at < qr_window_opens || issued_at > qr_window_closes {
        return Problem::conflict(request_id_value)
            .private()
            .into_response();
    }
    let Ok(qr_ttl_seconds) = i64::try_from(state.admission.qr_ttl.as_secs()) else {
        tracing::error!("configured admission QR TTL exceeds the supported duration range");
        return Problem::internal(request_id_value)
            .private()
            .into_response();
    };
    let Some(expires_at) = issued_at.checked_add(time::Duration::seconds(qr_ttl_seconds)) else {
        tracing::error!("admission QR expiry overflowed the supported timestamp range");
        return Problem::internal(request_id_value)
            .private()
            .into_response();
    };
    let claims = AdmissionQrClaims {
        version: 1,
        pass_id: pass.pass_id,
        event_id: pass.event_id,
        public_reference: pass.public_reference,
        issued_at: issued_at.unix_timestamp(),
        expires_at: expires_at.unix_timestamp(),
        nonce: Uuid::now_v7().to_string(),
    };
    match encode_qr(&claims, &signing_key) {
        Ok(token) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdmissionQrResponse { token, expires_at }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(?error, "failed to encode admission QR");
            Problem::internal(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// Gate redemption request containing either a signed QR or manual reference.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemPassRequest {
    event_slug: String,
    qr_token: Option<String>,
    public_reference: Option<String>,
}

/// Atomically verifies and redeems a winner pass at the venue gate.
pub async fn redeem_pass(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RedeemPassRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !(bearer_sha256_matches(&headers, state.admission.staff_api_key_sha256)
        || bearer_sha256_matches(&headers, state.admission.admin_api_key_sha256))
    {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some((idempotency_key, command_request_id)) = request_context(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let event_slug = match EventSlug::parse(payload.event_slug) {
        Ok(event_slug) => event_slug,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let (pass_id, event_id, public_reference) = match (payload.qr_token, payload.public_reference) {
        (Some(token), None) => {
            let Some(key) = state.admission.qr_signing_key else {
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            };
            let now = OffsetDateTime::now_utc().unix_timestamp();
            let decoded = if token.starts_with("t1.") {
                decode_ticket_qr(&token, &key, now).map(|identity| {
                    (
                        Some(identity.pass_id),
                        Some(identity.event_id),
                        identity.public_reference,
                    )
                })
            } else {
                decode_qr(&token, &key, now).map(|claims| {
                    (
                        Some(claims.pass_id),
                        Some(claims.event_id),
                        claims.public_reference,
                    )
                })
            };
            match decoded {
                Ok(identity) => identity,
                Err(AdmissionQrError::Expired) => {
                    return Problem::conflict(request_id_value)
                        .private()
                        .into_response();
                }
                Err(AdmissionQrError::Invalid) => {
                    return Problem::unprocessable(request_id_value)
                        .private()
                        .into_response();
                }
            }
        }
        (None, Some(reference)) => (None, None, reference.trim().to_ascii_uppercase()),
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let command = RedeemAdmissionPassCommand {
        workspace_id: state.admission.workspace_id,
        event_slug,
        pass_id,
        event_id,
        public_reference,
        idempotency_key,
        request_id: command_request_id,
    };
    match state.admission.redeem_pass.execute(&command).await {
        Ok(result) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Err(error) => admission_problem(error, request_id_value).into_response(),
    }
}

/// Revokes an unused pass by public reference.
pub async fn revoke_pass(
    State(state): State<crate::AppState>,
    Path(public_reference): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.admission.admin_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Some((idempotency_key, command_request_id)) = request_context(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = RevokeAdmissionPassCommand {
        workspace_id: state.admission.workspace_id,
        public_reference: public_reference.trim().to_ascii_uppercase(),
        idempotency_key,
        request_id: command_request_id,
    };
    match state.admission.revoke_pass.execute(&command).await {
        Ok(result) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Err(error) => admission_problem(error, request_id_value).into_response(),
    }
}

fn request_context(headers: &HeaderMap) -> Option<(IdempotencyKey, RequestId)> {
    let idempotency_key = headers
        .get(&IDEMPOTENCY_KEY)?
        .to_str()
        .ok()
        .and_then(|value| IdempotencyKey::parse(value).ok())?;
    let request_id = headers
        .get(&X_REQUEST_ID)?
        .to_str()
        .ok()
        .and_then(|value| RequestId::parse(value).ok())?;
    Some((idempotency_key, request_id))
}

fn pass_session_from_headers(headers: &HeaderMap) -> Option<PassSessionToken> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == PASS_SESSION_COOKIE)
                .then(|| PassSessionToken::parse(value).ok())
                .flatten()
        })
}

fn pass_session_cookie(
    token: &PassSessionToken,
    expires_at: OffsetDateTime,
    now: OffsetDateTime,
    secure: bool,
) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    let remaining_seconds = (expires_at - now).whole_seconds().max(1);
    let max_age = u32::try_from(remaining_seconds).unwrap_or(u32::MAX);
    format!(
        "{PASS_SESSION_COOKIE}={}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Lax{secure_attribute}",
        token.as_str()
    )
}

fn encode_qr(
    claims: &AdmissionQrClaims,
    key: &[u8; 32],
) -> Result<String, AdmissionQrEncodingError> {
    let payload =
        serde_json::to_vec(claims).map_err(|_| AdmissionQrEncodingError::Serialization)?;
    let payload = hex::encode(payload);
    let signed = format!("v1.{payload}");
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| AdmissionQrEncodingError::InvalidSigningKey)?;
    mac.update(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionQrEncodingError {
    Serialization,
    InvalidSigningKey,
}

fn decode_qr(token: &str, key: &[u8; 32], now: i64) -> Result<AdmissionQrClaims, AdmissionQrError> {
    let mut parts = token.split('.');
    let version = parts.next().ok_or(AdmissionQrError::Invalid)?;
    let payload = parts.next().ok_or(AdmissionQrError::Invalid)?;
    let signature = parts.next().ok_or(AdmissionQrError::Invalid)?;
    if version != "v1" || parts.next().is_some() || payload.len() > 4096 {
        return Err(AdmissionQrError::Invalid);
    }
    let signature = hex::decode(signature).map_err(|_| AdmissionQrError::Invalid)?;
    let signed = format!("{version}.{payload}");
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| AdmissionQrError::Invalid)?;
    mac.update(signed.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| AdmissionQrError::Invalid)?;
    let payload = hex::decode(payload).map_err(|_| AdmissionQrError::Invalid)?;
    let claims: AdmissionQrClaims =
        serde_json::from_slice(&payload).map_err(|_| AdmissionQrError::Invalid)?;
    claims.validate(now.saturating_sub(MAX_QR_CLOCK_SKEW_SECONDS), 180)?;
    if claims.issued_at > now.saturating_add(MAX_QR_CLOCK_SKEW_SECONDS) {
        return Err(AdmissionQrError::Invalid);
    }
    Ok(claims)
}

fn admission_problem(error: AdmissionUseCaseError, request_id: Option<String>) -> Problem {
    match error {
        AdmissionUseCaseError::InvalidInput => Problem::unprocessable(request_id),
        AdmissionUseCaseError::Repository(crowdrelay_application::RepositoryError::Unavailable) => {
            Problem::service_unavailable(request_id)
        }
        AdmissionUseCaseError::Repository(crowdrelay_application::RepositoryError::NotFound) => {
            Problem::not_found(request_id)
        }
        AdmissionUseCaseError::Repository(crowdrelay_application::RepositoryError::Conflict) => {
            Problem::conflict(request_id)
        }
        AdmissionUseCaseError::Repository(crowdrelay_application::RepositoryError::Unexpected) => {
            Problem::internal(request_id)
        }
    }
    .private()
}

#[cfg(test)]
mod tests {
    use crowdrelay_domain::{AdmissionPassId, EventId};

    use super::*;

    #[test]
    fn qr_round_trip_rejects_tampering_and_expiry() -> Result<(), Box<dyn std::error::Error>> {
        let key = [7_u8; 32];
        let claims = AdmissionQrClaims {
            version: 1,
            pass_id: AdmissionPassId::new(),
            event_id: EventId::new(),
            public_reference: "VIRYA-ABC12345".to_owned(),
            issued_at: 100,
            expires_at: 130,
            nonce: Uuid::now_v7().to_string(),
        };
        let token = encode_qr(&claims, &key).map_err(|e| format!("encode_qr: {e:?}"))?;
        assert_eq!(decode_qr(&token, &key, 110)?, claims);
        assert_eq!(
            decode_qr(&(token + "x"), &key, 110),
            Err(AdmissionQrError::Invalid)
        );
        assert_eq!(
            decode_qr(
                &encode_qr(&claims, &key).map_err(|e| format!("encode_qr: {e:?}"))?,
                &key,
                140
            ),
            Err(AdmissionQrError::Expired)
        );

        let future = AdmissionQrClaims {
            issued_at: 200,
            expires_at: 230,
            ..claims
        };
        assert_eq!(
            decode_qr(
                &encode_qr(&future, &key).map_err(|e| format!("encode_qr: {e:?}"))?,
                &key,
                110
            ),
            Err(AdmissionQrError::Invalid)
        );
        Ok(())
    }

    #[test]
    fn pass_cookie_tracks_the_persisted_session_expiry() -> Result<(), Box<dyn std::error::Error>> {
        let token = PassSessionToken::parse("a".repeat(64))?;
        let now = OffsetDateTime::UNIX_EPOCH;
        let cookie = pass_session_cookie(&token, now + time::Duration::days(60), now, true);

        assert!(cookie.contains("Max-Age=5184000"));
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("; HttpOnly"));
        Ok(())
    }
}
