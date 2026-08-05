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
const MAX_STOCKTAKE_ITEMS: usize = 500;
const MAX_STOCK_ON_HAND: i32 = 1_000_000;
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
pub struct InventoryStocktakeRequest {
    items: Vec<InventoryStocktakeItemRequest>,
    actor_id: Option<String>,
    reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InventoryStocktakeItemRequest {
    sku: String,
    on_hand: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryStocktakeView {
    id: Uuid,
    replayed: bool,
    created_at: OffsetDateTime,
    items: Vec<InventoryStocktakeItemView>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
struct InventoryStocktakeItemView {
    sku: String,
    label: String,
    target_on_hand: i32,
    on_hand_before: i64,
    reserved_at_apply: i64,
    applied_delta: i32,
    available_quantity: i64,
}

#[derive(Debug, FromRow)]
struct ExistingStocktake {
    id: Uuid,
    request_hash: Vec<u8>,
    created_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryOverviewView {
    generated_at: OffsetDateTime,
    items: Vec<InventoryOverviewItemView>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
struct InventoryOverviewItemView {
    product_slug: String,
    product_name: String,
    variant_id: Uuid,
    sku: String,
    variant_label: String,
    attributes: Value,
    active: bool,
    low_stock_threshold: i32,
    sell_without_stock: bool,
    counted: bool,
    last_counted_at: Option<OffsetDateTime>,
    on_hand: i64,
    order_reserved: i64,
    campaign_reserved: i64,
    operational_reserved: i64,
    reserved: i64,
    available_quantity: i64,
    sold_total: i64,
    sold_30d: i64,
    promotional_issued_total: i64,
    active_campaigns: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryActivationView {
    status: String,
    ready: bool,
    fully_enabled: bool,
    catalog_seed_version: i32,
    catalog_seeded_at: Option<OffsetDateTime>,
    ready_at: Option<OffsetDateTime>,
    ready_by: Option<String>,
    version: i64,
    total_active_variants: i64,
    counted_active_variants: i64,
    missing_skus: Vec<String>,
    blockers: Vec<String>,
    can_mark_ready: bool,
    public_enabled: bool,
    writes_enabled: bool,
    campaigns_enabled: bool,
}

#[derive(Debug, FromRow)]
struct InventoryActivationRow {
    status: String,
    catalog_seed_version: i32,
    catalog_seeded_at: Option<OffsetDateTime>,
    ready_at: Option<OffsetDateTime>,
    ready_by: Option<String>,
    version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkInventoryReadyRequest {
    actor_id: Option<String>,
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
    ) || !matches!(inventory_ready(&state).await, Ok(true))
    {
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

pub async fn inventory_activation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_inventory_activation(&state),
    )
    .await
    {
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

pub async fn inventory_overview(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_inventory_overview(&state),
    )
    .await
    {
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

pub async fn inventory_stocktake(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<InventoryStocktakeRequest>, JsonRejection>,
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
    let future = inventory_stocktake_inner(&state, mutation_key, payload);
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

pub async fn mark_inventory_ready(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<MarkInventoryReadyRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = mark_inventory_ready_inner(&state, payload, request_id_value.as_deref());
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
    ) && matches!(inventory_ready(state).await, Ok(true))
    {
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

async fn ensure_inventory_activation_row(state: &crate::AppState) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        INSERT INTO inventory_activation_state (workspace_id)
        VALUES ($1)
        ON CONFLICT (workspace_id) DO NOTHING
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .execute(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn inventory_ready(state: &crate::AppState) -> Result<bool, CommerceError> {
    ensure_inventory_activation_row(state).await?;
    sqlx::query_scalar::<_, bool>(
        "SELECT status = 'ready' FROM inventory_activation_state WHERE workspace_id = $1",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn load_inventory_activation(
    state: &crate::AppState,
) -> Result<InventoryActivationView, CommerceError> {
    ensure_inventory_activation_row(state).await?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, InventoryActivationRow>(
        r#"
        SELECT status, catalog_seed_version, catalog_seeded_at,
               ready_at, ready_by, version
        FROM inventory_activation_state
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let total_active_variants = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1 AND variant.active AND product.active
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let missing_skus = sqlx::query_scalar::<_, String>(
        r#"
        SELECT variant.sku
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1
          AND variant.active
          AND product.active
          AND NOT EXISTS (
              SELECT 1
              FROM inventory_stocktake_items AS item
              WHERE item.workspace_id = variant.workspace_id
                AND item.variant_id = variant.id
          )
        ORDER BY product.slug, variant.label, variant.sku
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    let counted_active_variants =
        total_active_variants.saturating_sub(i64::try_from(missing_skus.len()).unwrap_or(i64::MAX));

    let invalid_availability = sqlx::query_scalar::<_, i64>(
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
        SELECT COUNT(*)::bigint
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        LEFT JOIN stock ON stock.variant_id = variant.id
        LEFT JOIN reservations ON reservations.variant_id = variant.id
        WHERE variant.workspace_id = $1
          AND variant.active
          AND product.active
          AND NOT variant.sell_without_stock
          AND COALESCE(stock.on_hand, 0) < COALESCE(reservations.reserved, 0)
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let flags = sqlx::query_as::<_, (String, bool)>(
        r#"
        SELECT key, enabled
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1
          AND key IN (
              'merch_inventory_enabled',
              'merch_inventory_writes_enabled',
              'reward_campaigns_enabled'
          )
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    let flag = |key: &str| {
        flags
            .iter()
            .any(|(candidate, enabled)| candidate == key && *enabled)
    };
    let public_enabled = flag("merch_inventory_enabled");
    let writes_enabled = flag("merch_inventory_writes_enabled");
    let campaigns_enabled = flag("reward_campaigns_enabled");
    let fully_enabled = public_enabled && writes_enabled && campaigns_enabled;

    let mut blockers = Vec::new();
    if total_active_variants == 0 || row.catalog_seeded_at.is_none() {
        blockers.push("catalog_empty".to_owned());
    }
    if !missing_skus.is_empty() {
        blockers.push("uncounted_variants".to_owned());
    }
    if invalid_availability > 0 {
        blockers.push("reserved_exceeds_stock".to_owned());
    }
    let ready = row.status == "ready";
    if ready && !fully_enabled {
        blockers.push("feature_flags_inconsistent".to_owned());
    }
    let can_mark_ready = blockers
        .iter()
        .all(|blocker| blocker == "feature_flags_inconsistent");

    Ok(InventoryActivationView {
        status: row.status,
        ready,
        fully_enabled,
        catalog_seed_version: row.catalog_seed_version,
        catalog_seeded_at: row.catalog_seeded_at,
        ready_at: row.ready_at,
        ready_by: row.ready_by,
        version: row.version,
        total_active_variants,
        counted_active_variants,
        missing_skus,
        blockers,
        can_mark_ready,
        public_enabled,
        writes_enabled,
        campaigns_enabled,
    })
}

async fn load_inventory_overview(
    state: &crate::AppState,
) -> Result<InventoryOverviewView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let items = sqlx::query_as::<_, InventoryOverviewItemView>(
        r#"
        WITH stock AS (
            SELECT
                variant_id,
                COALESCE(SUM(delta), 0)::bigint AS on_hand,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'sale' AND delta < 0
                ), 0)::bigint AS sold_total,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'sale' AND delta < 0
                      AND occurred_at >= now() - interval '30 days'
                ), 0)::bigint AS sold_30d,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'promotional_issue' AND delta < 0
                ), 0)::bigint AS promotional_issued_total
            FROM inventory_ledger
            WHERE workspace_id = $1
            GROUP BY variant_id
        ), reservations AS (
            SELECT
                item.variant_id,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'order'
                ), 0)::bigint AS order_reserved,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'campaign'
                ), 0)::bigint AS campaign_reserved,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'operational'
                ), 0)::bigint AS operational_reserved,
                COALESCE(SUM(item.quantity), 0)::bigint AS reserved,
                COUNT(DISTINCT reservation.id) FILTER (
                    WHERE reservation.reservation_kind = 'campaign'
                )::bigint AS active_campaigns
            FROM inventory_reservation_items AS item
            JOIN inventory_reservations AS reservation
              ON reservation.workspace_id = item.workspace_id
             AND reservation.id = item.reservation_id
            WHERE item.workspace_id = $1
              AND reservation.status = 'active'
              AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            GROUP BY item.variant_id
        ), counted AS (
            SELECT variant_id, MAX(created_at) AS last_counted_at
            FROM inventory_stocktake_items
            WHERE workspace_id = $1
            GROUP BY variant_id
        )
        SELECT
            product.slug AS product_slug,
            product.name AS product_name,
            variant.id AS variant_id,
            variant.sku,
            variant.label AS variant_label,
            variant.attributes,
            variant.active,
            variant.low_stock_threshold,
            variant.sell_without_stock,
            counted.variant_id IS NOT NULL AS counted,
            counted.last_counted_at,
            COALESCE(stock.on_hand, 0)::bigint AS on_hand,
            COALESCE(reservations.order_reserved, 0)::bigint AS order_reserved,
            COALESCE(reservations.campaign_reserved, 0)::bigint AS campaign_reserved,
            COALESCE(reservations.operational_reserved, 0)::bigint AS operational_reserved,
            COALESCE(reservations.reserved, 0)::bigint AS reserved,
            (COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0))::bigint AS available_quantity,
            COALESCE(stock.sold_total, 0)::bigint AS sold_total,
            COALESCE(stock.sold_30d, 0)::bigint AS sold_30d,
            COALESCE(stock.promotional_issued_total, 0)::bigint AS promotional_issued_total,
            COALESCE(reservations.active_campaigns, 0)::bigint AS active_campaigns
        FROM merch_products AS product
        JOIN merch_variants AS variant
          ON variant.workspace_id = product.workspace_id
         AND variant.product_id = product.id
        LEFT JOIN stock ON stock.variant_id = variant.id
        LEFT JOIN reservations ON reservations.variant_id = variant.id
        LEFT JOIN counted ON counted.variant_id = variant.id
        WHERE product.workspace_id = $1
        ORDER BY product.slug, variant.label, variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(InventoryOverviewView {
        generated_at: OffsetDateTime::now_utc(),
        items,
    })
}

async fn inventory_stocktake_inner(
    state: &crate::AppState,
    mutation_key: String,
    payload: InventoryStocktakeRequest,
) -> Result<InventoryStocktakeView, CommerceError> {
    if inventory_ready(state).await? {
        require_inventory_writes(state).await?;
    }
    let normalized = normalize_stocktake(payload)?;
    let request_hash = stocktake_request_hash(&normalized)?;
    let actor_id = optional_text(normalized.actor_id.as_deref(), 200)?;
    let reason = optional_text(normalized.reason.as_deref(), 500)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    sqlx::query(
        "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(workspace_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    if let Some(existing) = sqlx::query_as::<_, ExistingStocktake>(
        r#"
        SELECT id, request_hash, created_at
        FROM inventory_stocktakes
        WHERE workspace_id = $1 AND idempotency_key = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&mutation_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.request_hash != request_hash {
            return Err(CommerceError::Conflict);
        }
        let items = load_stocktake_items_tx(&mut transaction, workspace_id, existing.id).await?;
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return Ok(InventoryStocktakeView {
            id: existing.id,
            replayed: true,
            created_at: existing.created_at,
            items,
        });
    }

    let stocktake_id = Uuid::now_v7();
    let created_at = sqlx::query_scalar::<_, OffsetDateTime>(
        r#"
        INSERT INTO inventory_stocktakes (
            id, workspace_id, idempotency_key, request_hash, actor_id, reason
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING created_at
        "#,
    )
    .bind(stocktake_id)
    .bind(workspace_id)
    .bind(&mutation_key)
    .bind(&request_hash)
    .bind(actor_id.as_deref())
    .bind(reason.as_deref())
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    for item in &normalized.items {
        let availability =
            lock_variant_availability(&mut transaction, workspace_id, &item.sku).await?;
        if !availability.sell_without_stock && i64::from(item.on_hand) < availability.reserved {
            return Err(CommerceError::Conflict);
        }
        let delta_i64 = i64::from(item.on_hand).saturating_sub(availability.on_hand);
        let delta = i32::try_from(delta_i64).map_err(|_| CommerceError::Invalid)?;
        if delta != 0 {
            sqlx::query(
                r#"
                INSERT INTO inventory_ledger (
                    workspace_id, variant_id, delta, movement_kind, idempotency_key,
                    actor_kind, actor_id, reason
                )
                VALUES ($1, $2, $3, 'stocktake', $4, 'admin', $5, $6)
                "#,
            )
            .bind(workspace_id)
            .bind(availability.id)
            .bind(delta)
            .bind(format!("stocktake:{stocktake_id}:{}", item.sku))
            .bind(actor_id.as_deref())
            .bind(reason.as_deref().unwrap_or("exact physical stocktake"))
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        sqlx::query(
            r#"
            INSERT INTO inventory_stocktake_items (
                workspace_id, stocktake_id, variant_id, target_on_hand,
                on_hand_before, reserved_at_apply, applied_delta
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(workspace_id)
        .bind(stocktake_id)
        .bind(availability.id)
        .bind(item.on_hand)
        .bind(availability.on_hand)
        .bind(availability.reserved)
        .bind(delta)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    let items = load_stocktake_items_tx(&mut transaction, workspace_id, stocktake_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(InventoryStocktakeView {
        id: stocktake_id,
        replayed: false,
        created_at,
        items,
    })
}

async fn load_stocktake_items_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    stocktake_id: Uuid,
) -> Result<Vec<InventoryStocktakeItemView>, CommerceError> {
    sqlx::query_as::<_, InventoryStocktakeItemView>(
        r#"
        SELECT
            variant.sku,
            variant.label,
            item.target_on_hand,
            item.on_hand_before,
            item.reserved_at_apply,
            item.applied_delta,
            (item.target_on_hand::bigint - item.reserved_at_apply)::bigint AS available_quantity
        FROM inventory_stocktake_items AS item
        JOIN merch_variants AS variant
          ON variant.workspace_id = item.workspace_id
         AND variant.id = item.variant_id
        WHERE item.workspace_id = $1 AND item.stocktake_id = $2
        ORDER BY variant.sku
        "#,
    )
    .bind(workspace_id)
    .bind(stocktake_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)
}

async fn mark_inventory_ready_inner(
    state: &crate::AppState,
    payload: MarkInventoryReadyRequest,
    request_id_value: Option<&str>,
) -> Result<InventoryActivationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?
        .unwrap_or_else(|| "virya-staff".to_owned());
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    sqlx::query(
        "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(workspace_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM inventory_activation_state WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(workspace_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    if status != "ready" {
        let _: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT variant.id
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1 AND variant.active AND product.active
            ORDER BY variant.id
            FOR UPDATE OF variant
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let missing_count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1
              AND variant.active AND product.active
              AND NOT EXISTS (
                  SELECT 1 FROM inventory_stocktake_items AS item
                  WHERE item.workspace_id = variant.workspace_id
                    AND item.variant_id = variant.id
              )
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let active_count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1 AND variant.active AND product.active
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let invalid_availability = sqlx::query_scalar::<_, i64>(
            r#"
            WITH stock AS (
                SELECT variant_id, COALESCE(SUM(delta), 0)::bigint AS on_hand
                FROM inventory_ledger WHERE workspace_id = $1 GROUP BY variant_id
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
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            LEFT JOIN stock ON stock.variant_id = variant.id
            LEFT JOIN reservations ON reservations.variant_id = variant.id
            WHERE variant.workspace_id = $1
              AND variant.active AND product.active
              AND NOT variant.sell_without_stock
              AND COALESCE(stock.on_hand, 0) < COALESCE(reservations.reserved, 0)
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        if active_count == 0 || missing_count > 0 || invalid_availability > 0 {
            return Err(CommerceError::Conflict);
        }

        sqlx::query(
            r#"
            UPDATE inventory_activation_state
            SET status = 'ready', ready_at = now(), ready_by = $2, version = version + 1
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .bind(&actor_id)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    sqlx::query(
        r#"
        INSERT INTO ecosystem_feature_flags (
            workspace_id, key, enabled, reason, updated_by_request_id
        )
        SELECT $1, flag.key, true, 'inventory activated from staff panel', $2
        FROM (VALUES
            ('merch_inventory_enabled'),
            ('merch_inventory_writes_enabled'),
            ('reward_campaigns_enabled')
        ) AS flag(key)
        ON CONFLICT (workspace_id, key) DO UPDATE SET
            enabled = true,
            reason = EXCLUDED.reason,
            version = ecosystem_feature_flags.version + 1,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(workspace_id)
    .bind(request_id_value)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    for key in [
        "merch_inventory_enabled",
        "merch_inventory_writes_enabled",
        "reward_campaigns_enabled",
    ] {
        crate::ecosystem::cache_feature_flag(workspace_id, key, true).await;
    }
    load_inventory_activation(state).await
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
}
