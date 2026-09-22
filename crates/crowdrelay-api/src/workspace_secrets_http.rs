//! Operator and runtime surface for tenant-held secrets.
//!
//! Three routes, three different trust levels:
//!
//! - `GET /v1/control-plane/secrets` — the masked inventory. Enough to answer
//!   "is the key set, and which one is it" — never the value.
//! - `PUT`/`DELETE /v1/control-plane/secrets/{name}` — write-only writes. The
//!   response echoes the masked hint, so a pasted value never comes back.
//! - `GET /v1/internal/stripe-credentials` — the reveal. Commerce-bearer
//!   server-to-server only: the tenant's own site asks for the Stripe pair
//!   its checkout runs against, plus the ticketing opt-in it must respect.
//!
//! The names are an allowlist (`KNOWN_SECRET_NAMES`) — a generic secret write
//! surface is a different, larger thing than what this feature needs.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::{
    tenant_settings::TenantSettingsRepository,
    workspace_secrets::{
        KNOWN_SECRET_NAMES, MaskedSecret, SECRET_STRIPE_SECRET_KEY, SECRET_STRIPE_WEBHOOK_SECRET,
        WorkspaceSecretsError, WorkspaceSecretsRepository, stripe_masked_hint,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
/// A Stripe credential is printable ASCII, 8–200 chars — past that it is not
/// a key somebody pasted, it is a payload.
const MAX_SECRET_VALUE_CHARS: usize = 200;
const MIN_SECRET_VALUE_CHARS: usize = 8;

fn repository(state: &crate::AppState) -> WorkspaceSecretsRepository {
    WorkspaceSecretsRepository::new(
        state.database.clone(),
        state.workspace_secrets_key.clone(),
        state.previous_workspace_secrets_key.clone(),
    )
}

fn masked_json(secret: &MaskedSecret) -> serde_json::Value {
    json!({
        "name": secret.name,
        "masked_hint": secret.masked_hint,
        "updated_at": secret.updated_at,
    })
}

fn secrets_error_response(
    error: WorkspaceSecretsError,
    request_id_value: Option<String>,
) -> Response {
    match error {
        // A row that exists but opens under neither key is a hard failure —
        // reporting it as "not set" would send the operator to paste a key
        // that is already there and unreadable.
        WorkspaceSecretsError::CannotOpen | WorkspaceSecretsError::Database(_) => {
            if let WorkspaceSecretsError::Database(error) = &error {
                tracing::warn!(%error, "workspace secrets store failed");
            } else {
                tracing::warn!("workspace secret failed to open under the configured keys");
            }
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// Shape validation for a value being stored. The prefix grammar is the part
/// that catches a paste error before it becomes a checkout that cannot run;
/// length and printable-ASCII are the part that keeps the row a credential
/// and not a document.
fn valid_secret_value(name: &str, value: &str) -> bool {
    if !(MIN_SECRET_VALUE_CHARS..=MAX_SECRET_VALUE_CHARS).contains(&value.len())
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return false;
    }
    match name {
        SECRET_STRIPE_SECRET_KEY => {
            value.starts_with("sk_live_")
                || value.starts_with("sk_test_")
                || value.starts_with("rk_live_")
                || value.starts_with("rk_test_")
        }
        SECRET_STRIPE_WEBHOOK_SECRET => value.starts_with("whsec_"),
        _ => false,
    }
}

/// `GET /v1/control-plane/secrets` — every configured secret, masked.
pub async fn list_secrets(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repository(&state).list_masked(workspace_id).await {
        Ok(secrets) => {
            let items: Vec<serde_json::Value> = secrets.iter().map(masked_json).collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({ "secrets": items })),
            )
                .into_response()
        }
        Err(error) => secrets_error_response(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSecretRequest {
    value: String,
}

/// `PUT /v1/control-plane/secrets/{name}` — seal and store one known secret.
/// The response is the masked view; the value that arrived never leaves.
pub async fn put_secret(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    payload: Result<Json<SetSecretRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !KNOWN_SECRET_NAMES.contains(&name.as_str()) {
        return Problem::bad_request_because("unknown secret name", request_id_value)
            .private()
            .into_response();
    }
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let value = request.value.trim().to_owned();
    if !valid_secret_value(&name, &value) {
        // `unprocessable`'s copy talks about a signup policy — wrong domain
        // for an operator pasting a key. Name the grammar the value missed.
        let detail = match name.as_str() {
            SECRET_STRIPE_SECRET_KEY => {
                "The value must be a Stripe secret or restricted key (sk_live_, sk_test_, rk_live_ or rk_test_), printable ASCII, 8–200 characters."
            }
            // `name` passed KNOWN_SECRET_NAMES above; the only other member
            // is the webhook secret.
            _ => {
                "The value must be a Stripe webhook signing secret (whsec_), printable ASCII, 8–200 characters."
            }
        };
        return Problem::bad_request_because(detail, request_id_value)
            .private()
            .into_response();
    }
    let masked_hint = stripe_masked_hint(&value);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repository(&state)
        .set(workspace_id, &name, value.into_bytes(), &masked_hint)
        .await
    {
        Ok(secret) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(masked_json(&secret)),
        )
            .into_response(),
        Err(error) => secrets_error_response(error, request_id_value),
    }
}

/// `DELETE /v1/control-plane/secrets/{name}` — unset a key the tenant no
/// longer runs. Removing the secret key while ticketing stays enabled leaves
/// checkout failing closed at key resolution, which is the honest answer to
/// "we stopped selling through that account".
pub async fn delete_secret(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !KNOWN_SECRET_NAMES.contains(&name.as_str()) {
        return Problem::bad_request_because("unknown secret name", request_id_value)
            .private()
            .into_response();
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repository(&state).delete(workspace_id, &name).await {
        Ok(removed) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "name": name, "removed": removed })),
        )
            .into_response(),
        Err(error) => secrets_error_response(error, request_id_value),
    }
}

