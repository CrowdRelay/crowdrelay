//! Public-source Latarnik discovery and reviewed invitation delivery.
//!
//! Discovery may create *candidate* Beacon rows only. Human review is the sole
//! path that can set `verified` + `accepts_outreach`. Raw Signal invite URLs are
//! minted only inside a one-shot internal claim and never enter the outbox.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use super::releases::{
    OperatorActionRecord, executor_capability_available_tx, record_operator_action,
};
use super::*;

mod admin;
mod internal;

pub use admin::{admin_beacon_network, admin_beacon_network_action};
pub use internal::{
    internal_claim_invite_delivery_job, internal_ingest_discovered_beacons,
    internal_report_discovery_run, internal_report_invite_delivery_job,
};

const MAX_DISCOVERY_TARGET: i32 = 500;
const MAX_DISCOVERY_BATCH: usize = 200;
const MAX_INVITE_BATCH: usize = 200;
const MAX_EVIDENCE_LEN: usize = 1000;
const MAX_REPORT_FILENAME_LEN: usize = 240;
const MAX_FAILURE_KIND_LEN: usize = 96;

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct DiscoveryRunView {
    id: Uuid,
    country_code: String,
    target_count: i32,
    status: String,
    discovered_count: i32,
    report_filename: Option<String>,
    report_sha256: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    requested_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
    failure_kind: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct DiscoveredBeaconView {
    id: Uuid,
    display_name: String,
    beacon_kind: String,
    contact_email: Option<String>,
    destination_url: Option<String>,
    source_url: Option<String>,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    metadata: Value,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct InviteJobView {
    id: Uuid,
    status: String,
    beacon_count: i32,
    ttl_days: i32,
    radius_km: i32,
    locale: String,
    claimed_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    claimed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    claim_expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    reported_at: Option<OffsetDateTime>,
    provider_summary: Value,
    exchanged_count: i64,
    web_count: i64,
    android_count: i64,
    ios_count: i64,
    active_count: i64,
    push_enabled_count: i64,
    helping_count: i64,
    coverage_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BeaconNetworkResponse {
    discovery_runs: Vec<DiscoveryRunView>,
    pending_candidates: Vec<DiscoveredBeaconView>,
    approved_candidates: Vec<DiscoveredBeaconView>,
    invite_jobs: Vec<InviteJobView>,
    /// Researched contacts not yet on the roster.
    ///
    /// Without it the Import button is a dare: press it and find out. With it
    /// the console can say how many contacts are waiting, and go quiet once
    /// there are none left to bring over.
    researched_available: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdminNetworkActionRequest {
    action: String,
    country_code: Option<String>,
    target_count: Option<i32>,
    beacon_id: Option<Uuid>,
    beacon_ids: Option<Vec<Uuid>>,
    source_verified: Option<bool>,
    marketing_email_consent_confirmed: Option<bool>,
    consent_evidence_url: Option<String>,
    ttl_days: Option<i32>,
    radius_km: Option<i32>,
    locale: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DiscoveredCandidate {
    beacon_kind: String,
    display_name: String,
    contact_email: Option<String>,
    destination_url: Option<String>,
    source_url: String,
    city_id: Option<Uuid>,
    source_note: Option<String>,
    relevance_basis_points: Option<i32>,
    confidence_basis_points: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IngestDiscoveryRequest {
    candidates: Vec<DiscoveredCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DiscoveryReportRequest {
    status: String,
    discovered_count: i32,
    report_filename: Option<String>,
    report_sha256: Option<String>,
    failure_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InviteJobClaimRequest {
    worker_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InviteJobClaimResponse {
    version: u8,
    job_id: Uuid,
    claim_token: String,
    batch: lifecycle::BatchInviteResponse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InviteJobReportRequest {
    claim_token: String,
    status: String,
    #[serde(default)]
    provider_summary: Value,
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("idempotency-key")?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 200 {
        None
    } else {
        Some(value.to_owned())
    }
}

fn clean_text(value: &str, max_len: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_len {
        None
    } else {
        Some(value.to_owned())
    }
}

fn valid_email(value: &str) -> bool {
    let value = value.trim();
    value.len() <= 320
        && !value.bytes().any(|byte| byte.is_ascii_whitespace())
        && value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
}

fn valid_https_url(value: &str) -> bool {
    value.len() <= 2048
        && Url::parse(value)
            .ok()
            .is_some_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_beacon_kind(value: &str) -> bool {
    matches!(
        value,
        "radio"
            | "local_press"
            | "television"
            | "reviewer"
            | "creator"
            | "photographer"
            | "promoter"
            | "patron"
            | "community"
    )
}

fn valid_locale(value: &str) -> bool {
    match value.as_bytes() {
        [language_0, language_1] => {
            language_0.is_ascii_lowercase() && language_1.is_ascii_lowercase()
        }
        [language_0, language_1, b'-', region_0, region_1] => {
            language_0.is_ascii_lowercase()
                && language_1.is_ascii_lowercase()
                && region_0.is_ascii_uppercase()
                && region_1.is_ascii_uppercase()
        }
        _ => false,
    }
}
