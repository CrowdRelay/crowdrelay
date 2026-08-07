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
    available_at: OffsetDateTime,
    anchor_kind: Option<String>,
    anchor_url: Option<String>,
    anchor_entry_id: Option<String>,
    anchor_sequence: Option<i64>,
    anchor_integrated_at: Option<OffsetDateTime>,
    anchor_log_id: Option<String>,
    anchor_receipt: Option<Value>,
    signer_fingerprint: Option<String>,
    signed_payload_sha256: Option<String>,
    last_error_kind: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
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

pub async fn admin_list_batches(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListBatchesQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_LIST_LIMIT).contains(&limit)
        || query.status.as_deref().is_some_and(|value| {
            !matches!(
                value,
                "queued" | "processing" | "confirmed" | "failed" | "dead"
            )
        })
    {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = async {
        let workspace_id = state.ticketing.workspace_id().into_uuid();
        let rows = if let Some(status) = query.status.as_deref() {
            sqlx::query_as::<_, BatchRow>(
                r#"
                SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
                       root_sha256, leaf_count, status, attempts, max_attempts,
                       available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner, created_at, updated_at, confirmed_at
                FROM external_proof_batches
                WHERE workspace_id = $1 AND status = $2
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#,
            )
            .bind(workspace_id)
            .bind(status)
            .bind(limit)
            .fetch_all(state.ticketing.pool())
            .await
            .map_err(ProofError::sqlx)?
        } else {
            sqlx::query_as::<_, BatchRow>(
                r#"
                SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
                       root_sha256, leaf_count, status, attempts, max_attempts,
                       available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner, created_at, updated_at, confirmed_at
                FROM external_proof_batches
                WHERE workspace_id = $1
                ORDER BY created_at DESC, id DESC
                LIMIT $2
                "#,
            )
            .bind(workspace_id)
            .bind(limit)
            .fetch_all(state.ticketing.pool())
            .await
            .map_err(ProofError::sqlx)?
        };
        rows.into_iter()
            .map(ProofBatchView::try_from)
            .collect::<Result<Vec<ProofBatchView>, ProofError>>()
    };
    respond_private(run(&state, future).await, request_id(&headers))
}

pub async fn admin_create_audit_batch(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateAuditBatchRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    let limit = payload.limit.unwrap_or(1_024);
    if !(1..=MAX_AUDIT_BATCH).contains(&limit) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let result = create_audit_batch(&state, &headers, limit);
    respond_private(run(&state, result).await, request_id_value)
}

