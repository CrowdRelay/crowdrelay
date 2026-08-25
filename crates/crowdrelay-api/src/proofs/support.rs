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
        // Full pairs hash directly; an odd trailing leaf duplicates itself,
        // matching the unwrap_or(left) semantics this replaces.
        let (pairs, tail) = level.as_chunks::<2>();
        for &[left, right] in pairs {
            next.push(node_hash(left, right));
        }
        if let [single] = tail {
            next.push(node_hash(*single, *single));
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
        let (pairs, tail) = level.as_chunks::<2>();
        for &[left, right] in pairs {
            next.push(node_hash(left, right));
        }
        if let [single] = tail {
            next.push(node_hash(*single, *single));
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
