//! Google Drive contacts — control-plane review surface.
//!
//! Every address the Drive connector extracts lands staged in
//! `drive_contacts`. Nothing here auto-classifies: the operator
//! promotes or dismisses per destination, and `fan_outcome` /
//! `beacon_outcome` stay independent because a beacon may also be a fan.
//!
//! - `promote: fan` routes through `fan_import::import_batch` — the same
//!   pending + double-opt-in path every fan import uses. A spreadsheet can
//!   never create an `active` fan.
//! - `promote: beacon` writes a `proposed` `agent_outreach_targets` row for
//!   press-side kinds — the screening queue, same as the curated-CRM import —
//!   or an admitted `booking_candidates` row for venue/promoter/
//!   festival kinds. Booking candidates are city-scoped: the request's `city`
//!   wins, else the sheet's own city column resolves against `cities`, else
//!   an unambiguous `place_venues` name match resolves the room.
//! - `dismiss` records the decision so a re-scan never re-suggests.
//! - `promote-batch` promotes one whole segment in a single transaction.
//!   The daily sync never promotes: this endpoint is the only bulk path,
//!   and it fires only when a person clicks with the segment's live count
//!   in front of them — a count that drifted since the page rendered is a
//!   409, not a wider send. Consent is unchanged: every address still goes
//!   through the pending + double-opt-in import, and a suppressed address
//!   stays staged.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const LIST_LIMIT: i64 = 500;
const ACCESS_TOKEN_TTL_DAYS: i64 = 30;
const ACCESS_RESEND_COOLDOWN_SECONDS: i64 = 300;

/// Beacon/outreach target kinds — mirrors the CHECK on
/// `agent_outreach_targets.target_kind`.
/// Press-side kinds land in `agent_outreach_targets`; booking-side kinds are
/// supply for `booking_candidates` — a promoter is who books the
/// room, not who writes about the band, and the two pipelines measure
/// different outcomes.
const OUTREACH_KINDS: &[&str] = &[
    "press",
    "radio",
    "playlist",
    "media_patronage",
    "endorsement",
    "creator",
];
const BOOKING_KINDS: &[&str] = &["venue", "promoter", "festival"];
// The §12-5 booking agent: a band pitches an agent to be represented, once —
// a different direction and cadence from a promoter's one-show pitch, so the
// kind has its own table (booking_agents) rather than a chair in the
// city-scoped candidate queue. The extractor files `talent_buyer` as
// `booking_agent`, but the promote accepts the sheet's own word too.
// The extractor's `kind_for` maps the sheet's "agent" spelling onto
// `booking_agent` — the explicit promote accepts the same spelling so the
// vocabulary does not fork between the sheet and the console.
const AGENT_KINDS: &[&str] = &["booking_agent", "talent_buyer", "agent"];
// Agents and labels the band already dealt with file onto the
// representation list — a different home from the press kinds because the
// consent they carry is different: not "published a route", but "the band
// says why they would hear from it", stated before the first approach.
const REPRESENTATION_KINDS: &[&str] = &["agent", "label"];

#[derive(Serialize)]
pub struct DriveContact {
    id: String,
    email: String,
    display_name: Option<String>,
    organization: Option<String>,
    suggested_kind: Option<String>,
    /// The city the sheet gave this contact — the console shows it as a
    /// hint on the booking promote rather than pre-filling the input, so
    /// a display name never goes out as an explicit slug and 409s. The
    /// backend resolves the staged value itself when no slug is sent.
    city: Option<String>,
    /// The verification sheet's liveness verdict on this row —
    /// `active`/`inactive`, `null` when no sheet claimed one. Shown so an
    /// operator sees "marked inactive" before they promote.
    staged_status: Option<String>,
    notes: Option<String>,
    source_file_name: String,
    sources: Vec<String>,
    last_seen_at: String,
    gone_from_source: bool,
    fan_outcome: String,
    beacon_outcome: String,
    /// P.2: the room's `place_venues` display name when this contact's
    /// organisation is already on record — `null` means a stranger.
    matched_venue: Option<String>,
    /// The band's own marks say they already played that room.
    venue_played_here: bool,
    /// The counterparty registry knows this address — somebody's event
    /// already recorded dealing with this person.
    matched_counterparty: Option<String>,
    /// The band's own marks say they already dealt with them.
    counterparty_worked_with: bool,
    /// P.6: the address's reply record across every tenant — anonymous
    /// counts. `null` means the prior read did not run, not "no history".
    counterparty_prior: Option<crowdrelay_infra::cross_tenant_priors::CounterpartyPrior>,
    /// P.6: the matched room's play record across every tenant. `null`
    /// when no venue matched or the read did not run.
    venue_prior: Option<crowdrelay_infra::cross_tenant_priors::VenuePrior>,
}