#[derive(Serialize)]
struct StripeCredentialsResponse {
    ticketing_enabled: bool,
    stripe_secret_key: Option<String>,
    stripe_webhook_secret: Option<String>,
}

/// `GET /v1/internal/stripe-credentials` — the one route that opens the
/// store, for the tenant's own checkout server over the commerce bearer.
///
/// The opt-in flag rides with the credentials so the caller makes one round
/// trip; the flag is advisory here and enforced at the reserve — a tenant
/// that opts out is refused where the order row would be written, not just
/// where the key is handed out. Merch checkout shares the same Stripe
/// account, so the keys are served regardless of the flag.
pub async fn stripe_credentials(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let repository = repository(&state);
    let settings = TenantSettingsRepository::new(state.database.clone());
    let joined = tokio::time::timeout(state.ticketing.operation_timeout(), async {
        tokio::join!(
            settings.brand_settings(workspace_id),
            repository.reveal(workspace_id, SECRET_STRIPE_SECRET_KEY),
            repository.reveal(workspace_id, SECRET_STRIPE_WEBHOOK_SECRET),
        )
    })
    .await;
    let Ok((brand, secret_key, webhook_secret)) = joined else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let (brand, secret_key, webhook_secret) = match (brand, secret_key, webhook_secret) {
        (Ok(brand), Ok(secret_key), Ok(webhook_secret)) => (brand, secret_key, webhook_secret),
        (Err(error), _, _)
        | (_, Err(WorkspaceSecretsError::Database(error)), _)
        | (_, _, Err(WorkspaceSecretsError::Database(error))) => {
            tracing::warn!(%error, "stripe credentials lookup failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        (_, Err(WorkspaceSecretsError::CannotOpen), _)
        | (_, _, Err(WorkspaceSecretsError::CannotOpen)) => {
            tracing::warn!("stored stripe credential failed to open under the configured keys");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    // A stored value that opens but is not UTF-8 cannot be a Stripe key —
    // answering null would send the caller to its env fallback and charge the
    // wrong account, so the honest answer is the same as CannotOpen: 503.
    let decode = |stored: Option<Vec<u8>>| -> Result<Option<String>, ()> {
        stored.map_or(Ok(None), |bytes| {
            String::from_utf8(bytes).map(Some).map_err(|_| ())
        })
    };
    let (Ok(secret_key), Ok(webhook_secret)) = (decode(secret_key), decode(webhook_secret)) else {
        tracing::warn!("stored stripe credential decrypted to non-UTF-8 bytes");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(StripeCredentialsResponse {
            ticketing_enabled: brand.ticketing_enabled,
            stripe_secret_key: secret_key,
            stripe_webhook_secret: webhook_secret,
        }),
    )
        .into_response()
}
