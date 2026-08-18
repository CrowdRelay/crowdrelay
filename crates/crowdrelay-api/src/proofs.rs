//! Optional externally anchored proofs for draws and append-only audit ledgers.
//!
//! PostgreSQL remains authoritative. This module only commits deterministic
//! SHA-256 roots to an asynchronous queue. No external log request or signing
//! operation is performed by the API, and a disabled/unavailable anchor never blocks normal
//! CrowdRelay traffic.

use std::future::Future;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, ecosystem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const PUBLIC_PROOF_CACHE: &str = "public, max-age=30, s-maxage=60, stale-while-revalidate=300";
const PUBLIC_DRAW_STATUS_CACHE: &str = "public, max-age=5, s-maxage=10, must-revalidate";
const MAX_AUDIT_BATCH: i64 = 4_096;
const MAX_LIST_LIMIT: i64 = 100;
const DEFAULT_LEASE_SECONDS: i64 = 300;
const MAX_LEASE_SECONDS: i64 = 900;
const MAX_CLAIM_BATCHES: i64 = 16;

#[derive(Debug, FromRow)]
struct BatchRow {
    id: Uuid,
    proof_kind: String,
    schema_version: i32,
    hash_algorithm: String,
    tree_algorithm: String,
    root_sha256: Vec<u8>,
    leaf_count: i32,
    status: String,
    attempts: i32,
    max_attempts: i32,
    available_at: OffsetDateTime,
    anchor_kind: Option<String>,
    anchor_url: Option<String>,
    anchor_entry_id: Option<String>,
    anchor_sequence: Option<i64>,
    anchor_integrated_at: Option<OffsetDateTime>,
    anchor_log_id: Option<String>,
    anchor_receipt: Option<Value>,
    signer_fingerprint: Option<String>,
    signed_payload_sha256: Option<Vec<u8>>,
    last_error_kind: Option<String>,
    lock_owner: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    confirmed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct ProofBatchView {
    id: Uuid,
    proof_kind: String,
    schema_version: i32,
    hash_algorithm: String,
    tree_algorithm: String,
    root_sha256: String,
    leaf_count: i32,
    status: String,
    attempts: i32,
    max_attempts: i32,
    #[serde(with = "time::serde::rfc3339")]
    available_at: OffsetDateTime,
    anchor_kind: Option<String>,
    anchor_url: Option<String>,
    anchor_entry_id: Option<String>,
    anchor_sequence: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    anchor_integrated_at: Option<OffsetDateTime>,
    anchor_log_id: Option<String>,
    anchor_receipt: Option<Value>,
    signer_fingerprint: Option<String>,
    signed_payload_sha256: Option<String>,
    last_error_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    confirmed_at: Option<OffsetDateTime>,
}

impl TryFrom<BatchRow> for ProofBatchView {
    type Error = ProofError;