/// The registry joins counted over the whole staging population — the page
/// is capped, so the "already on record" numbers cannot come from it.
#[derive(Serialize)]
pub struct RegistrySummary {
    total: i64,
    known_venues: i64,
    own_rooms: i64,
    known_counterparties: i64,
    dealt_with: i64,
}

#[derive(Serialize)]
pub struct DriveContactsResponse {
    contacts: Vec<DriveContact>,
    /// Null when the registry pass could not run — the contacts still
    /// render, and a summary that was never measured never reads as zero.
    registry_summary: Option<RegistrySummary>,
    /// The whole staging table in six numbers, counted server-side — the
    /// page is capped, so the panel's segment chips cannot come from it.
    /// Null when the count query failed, same rule as `registry_summary`.
    segment_counts: Option<crowdrelay_infra::gdrive::SegmentCounts>,
}

#[derive(Deserialize)]
pub struct ListContactsQuery {
    segment: Option<String>,
}

#[derive(Deserialize)]
pub struct PromoteRequest {
    destination: String,
    /// Beacon target kind. Falls back to the file's `suggested_kind`,
    /// then to `press` when the file said nothing.
    kind: Option<String>,
    /// City slug for booking kinds — booking candidates are city-scoped.
    /// Absent a slug, the venue registry is consulted for an unambiguous
    /// name match; neither resolving returns 400 rather than filing a
    /// candidate that can never promote.
    city: Option<String>,
}

#[derive(Deserialize)]
pub struct DismissRequest {
    destination: String,
}

fn contact_json(view: crowdrelay_infra::gdrive::DriveContactView) -> DriveContact {
    let row = view.row;
    DriveContact {
        id: row.id.to_string(),
        email: row.normalized_email,
        display_name: row.display_name,
        organization: row.organization,
        suggested_kind: row.suggested_kind,
        city: row.city,
        staged_status: row.staged_status,
        notes: row.notes,
        source_file_name: row.source_file_name,
        sources: row.sources,
        last_seen_at: row
            .last_seen_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        gone_from_source: row.disappeared_at.is_some(),
        fan_outcome: row.fan_outcome,
        beacon_outcome: row.beacon_outcome,
        matched_venue: row.matched_venue,
        venue_played_here: row.venue_played_here,
        matched_counterparty: row.matched_counterparty,
        counterparty_worked_with: row.counterparty_worked_with,
        counterparty_prior: view.counterparty_prior,
        venue_prior: view.venue_prior,
    }
}

fn repo(state: &crate::AppState) -> crowdrelay_infra::gdrive::PostgresGDriveRepository {
    crowdrelay_infra::gdrive::PostgresGDriveRepository::new(state.database.clone())
}

/// GET — the review queue, staged-first. `?segment=likely_fan` narrows the
/// page to one cut of the queue; an unknown segment name is a 400, not a
/// silent unfiltered list.
pub async fn list_contacts(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListContactsQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = repo(&state);
    let segment = match query
        .segment
        .as_deref()
        .map(crowdrelay_infra::gdrive::ContactSegment::parse)
    {
        Some(Some(segment)) => Some(segment),
        Some(None) => return Problem::bad_request(request_id_value).into_response(),
        None => None,
    };
    let rows = match repo.list_contacts(workspace_id, segment, LIST_LIMIT).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "gdrive contacts list failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    // A summary failure cannot take the list down — the contacts are the
    // payload, the registry counts are the annotation. A summary that could
    // not run reports as null rather than as zeroes nobody measured.
    let registry_summary = repo
        .registry_summary(workspace_id)
        .await
        .map(|summary| RegistrySummary {
            total: summary.total,
            known_venues: summary.known_venues,
            own_rooms: summary.own_rooms,
            known_counterparties: summary.known_counterparties,
            dealt_with: summary.dealt_with,
        })
        .map_err(|error| {
            tracing::warn!(%error, "gdrive registry summary failed");
            error
        })
        .ok();
    let segment_counts = repo
        .segment_counts(workspace_id)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "gdrive segment counts failed");
            error
        })
        .ok();
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(DriveContactsResponse {
            contacts: rows.into_iter().map(contact_json).collect(),
            registry_summary,
            segment_counts,
        }),
    )
        .into_response()
}

