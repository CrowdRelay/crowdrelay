//! Additive merch inventory and reward-campaign controls.
//!
//! This module intentionally stays separate from ticketing and fan mail flows.
//! Public reads are cacheable and bounded; every stock mutation is transactional,
//! idempotent and workspace-scoped.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::timeout;
use url::Url;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

const PUBLIC_CACHE: &str = "public, max-age=15, stale-while-revalidate=60";
const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_PRODUCTS: usize = 100;
const MAX_VARIANTS_PER_PRODUCT: usize = 50;
const MAX_RESERVATION_ITEMS: usize = 50;
const MAX_RESERVATION_QUANTITY: i32 = 100;
const MAX_TEXT_CHARS: usize = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommerceError {
    Invalid,
    NotFound,
    Conflict,
    Unavailable,
    Unexpected,
}

impl CommerceError {
    fn response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::Invalid => Problem::unprocessable(request_id_value)
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

    fn sqlx(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "commerce database operation failed");
        Self::Unavailable
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MerchCatalogView {
    generated_at: OffsetDateTime,
    products: Vec<MerchProductView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MerchProductView {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    image_url: Option<String>,
    currency: String,
    price_gross_minor: i64,
    active: bool,
    public: bool,
    variants: Vec<MerchVariantView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MerchVariantView {
    id: Uuid,
    sku: String,
    label: String,
    attributes: Value,
    active: bool,
    low_stock_threshold: i32,
    sell_without_stock: bool,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_hand: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reserved: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    available_quantity: Option<i64>,
    availability: &'static str,
}

#[derive(Debug, FromRow)]
struct CatalogRow {
    product_id: Uuid,
    product_slug: String,
    product_name: String,
    product_description: Option<String>,
    product_image_url: Option<String>,
    currency: String,
    price_gross_minor: i64,
    product_active: bool,
    product_public: bool,
    variant_id: Uuid,
    sku: String,
    variant_label: String,
    attributes: Value,
    variant_active: bool,
    low_stock_threshold: i32,
    sell_without_stock: bool,
    on_hand: i64,
    reserved: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertCatalogRequest {
    products: Vec<UpsertProductRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsertProductRequest {
    slug: String,
    name: String,
    description: Option<String>,
    image_url: Option<String>,
    currency: String,
    price_gross_minor: i64,
    active: bool,
    public: bool,
    variants: Vec<UpsertVariantRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsertVariantRequest {
    sku: String,
    label: String,
    #[serde(default = "empty_object")]
    attributes: Value,
    active: bool,
    #[serde(default = "default_low_stock_threshold")]
    low_stock_threshold: i32,
    #[serde(default)]
    sell_without_stock: bool,
}

const fn default_low_stock_threshold() -> i32 {
    3
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustInventoryRequest {
    sku: String,
    delta: i32,
    movement_kind: String,
    actor_id: Option<String>,
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryAdjustmentView {
    sku: String,
    delta: i32,
    movement_kind: String,
    on_hand: i64,
    reserved: i64,
    available_quantity: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveInventoryRequest {
    external_reference: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    items: Vec<ReserveInventoryItemRequest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReserveInventoryItemRequest {
    sku: String,
    quantity: i32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseInventoryRequest {
    reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryReservationView {
    id: Uuid,
    external_reference: String,
    status: String,
    expires_at: Option<OffsetDateTime>,
    items: Vec<InventoryReservationItemView>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
struct InventoryReservationItemView {
    sku: String,
    label: String,
    quantity: i32,
}

#[derive(Debug, FromRow)]
struct ReservationRow {
    id: Uuid,
    external_reference: String,
    request_hash: Vec<u8>,
    status: String,
    expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct VariantAvailabilityRow {
    id: Uuid,
    product_name: String,
    sku: String,
    sell_without_stock: bool,
    on_hand: i64,
    reserved: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRewardCampaignRequest {
    slug: String,
    name: String,
    prize_sku: String,
    winner_count: i32,
    #[serde(default = "default_units_per_winner")]
    units_per_winner: i32,
    #[serde(default = "default_draw_eligibility")]
    eligibility_kind: String,
    event_slug: Option<String>,
    #[serde(default = "default_base_entries")]
    base_entries: i32,
    #[serde(default = "default_entries_per_referral")]
    entries_per_referral: i32,
    #[serde(default)]
    entries_per_checkin: i32,
    #[serde(default = "default_max_entries")]
    max_entries: i32,
    #[serde(default = "default_claim_expires_hours")]
    claim_expires_hours: i32,
    #[serde(with = "time::serde::rfc3339")]
    opens_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    closes_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    draw_at: OffsetDateTime,
    status: String,
}

const fn default_units_per_winner() -> i32 {
    1
}
const fn default_base_entries() -> i32 {
    1
}
const fn default_entries_per_referral() -> i32 {
    1
}
const fn default_max_entries() -> i32 {
    1_000
}
const fn default_claim_expires_hours() -> i32 {
    168
}
fn default_draw_eligibility() -> String {
    "all_active".to_owned()
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct RewardCampaignView {
    id: Uuid,
    slug: String,
    name: String,
    status: String,
    eligibility_kind: String,
    event_slug: Option<String>,
    winner_count: i32,
    selected_winners: i64,
    prize_sku: String,
    prize_name: String,
    prize_variant: String,
    units_per_winner: i32,
    reserved_quantity: i32,
    pending_fulfillments: i64,
    delivered_fulfillments: i64,
    opens_at: OffsetDateTime,
    closes_at: OffsetDateTime,
    draw_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct PromotionRecommendationView {
    sku: String,
    product_name: String,
    variant_label: String,
    on_hand: i64,
    reserved: i64,
    available_quantity: i64,
    sold_7d: i64,
    sold_30d: i64,
    sold_90d: i64,
    promotional_issued_90d: i64,
    upcoming_events_60d: i64,
    history_days: i32,
    safety_stock: i64,
    recommended_max_giveaway: i64,
    recommendation: String,
    confidence: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FulfillRewardRequest {
    status: String,
    actor_id: Option<String>,
    note: Option<String>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct RewardFulfillmentView {
    id: Uuid,
    winner_id: Uuid,
    draw_id: Uuid,
    draw_slug: String,
    winner_rank: i32,
    fan_display_name: Option<String>,
    fan_email_masked: String,
    prize_sku: String,
    prize_name: String,
    prize_variant: String,
    quantity: i32,
    status: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

/// Public, cacheable merch availability. It fails closed until the staged
/// feature flag is enabled, while the Virya site can keep rendering static
/// product cards and degrade only the small availability block.
pub async fn public_catalog(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "merch_inventory_enabled").await,
        Ok(true)
    ) {
        return CommerceError::Unavailable.response(request_id(&headers));
    }
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_catalog(&state, true),
    )
    .await
    {
        Ok(Ok(catalog)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PUBLIC_CACHE)],
            Json(catalog),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn admin_catalog(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_catalog(&state, false),
    )
    .await
    {
        Ok(Ok(catalog)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(catalog),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn upsert_catalog(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpsertCatalogRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = upsert_catalog_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(catalog)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(catalog),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn adjust_inventory(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<AdjustInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let mutation_key = match idempotency_key(&headers) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = adjust_inventory_inner(&state, mutation_key, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn reserve_inventory(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ReserveInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = reserve_inventory_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn commit_inventory(
    State(state): State<crate::AppState>,
    Path(reservation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let reservation_id = match Uuid::parse_str(reservation_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = commit_inventory_inner(&state, reservation_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn release_inventory(
    State(state): State<crate::AppState>,
    Path(reservation_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ReleaseInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let reservation_id = match Uuid::parse_str(reservation_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = release_inventory_inner(&state, reservation_id, payload.reason);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn promotion_recommendations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_promotion_recommendations(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn list_reward_campaigns(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_reward_campaigns(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn create_reward_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateRewardCampaignRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = create_reward_campaign_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn cancel_reward_campaign(
    State(state): State<crate::AppState>,
    Path(draw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let draw_id = match Uuid::parse_str(draw_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = cancel_reward_campaign_inner(&state, draw_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn schedule_reward_campaign(
    State(state): State<crate::AppState>,
    Path(draw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let draw_id = match Uuid::parse_str(draw_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = schedule_reward_campaign_inner(&state, draw_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn list_reward_fulfillments(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_reward_fulfillments(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn fulfill_reward(
    State(state): State<crate::AppState>,
    Path(winner_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<FulfillRewardRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let winner_id = match Uuid::parse_str(winner_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = fulfill_reward_inner(&state, winner_id, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

#[derive(Debug, FromRow)]
struct ExistingLedgerMutation {
    variant_id: Uuid,
    delta: i32,
    movement_kind: String,
}

#[derive(Debug, FromRow)]
struct FulfillmentMutationRow {
    id: Uuid,
    reward_grant_id: Uuid,
    variant_id: Uuid,
    reservation_id: Uuid,
    quantity: i32,
    status: String,
}

async fn require_inventory_writes(state: &crate::AppState) -> Result<(), CommerceError> {
    if matches!(
        crate::ecosystem::feature_enabled(state, "merch_inventory_writes_enabled").await,
        Ok(true)
    ) {
        Ok(())
    } else {
        Err(CommerceError::Unavailable)
    }
}

async fn load_catalog(
    state: &crate::AppState,
    public_only: bool,
) -> Result<MerchCatalogView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let rows = sqlx::query_as::<_, CatalogRow>(
        r#"
        WITH stock AS (
            SELECT variant_id, COALESCE(SUM(delta), 0)::bigint AS on_hand
            FROM inventory_ledger
            WHERE workspace_id = $1
            GROUP BY variant_id
        ), reservations AS (
            SELECT item.variant_id, COALESCE(SUM(item.quantity), 0)::bigint AS reserved
            FROM inventory_reservation_items AS item
            JOIN inventory_reservations AS reservation
              ON reservation.workspace_id = item.workspace_id
             AND reservation.id = item.reservation_id
            WHERE item.workspace_id = $1
              AND reservation.status = 'active'
              AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            GROUP BY item.variant_id
        )
        SELECT
            product.id AS product_id,
            product.slug AS product_slug,
            product.name AS product_name,
            product.description AS product_description,
            product.image_url AS product_image_url,
            product.currency::text AS currency,
            product.price_gross_minor,
            product.active AS product_active,
            product.public AS product_public,
            variant.id AS variant_id,
            variant.sku,
            variant.label AS variant_label,
            variant.attributes,
            variant.active AS variant_active,
            variant.low_stock_threshold,
            variant.sell_without_stock,
            COALESCE(stock.on_hand, 0)::bigint AS on_hand,
            COALESCE(reservations.reserved, 0)::bigint AS reserved
        FROM merch_products AS product
        JOIN merch_variants AS variant
          ON variant.workspace_id = product.workspace_id
         AND variant.product_id = product.id
        LEFT JOIN stock ON stock.variant_id = variant.id
        LEFT JOIN reservations ON reservations.variant_id = variant.id
        WHERE product.workspace_id = $1
          AND (
              NOT $2::boolean
              OR (product.active AND product.public AND variant.active)
          )
        ORDER BY product.slug, product.id, variant.label, variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .bind(public_only)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let mut products: Vec<MerchProductView> = Vec::new();
    for row in rows {
        let available_quantity = row.on_hand.saturating_sub(row.reserved);
        let availability = if row.sell_without_stock && available_quantity <= 0 {
            "preorder"
        } else if available_quantity <= 0 {
            "out_of_stock"
        } else if available_quantity <= i64::from(row.low_stock_threshold) {
            "low_stock"
        } else {
            "in_stock"
        };
        let variant = MerchVariantView {
            id: row.variant_id,
            sku: row.sku,
            label: row.variant_label,
            attributes: row.attributes,
            active: row.variant_active,
            low_stock_threshold: row.low_stock_threshold,
            sell_without_stock: row.sell_without_stock,
            available: row.sell_without_stock || available_quantity > 0,
            on_hand: (!public_only).then_some(row.on_hand),
            reserved: (!public_only).then_some(row.reserved),
            available_quantity: (!public_only).then_some(available_quantity),
            availability,
        };

        if let Some(product) = products.last_mut()
            && product.id == row.product_id
        {
            product.variants.push(variant);
            continue;
        }
        products.push(MerchProductView {
            id: row.product_id,
            slug: row.product_slug,
            name: row.product_name,
            description: row.product_description,
            image_url: row.product_image_url,
            currency: row.currency,
            price_gross_minor: row.price_gross_minor,
            active: row.product_active,
            public: row.product_public,
            variants: vec![variant],
        });
    }

    Ok(MerchCatalogView {
        generated_at: OffsetDateTime::now_utc(),
        products,
    })
}

async fn upsert_catalog_inner(
    state: &crate::AppState,
    payload: UpsertCatalogRequest,
) -> Result<MerchCatalogView, CommerceError> {
    require_inventory_writes(state).await?;
    validate_catalog(&payload)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    for product in payload.products {
        let product_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO merch_products (
                workspace_id, slug, name, description, image_url,
                currency, price_gross_minor, active, public
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                image_url = EXCLUDED.image_url,
                currency = EXCLUDED.currency,
                price_gross_minor = EXCLUDED.price_gross_minor,
                active = EXCLUDED.active,
                public = EXCLUDED.public,
                updated_at = now()
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(normalize_slug(&product.slug)?)
        .bind(clean_text(&product.name, 200)?)
        .bind(optional_text(
            product.description.as_deref(),
            MAX_TEXT_CHARS,
        )?)
        .bind(validate_optional_https_url(product.image_url.as_deref())?)
        .bind(product.currency.trim().to_ascii_uppercase())
        .bind(product.price_gross_minor)
        .bind(product.active)
        .bind(product.public)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        for variant in product.variants {
            sqlx::query(
                r#"
                INSERT INTO merch_variants (
                    workspace_id, product_id, sku, label, attributes,
                    active, low_stock_threshold, sell_without_stock
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (workspace_id, sku) DO UPDATE SET
                    product_id = EXCLUDED.product_id,
                    label = EXCLUDED.label,
                    attributes = EXCLUDED.attributes,
                    active = EXCLUDED.active,
                    low_stock_threshold = EXCLUDED.low_stock_threshold,
                    sell_without_stock = EXCLUDED.sell_without_stock,
                    updated_at = now()
                "#,
            )
            .bind(workspace_id)
            .bind(product_id)
            .bind(clean_text(&variant.sku, 128)?)
            .bind(clean_text(&variant.label, 160)?)
            .bind(variant.attributes)
            .bind(variant.active)
            .bind(variant.low_stock_threshold)
            .bind(variant.sell_without_stock)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
    }

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_catalog(state, false).await
}

async fn adjust_inventory_inner(
    state: &crate::AppState,
    mutation_key: String,
    payload: AdjustInventoryRequest,
) -> Result<InventoryAdjustmentView, CommerceError> {
    require_inventory_writes(state).await?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let sku = clean_text(&payload.sku, 128)?;
    let movement_kind = clean_movement_kind(&payload.movement_kind)?;
    if payload.delta == 0 {
        return Err(CommerceError::Invalid);
    }
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?;
    let reason = optional_text(payload.reason.as_deref(), 500)?;

    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let availability = lock_variant_availability(&mut transaction, workspace_id, &sku).await?;
    if let Some(existing) = sqlx::query_as::<_, ExistingLedgerMutation>(
        r#"
        SELECT variant_id, delta, movement_kind
        FROM inventory_ledger
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id)
    .bind(&mutation_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.variant_id != availability.id
            || existing.delta != payload.delta
            || existing.movement_kind != movement_kind
        {
            return Err(CommerceError::Conflict);
        }
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return inventory_adjustment_view(state, &sku, payload.delta, &movement_kind).await;
    }

    let projected_on_hand = availability
        .on_hand
        .saturating_add(i64::from(payload.delta));
    if payload.delta < 0
        && !availability.sell_without_stock
        && projected_on_hand < availability.reserved
    {
        return Err(CommerceError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO inventory_ledger (
            workspace_id, variant_id, delta, movement_kind, idempotency_key,
            actor_kind, actor_id, reason
        )
        VALUES ($1, $2, $3, $4, $5, 'admin', $6, $7)
        "#,
    )
    .bind(workspace_id)
    .bind(availability.id)
    .bind(payload.delta)
    .bind(&movement_kind)
    .bind(&mutation_key)
    .bind(actor_id)
    .bind(reason)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    inventory_adjustment_view(state, &sku, payload.delta, &movement_kind).await
}

async fn inventory_adjustment_view(
    state: &crate::AppState,
    sku: &str,
    delta: i32,
    movement_kind: &str,
) -> Result<InventoryAdjustmentView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = variant_availability(state.ticketing.pool(), workspace_id, sku).await?;
    Ok(InventoryAdjustmentView {
        sku: row.sku,
        delta,
        movement_kind: movement_kind.to_owned(),
        on_hand: row.on_hand,
        reserved: row.reserved,
        available_quantity: row.on_hand.saturating_sub(row.reserved),
    })
}

async fn reserve_inventory_inner(
    state: &crate::AppState,
    payload: ReserveInventoryRequest,
) -> Result<InventoryReservationView, CommerceError> {
    require_inventory_writes(state).await?;
    let normalized = normalize_reservation(payload)?;
    let request_hash = reservation_request_hash(&normalized)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;
    expire_due_reservations(&mut transaction, workspace_id).await?;

    if let Some(existing) = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1
          AND reservation_kind = 'order'
          AND external_reference = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&normalized.external_reference)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.request_hash != request_hash || existing.status != "active" {
            return Err(CommerceError::Conflict);
        }
        let view = load_reservation_view_tx(&mut transaction, workspace_id, existing.id).await?;
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return Ok(view);
    }

    let mut locked = Vec::with_capacity(normalized.items.len());
    for item in &normalized.items {
        let row = lock_variant_availability(&mut transaction, workspace_id, &item.sku).await?;
        let available = row.on_hand.saturating_sub(row.reserved);
        if !row.sell_without_stock && available < i64::from(item.quantity) {
            return Err(CommerceError::Conflict);
        }
        locked.push((row.id, item));
    }

    let reservation_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO inventory_reservations (
            id, workspace_id, reservation_kind, external_reference,
            request_hash, status, expires_at
        )
        VALUES ($1, $2, 'order', $3, $4, 'active', $5)
        "#,
    )
    .bind(reservation_id)
    .bind(workspace_id)
    .bind(&normalized.external_reference)
    .bind(request_hash)
    .bind(normalized.expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    for (variant_id, item) in locked {
        sqlx::query(
            r#"
            INSERT INTO inventory_reservation_items (
                workspace_id, reservation_id, variant_id, quantity
            )
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(item.quantity)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}

async fn commit_inventory_inner(
    state: &crate::AppState,
    reservation_id: Uuid,
) -> Result<InventoryReservationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2 AND reservation_kind = 'order'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    match row.status.as_str() {
        "committed" => {
            let view =
                load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
            transaction.commit().await.map_err(CommerceError::sqlx)?;
            return Ok(view);
        }
        // Stripe payment is authoritative even when delivery of its signed
        // webhook was delayed beyond checkout expiry or arrived after an
        // out-of-order expiration/failure event released the reservation.
        // Committing an expired or released reservation can expose a temporary
        // negative stock correction, but it never loses a paid order and the
        // ledger idempotency key still prevents a double decrement.
        "active" | "expired" | "released" => {}
        _ => return Err(CommerceError::Conflict),
    }

    let items = reservation_items_tx(&mut transaction, workspace_id, reservation_id).await?;
    for item in &items {
        sqlx::query(
            r#"
            INSERT INTO inventory_ledger (
                workspace_id, variant_id, delta, movement_kind, idempotency_key,
                reservation_id, actor_kind, actor_id, reason
            )
            SELECT $1, variant.id, -$2, 'sale', $3, $4, 'stripe',
                   'stripe-checkout', 'paid Stripe checkout'
            FROM merch_variants AS variant
            WHERE variant.workspace_id = $1 AND variant.sku = $5
            ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(item.quantity)
        .bind(format!("reservation:{reservation_id}:{}", item.sku))
        .bind(reservation_id)
        .bind(&item.sku)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    sqlx::query(
        r#"
        UPDATE inventory_reservations
        SET status = 'committed', committed_at = now(),
            released_at = NULL, release_reason = NULL
        WHERE workspace_id = $1 AND id = $2 AND status IN ('active', 'expired', 'released')
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}

async fn release_inventory_inner(
    state: &crate::AppState,
    reservation_id: Uuid,
    reason: String,
) -> Result<InventoryReservationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let reason = clean_text(&reason, 240)?;
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let status: Option<String> = sqlx::query_scalar(
        r#"
        SELECT status
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2 AND reservation_kind = 'order'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    let status = status.ok_or(CommerceError::NotFound)?;
    match status.as_str() {
        "active" => {
            sqlx::query(
                r#"
                UPDATE inventory_reservations
                SET status = 'released', released_at = now(), release_reason = $3
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(reservation_id)
            .bind(reason)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "released" | "expired" => {}
        "committed" => return Err(CommerceError::Conflict),
        _ => return Err(CommerceError::Unexpected),
    }

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}

async fn load_promotion_recommendations(
    state: &crate::AppState,
) -> Result<Vec<PromotionRecommendationView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, PromotionRecommendationView>(
        r#"
        WITH stock AS (
            SELECT
                variant.id AS variant_id,
                COALESCE(SUM(ledger.delta), 0)::bigint AS on_hand,
                MIN(ledger.occurred_at) AS first_movement_at,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '7 days'
                ), 0)::bigint AS sold_7d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '30 days'
                ), 0)::bigint AS sold_30d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '90 days'
                ), 0)::bigint AS sold_90d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'promotional_issue'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '90 days'
                ), 0)::bigint AS promotional_issued_90d
            FROM merch_variants AS variant
            LEFT JOIN inventory_ledger AS ledger
              ON ledger.workspace_id = variant.workspace_id
             AND ledger.variant_id = variant.id
            WHERE variant.workspace_id = $1
            GROUP BY variant.id
        ), reservations AS (
            SELECT item.variant_id, COALESCE(SUM(item.quantity), 0)::bigint AS reserved
            FROM inventory_reservation_items AS item
            JOIN inventory_reservations AS reservation
              ON reservation.workspace_id = item.workspace_id
             AND reservation.id = item.reservation_id
            WHERE item.workspace_id = $1
              AND reservation.status = 'active'
              AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            GROUP BY item.variant_id
        ), event_pressure AS (
            SELECT COUNT(*)::bigint AS upcoming_events_60d
            FROM events
            WHERE workspace_id = $1
              AND status <> 'cancelled'
              AND starts_at >= now()
              AND starts_at < now() + interval '60 days'
        ), inputs AS (
            SELECT
                variant.sku,
                product.name AS product_name,
                variant.label AS variant_label,
                COALESCE(stock.on_hand, 0)::bigint AS on_hand,
                COALESCE(reservations.reserved, 0)::bigint AS reserved,
                GREATEST(
                    COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0),
                    0
                )::bigint AS available_quantity,
                COALESCE(stock.sold_7d, 0)::bigint AS sold_7d,
                COALESCE(stock.sold_30d, 0)::bigint AS sold_30d,
                COALESCE(stock.sold_90d, 0)::bigint AS sold_90d,
                COALESCE(stock.promotional_issued_90d, 0)::bigint AS promotional_issued_90d,
                event_pressure.upcoming_events_60d,
                GREATEST(
                    COALESCE(EXTRACT(day FROM now() - stock.first_movement_at), 0),
                    0
                )::integer AS history_days,
                GREATEST(
                    variant.low_stock_threshold::bigint * 2,
                    COALESCE(stock.sold_30d, 0)::bigint,
                    event_pressure.upcoming_events_60d * 2
                )::bigint AS safety_stock
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            LEFT JOIN stock ON stock.variant_id = variant.id
            LEFT JOIN reservations ON reservations.variant_id = variant.id
            CROSS JOIN event_pressure
            WHERE variant.workspace_id = $1
              AND variant.active
              AND product.active
        ), scored AS (
            SELECT
                inputs.*,
                CASE
                    WHEN sold_30d >= 3
                     AND sold_30d * 2 > GREATEST(sold_90d - sold_30d, 0)
                    THEN 0
                    WHEN history_days < 30
                    THEN LEAST(
                        GREATEST(available_quantity - safety_stock, 0),
                        available_quantity / 4
                    )
                    ELSE GREATEST(available_quantity - safety_stock, 0)
                END::bigint AS recommended_max_giveaway
            FROM inputs
        )
        SELECT
            sku,
            product_name,
            variant_label,
            on_hand,
            reserved,
            available_quantity,
            sold_7d,
            sold_30d,
            sold_90d,
            promotional_issued_90d,
            upcoming_events_60d,
            history_days,
            safety_stock,
            recommended_max_giveaway,
            CASE
                WHEN recommended_max_giveaway = 0 THEN 'hold'
                WHEN recommended_max_giveaway >= 5 THEN 'candidate'
                ELSE 'limited'
            END AS recommendation,
            CASE
                WHEN history_days < 30 THEN 'low'
                WHEN history_days < 90 THEN 'medium'
                ELSE 'high'
            END AS confidence,
            CASE
                WHEN sold_30d >= 3
                 AND sold_30d * 2 > GREATEST(sold_90d - sold_30d, 0)
                THEN 'Rosnąca sprzedaż — zachowaj stock dla zamówień.'
                WHEN available_quantity <= safety_stock
                THEN 'Dostępny stan nie przekracza konserwatywnego zapasu bezpieczeństwa.'
                WHEN history_days < 30
                THEN 'Historia jest krótka — rekomendacja ograniczona do 25% nadwyżki.'
                WHEN recommended_max_giveaway >= 5
                THEN 'Jest nadwyżka ponad sprzedaż, niski stan i presję najbliższych koncertów.'
                ELSE 'Możliwa mała akcja promocyjna bez naruszania zapasu bezpieczeństwa.'
            END AS reason
        FROM scored
        ORDER BY recommended_max_giveaway DESC, product_name, variant_label, sku
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn load_reward_campaigns(
    state: &crate::AppState,
) -> Result<Vec<RewardCampaignView>, CommerceError> {
    load_reward_campaigns_filtered(state, None).await
}

async fn load_reward_campaigns_filtered(
    state: &crate::AppState,
    draw_id: Option<Uuid>,
) -> Result<Vec<RewardCampaignView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, RewardCampaignView>(
        r#"
        SELECT
            draw.id,
            draw.slug,
            draw.name,
            draw.status,
            draw.eligibility_kind,
            event.slug AS event_slug,
            draw.winner_count,
            COALESCE(winner_totals.selected_winners, 0)::bigint AS selected_winners,
            variant.sku AS prize_sku,
            product.name AS prize_name,
            variant.label AS prize_variant,
            allocation.units_per_winner,
            COALESCE(reservation_item.quantity, 0)::integer AS reserved_quantity,
            COALESCE(fulfillment_totals.pending_fulfillments, 0)::bigint AS pending_fulfillments,
            COALESCE(fulfillment_totals.delivered_fulfillments, 0)::bigint AS delivered_fulfillments,
            draw.opens_at,
            draw.closes_at,
            draw.draw_at,
            draw.completed_at
        FROM reward_draws AS draw
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = draw.workspace_id
         AND allocation.draw_id = draw.id
        JOIN merch_variants AS variant
          ON variant.workspace_id = allocation.workspace_id
         AND variant.id = allocation.variant_id
        LEFT JOIN inventory_reservations AS allocation_reservation
          ON allocation_reservation.workspace_id = allocation.workspace_id
         AND allocation_reservation.id = allocation.reservation_id
         AND allocation_reservation.status = 'active'
        LEFT JOIN inventory_reservation_items AS reservation_item
          ON reservation_item.workspace_id = allocation_reservation.workspace_id
         AND reservation_item.reservation_id = allocation_reservation.id
         AND reservation_item.variant_id = allocation.variant_id
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        LEFT JOIN events AS event
          ON event.workspace_id = draw.workspace_id
         AND event.id = draw.event_id
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS selected_winners
            FROM reward_draw_winners AS winner
            WHERE winner.workspace_id = draw.workspace_id
              AND winner.draw_id = draw.id
        ) AS winner_totals ON true
        LEFT JOIN LATERAL (
            SELECT
                COUNT(*) FILTER (WHERE fulfillment.status IN ('pending', 'prepared'))::bigint
                    AS pending_fulfillments,
                COUNT(*) FILTER (WHERE fulfillment.status = 'delivered')::bigint
                    AS delivered_fulfillments
            FROM reward_draw_fulfillments AS fulfillment
            WHERE fulfillment.workspace_id = draw.workspace_id
              AND fulfillment.draw_id = draw.id
        ) AS fulfillment_totals ON true
        WHERE draw.workspace_id = $1
          AND ($2::uuid IS NULL OR draw.id = $2)
        ORDER BY draw.created_at DESC, draw.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn create_reward_campaign_inner(
    state: &crate::AppState,
    payload: CreateRewardCampaignRequest,
) -> Result<RewardCampaignView, CommerceError> {
    require_inventory_writes(state).await?;
    validate_reward_campaign(&payload)?;
    if payload.status == "scheduled"
        && !matches!(
            crate::ecosystem::feature_enabled(state, "reward_campaigns_enabled").await,
            Ok(true)
        )
    {
        return Err(CommerceError::Conflict);
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let slug = normalize_slug(&payload.slug)?;
    let name = clean_text(&payload.name, 200)?;
    let prize_sku = clean_text(&payload.prize_sku, 128)?;
    let event_slug = payload
        .event_slug
        .as_deref()
        .map(normalize_slug)
        .transpose()?;
    let reserved_quantity = payload
        .winner_count
        .checked_mul(payload.units_per_winner)
        .ok_or(CommerceError::Invalid)?;

    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let already_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reward_draws WHERE workspace_id = $1 AND slug = $2)",
    )
    .bind(workspace_id)
    .bind(&slug)
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    if already_exists {
        return Err(CommerceError::Conflict);
    }

    let variant = lock_variant_availability(&mut transaction, workspace_id, &prize_sku).await?;
    if !variant.sell_without_stock
        && variant.on_hand.saturating_sub(variant.reserved) < i64::from(reserved_quantity)
    {
        return Err(CommerceError::Conflict);
    }

    let event_id = match payload.eligibility_kind.as_str() {
        "all_active" => None,
        "event_interest" => {
            let slug = event_slug.as_deref().ok_or(CommerceError::Invalid)?;
            Some(
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    SELECT id
                    FROM events
                    WHERE workspace_id = $1 AND slug = $2 AND status <> 'cancelled'
                    FOR SHARE
                    "#,
                )
                .bind(workspace_id)
                .bind(slug)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(CommerceError::sqlx)?
                .ok_or(CommerceError::NotFound)?,
            )
        }
        _ => return Err(CommerceError::Invalid),
    };

    let reward_rule_id = Uuid::now_v7();
    let draw_id = Uuid::now_v7();
    let reservation_id = Uuid::now_v7();
    let expires_days = (payload.claim_expires_hours.saturating_add(23) / 24).clamp(1, 3650);
    sqlx::query(
        r#"
        INSERT INTO reward_rules (
            id, workspace_id, name, reward_type, threshold, config, active
        )
        VALUES ($1, $2, $3, 'physical_item', NULL, $4, true)
        "#,
    )
    .bind(reward_rule_id)
    .bind(workspace_id)
    .bind(format!("campaign:{slug}"))
    .bind(json!({
        "item_name": variant.product_name,
        "sku": variant.sku,
        "expires_days": expires_days,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO reward_draws (
            id, workspace_id, slug, name, prize_kind, eligibility_kind,
            event_id, reward_rule_id, winner_count, base_entries,
            entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, opens_at, closes_at, draw_at, status
        )
        VALUES (
            $1, $2, $3, $4, 'physical_item', $5,
            $6, $7, $8, $9, $10, $11, $12,
            $13, $14, $15, $16, $17
        )
        "#,
    )
    .bind(draw_id)
    .bind(workspace_id)
    .bind(&slug)
    .bind(name)
    .bind(&payload.eligibility_kind)
    .bind(event_id)
    .bind(reward_rule_id)
    .bind(payload.winner_count)
    .bind(payload.base_entries)
    .bind(payload.entries_per_referral)
    .bind(payload.entries_per_checkin)
    .bind(payload.max_entries)
    .bind(payload.claim_expires_hours)
    .bind(payload.opens_at)
    .bind(payload.closes_at)
    .bind(payload.draw_at)
    .bind(&payload.status)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let reservation_hash = Sha256::digest(
        serde_json::to_vec(&json!({
            "draw_id": draw_id,
            "sku": prize_sku,
            "quantity": reserved_quantity,
        }))
        .map_err(|_| CommerceError::Unexpected)?,
    );
    sqlx::query(
        r#"
        INSERT INTO inventory_reservations (
            id, workspace_id, reservation_kind, external_reference,
            request_hash, status, expires_at
        )
        VALUES ($1, $2, 'campaign', $3, $4, 'active', NULL)
        "#,
    )
    .bind(reservation_id)
    .bind(workspace_id)
    .bind(format!("reward-draw:{draw_id}"))
    .bind(reservation_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO inventory_reservation_items (
            workspace_id, reservation_id, variant_id, quantity
        )
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .bind(variant.id)
    .bind(reserved_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO reward_draw_inventory_allocations (
            workspace_id, draw_id, variant_id, reservation_id,
            units_per_winner, reserved_quantity
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .bind(variant.id)
    .bind(reservation_id)
    .bind(payload.units_per_winner)
    .bind(reserved_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn schedule_reward_campaign_inner(
    state: &crate::AppState,
    draw_id: Uuid,
) -> Result<RewardCampaignView, CommerceError> {
    if !matches!(
        crate::ecosystem::feature_enabled(state, "reward_campaigns_enabled").await,
        Ok(true)
    ) {
        return Err(CommerceError::Conflict);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let changed = sqlx::query(
        r#"
        UPDATE reward_draws
        SET status = 'scheduled', updated_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'draft'
          AND closes_at > now()
          AND draw_at >= closes_at
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .rows_affected();
    if changed != 1 {
        return Err(CommerceError::Conflict);
    }
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn cancel_reward_campaign_inner(
    state: &crate::AppState,
    draw_id: Uuid,
) -> Result<RewardCampaignView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, (String, Uuid, Uuid)>(
        r#"
        SELECT draw.status, draw.reward_rule_id, allocation.reservation_id
        FROM reward_draws AS draw
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = draw.workspace_id
         AND allocation.draw_id = draw.id
        WHERE draw.workspace_id = $1 AND draw.id = $2
        FOR UPDATE OF draw, allocation
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    match row.0.as_str() {
        "cancelled" => {}
        "draft" | "scheduled" => {
            sqlx::query(
                "UPDATE reward_draws SET status = 'cancelled' WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id)
            .bind(draw_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE inventory_reservations
                SET status = 'released', released_at = now(),
                    release_reason = 'reward campaign cancelled'
                WHERE workspace_id = $1 AND id = $2 AND status = 'active'
                "#,
            )
            .bind(workspace_id)
            .bind(row.2)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                "UPDATE reward_rules SET active = false WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id)
            .bind(row.1)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        _ => return Err(CommerceError::Conflict),
    }

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn load_reward_fulfillments(
    state: &crate::AppState,
) -> Result<Vec<RewardFulfillmentView>, CommerceError> {
    load_reward_fulfillments_filtered(state, None).await
}

async fn load_reward_fulfillments_filtered(
    state: &crate::AppState,
    winner_id: Option<Uuid>,
) -> Result<Vec<RewardFulfillmentView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, RewardFulfillmentView>(
        r#"
        SELECT
            fulfillment.id,
            fulfillment.winner_id,
            fulfillment.draw_id,
            draw.slug AS draw_slug,
            winner.winner_rank,
            fan.display_name AS fan_display_name,
            CASE
                WHEN position('@' IN fan.normalized_email) > 1
                THEN left(fan.normalized_email, 1) || '***@' || split_part(fan.normalized_email, '@', 2)
                ELSE '***'
            END AS fan_email_masked,
            variant.sku AS prize_sku,
            product.name AS prize_name,
            variant.label AS prize_variant,
            fulfillment.quantity,
            fulfillment.status,
            fulfillment.created_at,
            fulfillment.updated_at
        FROM reward_draw_fulfillments AS fulfillment
        JOIN reward_draws AS draw
          ON draw.workspace_id = fulfillment.workspace_id
         AND draw.id = fulfillment.draw_id
        JOIN reward_draw_winners AS winner
          ON winner.workspace_id = fulfillment.workspace_id
         AND winner.id = fulfillment.winner_id
        JOIN fans AS fan
          ON fan.workspace_id = winner.workspace_id
         AND fan.id = winner.fan_id
        JOIN merch_variants AS variant
          ON variant.workspace_id = fulfillment.workspace_id
         AND variant.id = fulfillment.variant_id
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE fulfillment.workspace_id = $1
          AND ($2::uuid IS NULL OR fulfillment.winner_id = $2)
        ORDER BY fulfillment.created_at DESC, fulfillment.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(winner_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn fulfill_reward_inner(
    state: &crate::AppState,
    winner_id: Uuid,
    payload: FulfillRewardRequest,
) -> Result<RewardFulfillmentView, CommerceError> {
    let status = clean_fulfillment_status(&payload.status)?;
    if status == "delivered" {
        require_inventory_writes(state).await?;
    }
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?;
    let note = optional_text(payload.note.as_deref(), 500)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, FulfillmentMutationRow>(
        r#"
        SELECT
            fulfillment.id,
            fulfillment.reward_grant_id,
            fulfillment.variant_id,
            allocation.reservation_id,
            fulfillment.quantity,
            fulfillment.status
        FROM reward_draw_fulfillments AS fulfillment
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = fulfillment.workspace_id
         AND allocation.draw_id = fulfillment.draw_id
        WHERE fulfillment.workspace_id = $1 AND fulfillment.winner_id = $2
        FOR UPDATE OF fulfillment, allocation
        "#,
    )
    .bind(workspace_id)
    .bind(winner_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    if row.status == status {
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return load_reward_fulfillments_filtered(state, Some(winner_id))
            .await?
            .into_iter()
            .next()
            .ok_or(CommerceError::Unexpected);
    }
    if matches!(row.status.as_str(), "delivered" | "cancelled") {
        return Err(CommerceError::Conflict);
    }

    match status.as_str() {
        "prepared" => {
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'prepared', prepared_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "delivered" => {
            sqlx::query(
                r#"
                INSERT INTO inventory_ledger (
                    workspace_id, variant_id, delta, movement_kind, idempotency_key,
                    reservation_id, actor_kind, actor_id, reason
                )
                VALUES ($1, $2, -$3, 'promotional_issue', $4, $5, 'staff', $6, $7)
                ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
                "#,
            )
            .bind(workspace_id)
            .bind(row.variant_id)
            .bind(row.quantity)
            .bind(format!("reward-fulfillment:{}", row.id))
            .bind(row.reservation_id)
            .bind(actor_id.as_deref())
            .bind(note.as_deref().map_or("reward delivered", |value| value))
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            consume_campaign_reservation_item(
                &mut transaction,
                workspace_id,
                row.reservation_id,
                row.variant_id,
                row.quantity,
            )
            .await?;
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'delivered',
                    prepared_at = COALESCE(prepared_at, now()),
                    delivered_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE reward_grants
                SET status = 'delivered', delivered_at = COALESCE(delivered_at, now())
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(row.reward_grant_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "cancelled" => {
            consume_campaign_reservation_item(
                &mut transaction,
                workspace_id,
                row.reservation_id,
                row.variant_id,
                row.quantity,
            )
            .await?;
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'cancelled', cancelled_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE reward_grants
                SET status = 'revoked', revoked_at = COALESCE(revoked_at, now())
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(row.reward_grant_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        _ => return Err(CommerceError::Invalid),
    }

    finalize_campaign_reservation_if_empty(&mut transaction, workspace_id, row.reservation_id)
        .await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_fulfillments_filtered(state, Some(winner_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn consume_campaign_reservation_item(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
    variant_id: Uuid,
    quantity: i32,
) -> Result<(), CommerceError> {
    let current: i32 = sqlx::query_scalar(
        r#"
        SELECT quantity
        FROM inventory_reservation_items
        WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .bind(variant_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::Conflict)?;
    if current < quantity {
        return Err(CommerceError::Conflict);
    }
    if current == quantity {
        sqlx::query(
            r#"
            DELETE FROM inventory_reservation_items
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .execute(&mut **transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    } else {
        sqlx::query(
            r#"
            UPDATE inventory_reservation_items
            SET quantity = quantity - $4
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(quantity)
        .execute(&mut **transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }
    Ok(())
}

async fn finalize_campaign_reservation_if_empty(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        UPDATE inventory_reservations AS reservation
        SET status = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN 'committed'
                ELSE 'released'
            END,
            committed_at = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN now()
                ELSE NULL
            END,
            released_at = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN NULL
                ELSE now()
            END,
            release_reason = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN NULL
                ELSE 'campaign allocation closed without delivery'
            END
        WHERE reservation.workspace_id = $1
          AND reservation.id = $2
          AND reservation.status = 'active'
          AND NOT EXISTS (
              SELECT 1
              FROM inventory_reservation_items AS item
              WHERE item.workspace_id = reservation.workspace_id
                AND item.reservation_id = reservation.id
          )
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .execute(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn reservation_items_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<Vec<InventoryReservationItemView>, CommerceError> {
    sqlx::query_as::<_, InventoryReservationItemView>(
        r#"
        SELECT variant.sku, variant.label, item.quantity
        FROM inventory_reservation_items AS item
        JOIN merch_variants AS variant
          ON variant.workspace_id = item.workspace_id
         AND variant.id = item.variant_id
        WHERE item.workspace_id = $1 AND item.reservation_id = $2
        ORDER BY variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)
}

async fn load_reservation_view_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<InventoryReservationView, CommerceError> {
    let row = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;
    let items = reservation_items_tx(transaction, workspace_id, reservation_id).await?;
    Ok(InventoryReservationView {
        id: row.id,
        external_reference: row.external_reference,
        status: row.status,
        expires_at: row.expires_at,
        items,
    })
}

async fn expire_due_reservations(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        UPDATE inventory_reservations
        SET status = 'expired', released_at = now(), release_reason = 'reservation expired'
        WHERE workspace_id = $1
          AND status = 'active'
          AND expires_at IS NOT NULL
          AND expires_at <= now()
        "#,
    )
    .bind(workspace_id)
    .execute(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn lock_variant_availability(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    sku: &str,
) -> Result<VariantAvailabilityRow, CommerceError> {
    sqlx::query_as::<_, VariantAvailabilityRow>(
        r#"
        SELECT
            variant.id,
            product.name AS product_name,
            variant.sku,
            variant.sell_without_stock,
            COALESCE((
                SELECT SUM(ledger.delta)::bigint
                FROM inventory_ledger AS ledger
                WHERE ledger.workspace_id = variant.workspace_id
                  AND ledger.variant_id = variant.id
            ), 0)::bigint AS on_hand,
            COALESCE((
                SELECT SUM(item.quantity)::bigint
                FROM inventory_reservation_items AS item
                JOIN inventory_reservations AS reservation
                  ON reservation.workspace_id = item.workspace_id
                 AND reservation.id = item.reservation_id
                WHERE item.workspace_id = variant.workspace_id
                  AND item.variant_id = variant.id
                  AND reservation.status = 'active'
                  AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            ), 0)::bigint AS reserved
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1 AND variant.sku = $2 AND variant.active
        FOR UPDATE OF variant
        "#,
    )
    .bind(workspace_id)
    .bind(sku)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)
}

async fn variant_availability(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    sku: &str,
) -> Result<VariantAvailabilityRow, CommerceError> {
    sqlx::query_as::<_, VariantAvailabilityRow>(
        r#"
        SELECT
            variant.id,
            product.name AS product_name,
            variant.sku,
            variant.sell_without_stock,
            COALESCE((
                SELECT SUM(ledger.delta)::bigint
                FROM inventory_ledger AS ledger
                WHERE ledger.workspace_id = variant.workspace_id
                  AND ledger.variant_id = variant.id
            ), 0)::bigint AS on_hand,
            COALESCE((
                SELECT SUM(item.quantity)::bigint
                FROM inventory_reservation_items AS item
                JOIN inventory_reservations AS reservation
                  ON reservation.workspace_id = item.workspace_id
                 AND reservation.id = item.reservation_id
                WHERE item.workspace_id = variant.workspace_id
                  AND item.variant_id = variant.id
                  AND reservation.status = 'active'
                  AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            ), 0)::bigint AS reserved
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1 AND variant.sku = $2
        "#,
    )
    .bind(workspace_id)
    .bind(sku)
    .fetch_optional(pool)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(expires_at: OffsetDateTime, items: Vec<(&str, i32)>) -> ReserveInventoryRequest {
        ReserveInventoryRequest {
            external_reference: "checkout-123".to_owned(),
            expires_at,
            items: items
                .into_iter()
                .map(|(sku, quantity)| ReserveInventoryItemRequest {
                    sku: sku.to_owned(),
                    quantity,
                })
                .collect(),
        }
    }

    #[test]
    fn reservation_hash_is_stable_across_expiry_retries() {
        let first = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-B", 1), ("SKU-A", 2)],
        );
        let second = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(31),
            vec![("SKU-A", 2), ("SKU-B", 1)],
        );
        let first = normalize_reservation(first);
        let second = normalize_reservation(second);
        assert!(first.is_ok());
        assert!(second.is_ok());
        let Ok(first) = first else { return };
        let Ok(second) = second else { return };
        let first_hash = reservation_request_hash(&first);
        let second_hash = reservation_request_hash(&second);
        assert!(first_hash.is_ok());
        assert!(second_hash.is_ok());
        let Ok(first_hash) = first_hash else { return };
        let Ok(second_hash) = second_hash else { return };
        assert_eq!(first_hash, second_hash);
    }

    #[test]
    fn reservation_normalization_merges_duplicate_skus() {
        let normalized = normalize_reservation(reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-A", 1), ("SKU-B", 1), ("SKU-A", 2)],
        ));
        assert!(normalized.is_ok());
        let Ok(normalized) = normalized else { return };

        assert_eq!(normalized.items.len(), 2);
        assert_eq!(normalized.items[0].sku, "SKU-A");
        assert_eq!(normalized.items[0].quantity, 3);
        assert_eq!(normalized.items[1].sku, "SKU-B");
    }

    #[test]
    fn product_slug_is_canonicalized_before_persistence() {
        let normalized = normalize_slug("  Echoes-CD  ");
        assert_eq!(normalized.as_deref(), Ok("echoes-cd"));
        assert!(matches!(
            normalize_slug("echoes cd"),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn unsafe_inventory_mutation_keys_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY,
            axum::http::HeaderValue::from_static("tiny"),
        );
        assert!(matches!(
            idempotency_key(&headers),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn valid_inventory_mutation_key_is_accepted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY,
            axum::http::HeaderValue::from_static("adjust-2026-08-05-0001"),
        );
        assert_eq!(
            idempotency_key(&headers).as_deref(),
            Ok("adjust-2026-08-05-0001")
        );
    }

    #[test]
    fn reservation_hash_changes_when_items_or_reference_change() {
        let base = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-A", 2)],
        );
        let mut different_reference = base.clone();
        different_reference.external_reference = "checkout-456".to_owned();
        let different_quantity = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-A", 3)],
        );

        let base_hash = reservation_request_hash(&normalize_reservation(base).unwrap()).unwrap();
        let reference_hash =
            reservation_request_hash(&normalize_reservation(different_reference).unwrap()).unwrap();
        let quantity_hash =
            reservation_request_hash(&normalize_reservation(different_quantity).unwrap()).unwrap();

        assert_ne!(base_hash, reference_hash);
        assert_ne!(base_hash, quantity_hash);
    }

    #[test]
    fn reservation_rejects_expiry_outside_the_allowed_window() {
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            normalize_reservation(reservation(
                now + TimeDuration::seconds(30),
                vec![("SKU-A", 1)]
            )),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            normalize_reservation(reservation(
                now + TimeDuration::hours(25),
                vec![("SKU-A", 1)]
            )),
            Err(CommerceError::Invalid)
        ));
        assert!(
            normalize_reservation(reservation(
                now + TimeDuration::hours(1),
                vec![("SKU-A", 1)]
            ))
            .is_ok()
        );
    }

    #[test]
    fn reservation_rejects_empty_items_and_quantity_overflow() {
        assert!(matches!(
            normalize_reservation(reservation(
                OffsetDateTime::now_utc() + TimeDuration::minutes(30),
                vec![],
            )),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            normalize_reservation(reservation(
                OffsetDateTime::now_utc() + TimeDuration::minutes(30),
                vec![("SKU-A", MAX_RESERVATION_QUANTITY + 1)],
            )),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            normalize_reservation(reservation(
                OffsetDateTime::now_utc() + TimeDuration::minutes(30),
                vec![("SKU-A", MAX_RESERVATION_QUANTITY), ("SKU-A", 1)],
            )),
            Err(CommerceError::Invalid)
        ));
    }

    fn reward_campaign(status: &str, eligibility_kind: &str) -> CreateRewardCampaignRequest {
        let now = OffsetDateTime::now_utc();
        CreateRewardCampaignRequest {
            slug: "cd-giveaway".to_owned(),
            name: "CD giveaway".to_owned(),
            prize_sku: "virya-signal-cd".to_owned(),
            winner_count: 5,
            units_per_winner: 1,
            eligibility_kind: eligibility_kind.to_owned(),
            event_slug: if eligibility_kind == "event_interest" {
                Some("wroclaw-2026".to_owned())
            } else {
                None
            },
            base_entries: 1,
            entries_per_referral: 1,
            entries_per_checkin: 0,
            max_entries: 1_000,
            claim_expires_hours: 168,
            opens_at: now,
            closes_at: now + TimeDuration::days(7),
            draw_at: now + TimeDuration::days(8),
            status: status.to_owned(),
        }
    }

    #[test]
    fn reward_campaign_accepts_a_well_formed_payload() {
        assert!(validate_reward_campaign(&reward_campaign("draft", "all_active")).is_ok());
        assert!(validate_reward_campaign(&reward_campaign("scheduled", "event_interest")).is_ok());
    }

    #[test]
    fn reward_campaign_rejects_out_of_order_milestones() {
        let mut payload = reward_campaign("draft", "all_active");
        payload.closes_at = payload.opens_at;
        assert!(matches!(
            validate_reward_campaign(&payload),
            Err(CommerceError::Invalid)
        ));

        let mut payload = reward_campaign("draft", "all_active");
        payload.draw_at = payload.closes_at - TimeDuration::minutes(1);
        assert!(matches!(
            validate_reward_campaign(&payload),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn reward_campaign_rejects_max_entries_below_base_entries() {
        let mut payload = reward_campaign("draft", "all_active");
        payload.base_entries = 100;
        payload.max_entries = 50;
        assert!(matches!(
            validate_reward_campaign(&payload),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn reward_campaign_event_interest_requires_a_slug_and_all_active_forbids_one() {
        let mut missing_slug = reward_campaign("draft", "event_interest");
        missing_slug.event_slug = None;
        assert!(matches!(
            validate_reward_campaign(&missing_slug),
            Err(CommerceError::Invalid)
        ));

        let mut unexpected_slug = reward_campaign("draft", "all_active");
        unexpected_slug.event_slug = Some("wroclaw-2026".to_owned());
        assert!(matches!(
            validate_reward_campaign(&unexpected_slug),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn reward_campaign_rejects_unknown_status_and_eligibility() {
        assert!(matches!(
            validate_reward_campaign(&reward_campaign("active", "all_active")),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            validate_reward_campaign(&reward_campaign("draft", "everyone")),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn reward_campaign_rejects_out_of_range_counts() {
        let mut payload = reward_campaign("draft", "all_active");
        payload.winner_count = 0;
        assert!(matches!(
            validate_reward_campaign(&payload),
            Err(CommerceError::Invalid)
        ));

        let mut payload = reward_campaign("draft", "all_active");
        payload.units_per_winner = 101;
        assert!(matches!(
            validate_reward_campaign(&payload),
            Err(CommerceError::Invalid)
        ));
    }

    fn variant(sku: &str) -> UpsertVariantRequest {
        UpsertVariantRequest {
            sku: sku.to_owned(),
            label: "Default".to_owned(),
            attributes: empty_object(),
            active: true,
            low_stock_threshold: 3,
            sell_without_stock: false,
        }
    }

    fn product(slug: &str, variants: Vec<UpsertVariantRequest>) -> UpsertProductRequest {
        UpsertProductRequest {
            slug: slug.to_owned(),
            name: "Signal (CD)".to_owned(),
            description: None,
            image_url: None,
            currency: "PLN".to_owned(),
            price_gross_minor: 4_999,
            active: true,
            public: true,
            variants,
        }
    }

    #[test]
    fn catalog_accepts_a_well_formed_payload() {
        let payload = UpsertCatalogRequest {
            products: vec![product("signal-cd", vec![variant("VIRYA-CD")])],
        };
        assert!(validate_catalog(&payload).is_ok());
    }

    #[test]
    fn catalog_rejects_duplicate_slugs_and_skus() {
        let duplicate_slugs = UpsertCatalogRequest {
            products: vec![
                product("signal-cd", vec![variant("VIRYA-CD")]),
                product("signal-cd", vec![variant("VIRYA-CD-2")]),
            ],
        };
        assert!(matches!(
            validate_catalog(&duplicate_slugs),
            Err(CommerceError::Invalid)
        ));

        let duplicate_skus = UpsertCatalogRequest {
            products: vec![
                product("signal-cd", vec![variant("VIRYA-CD")]),
                product("signal-vinyl", vec![variant("VIRYA-CD")]),
            ],
        };
        assert!(matches!(
            validate_catalog(&duplicate_skus),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn catalog_rejects_empty_products_and_empty_variants() {
        assert!(matches!(
            validate_catalog(&UpsertCatalogRequest { products: vec![] }),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            validate_catalog(&UpsertCatalogRequest {
                products: vec![product("signal-cd", vec![])],
            }),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn catalog_rejects_malformed_currency_and_negative_price() {
        let mut bad_currency = product("signal-cd", vec![variant("VIRYA-CD")]);
        bad_currency.currency = "PL".to_owned();
        assert!(matches!(
            validate_catalog(&UpsertCatalogRequest {
                products: vec![bad_currency],
            }),
            Err(CommerceError::Invalid)
        ));

        let mut negative_price = product("signal-cd", vec![variant("VIRYA-CD")]);
        negative_price.price_gross_minor = -1;
        assert!(matches!(
            validate_catalog(&UpsertCatalogRequest {
                products: vec![negative_price],
            }),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn movement_kind_only_allows_the_manual_admin_actions() {
        for kind in [
            "initial",
            "receipt",
            "refund",
            "adjustment",
            "damage",
            "staff_issue",
        ] {
            assert_eq!(clean_movement_kind(kind).as_deref(), Ok(kind));
        }
        for kind in ["sale", "promotional_issue", "bogus"] {
            assert!(matches!(
                clean_movement_kind(kind),
                Err(CommerceError::Invalid)
            ));
        }
    }

    #[test]
    fn fulfillment_status_only_allows_the_documented_transitions() {
        for status in ["prepared", "delivered", "cancelled"] {
            assert_eq!(clean_fulfillment_status(status).as_deref(), Ok(status));
        }
        assert!(matches!(
            clean_fulfillment_status("pending"),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn https_url_validation_rejects_non_https_and_embedded_credentials() {
        assert_eq!(validate_optional_https_url(None), Ok(None));
        assert!(
            validate_optional_https_url(Some("https://cdn.example.com/cover.jpg"))
                .is_ok_and(|value| value.is_some())
        );
        assert!(matches!(
            validate_optional_https_url(Some("http://cdn.example.com/cover.jpg")),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            validate_optional_https_url(Some("https://user:pass@cdn.example.com/cover.jpg")),
            Err(CommerceError::Invalid)
        ));
        assert!(matches!(
            validate_optional_https_url(Some("not a url")),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn duration_milliseconds_rejects_zero_and_overflow() {
        assert!(matches!(
            duration_milliseconds(Duration::from_millis(0)),
            Err(CommerceError::Unexpected)
        ));
        assert!(matches!(
            duration_milliseconds(Duration::from_secs(u64::MAX)),
            Err(CommerceError::Unexpected)
        ));
        assert_eq!(duration_milliseconds(Duration::from_millis(250)), Ok(250));
    }
}