pub async fn public_batch(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let future = load_batch(&state, batch_id);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn public_inclusion(
    State(state): State<crate::AppState>,
    Path((batch_id, source_kind, source_id)): Path<(Uuid, String, Uuid)>,
    headers: HeaderMap,
) -> Response {
    if !matches!(
        source_kind.as_str(),
        "audit_event" | "operator_action" | "reward_draw_run"
    ) {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = inclusion_proof(&state, batch_id, &source_kind, source_id);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn public_draw_status(
    State(state): State<crate::AppState>,
    Path(draw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if draw_slug.is_empty() || draw_slug.len() > 128 {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = load_draw_status(&state, &draw_slug);
    respond_public_status(run(&state, future).await, request_id(&headers))
}

pub async fn public_draw(
    State(state): State<crate::AppState>,
    Path(draw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if draw_slug.is_empty() || draw_slug.len() > 128 {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = load_draw_proof(&state, &draw_slug);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn internal_claim(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_worker_id(&payload.worker_id) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let lease_seconds = payload.lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS);
    if !(30..=MAX_LEASE_SECONDS).contains(&lease_seconds) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let limit = payload.limit.unwrap_or(MAX_CLAIM_BATCHES);
    if !(1..=MAX_CLAIM_BATCHES).contains(&limit) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = claim_batches(&state, &payload.worker_id, lease_seconds, limit);
    respond_private(run(&state, future).await, request_id_value)
}

pub async fn internal_confirm(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_confirm(&payload) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = confirm_batch(&state, batch_id, payload, request_id_value.as_deref());
    respond_private(run(&state, future).await, request_id_value)
}

pub async fn internal_fail(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<FailRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_worker_id(&payload.worker_id)
        || payload.error_kind.is_empty()
        || payload.error_kind.len() > 96
        || !payload
            .error_kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = fail_batch(&state, batch_id, &payload, request_id_value.as_deref());
    respond_private(run(&state, future).await, request_id_value)
}

async fn create_audit_batch(
    state: &crate::AppState,
    headers: &HeaderMap,
    limit: i64,
) -> Result<AuditBatchResult, ProofError> {
    let idempotency_key = idempotency_key(headers)?;
    let request_id_value = request_id(headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{workspace_id}:external-proof-audit"))
        .execute(&mut *tx)
        .await
        .map_err(ProofError::sqlx)?;

    if let Some(existing) = sqlx::query_as::<_, ExistingAction>(
        r#"
        SELECT action, target_id, details
        FROM operator_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    {
        if existing.action != "external_proof.audit_batch_created"
            || existing.details.get("limit").and_then(Value::as_i64) != Some(limit)
        {
            return Err(ProofError::Conflict);
        }
        if existing.details.get("empty").and_then(Value::as_bool) == Some(true) {
            tx.commit().await.map_err(ProofError::sqlx)?;
            return Ok(AuditBatchResult {
                batch: None,
                replayed: true,
            });
        }
        let batch = load_batch_tx(&mut tx, workspace_id, existing.target_id).await?;
        tx.commit().await.map_err(ProofError::sqlx)?;
        return Ok(AuditBatchResult {
            batch: Some(batch),
            replayed: true,
        });
    }

    let rows = sqlx::query_as::<_, LedgerRow>(
        r#"
        WITH candidates AS (
            SELECT 'audit_event'::text AS source_kind,
                   audit.id AS source_id,
                   audit.occurred_at,
                   jsonb_build_array(
                       'crowdrelay/audit-event/v1', audit.id, audit.actor_kind,
                       audit.actor_member_id, audit.action, audit.target_type,
                       audit.target_id, audit.request_id, audit.metadata,
                       audit.occurred_at
                   )::text AS canonical
            FROM audit_events AS audit
            WHERE audit.workspace_id = $1
              AND audit.action NOT LIKE 'external_proof.%'
              AND NOT EXISTS (
                  SELECT 1 FROM external_proof_items AS item
                  WHERE item.workspace_id = audit.workspace_id
                    AND item.source_kind = 'audit_event'
                    AND item.source_id = audit.id
              )
            UNION ALL
            SELECT 'operator_action'::text,
                   action.id,
                   action.created_at,
                   jsonb_build_array(
                       'crowdrelay/operator-action/v1', action.id, action.action,
                       action.target_type, action.target_id, action.actor_type,
                       action.idempotency_key, action.request_id, action.details,
                       action.created_at
                   )::text
            FROM operator_actions AS action
            WHERE action.workspace_id = $1
              AND action.action NOT LIKE 'external_proof.%'
              AND NOT EXISTS (
                  SELECT 1 FROM external_proof_items AS item
                  WHERE item.workspace_id = action.workspace_id
                    AND item.source_kind = 'operator_action'
                    AND item.source_id = action.id
              )
        )
        SELECT source_kind, source_id, occurred_at, canonical
        FROM candidates
        ORDER BY occurred_at, source_kind, source_id
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    if rows.is_empty() {
        let no_op_id = Uuid::now_v7();
        append_operator_action(
            &mut tx,
            workspace_id,
            "external_proof.audit_batch_created",
            no_op_id,
            &idempotency_key,
            request_id_value.as_deref(),
            json!({"limit": limit, "empty": true}),
        )
        .await?;
        append_audit(
            &mut tx,
            workspace_id,
            "external_proof.audit_batch_empty",
            no_op_id,
            request_id_value.as_deref(),
            json!({"limit": limit}),
        )
        .await?;
        tx.commit().await.map_err(ProofError::sqlx)?;
        return Ok(AuditBatchResult {
            batch: None,
            replayed: false,
        });
    }

    let leaves: Vec<[u8; 32]> = rows
        .iter()
        .map(|row| leaf_hash(row.canonical.as_bytes()))
        .collect();
    let root = merkle_root(&leaves).ok_or(ProofError::Unexpected)?;
    let batch_id = Uuid::now_v7();
    let leaf_count = i32::try_from(leaves.len()).map_err(|_| ProofError::Unexpected)?;

    sqlx::query(
        r#"
        INSERT INTO external_proof_batches (
            id, workspace_id, proof_kind, schema_version, tree_algorithm,
            root_sha256, leaf_count, request_id
        ) VALUES ($1, $2, 'audit_ledger', 1, 'binary-duplicate-last-v1', $3, $4, $5)
        "#,
    )
    .bind(batch_id)
    .bind(workspace_id)
    .bind(root.to_vec())
    .bind(leaf_count)
    .bind(request_id_value.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    let sequences: Vec<i32> = (0..leaf_count).collect();
    let source_kinds: Vec<String> = rows.iter().map(|row| row.source_kind.clone()).collect();
    let source_ids: Vec<Uuid> = rows.iter().map(|row| row.source_id).collect();
    let leaf_values: Vec<Vec<u8>> = leaves.iter().map(|leaf| leaf.to_vec()).collect();
    let occurred: Vec<OffsetDateTime> = rows.iter().map(|row| row.occurred_at).collect();
    sqlx::query(
        r#"
        INSERT INTO external_proof_items (
            workspace_id, batch_id, sequence, source_kind,
            source_id, leaf_sha256, occurred_at
        )
        SELECT $1, $2, item.sequence, item.source_kind,
               item.source_id, item.leaf_sha256, item.occurred_at
        FROM unnest(
            $3::integer[], $4::text[], $5::uuid[], $6::bytea[], $7::timestamptz[]
        ) AS item(sequence, source_kind, source_id, leaf_sha256, occurred_at)
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(sequences)
    .bind(source_kinds)
    .bind(source_ids)
    .bind(leaf_values)
    .bind(occurred)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    append_operator_action(
        &mut tx,
        workspace_id,
        "external_proof.audit_batch_created",
        batch_id,
        &idempotency_key,
        request_id_value.as_deref(),
        json!({"limit": limit, "empty": false, "leaf_count": leaf_count, "root_sha256": hex::encode(root)}),
    )
    .await?;
    append_audit(
        &mut tx,
        workspace_id,
        "external_proof.audit_batch_created",
        batch_id,
        request_id_value.as_deref(),
        json!({"leaf_count": leaf_count, "root_sha256": hex::encode(root)}),
    )
    .await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    let batch = load_batch(state, batch_id).await?;
    Ok(AuditBatchResult {
        batch: Some(batch),
        replayed: false,
    })
}

async fn load_batch(state: &crate::AppState, batch_id: Uuid) -> Result<ProofBatchView, ProofError> {
    let row = load_batch_row(state, batch_id).await?;
    ProofBatchView::try_from(row)
}

async fn load_batch_row(state: &crate::AppState, batch_id: Uuid) -> Result<BatchRow, ProofError> {
    sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(batch_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)
}

async fn load_batch_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    batch_id: Uuid,
) -> Result<ProofBatchView, ProofError> {
    let row = sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;
    ProofBatchView::try_from(row)
}

async fn inclusion_proof(
    state: &crate::AppState,
    batch_id: Uuid,
    source_kind: &str,
    source_id: Uuid,
) -> Result<InclusionProof, ProofError> {
    let batch = load_batch(state, batch_id).await?;
    let items = sqlx::query_as::<_, ProofItemRow>(
        r#"
        SELECT sequence, source_kind, source_id, leaf_sha256
        FROM external_proof_items
        WHERE workspace_id = $1 AND batch_id = $2
        ORDER BY sequence
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(batch_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?;
    let target = items
        .iter()
        .position(|item| item.source_kind == source_kind && item.source_id == source_id)
        .ok_or(ProofError::NotFound)?;
    let leaves: Vec<[u8; 32]> = items
        .iter()
        .map(|item| hash_array(&item.leaf_sha256))
        .collect::<Result<_, _>>()?;
    let root = merkle_root(&leaves).ok_or(ProofError::Unexpected)?;
    let expected_root = decode_hash(&batch.root_sha256)?;
    let proof = merkle_path(&leaves, target)?;
    let leaf = leaves.get(target).copied().ok_or(ProofError::Unexpected)?;
    let sequence = items
        .get(target)
        .map(|item| item.sequence)
        .ok_or(ProofError::Unexpected)?;
    let verified = verify_path(leaf, target, &proof, expected_root) && root == expected_root;
    Ok(InclusionProof {
        batch,
        source_kind: source_kind.to_owned(),
        source_id,
        sequence,
        leaf_sha256: hex::encode(leaf),
        proof: proof
            .into_iter()
            .map(|step| MerkleStep {
                side: if step.sibling_left { "left" } else { "right" },
                sha256: hex::encode(step.hash),
            })
            .collect(),
        verified,
    })
}

async fn load_draw_status(
    state: &crate::AppState,
    draw_slug: &str,
) -> Result<PublicDrawStatus, ProofError> {
    let row = sqlx::query_as::<_, DrawStatusRow>(
        r#"
        SELECT draw.slug AS draw_slug,
               draw.name AS draw_name,
               draw.status,
               draw.draw_at,
               draw.completed_at,
               EXISTS (
                   SELECT 1
                   FROM reward_draw_proofs AS proof
                   WHERE proof.workspace_id = draw.workspace_id
                     AND proof.draw_id = draw.id
               ) AS proof_available
        FROM reward_draws AS draw
        WHERE draw.workspace_id = $1 AND draw.slug = $2
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(draw_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    Ok(PublicDrawStatus {
        schema: "crowdrelay/draw-status/v1",
        draw_slug: row.draw_slug,
        draw_name: row.draw_name,
        status: row.status,
        draw_at: row.draw_at,
        completed_at: row.completed_at,
        proof_available: row.proof_available,
    })
}

async fn load_draw_proof(
    state: &crate::AppState,
    draw_slug: &str,
) -> Result<PublicDrawProof, ProofError> {
    let row = sqlx::query_as::<_, DrawProofRow>(
        r#"
        SELECT draw.slug AS draw_slug,
               draw.name AS draw_name,
               run.id AS run_id,
               run.algorithm_version,
               run.seed_hash,
               run.revealed_seed_hex,
               run.eligible_count,
               run.total_entries,
               run.requested_winners,
               run.selected_winners,
               proof.receipt_sha256,
               proof.candidate_snapshot_sha256,
               proof.winner_snapshot_sha256,
               batch.id AS batch_id,
               batch.status AS batch_status,
               batch.anchor_kind,
               batch.anchor_url,
               batch.anchor_entry_id,
               batch.anchor_sequence,
               batch.anchor_integrated_at,
               batch.anchor_log_id,
               batch.anchor_receipt,
               batch.signer_fingerprint,
               batch.signed_payload_sha256,
               batch.confirmed_at,
               run.completed_at
        FROM reward_draws AS draw
        JOIN reward_draw_runs AS run
          ON run.workspace_id = draw.workspace_id AND run.draw_id = draw.id
        JOIN reward_draw_proofs AS proof
          ON proof.workspace_id = run.workspace_id AND proof.run_id = run.id
        JOIN external_proof_batches AS batch
          ON batch.workspace_id = proof.workspace_id AND batch.id = proof.anchor_batch_id
        WHERE draw.workspace_id = $1 AND draw.slug = $2
          AND run.status = 'completed'
        ORDER BY run.completed_at DESC, run.id DESC
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(draw_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    let receipt = draw_receipt_hash(
        row.run_id,
        &row.algorithm_version,
        &row.seed_hash,
        &row.revealed_seed_hex,
        row.eligible_count,
        row.total_entries,
        row.requested_winners,
        row.selected_winners,
        &row.candidate_snapshot_sha256,
        &row.winner_snapshot_sha256,
    )?;
    let stored = hash_array(&row.receipt_sha256)?;
    Ok(PublicDrawProof {
        schema: "crowdrelay/draw-receipt/v1",
        draw_slug: row.draw_slug,
        draw_name: row.draw_name,
        run_id: row.run_id,
        algorithm_version: row.algorithm_version,
        seed_hash_sha256: encode_hash(&row.seed_hash)?,
        revealed_seed_hex: row.revealed_seed_hex,
        eligible_count: row.eligible_count,
        total_entries: row.total_entries,
        requested_winners: row.requested_winners,
        selected_winners: row.selected_winners,
        candidate_snapshot_sha256: encode_hash(&row.candidate_snapshot_sha256)?,
        winner_snapshot_sha256: encode_hash(&row.winner_snapshot_sha256)?,
        receipt_sha256: hex::encode(stored),
        locally_verified: receipt == stored,
        anchor: PublicAnchor {
            batch_id: row.batch_id,
            status: row.batch_status,
            anchor_kind: row.anchor_kind,
            anchor_url: row.anchor_url,
            entry_id: row.anchor_entry_id,
            sequence: row.anchor_sequence,
            integrated_at: row.anchor_integrated_at,
            log_id: row.anchor_log_id,
            receipt: row.anchor_receipt,
            signer_fingerprint: row.signer_fingerprint,
            signed_payload_sha256: row
                .signed_payload_sha256
                .as_deref()
                .map(encode_hash)
                .transpose()?,
            confirmed_at: row.confirmed_at,
        },
        completed_at: row.completed_at,
    })
}

async fn claim_batches(
    state: &crate::AppState,
    worker_id: &str,
    lease_seconds: i64,
    limit: i64,
) -> Result<ClaimResponse, ProofError> {
    if !matches!(
        ecosystem::feature_enabled(state, "external_proof_anchoring_enabled").await,
        Ok(true)
    ) {
        return Ok(ClaimResponse {
            batches: Vec::new(),
        });
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    sqlx::query(
        r#"
        WITH recovered AS (
            UPDATE external_proof_batches
            SET status = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'failed' END,
                locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
                available_at = CASE WHEN attempts >= max_attempts THEN available_at ELSE now() END,
                last_error_kind = COALESCE(last_error_kind, 'lease_expired')
            WHERE workspace_id = $1 AND status = 'processing'
              AND lease_expires_at <= now()
            RETURNING id, proof_kind, root_sha256, attempts, status, last_error_kind
        )
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        )
        SELECT $1, 'blockchain.proof.dead', 1,
               jsonb_build_object(
                   'batch_id', id,
                   'proof_kind', proof_kind,
                   'root_sha256', encode(root_sha256, 'hex'),
                   'attempts', attempts,
                   'error_kind', last_error_kind
               ),
               id::text
        FROM recovered
        WHERE status = 'dead'
        "#,
    )
    .bind(workspace_id)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    #[derive(FromRow)]
    struct Claimed {
        id: Uuid,
        proof_kind: String,
        schema_version: i32,
        root_sha256: Vec<u8>,
        leaf_count: i32,
        tree_algorithm: String,
        attempts: i32,
        lease_expires_at: OffsetDateTime,
    }
    let row = sqlx::query_as::<_, Claimed>(
        r#"
        WITH candidate AS (
            SELECT id
            FROM external_proof_batches
            WHERE workspace_id = $1
              AND status IN ('queued', 'failed')
              AND available_at <= now()
              AND attempts < max_attempts
            ORDER BY available_at, created_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT $4
        )
        UPDATE external_proof_batches AS batch
        SET status = 'processing', attempts = attempts + 1,
            locked_at = now(), lock_owner = $2,
            lease_expires_at = now() + make_interval(secs => $3::double precision),
            last_error_kind = NULL
        FROM candidate
        WHERE batch.workspace_id = $1 AND batch.id = candidate.id
        RETURNING batch.id, batch.proof_kind, batch.schema_version,
                  batch.root_sha256, batch.leaf_count, batch.tree_algorithm,
                  batch.attempts, batch.lease_expires_at
        "#,
    )
    .bind(workspace_id)
    .bind(worker_id)
    .bind(lease_seconds)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    let batches = row
        .into_iter()
        .map(|row| {
            Ok(RelayerBatch {
                id: row.id,
                proof_kind: row.proof_kind,
                schema_version: row.schema_version,
                root_sha256: encode_hash(&row.root_sha256)?,
                leaf_count: row.leaf_count,
                tree_algorithm: row.tree_algorithm,
                attempt: row.attempts,
                lease_expires_at: row.lease_expires_at,
            })
        })
        .collect::<Result<Vec<_>, ProofError>>()?;
    Ok(ClaimResponse { batches })
}

async fn confirm_batch(
    state: &crate::AppState,
    batch_id: Uuid,
    payload: ConfirmRequest,
    request_id_value: Option<&str>,
) -> Result<ProofBatchView, ProofError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let entry_id = payload.entry_uuid.to_ascii_lowercase();
    let log_id = payload.log_id.to_ascii_lowercase();
    let signer_fingerprint = payload.signer_fingerprint.to_ascii_lowercase();
    let signed_payload =
        hex::decode(&payload.payload_sha256).map_err(|_| ProofError::BadRequest)?;
    let integrated_at = OffsetDateTime::from_unix_timestamp(payload.integrated_time)
        .map_err(|_| ProofError::BadRequest)?;
    let receipt = json!({
        "canonicalized_body": &payload.canonicalized_body,
        "signed_entry_timestamp": &payload.signed_entry_timestamp,
        "inclusion_proof": &payload.inclusion_proof,
        "signature_base64": &payload.signature_base64,
        "public_key_pem": &payload.public_key_pem,
    });

    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    let existing = sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
               anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
               signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    let expected_payload = proof_anchor_payload(&existing)?;
    let expected_signed_payload: [u8; 32] = Sha256::digest(expected_payload.as_bytes()).into();
    if signed_payload.as_slice() != expected_signed_payload.as_ref() {
        return Err(ProofError::Conflict);
    }
    validate_rekor_receipt(&payload, expected_payload.as_bytes())?;

    if existing.status == "confirmed" {
        if existing.anchor_kind.as_deref() != Some(payload.anchor_kind.as_str())
            || existing.anchor_url.as_deref() != Some(payload.anchor_url.as_str())
            || existing.anchor_entry_id.as_deref() != Some(entry_id.as_str())
            || existing.anchor_sequence != Some(payload.log_index)
            || existing.anchor_integrated_at != Some(integrated_at)
            || existing.anchor_log_id.as_deref() != Some(log_id.as_str())
            || existing.anchor_receipt.as_ref() != Some(&receipt)
            || existing.signer_fingerprint.as_deref() != Some(signer_fingerprint.as_str())
            || existing.signed_payload_sha256.as_deref() != Some(signed_payload.as_slice())
        {
            return Err(ProofError::Conflict);
        }
        tx.commit().await.map_err(ProofError::sqlx)?;
        return ProofBatchView::try_from(existing);
    }
    if existing.status != "processing"
        || existing.anchor_kind.is_some()
        || existing.lock_owner.as_deref() != Some(payload.worker_id.as_str())
    {
        return Err(ProofError::Conflict);
    }

    sqlx::query(
        r#"
        UPDATE external_proof_batches
        SET status = 'confirmed', locked_at = NULL, lock_owner = NULL,
            lease_expires_at = NULL,
            anchor_kind = $3, anchor_url = $4, anchor_entry_id = $5,
            anchor_sequence = $6, anchor_integrated_at = $7, anchor_log_id = $8,
            anchor_receipt = $9, signer_fingerprint = $10,
            signed_payload_sha256 = $11, confirmed_at = now(),
            last_error_kind = NULL
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(&payload.anchor_kind)
    .bind(&payload.anchor_url)
    .bind(&entry_id)
    .bind(payload.log_index)
    .bind(integrated_at)
    .bind(&log_id)
    .bind(&receipt)
    .bind(&signer_fingerprint)
    .bind(&signed_payload)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    append_audit(
        &mut tx,
        workspace_id,
        "external_proof.confirmed",
        batch_id,
        request_id_value,
        json!({
            "anchor_kind": &payload.anchor_kind,
            "anchor_url": &payload.anchor_url,
            "entry_id": &entry_id,
            "sequence": payload.log_index,
            "integrated_time": payload.integrated_time,
            "log_id": &log_id,
            "signer_fingerprint": &signer_fingerprint,
            "signed_payload_sha256": &payload.payload_sha256,
        }),
    )
    .await?;
    append_outbox(
        &mut tx,
        workspace_id,
        "proof.anchor.confirmed",
        batch_id,
        request_id_value,
        json!({
            "batch_id": batch_id,
            "proof_kind": existing.proof_kind,
            "root_sha256": encode_hash(&existing.root_sha256)?,
            "anchor_kind": &payload.anchor_kind,
            "anchor_url": &payload.anchor_url,
            "entry_id": &entry_id,
            "sequence": payload.log_index,
            "integrated_time": payload.integrated_time,
            "log_id": &log_id,
            "signer_fingerprint": &signer_fingerprint,
        }),
    )
    .await?;
    let batch = load_batch_tx(&mut tx, workspace_id, batch_id).await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    Ok(batch)
}

async fn fail_batch(
    state: &crate::AppState,
    batch_id: Uuid,
    payload: &FailRequest,
    request_id_value: Option<&str>,
) -> Result<ProofBatchView, ProofError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    #[derive(FromRow)]
    struct FailureState {
        attempts: i32,
        max_attempts: i32,
        proof_kind: String,
        root_sha256: Vec<u8>,
        status: String,
        lock_owner: Option<String>,
    }
    let current = sqlx::query_as::<_, FailureState>(
        r#"
        SELECT attempts, max_attempts, proof_kind, root_sha256, status, lock_owner
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;
    if current.status == "confirmed" {
        return Err(ProofError::Conflict);
    }
    if current.status != "processing"
        || current.lock_owner.as_deref() != Some(payload.worker_id.as_str())
    {
        return Err(ProofError::Conflict);
    }
    let dead = current.attempts >= current.max_attempts;
    let delay_seconds = retry_delay_seconds(current.attempts);
    sqlx::query(
        r#"
        UPDATE external_proof_batches
        SET status = CASE WHEN $3 THEN 'dead' ELSE 'failed' END,
            locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
            available_at = CASE
                WHEN $3 THEN available_at
                ELSE now() + make_interval(secs => $4::double precision)
            END,
            last_error_kind = $5
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(dead)
    .bind(delay_seconds)
    .bind(&payload.error_kind)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;
    if dead {
        append_outbox(
            &mut tx,
            workspace_id,
            "proof.anchor.dead",
            batch_id,
            request_id_value,
            json!({
                "batch_id": batch_id,
                "proof_kind": current.proof_kind,
                "root_sha256": encode_hash(&current.root_sha256)?,
                "attempts": current.attempts,
                "error_kind": payload.error_kind
            }),
        )
        .await?;
    }
    let batch = load_batch_tx(&mut tx, workspace_id, batch_id).await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    Ok(batch)
}

fn proof_anchor_payload(batch: &BatchRow) -> Result<String, ProofError> {
    let proof_kind =
        serde_json::to_string(&batch.proof_kind).map_err(|_| ProofError::Unexpected)?;
    let tree_algorithm =
        serde_json::to_string(&batch.tree_algorithm).map_err(|_| ProofError::Unexpected)?;
    let root_sha256 = encode_hash(&batch.root_sha256)?;
    Ok(format!(
        concat!(
            r#"{{"batch_id":"{}","hash_algorithm":"sha256","leaf_count":{},"proof_kind":{},"#,
            r#""root_sha256":"{}","schema":"crowdrelay/proof-anchor/v1","schema_version":{},"#,
            r#""tree_algorithm":{}}}"#
        ),
        batch.id, batch.leaf_count, proof_kind, root_sha256, batch.schema_version, tree_algorithm,
    ))
}

fn validate_rekor_receipt(
    payload: &ConfirmRequest,
    expected_payload: &[u8],
) -> Result<(), ProofError> {
    let body_bytes = BASE64_STANDARD
        .decode(&payload.canonicalized_body)
        .map_err(|_| ProofError::BadRequest)?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|_| ProofError::BadRequest)?;
    let embedded_payload_matches = match body.pointer("/spec/data/content").and_then(Value::as_str)
    {
        Some(value) => {
            let decoded = BASE64_STANDARD
                .decode(value)
                .map_err(|_| ProofError::BadRequest)?;
            decoded.as_slice() == expected_payload
        }
        None => false,
    };
    let expected_payload_hash = hex::encode(Sha256::digest(expected_payload));
    let embedded_hash_matches = body
        .pointer("/spec/data/hash/algorithm")
        .and_then(Value::as_str)
        == Some("sha256")
        && body
            .pointer("/spec/data/hash/value")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(&expected_payload_hash));
    let embedded_signature = body
        .pointer("/spec/signature/content")
        .and_then(Value::as_str)
        .ok_or(ProofError::BadRequest)?;
    let embedded_key = body
        .pointer("/spec/signature/publicKey/content")
        .and_then(Value::as_str)
        .ok_or(ProofError::BadRequest)?;
    let expected_key = BASE64_STANDARD.encode(payload.public_key_pem.as_bytes());
    if body.get("apiVersion").and_then(Value::as_str) != Some("0.0.1")
        || body.get("kind").and_then(Value::as_str) != Some("rekord")
        || body
            .pointer("/spec/signature/format")
            .and_then(Value::as_str)
            != Some("x509")
        || (!embedded_payload_matches && !embedded_hash_matches)
        || embedded_signature != payload.signature_base64
        || embedded_key != expected_key.as_str()
    {
        return Err(ProofError::Conflict);
    }
    Ok(())
}

fn valid_confirm(payload: &ConfirmRequest) -> bool {
    valid_worker_id(&payload.worker_id)
        && payload.anchor_kind == "sigstore.rekor.v1"
        && payload.anchor_url.starts_with("https://")
        && payload.anchor_url.len() <= 512
        && !payload
            .anchor_url
            .bytes()
            .any(|byte| byte.is_ascii_whitespace())
        && (64..=128).contains(&payload.entry_uuid.len())
        && is_lower_hex(&payload.entry_uuid)
        && payload.log_index >= 0
        && payload.integrated_time > 0
        && payload.log_id.len() == 64
        && is_lower_hex(&payload.log_id)
        && payload.canonicalized_body.len() <= 200_000
        && is_base64_value(&payload.canonicalized_body)
        && payload.signed_entry_timestamp.len() <= 16_384
        && is_base64_value(&payload.signed_entry_timestamp)
        && payload.inclusion_proof.is_object()
        && serde_json::to_vec(&payload.inclusion_proof).is_ok_and(|encoded| encoded.len() <= 65_536)
        && payload
            .signer_fingerprint
            .strip_prefix("sha256:")
            .is_some_and(|hash| hash.len() == 64 && is_lower_hex(hash))
        && payload.signature_base64.len() <= 16_384
        && is_base64_value(&payload.signature_base64)
        && payload.public_key_pem.len() <= 16_384
        && payload
            .public_key_pem
            .starts_with("-----BEGIN PUBLIC KEY-----")
        && payload
            .public_key_pem
            .trim_end()
            .ends_with("-----END PUBLIC KEY-----")
        && payload.payload_sha256.len() == 64
        && is_lower_hex(&payload.payload_sha256)
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_base64_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn valid_worker_id(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ProofError> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .ok_or(ProofError::BadRequest)?;
    if !(8..=128).contains(&value.len()) || !value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
    {
        return Err(ProofError::BadRequest);
    }
    Ok(value.to_owned())
}

fn leaf_hash(canonical: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0_u8]);
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(canonical);
    hasher.finalize().into()
}

fn node_hash(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([1_u8]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

fn merkle_root(leaves: &[[u8; 32]]) -> Option<[u8; 32]> {
    if leaves.is_empty() {
        return None;
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = pair.first().copied()?;
            let right = pair.get(1).copied().unwrap_or(left);
            next.push(node_hash(left, right));
        }
        level = next;
    }
    level.first().copied()
}

#[derive(Clone, Copy)]
struct RawMerkleStep {
    sibling_left: bool,
    hash: [u8; 32],
}

fn merkle_path(leaves: &[[u8; 32]], index: usize) -> Result<Vec<RawMerkleStep>, ProofError> {
    if leaves.is_empty() || index >= leaves.len() {
        return Err(ProofError::BadRequest);
    }
    let mut path = Vec::new();
    let mut level = leaves.to_vec();
    let mut cursor = index;
    while level.len() > 1 {
        let sibling_index = if cursor.is_multiple_of(2) {
            (cursor + 1).min(level.len() - 1)
        } else {
            cursor - 1
        };
        let sibling = level
            .get(sibling_index)
            .copied()
            .ok_or(ProofError::Unexpected)?;
        path.push(RawMerkleStep {
            sibling_left: sibling_index < cursor,
            hash: sibling,
        });
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = pair.first().copied().ok_or(ProofError::Unexpected)?;
            let right = pair.get(1).copied().unwrap_or(left);
            next.push(node_hash(left, right));
        }
        cursor /= 2;
        level = next;
    }
    Ok(path)
}

fn verify_path(
    leaf: [u8; 32],
    _index: usize,
    path: &[RawMerkleStep],
    expected_root: [u8; 32],
) -> bool {
    let mut current = leaf;
    for step in path {
        current = if step.sibling_left {
            node_hash(step.hash, current)
        } else {
            node_hash(current, step.hash)
        };
    }
    current == expected_root
}

#[allow(clippy::too_many_arguments)]
fn draw_receipt_hash(
    run_id: Uuid,
    algorithm_version: &str,
    seed_hash: &[u8],
    revealed_seed_hex: &str,
    eligible_count: i32,
    total_entries: i64,
    requested_winners: i32,
    selected_winners: i32,
    candidate_snapshot_sha256: &[u8],
    winner_snapshot_sha256: &[u8],
) -> Result<[u8; 32], ProofError> {
    let seed = hash_array(seed_hash)?;
    let candidate = hash_array(candidate_snapshot_sha256)?;
    let winner = hash_array(winner_snapshot_sha256)?;
    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay/draw-receipt/v1\0");
    hasher.update(run_id.as_bytes());
    update_length_prefixed(&mut hasher, algorithm_version.as_bytes());
    hasher.update(seed);
    update_length_prefixed(&mut hasher, revealed_seed_hex.as_bytes());
    hasher.update(eligible_count.to_be_bytes());
    hasher.update(total_entries.to_be_bytes());
    hasher.update(requested_winners.to_be_bytes());
    hasher.update(selected_winners.to_be_bytes());
    hasher.update(candidate);
    hasher.update(winner);
    Ok(hasher.finalize().into())
}

fn update_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hash_array(value: &[u8]) -> Result<[u8; 32], ProofError> {
    value.try_into().map_err(|_| ProofError::Unexpected)
}

fn decode_hash(value: &str) -> Result<[u8; 32], ProofError> {
    let decoded = hex::decode(value).map_err(|_| ProofError::Unexpected)?;
    hash_array(&decoded)
}

fn encode_hash(value: &[u8]) -> Result<String, ProofError> {
    let hash = hash_array(value)?;
    Ok(hex::encode(hash))
}

fn retry_delay_seconds(attempts: i32) -> i64 {
    let exponent = u32::try_from(attempts.saturating_sub(1).min(8)).unwrap_or(8);
    15_i64
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(3_600)
}

async fn append_operator_action(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_id: Uuid,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    details: Value,
) -> Result<(), ProofError> {
    sqlx::query(
        r#"
        INSERT INTO operator_actions (
            workspace_id, action, target_type, target_id,
            idempotency_key, request_id, details
        ) VALUES ($1, $2, 'external_proof_batch', $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_id)
    .bind(idempotency_key)
    .bind(request_id_value)
    .bind(details)
    .execute(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?;
    Ok(())
}

async fn append_audit(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_id: Uuid,
    request_id_value: Option<&str>,
    metadata: Value,
) -> Result<(), ProofError> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type,
            target_id, request_id, metadata
        ) VALUES ($1, 'service', $2, 'external_proof_batch', $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_id.to_string())
    .bind(request_id_value)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?;
    Ok(())
}

async fn append_outbox(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_type: &str,
    batch_id: Uuid,
    request_id_value: Option<&str>,
    payload: Value,
) -> Result<(), ProofError> {
    let fallback_request_id = batch_id.to_string();
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        ) VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(request_id_value.unwrap_or(&fallback_request_id))
    .execute(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?;
    Ok(())
}

async fn configure_transaction(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), ProofError> {
    let statement_ms = state.ticketing.operation_timeout().as_millis();
    let lock_ms = state.ticketing.lock_timeout().as_millis();
    if statement_ms == 0
        || lock_ms == 0
        || statement_ms > i32::MAX as u128
        || lock_ms > i32::MAX as u128
    {
        return Err(ProofError::Unexpected);
    }
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?;
    Ok(())
}

async fn run<T>(
    state: &crate::AppState,
    future: impl Future<Output = Result<T, ProofError>>,
) -> Result<T, ProofError> {
    timeout(state.ticketing.operation_timeout(), future)
        .await
        .map_err(|_| ProofError::Unavailable)?
}

fn respond_private<T: Serialize>(
    result: Result<T, ProofError>,
    request_id_value: Option<String>,
) -> Response {
    respond(result, request_id_value, PRIVATE_NO_STORE)
}

fn respond_public<T: Serialize>(
    result: Result<T, ProofError>,
    request_id_value: Option<String>,
) -> Response {
    respond(result, request_id_value, PUBLIC_PROOF_CACHE)
}

fn respond_public_status<T: Serialize>(
    result: Result<T, ProofError>,
    request_id_value: Option<String>,
) -> Response {
    respond(result, request_id_value, PUBLIC_DRAW_STATUS_CACHE)
}

fn respond<T: Serialize>(
    result: Result<T, ProofError>,
    request_id_value: Option<String>,
    cache_control: &'static str,
) -> Response {
    match result {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, cache_control)],
            Json(value),
        )
            .into_response(),
        Err(error) => error.into_response(request_id_value),
    }
}

#[derive(Debug)]
enum ProofError {
    BadRequest,
    NotFound,
    Conflict,
    Unavailable,
    Unexpected,
}

impl ProofError {
    fn sqlx(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "external proof query failed");
        Self::Unexpected
    }

    fn into_response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::BadRequest => Problem::bad_request(request_id_value)
                .private()
                .into_response(),
            Self::NotFound => Problem::not_found(request_id_value)
                .private()
                .into_response(),
            Self::Conflict => Problem::conflict(request_id_value)
                .private()
                .into_response(),
            Self::Unavailable => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
            Self::Unexpected => Problem::internal(request_id_value)
                .private()
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_tree_is_deterministic_and_detects_changes() {
        let leaves = [leaf_hash(b"one"), leaf_hash(b"two"), leaf_hash(b"three")];
        let root = merkle_root(&leaves).expect("root");
        assert_eq!(root, merkle_root(&leaves).expect("root"));
        let changed = [leaf_hash(b"one"), leaf_hash(b"two"), leaf_hash(b"changed")];
        assert_ne!(root, merkle_root(&changed).expect("root"));
    }

    #[test]
    fn every_leaf_gets_a_verifiable_path() {
        let leaves = [
            leaf_hash(b"a"),
            leaf_hash(b"b"),
            leaf_hash(b"c"),
            leaf_hash(b"d"),
            leaf_hash(b"e"),
        ];
        let root = merkle_root(&leaves).expect("root");
        for index in 0..leaves.len() {
            let path = merkle_path(&leaves, index).expect("path");
            assert!(verify_path(leaves[index], index, &path, root));
        }
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(retry_delay_seconds(1), 15);
        assert_eq!(retry_delay_seconds(2), 30);
        assert_eq!(retry_delay_seconds(100), 3_600);
    }

    #[test]
    fn anchor_input_validation_is_strict() {
        assert!(valid_worker_id("virya-rekor-anchor-01"));
        assert!(!valid_worker_id("bad worker"));
        assert!(is_lower_hex(&"ab".repeat(32)));
        assert!(!is_lower_hex("ABCD"));
        assert!(is_base64_value("dGVzdA=="));
        assert!(!is_base64_value("not base64!"));
    }

    fn receipt_request(expected_payload: &[u8], data: Value) -> ConfirmRequest {
        let signature_base64 = BASE64_STANDARD.encode(b"rekor-test-signature");
        let public_key_pem = concat!(
            "-----BEGIN PUBLIC KEY-----\n",
            "rekor-test-public-key\n",
            "-----END PUBLIC KEY-----\n"
        )
        .to_owned();
        let canonical_body = json!({
            "apiVersion": "0.0.1",
            "kind": "rekord",
            "spec": {
                "data": data,
                "signature": {
                    "content": &signature_base64,
                    "format": "x509",
                    "publicKey": {
                        "content": BASE64_STANDARD.encode(public_key_pem.as_bytes())
                    }
                }
            }
        });
        ConfirmRequest {
            worker_id: "virya-rekor-anchor-01".to_owned(),
            anchor_kind: "sigstore.rekor.v1".to_owned(),
            anchor_url: "https://rekor.sigstore.dev".to_owned(),
            entry_uuid: "a".repeat(64),
            log_index: 1,
            integrated_time: 1_700_000_000,
            log_id: "b".repeat(64),
            canonicalized_body: BASE64_STANDARD.encode(
                serde_json::to_vec(&canonical_body)
                    .expect("canonical Rekor test body must serialize"),
            ),
            signed_entry_timestamp: BASE64_STANDARD.encode(b"rekor-test-set-value"),
            inclusion_proof: json!({"treeSize": 1, "logIndex": 0, "hashes": []}),
            signer_fingerprint: format!("sha256:{}", "c".repeat(64)),
            signature_base64,
            public_key_pem,
            payload_sha256: hex::encode(Sha256::digest(expected_payload)),
        }
    }

    #[test]
    fn rekor_receipt_accepts_canonical_sha256_hash() {
        let expected = b"crowdrelay canonical proof payload";
        let request = receipt_request(
            expected,
            json!({
                "hash": {
                    "algorithm": "sha256",
                    "value": hex::encode(Sha256::digest(expected))
                }
            }),
        );
        assert!(validate_rekor_receipt(&request, expected).is_ok());
    }

    #[test]
    fn rekor_receipt_retains_legacy_content_compatibility() {
        let expected = b"crowdrelay legacy proof payload";
        let request = receipt_request(
            expected,
            json!({"content": BASE64_STANDARD.encode(expected)}),
        );
        assert!(validate_rekor_receipt(&request, expected).is_ok());
    }

    #[test]
    fn rekor_receipt_rejects_wrong_canonical_hash() {
        let expected = b"crowdrelay canonical proof payload";
        let request = receipt_request(
            expected,
            json!({
                "hash": {
                    "algorithm": "sha256",
                    "value": "d".repeat(64)
                }
            }),
        );
        assert!(validate_rekor_receipt(&request, expected).is_err());
    }
}
