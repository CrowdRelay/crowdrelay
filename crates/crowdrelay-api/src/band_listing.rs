//! The band's listing and representation surface.
//!
//! Mounted under `/v1/control-plane` — the Control Plane proxies the band's
//! editor here. One public read lives in `routing.rs` (`/v1/public/listings/{token}`)
//! and answers a different question for a different reader; everything below
//! is the owner talking to its own workspace.
//!
//! Three rules the endpoints exist to keep:
//! - saving never publishes — `PUT` writes the draft and nothing else;
//! - `POST .../publish` is the only path through the domain's review;
//! - the share token mints once and only rotates when the band asks, which
//!   is what makes "the link stopped working" a real revocation.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{AutopilotOutreachStateRepository, UpsertOutreachTarget};
use crowdrelay_domain::OutreachTargetId;
use crowdrelay_domain::listing::{BandListing, ListedClaim, ListingVisibility};
use crowdrelay_domain::outreach::OutreachTargetKind;
use crowdrelay_domain::value_tier::MetricValueTier;
use crowdrelay_infra::band_listing::{BandListingError, PostgresBandListingRepository};
use crowdrelay_infra::representation::{ApproachOutcome, PostgresRepresentationRepository};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

fn listing_repo(state: &crate::AppState) -> PostgresBandListingRepository {
    PostgresBandListingRepository::new(state.database.clone())
}

fn representation_repo(state: &crate::AppState) -> PostgresRepresentationRepository {
    PostgresRepresentationRepository::new(state.database.clone())
}

fn listing_problem(error: BandListingError, request_id: Option<String>) -> Response {
    match error {
        BandListingError::NotFound => Problem::not_found(request_id).private().into_response(),
        BandListingError::Refused(detail) => Problem::conflict_owned(detail.into(), request_id)
            .private()
            .into_response(),
        BandListingError::Database(error) => {
            tracing::warn!(%error, "band listing query failed");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
    }
}

// ── The editor ───────────────────────────────────────────────────────

/// One claim as the form sends it. `value` may be absent — a claim the band
/// cannot yet support is a valid draft, it just never reaches a reader.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimBody {
    label: String,
    value: Option<i64>,
    tier: MetricValueTier,
    basis: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingBody {
    act_name: String,
    #[serde(default)]
    genre_tags: Vec<String>,
    #[serde(default)]
    cities: Vec<String>,
    #[serde(default)]
    claims: Vec<ClaimBody>,
    #[serde(default)]
    published_dates: Vec<String>,
    #[serde(default)]
    seeking: Vec<String>,
}

/// The editor's state: the draft as saved, the share token (the owner may
/// see it — it is the band's to hand out), and the approach allowance the
/// meter under the contacts reads.
#[derive(Serialize)]
struct ListingStateResponse {
    listing: Option<BandListing>,
    share_token: Option<Uuid>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none",
        default
    )]
    published_at: Option<OffsetDateTime>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none",
        default
    )]
    updated_at: Option<OffsetDateTime>,
    approaches_used_this_month: u32,
    monthly_approach_allowance: u32,
}

/// GET — everything the editor needs in one read.
pub async fn get_listing(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = listing_repo(&state);
    let state_row = match repo.load_state(workspace_id).await {
        Ok(value) => value,
        Err(error) => return listing_problem(error, request_id_value),
    };
    let used = match representation_repo(&state)
        .approaches_used_this_month(workspace_id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return representation_problem(error, request_id_value);
        }
    };
    let (listing, share_token, published_at, updated_at) = match state_row {
        Some(row) => (
            Some(row.listing),
            Some(row.share_token),
            row.published_at,
            Some(row.updated_at),
        ),
        None => (None, None, None, None),
    };
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, no-store")],
        Json(ListingStateResponse {
            listing,
            share_token,
            published_at,
            updated_at,
            approaches_used_this_month: used,
            monthly_approach_allowance:
                crowdrelay_domain::representation::MONTHLY_APPROACH_ALLOWANCE,
        }),
    )
        .into_response()
}

