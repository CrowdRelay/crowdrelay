//! HTTP for audience attestations: issue, withdraw, and let a stranger check.
//!
//! Three surfaces with three different audiences, and the differences between
//! them are the feature.
//!
//! **The tenant** issues and withdraws through the control-plane routes. It
//! chooses whether a document exists and who gets the link. It cannot choose
//! what the document says — no request body here carries a figure, because the
//! whole claim is that no number in an attestation was typed by a person.
//!
//! **A link-holder** reads by share token. No account, no workspace
//! resolution: the link is the admission, the same shape as a published band
//! listing.
//!
//! **Anyone at all** can verify by digest. This is the one route with no
//! credential of any kind, and that is deliberate — a label's lawyer, handed a
//! PDF, needs to check it against us without asking the band for access. The
//! digest is printed on the document; presenting it proves nothing and
//! therefore costs nothing to accept.
//!
//! # The verify response never collapses its answers
//!
//! A document can be unedited and forged, or authentic and expired. A single
//! `valid: true/false` makes those indistinguishable, and "invalid" is exactly
//! the word a band would argue with. Four flags plus a sentence.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::attestation::{
    AttestationError, PostgresAttestationRepository, VerifiedAttestation,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

fn repo(state: &crate::AppState) -> PostgresAttestationRepository {
    PostgresAttestationRepository::new(
        state.database.clone(),
        state.attestation_signing_key.clone(),
    )
}