    fn try_from(row: BatchRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            proof_kind: row.proof_kind,
            schema_version: row.schema_version,
            hash_algorithm: row.hash_algorithm,
            tree_algorithm: row.tree_algorithm,
            root_sha256: encode_hash(&row.root_sha256)?,
            leaf_count: row.leaf_count,
            status: row.status,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            available_at: row.available_at,
            anchor_kind: row.anchor_kind,
            anchor_url: row.anchor_url,
            anchor_entry_id: row.anchor_entry_id,
            anchor_sequence: row.anchor_sequence,
            anchor_integrated_at: row.anchor_integrated_at,
            anchor_log_id: row.anchor_log_id,
            anchor_receipt: row.anchor_receipt,
            signer_fingerprint: row.signer_fingerprint,
            signed_payload_sha256: row
                .signed_payload_sha256
                .as_deref()
                .map(encode_hash)
                .transpose()?,
            last_error_kind: row.last_error_kind,
            created_at: row.created_at,
            updated_at: row.updated_at,
            confirmed_at: row.confirmed_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct LedgerRow {
    source_kind: String,
    source_id: Uuid,
    occurred_at: OffsetDateTime,
    canonical: String,
}

#[derive(Debug, FromRow)]
struct ProofItemRow {
    sequence: i32,
    source_kind: String,
    source_id: Uuid,
    leaf_sha256: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct AuditBatchResult {
    batch: Option<ProofBatchView>,
    replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAuditBatchRequest {
    limit: Option<i64>,
    #[serde(default)]
    canary: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListBatchesQuery {
    limit: Option<i64>,
    status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InclusionProof {
    batch: ProofBatchView,
    source_kind: String,
    source_id: Uuid,
    sequence: i32,
    leaf_sha256: String,
    proof: Vec<MerkleStep>,
    verified: bool,
}

#[derive(Debug, Serialize)]
pub struct MerkleStep {
    side: &'static str,
    sha256: String,
}

#[derive(Debug, FromRow)]
struct DrawProofRow {
    draw_slug: String,
    draw_name: String,
    run_id: Uuid,
    algorithm_version: String,
    seed_hash: Vec<u8>,
    revealed_seed_hex: String,
    eligible_count: i32,
    total_entries: i64,
    requested_winners: i32,
    selected_winners: i32,
    receipt_sha256: Vec<u8>,
    candidate_snapshot_sha256: Vec<u8>,
    winner_snapshot_sha256: Vec<u8>,
    batch_id: Uuid,
    batch_status: String,
    anchor_kind: Option<String>,
    anchor_url: Option<String>,
    anchor_entry_id: Option<String>,
    anchor_sequence: Option<i64>,
    anchor_integrated_at: Option<OffsetDateTime>,
    anchor_log_id: Option<String>,
    anchor_receipt: Option<Value>,
    signer_fingerprint: Option<String>,
    signed_payload_sha256: Option<Vec<u8>>,
    confirmed_at: Option<OffsetDateTime>,
    completed_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct DrawStatusRow {
    draw_slug: String,
    draw_name: String,
    status: String,
    draw_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
    proof_available: bool,
}

#[derive(Debug, Serialize)]
pub struct PublicDrawStatus {
    schema: &'static str,
    draw_slug: String,
    draw_name: String,
    status: String,
    #[serde(with = "time::serde::rfc3339")]
    draw_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
    proof_available: bool,
}

#[derive(Debug, Serialize)]
pub struct PublicDrawProof {
    schema: &'static str,
    draw_slug: String,
    draw_name: String,
    run_id: Uuid,
    algorithm_version: String,
    seed_hash_sha256: String,
    revealed_seed_hex: String,
    eligible_count: i32,
    total_entries: i64,
    requested_winners: i32,
    selected_winners: i32,
    candidate_snapshot_sha256: String,
    winner_snapshot_sha256: String,
    receipt_sha256: String,
    locally_verified: bool,
    anchor: PublicAnchor,
    #[serde(with = "time::serde::rfc3339")]
    completed_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct PublicAnchor {
    batch_id: Uuid,
    status: String,
    anchor_kind: Option<String>,
    anchor_url: Option<String>,
    entry_id: Option<String>,
    sequence: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    integrated_at: Option<OffsetDateTime>,
    log_id: Option<String>,
    receipt: Option<Value>,
    signer_fingerprint: Option<String>,
    signed_payload_sha256: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    confirmed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    worker_id: String,
    lease_seconds: Option<i64>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    batches: Vec<RelayerBatch>,
}

#[derive(Debug, Serialize)]
pub struct RelayerBatch {
    id: Uuid,
    proof_kind: String,
    schema_version: i32,
    root_sha256: String,
    leaf_count: i32,
    tree_algorithm: String,
    attempt: i32,
    #[serde(with = "time::serde::rfc3339")]
    lease_expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    worker_id: String,
    anchor_kind: String,
    anchor_url: String,
    entry_uuid: String,
    log_index: i64,
    integrated_time: i64,
    log_id: String,
    canonicalized_body: String,
    signed_entry_timestamp: String,
    inclusion_proof: Value,
    signer_fingerprint: String,
    signature_base64: String,
    public_key_pem: String,
    payload_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailRequest {
    worker_id: String,
    error_kind: String,
}

#[derive(Debug, FromRow)]
struct ExistingAction {
    action: String,
    target_id: Uuid,
    details: Value,
}

include!("proofs/admin_and_public.rs");
include!("proofs/read_support.rs");

include!("proofs/relayer.rs");
include!("proofs/support.rs");
