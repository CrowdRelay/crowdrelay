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
use crowdrelay_domain::{FanActionToken, FanStatus, NormalizedEmail, WorkspaceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Problem, X_REQUEST_ID, request_id};

const FAN_SESSION_COOKIE: &str = "crowdrelay_fan";
const FAN_SESSION_COOKIE_MAX_AGE_SECONDS: u32 = 90 * 24 * 60 * 60;
const PRIVATE_NO_STORE: &str = "private, no-store";
const ACCESS_RESEND_COOLDOWN_SECONDS: i64 = 60;
const ACCESS_TOKEN_TTL_DAYS: i64 = 2;

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
    fan_session_token: String,
    email: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanAccessRequest {
    email: String,
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Serialize)]
struct FanAccessResponse {
    accepted: bool,
}

#[derive(Serialize)]
struct FanUnsubscribeResponse {
    fan_id: crowdrelay_domain::FanId,
    status: FanStatus,
}

fn normalize_fan_action_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if FanActionToken::parse(trimmed).is_ok() {
        return Some(trimmed.to_ascii_lowercase());
    }
    let url = Url::parse(trimmed).ok()?;
    let candidate = url
        .query_pairs()
        .find(|(name, _)| name == "token")
        .map(|(_, token)| token.into_owned())
        .or_else(|| {
            url.fragment().and_then(|fragment| {
                url::form_urlencoded::parse(fragment.as_bytes())
                    .find(|(name, _)| name == "token")
                    .map(|(_, token)| token.into_owned())
            })
        })?;
    FanActionToken::parse(&candidate)
        .ok()
        .map(|token| token.as_str().to_owned())
}

