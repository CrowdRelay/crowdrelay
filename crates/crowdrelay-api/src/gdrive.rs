//! Google Drive contacts — control-plane review surface.
//!
//! Every address the Drive connector extracts lands staged in
//! `viryaos_drive_contacts`. Nothing here auto-classifies: the operator
//! promotes or dismisses per destination, and `fan_outcome` /
//! `beacon_outcome` stay independent because a beacon may also be a fan.
//!
//! - `promote: fan` routes through `fan_import::import_batch` — the same
//!   pending + double-opt-in path every fan import uses. A spreadsheet can
//!   never create an `active` fan.
//! - `promote: beacon` writes a `proposed` `agent_outreach_targets` row —
//!   the screening queue, same as the curated-CRM import.
//! - `dismiss` records the decision so a re-scan never re-suggests.

use axum::{
    Json,
    extract::{Path, State},
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
const TARGET_KINDS: &[&str] = &[
    "press",
    "radio",
    "playlist",
    "media_patronage",
    "endorsement",
    "creator",
];

#[derive(Serialize)]
pub struct DriveContact {
    id: String,
    email: String,
    display_name: Option<String>,
    organization: Option<String>,
    suggested_kind: Option<String>,
    notes: Option<String>,
    source_file_name: String,
    sources: Vec<String>,
    last_seen_at: String,
    gone_from_source: bool,
    fan_outcome: String,
    beacon_outcome: String,
}

#[derive(Serialize)]
pub struct DriveContactsResponse {
    contacts: Vec<DriveContact>,
}

#[derive(Deserialize)]
pub struct PromoteRequest {
    destination: String,
    /// Beacon target kind. Falls back to the file's `suggested_kind`,
    /// then to `press` when the file said nothing.
    kind: Option<String>,
}

#[derive(Deserialize)]
pub struct DismissRequest {
    destination: String,
}

fn contact_json(row: crowdrelay_infra::gdrive::DriveContactRow) -> DriveContact {
    DriveContact {
        id: row.id.to_string(),
        email: row.normalized_email,
        display_name: row.display_name,
        organization: row.organization,
        suggested_kind: row.suggested_kind,
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
    }
}

fn repo(state: &crate::AppState) -> crowdrelay_infra::gdrive::PostgresGDriveRepository {
    crowdrelay_infra::gdrive::PostgresGDriveRepository::new(state.database.clone())
}

/// GET — the review queue, staged-first.
pub async fn list_contacts(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match repo(&state).list_contacts(workspace_id, LIST_LIMIT).await {
        Ok(rows) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(DriveContactsResponse {
                contacts: rows.into_iter().map(contact_json).collect(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "gdrive contacts list failed");
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
            match import
                .import_batch(
                    workspace_id,
                    &arrival_source,
                    &entries,
                    ACCESS_TOKEN_TTL_DAYS,
                    ACCESS_RESEND_COOLDOWN_SECONDS,
                )
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "gdrive fan promote failed");
                    return Problem::service_unavailable(request_id_value)
                        .private()
                        .into_response();
                }
            }
            if let Err(error) = repo.mark_fan_promoted(workspace_id, contact_id).await {
                tracing::warn!(%error, "gdrive fan outcome mark failed");
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({ "fan_outcome": "promoted" })),
            )
                .into_response()
        }
        "beacon" => {
            if contact.beacon_outcome != "staged" {
                return Problem::bad_request(request_id_value).into_response();
            }
            let kind = request
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .or(contact.suggested_kind.as_deref())
                .unwrap_or("press");
            let kind = if TARGET_KINDS.contains(&kind) {
                kind
            } else {
                "press"
            };
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