fn problem(error: &AttestationError, request_id: Option<String>) -> Response {
    match error {
        AttestationError::NotFound => Problem::not_found(request_id).private().into_response(),
        // The domain's refusal sentence is written for the operator to read and
        // act on ("re-run the measurement", "include something banked"), so it
        // travels rather than being flattened into a code. Same treatment the
        // listing and representation refusals get.
        AttestationError::Refused(message) => {
            Problem::conflict_owned(message.clone().into(), request_id)
                .private()
                .into_response()
        }
        AttestationError::Database(error) => {
            tracing::warn!(%error, "attestation query failed");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
        // A row whose digest disagrees with its figures. Reported as
        // unavailable rather than as an empty document, because publishing
        // "this act has no figures" would be a false statement about the act
        // rather than an honest failure to read a row.
        AttestationError::Corrupt => {
            tracing::error!("a stored attestation could not be read back");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
    }
}

/// What the tenant asks for. Cities only — everything else about the document
/// is decided by what the ledger holds.
#[derive(Debug, Deserialize)]
pub struct IssueRequest {
    /// City slugs to include a reachable-fans figure for. Empty is fine: the
    /// audience-wide figures stand on their own.
    #[serde(default)]
    pub cities: Vec<String>,
}

/// Each city costs the measurement one query, and each figure costs the
/// reader one more line of a document that is supposed to stay readable. A
/// request past this is not an attestation, it is a report — and reports are
/// a different endpoint's job.
const MAX_ISSUE_CITIES: usize = 32;

/// The document plus the two things a tenant needs to act: where to send it,
/// and what a reader will see if they check it.
#[derive(Debug, Serialize)]
struct IssuedView {
    digest: String,
    share_token: Uuid,
    signature: String,
    issued_at: String,
    valid_until: String,
    figures: Vec<serde_json::Value>,
}

/// `POST /v1/control-plane/attestations` — measure, issue, sign, store.
pub async fn issue_attestation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(request): Json<IssueRequest>,
) -> Response {
    if request.cities.len() > MAX_ISSUE_CITIES {
        return Problem::bad_request_because("cities holds at most 32 slugs", request_id(&headers))
            .private()
            .into_response();
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let act_name = state.tenant.display_name.clone();
    let repository = repo(&state);
    let now = OffsetDateTime::now_utc();

    match repository
        .issue_for_workspace(workspace_id, &act_name, &request.cities, now)
        .await
    {
        Ok(attestation) => {
            let share_token = match repository
                .share_token_for(workspace_id, &attestation.digest)
                .await
            {
                Ok(token) => token,
                Err(error) => return problem(&error, request_id(&headers)),
            };
            let signature = repository.sign_digest(&attestation.digest);
            let figures = attestation
                .figures
                .iter()
                .map(|figure| {
                    json!({
                        "metric": figure.metric.as_str(),
                        "method": figure.metric.method(),
                        "scope": figure.scope,
                        "value": figure.value,
                        "reads_as": figure.value.describe(),
                        "window_days": figure.window_days,
                        "observed_at": figure.observed_at.unix_timestamp(),
                    })
                })
                .collect();
            (
                StatusCode::CREATED,
                Json(IssuedView {
                    digest: attestation.digest.clone(),
                    share_token,
                    signature,
                    issued_at: attestation.issued_at.unix_timestamp().to_string(),
                    valid_until: attestation.valid_until.unix_timestamp().to_string(),
                    figures,
                }),
            )
                .into_response()
        }
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// `GET /v1/control-plane/attestations` — the tenant's documents, newest
/// first. Enough to manage them; the figures stay on the document itself.
pub async fn list_attestations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repo(&state).list_for_workspace(workspace_id).await {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "digest": row.digest,
                        "share_token": row.share_token,
                        "act_name": row.act_name,
                        "issued_at": row.issued_at
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                        "valid_until": row.valid_until
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                        "revoked": row.revoked,
                    })
                })
                .collect();
            (StatusCode::OK, Json(items)).into_response()
        }
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// `POST /v1/control-plane/attestations/{digest}/revoke` — withdraw a document.
///
/// The row stays. A band withdrawing a link wants it to stop working, not to
/// erase the record of having issued it, and a reader who gets "we cannot find
/// this" hears "this was forged".
pub async fn revoke_attestation(
    State(state): State<crate::AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repo(&state)
        .revoke(workspace_id, &digest, OffsetDateTime::now_utc())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// `POST /v1/control-plane/attestations/{digest}/rotate` — mint a fresh link.
pub async fn rotate_attestation_link(
    State(state): State<crate::AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repo(&state).rotate_share_token(workspace_id, &digest).await {
        Ok(token) => (StatusCode::OK, Json(json!({ "share_token": token }))).into_response(),
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// The document as a reader receives it, with its verification alongside.
///
/// Methods are inlined per figure rather than referenced, because the reader
/// has no second page to turn to and the method is the part worth arguing
/// with.
fn reader_view(verified: &VerifiedAttestation) -> serde_json::Value {
    json!({
        "act": verified.attestation.act_name,
        "issued_at": verified.attestation.issued_at.unix_timestamp(),
        "valid_until": verified.attestation.valid_until.unix_timestamp(),
        "digest": verified.attestation.digest,
        "figures": verified
            .attestation
            .figures
            .iter()
            .map(|figure| json!({
                "metric": figure.metric.as_str(),
                "method": figure.metric.method(),
                "scope": figure.scope,
                "reads_as": figure.value.describe(),
                "window_days": figure.window_days,
                "observed_at": figure.observed_at.unix_timestamp(),
            }))
            .collect::<Vec<_>>(),
        "verification": {
            "verdict": verified.verdict(),
            "unedited": verified.unedited,
            "issued_by_us": verified.issued_by_us,
            "current": verified.current,
            "revoked": verified.revoked,
        },
    })
}

/// `GET /v1/public/attestations/{token}` — what a link-holder sees.
pub async fn public_attestation(
    State(state): State<crate::AppState>,
    Path(token): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    match repo(&state)
        .read_by_token(token, OffsetDateTime::now_utc())
        .await
    {
        Ok(verified) => (
            StatusCode::OK,
            // Short cache only. A revoke or a rotation has to show up in about
            // a minute; a document cached for an hour after being withdrawn is
            // the band's word still circulating against their wishes.
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(reader_view(&verified)),
        )
            .into_response(),
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// `GET /v1/public/attestations/verify/{digest}` — what anybody can check.
///
/// No credential at all. Somebody holding a printed document types the digest
/// and learns whether we issued it. A wrong or invented digest is a 404, which
/// is the honest answer: we have no record of that document.
pub async fn verify_attestation(
    State(state): State<crate::AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Bounded before it reaches the database. The route is unauthenticated, so
    // the cheapest rejection is the one that never becomes a query, and a
    // digest is a fixed 64 lowercase hex characters by construction.
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Problem::not_found(request_id(&headers)).into_response();
    }
    match repo(&state)
        .verify_digest(&digest.to_lowercase(), OffsetDateTime::now_utc())
        .await
    {
        Ok(verified) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(reader_view(&verified)),
        )
            .into_response(),
        Err(error) => problem(&error, request_id(&headers)),
    }
}

/// `GET /v1/public/attestations/digest/{digest}/anchor` — where the document
/// sits in the transparency ledger.
///
/// The verify route answers "did we issue this"; this one answers "can a buyer
/// check that against a public root rather than trusting our HMAC". Two
/// answers that are not errors and must not read as one: `anchored: false`
/// means the document is ours and no batch has committed it yet — a stated
/// state, because "we cannot find this" reads like "this was forged" — while a
/// digest no attestation carries is still a 404.
pub async fn attestation_anchor(
    State(state): State<crate::AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Stricter than the verify route on purpose: this endpoint is asked by a
    // machine comparing digests, and a malformed one is a client bug worth a
    // 400 rather than a quiet "no record". Stored digests are lowercase hex by
    // CHECK, so an uppercase presentation can only ever miss.
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Problem::bad_request(request_id(&headers)).into_response();
    }
    match repo(&state).anchor_for_digest(&digest).await {
        Ok(Some(anchor)) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(json!({
                "anchored": true,
                "anchor": anchor,
                // Points at the existing public inclusion route, so a reader
                // holding this response needs nothing else to fetch the Merkle
                // path to the batch root.
                "inclusion_proof_path": format!(
                    "/v1/public/proofs/batches/{}/attestation/{}",
                    anchor.batch_id, anchor.attestation_id
                ),
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(json!({ "anchored": false })),
        )
            .into_response(),
        Err(error) => problem(&error, request_id(&headers)),
    }
}