/// Requests a fresh inbox link for an existing fan without exposing account existence.
pub async fn request_fan_access(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FanAccessRequest>, JsonRejection>,
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
    let email = match NormalizedEmail::parse(payload.email) {
        Ok(email) => email,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let locale = payload
        .locale
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if locale.as_ref().is_some_and(|value| {
        value.len() > 35
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let Some(raw_request_id) = headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::error!("server request ID middleware did not populate the request");
        return Problem::internal(None).private().into_response();
    };

    let mut transaction = match state.database.begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::warn!(%error, "could not start fan access transaction");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let row = sqlx::query_as::<_, (uuid::Uuid, String, Option<String>, Option<String>)>(
        r#"
        SELECT id, status, display_name, locale
        FROM fans
        WHERE workspace_id = $1 AND normalized_email = $2
        FOR UPDATE
        "#,
    )
    .bind(state.fan_lifecycle.workspace_id.into_uuid())
    .bind(email.as_str())
    .fetch_optional(&mut *transaction)
    .await;
    let row = match row {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%error, "could not load fan for access request");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let Some((fan_id, status, display_name, stored_locale)) = row else {
        if let Err(error) = transaction.commit().await {
            tracing::warn!(%error, "could not finish neutral fan access request");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        return (
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(FanAccessResponse { accepted: true }),
        )
            .into_response();
    };

    let (purpose, event_type, token_field) = match status.as_str() {
        "active" => ("session", "fan.session_requested", "session_recovery_token"),
        "pending" => (
            "confirm",
            "fan.confirmation_requested",
            "confirmation_token",
        ),
        "unsubscribed" | "suppressed" => {
            if let Err(error) = transaction.commit().await {
                tracing::warn!(%error, "could not finish neutral fan access request");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
            return (
                StatusCode::ACCEPTED,
                [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
                Json(FanAccessResponse { accepted: true }),
            )
                .into_response();
        }
        _ => {
            tracing::error!(status, "unexpected fan status during access request");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
    };

    let in_cooldown = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM fan_action_tokens
            WHERE workspace_id = $1
              AND fan_id = $2
              AND purpose = $3
              AND consumed_at IS NULL
              AND expires_at > now()
              AND created_at > now() - ($4::bigint * interval '1 second')
        )
        "#,
    )
    .bind(state.fan_lifecycle.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(purpose)
    .bind(ACCESS_RESEND_COOLDOWN_SECONDS)
    .fetch_one(&mut *transaction)
    .await;
    let in_cooldown = match in_cooldown {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not check fan access cooldown");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    if !in_cooldown {
        if let Err(error) = sqlx::query(
            r#"
            UPDATE fan_action_tokens
            SET consumed_at = COALESCE(consumed_at, now())
            WHERE workspace_id = $1
              AND fan_id = $2
              AND purpose = $3
              AND consumed_at IS NULL
            "#,
        )
        .bind(state.fan_lifecycle.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(purpose)
        .execute(&mut *transaction)
        .await
        {
            tracing::warn!(%error, "could not rotate fan access token");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }

        let raw_token = sqlx::query_scalar::<_, String>(
            r#"
            WITH material AS (
                SELECT encode(gen_random_bytes(32), 'hex') AS token
            ), inserted AS (
                INSERT INTO fan_action_tokens (
                    workspace_id, fan_id, purpose, token_hash, expires_at
                )
                SELECT $1, $2, $3, digest(material.token, 'sha256'),
                    now() + ($4::bigint * interval '1 day')
                FROM material
                RETURNING id
            )
            SELECT material.token
            FROM material, inserted
            "#,
        )
        .bind(state.fan_lifecycle.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(purpose)
        .bind(ACCESS_TOKEN_TTL_DAYS)
        .fetch_one(&mut *transaction)
        .await;
        let raw_token = match raw_token {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "could not issue fan access token");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        };
        let effective_locale = locale.or(stored_locale);
        let policy_version = sqlx::query_scalar::<_, String>(
            r#"
            SELECT policy_version
            FROM fan_consents
            WHERE workspace_id = $1 AND fan_id = $2
              AND purpose = 'marketing' AND granted
            ORDER BY recorded_at DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(state.fan_lifecycle.workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_optional(&mut *transaction)
        .await
        .ok()
        .flatten();
        let mut event_payload = serde_json::json!({
            "workspace_id": state.fan_lifecycle.workspace_id,
            "fan_id": fan_id,
            "email": email.as_str(),
            "display_name": display_name,
            "locale": effective_locale,
        });
        if let Some(object) = event_payload.as_object_mut() {
            object.insert(token_field.to_owned(), serde_json::Value::String(raw_token));
            if let Some(policy_version) = policy_version {
                object.insert(
                    "policy_version".to_owned(),
                    serde_json::Value::String(policy_version),
                );
            }
        }
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            ) VALUES ($1, $2, 1, $3, $4)
            "#,
        )
        .bind(state.fan_lifecycle.workspace_id.into_uuid())
        .bind(event_type)
        .bind(event_payload)
        .bind(raw_request_id)
        .execute(&mut *transaction)
        .await
        {
            tracing::warn!(%error, "could not enqueue fan access email");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    }

    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit fan access request");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    (
        StatusCode::ACCEPTED,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(FanAccessResponse { accepted: true }),
    )
        .into_response()
}

fn confirmation_idempotency_key(token: &FanActionToken) -> Result<IdempotencyKey, ()> {
    let token_digest = Sha256::digest(token.as_str().as_bytes());
    IdempotencyKey::parse(format!("fan-confirm-{}", hex::encode(token_digest))).map_err(|_| ())
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
    let token = match normalize_fan_action_token(&payload.token)
        .and_then(|value| FanActionToken::parse(value).ok())
    {
        Some(token) => token,
        None => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    // The token itself is the stable operation identity. This lets a browser
    // click, QR scan and native paste safely replay the same successful exchange
    // instead of turning the second device into a misleading conflict.
    let idempotency_key = match confirmation_idempotency_key(&token) {
        Ok(key) => key,
        Err(()) => {
            tracing::error!("server-derived fan confirmation key was invalid");
            return Problem::internal(request_id_value)
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
    let canonical_identity = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT normalized_email, display_name FROM fans WHERE workspace_id=$1 AND id=$2",
    )
    .bind(state.fan_lifecycle.workspace_id.into_uuid())
    .bind(result.fan_id.into_uuid())
    .fetch_optional(&state.database)
    .await;
    let (email, display_name) = match canonical_identity {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            tracing::error!(fan_id=%result.fan_id, "confirmed fan disappeared before identity response");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, fan_id=%result.fan_id, "could not load canonical fan identity after confirmation");
            return Problem::service_unavailable(request_id_value)
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
            fan_session_token: result.fan_session_token.as_str().to_owned(),
            email,
            display_name,
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
    let token = match normalize_fan_action_token(&payload.token)
        .and_then(|value| FanActionToken::parse(value).ok())
    {
        Some(token) => token,
        None => {
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

    #[test]
    fn confirmation_key_is_stable_for_the_same_token() {
        let token = FanActionToken::parse("a".repeat(64)).expect("valid token fixture");
        let first = confirmation_idempotency_key(&token).expect("derived key");
        let second = confirmation_idempotency_key(&token).expect("derived key");
        assert_eq!(first.as_str(), second.as_str());
        assert!(first.as_str().starts_with("fan-confirm-"));
    }

    #[test]
    fn confirmation_normalizer_accepts_query_and_fragment_links() {
        let token = "b".repeat(64);
        assert_eq!(
            normalize_fan_action_token(&format!(
                "https://virya.music/signal/confirm?token={token}"
            )),
            Some(token.clone())
        );
        assert_eq!(
            normalize_fan_action_token(&format!(
                "https://virya.music/signal/confirm#token={token}"
            )),
            Some(token)
        );
    }
}

/// Pilot onboarding: bulk-import an operator's existing mailing list.
///
/// Consent comes first, so an import can never manufacture an active fan:
/// every address lands as `pending` and receives the same double-opt-in
/// confirmation email the signup flow sends. Existing `active` fans are left
/// untouched (never downgraded), `unsubscribed`/`suppressed` are skipped
/// outright — a list someone exported elsewhere does not resurrect opt-outs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ImportFansRequest {
    source: String,
    entries: Vec<ImportFanEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ImportFanEntry {
    email: String,
    display_name: Option<String>,
    locale: Option<String>,
}

#[derive(Serialize)]
pub struct ImportFansResponse {
    imported_pending: u32,
    confirmation_resent: u32,
    already_active: u32,
    skipped_suppressed: u32,
    cooldown_skipped: u32,
    invalid: u32,
}

pub async fn import_fans_admin(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ImportFansRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    const MAX_IMPORT_ENTRIES: usize = 500;
    if request.entries.is_empty() || request.entries.len() > MAX_IMPORT_ENTRIES {
        return Problem::unprocessable(request_id_value).into_response();
    }
    if request.source.trim().is_empty() || request.source.len() > 200 {
        return Problem::unprocessable(request_id_value).into_response();
    }

    let workspace = state.fan_lifecycle.workspace_id.into_uuid();
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not start fan import transaction");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    // One shared raw id groups the whole batch's outbox rows for the timeline.
    let batch_request_id = format!("fan-import-{}", uuid::Uuid::now_v7().simple());

    let mut counts = ImportFansResponse {
        imported_pending: 0,
        confirmation_resent: 0,
        already_active: 0,
        skipped_suppressed: 0,
        cooldown_skipped: 0,
        invalid: 0,
    };

    for entry in &request.entries {
        let Ok(email) = NormalizedEmail::parse(entry.email.trim()) else {
            counts.invalid += 1;
            continue;
        };
        let locale = entry
            .locale
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let display_name = entry
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        // Load-or-create inside the row lock; nothing ever downgrades status.
        let existing = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status FROM fans
            WHERE workspace_id = $1 AND normalized_email = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace)
        .bind(email.as_str())
        .fetch_optional(&mut *transaction)
        .await;

        let fan_status = match existing {
            Ok(Some(status)) => status,
            Ok(None) => {
                if let Err(error) = sqlx::query(
                    r#"
                    INSERT INTO fans (workspace_id, normalized_email, display_name, locale, status)
                    VALUES ($1, $2, $3, $4, 'pending')
                    "#,
                )
                .bind(workspace)
                .bind(email.as_str())
                .bind(display_name)
                .bind(locale)
                .execute(&mut *transaction)
                .await
                {
                    tracing::warn!(%error, "fan import insert failed");
                    return Problem::service_unavailable(request_id_value)
                        .private()
                        .into_response();
                }
                counts.imported_pending += 1;
                "pending".to_owned()
            }
            Err(error) => {
                tracing::warn!(%error, "fan import lookup failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        };

        match fan_status.as_str() {
            "active" => {
                counts.already_active += 1;
                continue;
            }
            "unsubscribed" | "suppressed" => {
                counts.skipped_suppressed += 1;
                continue;
            }
            "pending" => {}
            unexpected => {
                tracing::error!(status = %unexpected, "unexpected fan status during import");
                return Problem::internal(request_id_value)
                    .private()
                    .into_response();
            }
        }

        // The fan row exists as pending; fetch its id for token issuance.
        let fan_id = match sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id FROM fans WHERE workspace_id = $1 AND normalized_email = $2",
        )
        .bind(workspace)
        .bind(email.as_str())
        .fetch_one(&mut *transaction)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "fan import id lookup failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        };

        // Same resend cooldown the interactive flow uses: a fresh import must
        // not machine-gun confirmation emails at an address already waiting.
        let in_cooldown = matches!(
            sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM fan_action_tokens
                    WHERE workspace_id = $1 AND fan_id = $2
                      AND purpose = 'confirm'
                      AND consumed_at IS NULL AND expires_at > now()
                      AND created_at > now() - ($3::bigint * interval '1 second')
                )
                "#,
            )
            .bind(workspace)
            .bind(fan_id)
            .bind(ACCESS_RESEND_COOLDOWN_SECONDS)
            .fetch_one(&mut *transaction)
            .await,
            Ok(true)
        );
        if in_cooldown {
            counts.cooldown_skipped += 1;
            continue;
        }

        if let Err(error) = sqlx::query(
            r#"
            UPDATE fan_action_tokens
            SET consumed_at = COALESCE(consumed_at, now())
            WHERE workspace_id = $1 AND fan_id = $2
              AND purpose = 'confirm' AND consumed_at IS NULL
            "#,
        )
        .bind(workspace)
        .bind(fan_id)
        .execute(&mut *transaction)
        .await
        {
            tracing::warn!(%error, "fan import token rotation failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }

        let raw_token = match sqlx::query_scalar::<_, String>(
            r#"
            WITH material AS (
                SELECT encode(gen_random_bytes(32), 'hex') AS token
            ), inserted AS (
                INSERT INTO fan_action_tokens (
                    workspace_id, fan_id, purpose, token_hash, expires_at
                )
                SELECT $1, $2, 'confirm', digest(material.token, 'sha256'),
                    now() + ($3::bigint * interval '1 day')
                FROM material
                RETURNING id
            )
            SELECT material.token FROM material, inserted
            "#,
        )
        .bind(workspace)
        .bind(fan_id)
        .bind(ACCESS_TOKEN_TTL_DAYS)
        .fetch_one(&mut *transaction)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "fan import token issue failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        };

        let event_payload = serde_json::json!({
            "workspace_id": state.fan_lifecycle.workspace_id,
            "fan_id": fan_id,
            "email": email.as_str(),
            "display_name": display_name,
            "locale": locale,
            "confirmation_token": raw_token,
            "import_source": request.source.trim(),
        });
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            ) VALUES ($1, 'fan.confirmation_requested', 1, $2, $3)
            "#,
        )
        .bind(workspace)
        .bind(event_payload)
        .bind(format!("{batch_request_id}:{}", counts.imported_pending))
        .execute(&mut *transaction)
        .await
        {
            tracing::warn!(%error, "fan import confirmation enqueue failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        counts.confirmation_resent += 1;
    }

    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        ) VALUES ($1, 'operator', 'fans.imported', 'workspace', $2, $3)
        "#,
    )
    .bind(workspace)
    .bind(workspace.to_string())
    .bind(serde_json::json!({
        "source": request.source.trim(),
        "imported_pending": counts.imported_pending,
        "confirmation_resent": counts.confirmation_resent,
        "already_active": counts.already_active,
        "skipped_suppressed": counts.skipped_suppressed,
        "cooldown_skipped": counts.cooldown_skipped,
        "invalid": counts.invalid,
    }))
    .execute(&mut *transaction)
    .await
    {
        tracing::warn!(%error, "fan import audit failed");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }

    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit fan import");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(counts),
    )
        .into_response()
}
