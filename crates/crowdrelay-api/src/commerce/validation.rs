async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::ticketing::TicketingState,
) -> Result<(), CommerceError> {
    let statement_ms = duration_milliseconds(state.operation_timeout())?;
    let lock_ms = duration_milliseconds(state.lock_timeout())?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<u128, CommerceError> {
    let milliseconds = value.as_millis();
    if milliseconds == 0 || milliseconds > 2_147_483_647_u128 {
        return Err(CommerceError::Unexpected);
    }
    Ok(milliseconds)
}

fn normalize_reservation(
    payload: ReserveInventoryRequest,
) -> Result<ReserveInventoryRequest, CommerceError> {
    let external_reference = clean_text(&payload.external_reference, 200)?;
    let now = OffsetDateTime::now_utc();
    if payload.expires_at <= now + TimeDuration::seconds(60)
        || payload.expires_at > now + TimeDuration::hours(24)
    {
        return Err(CommerceError::Invalid);
    }
    if payload.items.is_empty() || payload.items.len() > MAX_RESERVATION_ITEMS {
        return Err(CommerceError::Invalid);
    }
    let mut merged = BTreeMap::<String, i32>::new();
    for item in payload.items {
        let sku = clean_text(&item.sku, 128)?;
        if item.quantity <= 0 || item.quantity > MAX_RESERVATION_QUANTITY {
            return Err(CommerceError::Invalid);
        }
        let quantity = merged.entry(sku).or_default();
        *quantity = quantity
            .checked_add(item.quantity)
            .ok_or(CommerceError::Invalid)?;
        if *quantity > MAX_RESERVATION_QUANTITY {
            return Err(CommerceError::Invalid);
        }
    }
    Ok(ReserveInventoryRequest {
        external_reference,
        expires_at: payload.expires_at,
        items: merged
            .into_iter()
            .map(|(sku, quantity)| ReserveInventoryItemRequest { sku, quantity })
            .collect(),
    })
}

fn reservation_request_hash(
    normalized: &ReserveInventoryRequest,
) -> Result<Vec<u8>, CommerceError> {
    #[derive(Serialize)]
    struct StableReservationHash<'a> {
        external_reference: &'a str,
        items: &'a [ReserveInventoryItemRequest],
    }

    let stable = StableReservationHash {
        external_reference: &normalized.external_reference,
        items: &normalized.items,
    };
    Ok(
        Sha256::digest(serde_json::to_vec(&stable).map_err(|_| CommerceError::Unexpected)?)
            .to_vec(),
    )
}

fn validate_catalog(payload: &UpsertCatalogRequest) -> Result<(), CommerceError> {
    if payload.products.is_empty() || payload.products.len() > MAX_PRODUCTS {
        return Err(CommerceError::Invalid);
    }
    let mut slugs = BTreeSet::new();
    let mut skus = BTreeSet::new();
    for product in &payload.products {
        let slug = normalize_slug(&product.slug)?;
        if !slugs.insert(slug) {
            return Err(CommerceError::Invalid);
        }
        clean_text(&product.name, 200)?;
        optional_text(product.description.as_deref(), MAX_TEXT_CHARS)?;
        validate_optional_https_url(product.image_url.as_deref())?;
        let currency = product.currency.trim();
        if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(CommerceError::Invalid);
        }
        if product.price_gross_minor < 0 {
            return Err(CommerceError::Invalid);
        }
        if product.variants.is_empty() || product.variants.len() > MAX_VARIANTS_PER_PRODUCT {
            return Err(CommerceError::Invalid);
        }
        for variant in &product.variants {
            let sku = clean_text(&variant.sku, 128)?;
            if !skus.insert(sku) {
                return Err(CommerceError::Invalid);
            }
            clean_text(&variant.label, 160)?;
            if !variant.attributes.is_object() || variant.low_stock_threshold < 0 {
                return Err(CommerceError::Invalid);
            }
        }
    }
    Ok(())
}