/// PUT — saves the draft. Never changes visibility: a band edits a live
/// listing under its readers, and `publish` is where the review runs.
pub async fn put_listing(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ListingBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    // Match the schema caps here so a form typo is a 400 that names the
    // field rather than a CHECK violation surfacing as a 503.
    let bounded = !body.act_name.trim().is_empty()
        && body.act_name.chars().count() <= 160
        && body.genre_tags.len() <= 12
        && body.cities.len() <= 40
        && body.published_dates.len() <= 40
        && body.seeking.len() <= 8
        && body.claims.len() <= 24
        && body.claims.iter().all(|claim| {
            !claim.label.trim().is_empty()
                && claim.label.chars().count() <= 120
                && !claim.basis.trim().is_empty()
                && claim.basis.chars().count() <= 200
                // A claim is a count or a date — a negative number is not a
                // thing the band can stand behind, and the public page
                // refuses the whole listing over one.
                && claim.value.is_none_or(|value| value >= 0)
        });
    if !bounded {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let workspace_id = state.ops.workspace_id().into_uuid();
    let listing = BandListing {
        act_name: body.act_name,
        genre_tags: body.genre_tags,
        cities: body.cities,
        claims: body
            .claims
            .into_iter()
            .map(|claim| ListedClaim {
                label: claim.label,
                value: claim.value,
                tier: claim.tier,
                basis: claim.basis,
            })
            .collect(),
        published_dates: body.published_dates,
        seeking: body.seeking,
        // `save` never touches the visibility column — this field is the
        // domain type's shape, not a decision being written.
        visibility: ListingVisibility::Unlisted,
    };
    match listing_repo(&state).save(workspace_id, &listing).await {
        Ok(()) => get_listing(State(state), headers).await,
        Err(error) => listing_problem(error, request_id_value),
    }
}

/// POST — run the domain's review and publish. The refusal sentence the
/// domain writes reaches the band as the 409 detail.
pub async fn publish_listing(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = listing_repo(&state);
    match repo.publish(workspace_id).await {
        Ok(()) => get_listing(State(state), headers).await,
        Err(error) => listing_problem(error, request_id_value),
    }
}

/// POST — take the listing down. Links already sent go to 404; the draft
/// stays for editing.
pub async fn unlist_listing(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = listing_repo(&state);
    match repo.unlist(workspace_id).await {
        Ok(()) => get_listing(State(state), headers).await,
        Err(error) => listing_problem(error, request_id_value),
    }
}

/// POST — mint a fresh share token. Every link already sent stops working;
/// the response carries the new one.
pub async fn rotate_listing_token(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = listing_repo(&state);
    match repo.rotate_share_token(workspace_id).await {
        Ok(_) => get_listing(State(state), headers).await,
        Err(error) => listing_problem(error, request_id_value),
    }
}

// ── Representation contacts and the approach ─────────────────────────

/// GET — the contacts the band may approach, plus the month's allowance.
/// No `contact_email` is ever serialized: the platform brokers the send,
/// and an address the band cannot see cannot leak into its own tooling.
pub async fn list_representation_targets(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = representation_repo(&state);
    let targets = match repo.list_targets(workspace_id).await {
        Ok(value) => value,
        Err(error) => return representation_problem(error, request_id_value),
    };
    let used = match repo.approaches_used_this_month(workspace_id).await {
        Ok(value) => value,
        Err(error) => return representation_problem(error, request_id_value),
    };
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, no-store")],
        Json(json!({
            "targets": targets,
            "approaches_used_this_month": used,
            "monthly_approach_allowance":
                crowdrelay_domain::representation::MONTHLY_APPROACH_ALLOWANCE,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepresentationTargetBody {
    /// Present on an edit, absent on a create.
    target_id: Option<Uuid>,
    kind: String,
    display_name: String,
    contact_email: String,
    accepts_outreach: bool,
    #[serde(default)]
    accepts_outreach_basis: Option<String>,
    #[serde(default = "default_true")]
    active: bool,
    /// Confirmed means the band attests the address reaches the person —
    /// it must be said, not defaulted, or the NotVerified gate is a
    /// pretense on this feature's primary path.
    #[serde(default)]
    verified: bool,
    do_not_contact: bool,
    #[serde(default)]
    expected_version: i64,
}

fn default_true() -> bool {
    true
}

/// POST — add or update an agent/label contact. Rides the same
/// `upsert_outreach_target` port the admin surface uses; the kind is pinned
/// to representation here so this route can never be used to file a press
/// contact with a hidden address.
pub async fn upsert_representation_target(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RepresentationTargetBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let kind = match body.kind.trim().to_ascii_lowercase().as_str() {
        "agent" => OutreachTargetKind::Agent,
        "label" => OutreachTargetKind::Label,
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let basis = body
        .accepts_outreach_basis
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let valid = !body.display_name.trim().is_empty()
        && body.display_name.chars().count() <= 200
        && body.contact_email.contains('@')
        && body.contact_email.chars().count() <= 320
        && body.expected_version >= 0
        && (body.expected_version == 0 || body.target_id.is_some())
        && basis.is_none_or(|value| value.chars().count() <= 240)
        // The schema CHECK enforces this too; refusing here is where the
        // band gets a sentence instead of a constraint name.
        && (!body.accepts_outreach || basis.is_some());
    if !valid {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let Some(idempotency_key) = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = UpsertOutreachTarget {
        target_id: body.target_id.map(OutreachTargetId::from_uuid),
        kind,
        display_name: body.display_name,
        contact_email: body.contact_email,
        // The band's own contacts carry neutral priors — the score is for
        // researched targets, and a contact the band entered is already the
        // decision.
        priority: 50,
        relationship_score: 50,
        active: body.active,
        verified: body.verified,
        accepts_outreach: body.accepts_outreach,
        do_not_contact: body.do_not_contact,
        accepts_outreach_basis: basis.map(str::to_owned),
        expected_version: body.expected_version,
    };
    match state
        .autopilot
        .upsert_outreach_target(state.ops.workspace_id(), command, &idempotency_key, None)
        .await
    {
        Ok(result) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(result),
        )
            .into_response(),
        Err(error) => match error {
            crowdrelay_application::RepositoryError::Conflict => {
                Problem::conflict(request_id_value)
                    .private()
                    .into_response()
            }
            crowdrelay_application::RepositoryError::ConflictBecause(detail) => {
                Problem::conflict_because(detail, request_id_value)
                    .private()
                    .into_response()
            }
            crowdrelay_application::RepositoryError::NotFound => {
                Problem::not_found(request_id_value)
                    .private()
                    .into_response()
            }
            _ => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproachBody {
    target_id: Uuid,
    #[serde(default)]
    note: Option<String>,
}

/// POST — the band asks to approach an agent or a label. Runs the domain
/// gate and queues an `awaiting_approval` action; dispatch re-runs the same
/// gate, so an approval that goes stale cannot send.
pub async fn request_representation_approach(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ApproachBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(idempotency_key) = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    match representation_repo(&state)
        .request_approach(
            workspace_id,
            body.target_id,
            body.note.as_deref(),
            &idempotency_key,
        )
        .await
    {
        Ok(ApproachOutcome::Queued { action_id }) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(json!({ "action_id": action_id, "status": "awaiting_approval" })),
        )
            .into_response(),
        Ok(ApproachOutcome::Replayed { action_id, status }) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(json!({ "action_id": action_id, "status": status })),
        )
            .into_response(),
        Err(error) => representation_problem(error, request_id_value),
    }
}

fn representation_problem(
    error: crowdrelay_infra::representation::RepresentationError,
    request_id: Option<String>,
) -> Response {
    use crowdrelay_infra::representation::RepresentationError as E;
    match error {
        E::NotFound => Problem::not_found(request_id).private().into_response(),
        E::Refused(detail) => Problem::conflict_owned(detail.into(), request_id)
            .private()
            .into_response(),
        E::Database(error) => {
            tracing::warn!(%error, "representation query failed");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
    }
}

// ── The public read ──────────────────────────────────────────────────

/// GET `/v1/public/listings/{token}` — what a link-holder receives.
///
/// The token is the whole credential: a correct token on a published
/// listing returns the domain-redacted profile; anything else — wrong
/// token, rotated token, unlisted listing — is the same 404. An admitted
/// reader must not be able to tell "never existed" from "taken down",
/// because that difference is the band's business, not the reader's.
pub async fn public_listing(
    State(state): State<crate::AppState>,
    Path(token): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    // The token is the whole credential — no workspace resolution happens
    // because the reader carries it in the link.
    match PostgresBandListingRepository::new(state.database.clone())
        .read_visible_by_token(token)
        .await
    {
        Ok(Some(listing)) => (
            StatusCode::OK,
            // Link-holders may cache briefly; a rotation or unlist shows up
            // as a 404 within a minute, not a stale copy living forever.
            [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
            Json(listing),
        )
            .into_response(),
        Ok(None) => Problem::not_found(request_id(&headers)).into_response(),
        Err(error) => {
            tracing::warn!(%error, "public listing read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