/// `POST /v1/control-plane/gdrive/contacts/upload` — `{file_name, csv}`.
///
/// The operator's own spreadsheet is an intake source with the same
/// semantics as the connectors: parse, extract, stage, and let the review
/// queue's one-by-one promote decide each row. The extractor is the same
/// one Drive feeds, so a sheet that yields no email column answers
/// "not a contact list" rather than filing noise.
///
/// `mark_disappeared` stays false: an upload is an addition the operator
/// made, and a partial paste must not retract rows a file still carries.
const UPLOAD_MAX_BYTES: usize = 2 * 1024 * 1024;
/// The staging table is a review queue — past a few thousand rows the
/// honest answer is to split the sheet, not to file it all.
const UPLOAD_MAX_ROWS: usize = 5_000;

#[derive(Deserialize)]
pub struct UploadContactsRequest {
    file_name: String,
    csv: String,
}

pub async fn upload_contacts(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UploadContactsRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let file_name = request.file_name.trim();
    if file_name.is_empty() || file_name.chars().count() > 500 {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    if request.csv.is_empty() || request.csv.len() > UPLOAD_MAX_BYTES {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }

    // Comma first, tab when the comma pass finds no email column — a sheet
    // exported as TSV is the same file wearing a different separator.
    let grid =
        match crowdrelay_domain::drive_contacts::parse_delimited(request.csv.as_bytes(), b',') {
            Ok(grid) => grid,
            Err(_) => {
                return Problem::bad_request(request_id_value)
                    .private()
                    .into_response();
            }
        };
    let mut report = crowdrelay_domain::drive_contacts::extract_contacts(&grid);
    if report.no_email_column
        && let Ok(grid) =
            crowdrelay_domain::drive_contacts::parse_delimited(request.csv.as_bytes(), b'\t')
    {
        report = crowdrelay_domain::drive_contacts::extract_contacts(&grid);
    }
    if report.no_email_column {
        return Problem::conflict_because(
            "No column looked like email — this is not a contact list.",
            request_id_value,
        )
        .private()
        .into_response();
    }
    if report.contacts.len() > UPLOAD_MAX_ROWS {
        return Problem::conflict_because(
            "That sheet is bigger than the review queue can hold — split it and upload in parts.",
            request_id_value,
        )
        .private()
        .into_response();
    }

    let repo = repo(&state);
    match repo
        .upsert_contacts_for_source(
            state.ops.workspace_id().into_uuid(),
            "upload",
            &format!("upload:{file_name}"),
            file_name,
            &report.contacts,
            false,
            // The control-plane upload is operator-initiated — its agent
            // verdicts are trusted the way the synced registry's are.
            true,
        )
        .await
    {
        Ok(summary) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(serde_json::json!({
                "staged": summary.upserted,
                "rows_read": report.rows_read,
                "rows_without_email": report.rows_without_email,
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "gdrive contacts upload failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// POST — wake the contacts worker for an immediate scan. The notify is
/// fire-and-forget: if the worker is down the next periodic sweep covers.
pub async fn scan_now(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    if let Err(error) = sqlx::query("SELECT pg_notify('gdrive_contacts', 'scan')")
        .execute(&state.database)
        .await
    {
        tracing::warn!(%error, "gdrive scan notify failed");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "scan": "requested" })),
    )
        .into_response()
}

/// POST /contacts/{id}/promote — `{destination: "fan"|"beacon", kind?}`.
pub async fn promote_contact(
    State(state): State<crate::AppState>,
    Path(contact_id): Path<uuid::Uuid>,
    headers: HeaderMap,
    payload: Result<Json<PromoteRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = repo(&state);
    let contact = match repo.get_contact(workspace_id, contact_id).await {
        Ok(c) => c,
        Err(_) => return Problem::bad_request(request_id_value).into_response(),
    };

    match request.destination.as_str() {
        "fan" => {
            if contact.fan_outcome != "staged" {
                return Problem::bad_request(request_id_value).into_response();
            }
            let import = crowdrelay_infra::fan_import::PostgresFanImportRepository::new(
                state.database.clone(),
            );
            let entries = [crowdrelay_infra::fan_import::ImportEntry {
                email: contact.normalized_email.clone(),
                display_name: contact.display_name.clone(),
                locale: None,
            }];
            // Attribution follows the contact, not the connector: an
            // address sighted in both Drive and Gmail reads "gdrive+gmail".
            let arrival_source = contact.sources.join("+");
            let counts = match import
                .import_batch(
                    workspace_id,
                    &arrival_source,
                    &entries,
                    ACCESS_TOKEN_TTL_DAYS,
                    ACCESS_RESEND_COOLDOWN_SECONDS,
                )
                .await
            {
                Ok(counts) => counts,
                Err(error) => {
                    tracing::warn!(%error, "gdrive fan promote failed");
                    return Problem::service_unavailable(request_id_value)
                        .private()
                        .into_response();
                }
            };
            // A suppressed address is not promoted, whatever the click said:
            // no opt-in mail leaves for them, and marking the row would lie
            // about what happened.
            if counts.skipped_suppressed > 0 {
                return Problem::conflict_because(
                    "This address is suppressed on the fan list — an earlier complaint or bounce. Nothing was sent, and the row stays staged.",
                    request_id_value,
                )
                .private()
                .into_response();
            }
            if let Err(error) = repo.mark_fan_promoted(workspace_id, contact_id).await {
                tracing::warn!(%error, "gdrive fan outcome mark failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "fan_outcome": "promoted",
                    // The panel's notice says the opt-in mail is on its way —
                    // only true when one actually left or was re-sent. An
                    // already-active fan or a cooldown gets the honest flag.
                    "opt_in_emailed": counts.imported_pending + counts.confirmation_resent > 0,
                })),
            )
                .into_response()
        }
        "beacon" => {
            if contact.beacon_outcome != "staged" {
                return Problem::bad_request(request_id_value).into_response();
            }
            // Same normalisation the file extractor applies — "Venue" and
            // "venue" must file the same way, and an explicit kind the
            // vocabulary does not know is a caller bug, not a press contact.
            let normalized_kind = request
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(|k| k.to_ascii_lowercase().replace([' ', '-'], "_"));
            let kind = match normalized_kind.as_deref() {
                Some(k)
                    if OUTREACH_KINDS.contains(&k)
                        || BOOKING_KINDS.contains(&k)
                        || REPRESENTATION_KINDS.contains(&k)
                        || AGENT_KINDS.contains(&k) =>
                {
                    k.to_owned()
                }
                Some(_) => return Problem::bad_request(request_id_value).into_response(),
                None => contact
                    .suggested_kind
                    .clone()
                    .unwrap_or_else(|| "press".to_owned()),
            };
            let kind = kind.as_str();
            if AGENT_KINDS.contains(&kind) {
                return match repo.promote_beacon_agent(workspace_id, &contact).await {
                    Ok(()) => (
                        StatusCode::OK,
                        Json(serde_json::json!({ "beacon_outcome": "promoted" })),
                    )
                        .into_response(),
                    Err(error) => {
                        tracing::warn!(%error, "gdrive agent promote failed");
                        Problem::service_unavailable(request_id_value)
                            .private()
                            .into_response()
                    }
                };
            }
            if REPRESENTATION_KINDS.contains(&kind) {
                return match repo
                    .promote_beacon_representation(workspace_id, &contact, kind)
                    .await
                {
                    Ok(()) => (
                        StatusCode::OK,
                        Json(serde_json::json!({ "beacon_outcome": "promoted" })),
                    )
                        .into_response(),
                    Err(error) => {
                        tracing::warn!(%error, "gdrive representation promote failed");
                        Problem::service_unavailable(request_id_value)
                            .private()
                            .into_response()
                    }
                };
            }
            // A `suggested_kind` from the sheet is a fan-side vocabulary —
            // "fan" is legal on the staging row but not a beacon kind, and
            // writing it into agent_outreach_targets trips the CHECK. An
            // operator who wants a beacon outcome on a fan-typed contact
            // picks the kind explicitly; the fallback is not guessed.
            if !OUTREACH_KINDS.contains(&kind) && !BOOKING_KINDS.contains(&kind) {
                return Problem::bad_request(request_id_value).into_response();
            }
            if BOOKING_KINDS.contains(&kind) {
                return match repo
                    .promote_beacon_booking(
                        workspace_id,
                        &contact,
                        kind,
                        request.city.as_deref(),
                    )
                    .await
                {
                    Ok(crowdrelay_infra::gdrive::BookingPromoteOutcome::Done) => (
                        StatusCode::OK,
                        Json(serde_json::json!({ "beacon_outcome": "promoted" })),
                    )
                        .into_response(),
                    Ok(crowdrelay_infra::gdrive::BookingPromoteOutcome::CityRequired) => {
                        Problem::conflict_because(
                            "Booking contacts are filed per city — pass `city` or play the room first so the registry can place it.",
                            request_id_value,
                        )
                        .private()
                        .into_response()
                    }
                    Ok(crowdrelay_infra::gdrive::BookingPromoteOutcome::UnknownCity) => {
                        Problem::conflict_because(
                            "That city is not in the catalogue yet — use the slug form, e.g. `wroclaw`.",
                            request_id_value,
                        )
                        .private()
                        .into_response()
                    }
                    Ok(crowdrelay_infra::gdrive::BookingPromoteOutcome::RouteRefused) => {
                        Problem::conflict_because(
                            "A candidate at this address was refused before — the refusal stands.",
                            request_id_value,
                        )
                        .private()
                        .into_response()
                    }
                    Err(error) => {
                        tracing::warn!(%error, "gdrive booking promote failed");
                        Problem::service_unavailable(request_id_value)
                            .private()
                            .into_response()
                    }
                };
            }
            match repo.promote_beacon(workspace_id, &contact, kind).await {
                Ok(()) => (
                    StatusCode::OK,
                    Json(serde_json::json!({ "beacon_outcome": "promoted" })),
                )
                    .into_response(),
                Err(error) => {
                    tracing::warn!(%error, "gdrive beacon promote failed");
                    Problem::service_unavailable(request_id_value)
                        .private()
                        .into_response()
                }
            }
        }
        _ => Problem::bad_request(request_id_value).into_response(),
    }
}

/// POST /contacts/promote-batch — `{destination: "fan", segment:
/// "likely_fan", expected_count, reason?}`.
///
/// One click promotes every staged row in the segment: the import (pending
/// fan + double-opt-in confirmation) and the staging-row marks commit in
/// one transaction, so a mark failure cannot leave pending fans behind
/// rows that still read `staged`. `expected_count` is the number the
/// operator confirmed; a segment that drifted between render and click is
/// a 409 naming both numbers, never a wider send.
#[derive(Deserialize)]
pub struct PromoteBatchRequest {
    destination: String,
    segment: String,
    /// The segment size the operator confirmed. Bounded so a nonsense
    /// number is a 400, not a successful no-op.
    expected_count: i64,
    /// The operator's one-line invitation reason, printed in the
    /// confirmation mail. Trimmed; empty is none.
    reason: Option<String>,
}

const EXPECTED_COUNT_MAX: i64 = 100_000;
const REASON_MAX_CHARS: usize = 200;

pub async fn promote_batch(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<PromoteBatchRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if request.destination != "fan" {
        return Problem::bad_request(request_id_value).into_response();
    }
    if crowdrelay_infra::gdrive::ContactSegment::parse(&request.segment)
        != Some(crowdrelay_infra::gdrive::ContactSegment::LikelyFan)
    {
        // Bulk promote exists for the fan cut only — organisations and
        // beacons stay per-row decisions a person makes.
        return Problem::bad_request(request_id_value).into_response();
    }
    if !(0..=EXPECTED_COUNT_MAX).contains(&request.expected_count) {
        return Problem::bad_request(request_id_value).into_response();
    }
    let reason = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if reason.is_some_and(|line| line.chars().count() > REASON_MAX_CHARS)
        || reason.is_some_and(|line| line.contains('\n'))
    {
        return Problem::bad_request(request_id_value).into_response();
    }

    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = repo(&state);

    // Counted at write time: the number the operator confirmed is checked
    // against the table as it stands now, not the page they looked at.
    let contacts = match repo
        .staged_fan_contacts_in_segment(
            workspace_id,
            crowdrelay_infra::gdrive::ContactSegment::LikelyFan,
        )
        .await
    {
        Ok(contacts) => contacts,
        Err(error) => {
            tracing::warn!(%error, "gdrive segment read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let live = contacts.len() as i64;
    if live != request.expected_count {
        return Problem::conflict_owned(
            std::borrow::Cow::Owned(format!(
                "The segment holds {live} contacts now, you confirmed {expected}. Refresh and confirm again.",
                expected = request.expected_count,
            )),
            request_id_value,
        )
        .private()
        .into_response();
    }

    // The crew's language tag only if the tenant set one — an unmeasured
    // locale reaches the payload as `null`, never a guessed "en".
    let crew_locale =
        crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(state.database.clone())
            .crew_locale_if_set(workspace_id)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "gdrive promote-batch locale read failed");
                error
            })
            .ok()
            .flatten();
    let invitation = crowdrelay_infra::fan_import::InvitationContext {
        locale: crew_locale,
        reason: reason.map(str::to_owned),
    };

    let mut tx = match state.database.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(%error, "gdrive promote-batch could not begin");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    // `import_batch` takes one source per batch; a contact sighted in Drive
    // and Gmail imports as "gdrive+gmail", so the batch is grouped on the
    // joined source string. One transaction holds every group plus the
    // marks — the whole promote commits or none of it does.
    let mut groups: std::collections::BTreeMap<
        String,
        Vec<&crowdrelay_infra::gdrive::StagedFanContact>,
    > = std::collections::BTreeMap::new();
    for contact in &contacts {
        groups
            .entry(contact.sources.join("+"))
            .or_default()
            .push(contact);
    }
    let import =
        crowdrelay_infra::fan_import::PostgresFanImportRepository::new(state.database.clone());
    let mut totals = crowdrelay_infra::fan_import::ImportCounts::default();
    let mut suppressed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (source, group) in &groups {
        let entries: Vec<crowdrelay_infra::fan_import::ImportEntry> = group
            .iter()
            .map(|contact| crowdrelay_infra::fan_import::ImportEntry {
                email: contact.normalized_email.clone(),
                display_name: contact.display_name.clone(),
                locale: None,
            })
            .collect();
        match import
            .import_batch_in_tx(
                &mut tx,
                workspace_id,
                source,
                &entries,
                ACCESS_TOKEN_TTL_DAYS,
                ACCESS_RESEND_COOLDOWN_SECONDS,
                &invitation,
            )
            .await
        {
            Ok(outcome) => {
                totals.imported_pending += outcome.counts.imported_pending;
                totals.confirmation_resent += outcome.counts.confirmation_resent;
                totals.already_active += outcome.counts.already_active;
                totals.skipped_suppressed += outcome.counts.skipped_suppressed;
                totals.cooldown_skipped += outcome.counts.cooldown_skipped;
                suppressed.extend(outcome.suppressed_emails);
            }
            Err(error) => {
                tracing::warn!(%error, "gdrive promote-batch import failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        }
    }

    // A suppressed address is not promoted, whatever the click said — same
    // rule the single promote applies.
    let promotable: Vec<uuid::Uuid> = contacts
        .iter()
        .filter(|contact| !suppressed.contains(contact.normalized_email.as_str()))
        .map(|contact| contact.id)
        .collect();
    let promoted = match repo
        .mark_fans_promoted_by_ids(&mut tx, workspace_id, &promotable)
        .await
    {
        Ok(marked) => marked,
        Err(error) => {
            tracing::warn!(%error, "gdrive promote-batch mark failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, "gdrive promote-batch commit failed");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "promoted": promoted,
            "imported_pending": totals.imported_pending,
            "confirmation_resent": totals.confirmation_resent,
            "already_active": totals.already_active,
            "skipped_suppressed": totals.skipped_suppressed,
            "cooldown_skipped": totals.cooldown_skipped,
        })),
    )
        .into_response()
}

/// POST /contacts/{id}/dismiss — `{destination: "fan"|"beacon"}`. Records
/// the decision so a re-scan does not re-suggest the address for that
/// destination; the other destination stays staged.
pub async fn dismiss_contact(
    State(state): State<crate::AppState>,
    Path(contact_id): Path<uuid::Uuid>,
    headers: HeaderMap,
    payload: Result<Json<DismissRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if request.destination != "fan" && request.destination != "beacon" {
        return Problem::bad_request(request_id_value).into_response();
    }
    let workspace_id = state.ops.workspace_id().into_uuid();
    let repo = repo(&state);
    let contact = match repo.get_contact(workspace_id, contact_id).await {
        Ok(c) => c,
        Err(_) => return Problem::bad_request(request_id_value).into_response(),
    };
    // Dismissing a promoted outcome would erase the record while the fan /
    // outreach row it created still exists — only staged is dismissable.
    let staged = if request.destination == "fan" {
        &contact.fan_outcome
    } else {
        &contact.beacon_outcome
    };
    if staged != "staged" {
        return Problem::bad_request(request_id_value).into_response();
    }
    match repo
        .set_outcome(workspace_id, contact_id, &request.destination, "dismissed")
        .await
    {
        Ok(row) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "fan_outcome": row.fan_outcome,
                "beacon_outcome": row.beacon_outcome,
            })),
        )
            .into_response(),
        Err(_) => Problem::bad_request(request_id_value).into_response(),
    }
}