fn validate_reward_campaign(payload: &CreateRewardCampaignRequest) -> Result<(), CommerceError> {
    normalize_slug(&payload.slug)?;
    clean_text(&payload.name, 200)?;
    clean_text(&payload.prize_sku, 128)?;
    if !matches!(payload.status.as_str(), "draft" | "scheduled")
        || !matches!(
            payload.eligibility_kind.as_str(),
            "all_active" | "event_interest"
        )
        || payload.winner_count <= 0
        || payload.winner_count > 10_000
        || payload.units_per_winner <= 0
        || payload.units_per_winner > 100
        || payload.base_entries <= 0
        || payload.base_entries > 100_000
        || payload.entries_per_referral < 0
        || payload.entries_per_referral > 100_000
        || payload.entries_per_checkin < 0
        || payload.entries_per_checkin > 100_000
        || payload.max_entries < payload.base_entries
        || payload.max_entries > 1_000_000
        || payload.claim_expires_hours <= 0
        || payload.claim_expires_hours > 8_760
        || payload.opens_at >= payload.closes_at
        || payload.closes_at > payload.draw_at
    {
        return Err(CommerceError::Invalid);
    }
    match payload.eligibility_kind.as_str() {
        "event_interest" => {
            normalize_slug(
                payload
                    .event_slug
                    .as_deref()
                    .ok_or(CommerceError::Invalid)?,
            )?;
        }
        "all_active" if payload.event_slug.is_some() => return Err(CommerceError::Invalid),
        _ => {}
    }
    Ok(())
}

fn normalize_slug(value: &str) -> Result<String, CommerceError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized.len() > 128 {
        return Err(CommerceError::Invalid);
    }
    let mut bytes = normalized.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(CommerceError::Invalid);
    }
    Ok(normalized)
}

fn normalize_stocktake(
    payload: InventoryStocktakeRequest,
) -> Result<InventoryStocktakeRequest, CommerceError> {
    if payload.items.is_empty() || payload.items.len() > MAX_STOCKTAKE_ITEMS {
        return Err(CommerceError::Invalid);
    }
    let mut unique = BTreeSet::new();
    let mut items = Vec::with_capacity(payload.items.len());
    for item in payload.items {
        let sku = clean_text(&item.sku, 128)?;
        if item.on_hand < 0 || item.on_hand > MAX_STOCK_ON_HAND || !unique.insert(sku.clone()) {
            return Err(CommerceError::Invalid);
        }
        items.push(InventoryStocktakeItemRequest {
            sku,
            on_hand: item.on_hand,
        });
    }
    items.sort_by(|left, right| left.sku.cmp(&right.sku));
    Ok(InventoryStocktakeRequest {
        items,
        actor_id: optional_text(payload.actor_id.as_deref(), 200)?,
        reason: optional_text(payload.reason.as_deref(), 500)?,
    })
}

fn stocktake_request_hash(payload: &InventoryStocktakeRequest) -> Result<Vec<u8>, CommerceError> {
    let encoded = serde_json::to_vec(payload).map_err(|_| CommerceError::Unexpected)?;
    Ok(Sha256::digest(encoded).to_vec())
}

fn clean_text(value: &str, max_chars: usize) -> Result<String, CommerceError> {
    let cleaned = value.trim();
    if cleaned.is_empty()
        || cleaned.chars().count() > max_chars
        || cleaned.chars().any(char::is_control)
    {
        return Err(CommerceError::Invalid);
    }
    Ok(cleaned.to_owned())
}

fn optional_text(value: Option<&str>, max_chars: usize) -> Result<Option<String>, CommerceError> {
    value.map(|item| clean_text(item, max_chars)).transpose()
}

fn validate_optional_https_url(value: Option<&str>) -> Result<Option<String>, CommerceError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = clean_text(value, 2_000)?;
    let url = Url::parse(&value).map_err(|_| CommerceError::Invalid)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(CommerceError::Invalid);
    }
    Ok(Some(value))
}

fn clean_movement_kind(value: &str) -> Result<String, CommerceError> {
    let value = value.trim();
    if !matches!(
        value,
        "initial" | "receipt" | "refund" | "adjustment" | "damage" | "staff_issue"
    ) {
        return Err(CommerceError::Invalid);
    }
    Ok(value.to_owned())
}

fn clean_fulfillment_status(value: &str) -> Result<String, CommerceError> {
    let value = value.trim();
    if !matches!(value, "prepared" | "delivered" | "cancelled") {
        return Err(CommerceError::Invalid);
    }
    Ok(value.to_owned())
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, CommerceError> {
    let value = headers
        .get(IDEMPOTENCY_KEY)
        .ok_or(CommerceError::Invalid)?
        .to_str()
        .map_err(|_| CommerceError::Invalid)?
        .trim();
    if value.len() < 8 || value.len() > 128 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(CommerceError::Invalid);
    }
    Ok(value.to_owned())
}
