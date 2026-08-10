async fn lock_area_player(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM area_players
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await
    .map(|value| value.is_some())
}

async fn area_credit_balance_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(sum(delta), 0)::bigint
        FROM area_credit_ledger
        WHERE workspace_id = $1 AND player_id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .fetch_one(&mut **transaction)
    .await
}

async fn insert_credit_delta(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    delta: i32,
    reason: &str,
    reference_key: &str,
    created_at: Option<OffsetDateTime>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO area_credit_ledger (
            workspace_id, player_id, delta, reason, reference_key, created_at
        )
        VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()))
        ON CONFLICT (workspace_id, player_id, reference_key) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(delta)
    .bind(reason)
    .bind(reference_key)
    .bind(created_at)
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn safe_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn valid_event_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || ((byte == b'-' || byte == b'_') && index > 0))
}

fn valid_small_text(value: &str, max: usize) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn area_reward_code_hash(code: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"virya-area-reward\0");
    hasher.update(code.as_bytes());
    hasher.finalize().to_vec()
}

fn normalize_reward_code(raw: &str) -> Option<String> {
    let code = raw.trim().to_ascii_uppercase();
    let bytes = code.as_bytes();
    let valid = bytes.len() == 35
        && code.starts_with("VIRYA-")
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                if matches!(index, 5 | 10 | 15 | 20 | 25 | 30) {
                    *byte == b'-'
                } else if index < 5 {
                    b"VIRYA".get(index).is_some_and(|expected| expected == byte)
                } else {
                    byte.is_ascii_uppercase() || byte.is_ascii_digit()
                }
            });
    valid.then_some(code)
}

fn new_reward_code() -> Option<String> {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes).ok()?;
    let hex = hex::encode_upper(bytes);
    let groups = hex
        .as_bytes()
        .chunks_exact(4)
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .join("-");
    Some(format!("VIRYA-{groups}"))
}
