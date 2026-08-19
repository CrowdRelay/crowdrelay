//! Additive merch inventory and reward-campaign controls.
//!
//! This module intentionally stays separate from ticketing and fan mail flows.
//! Public reads are cacheable and bounded; every stock mutation is transactional,
//! idempotent and workspace-scoped.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH},
    },
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

fn merch_catalog_etag(catalog: &MerchCatalogView) -> Option<String> {
    // `generated_at` is deliberately excluded: it changes on every DB read and
    // would defeat conditional revalidation even when inventory is identical.
    let payload = serde_json::to_vec(&catalog.products).ok()?;
    let digest = Sha256::digest(payload);
    Some(format!(
        "\"merch-{}-{}\"",
        catalog.products.len(),
        hex::encode(digest)
    ))
}

fn merch_etag_matches(candidate: Option<&HeaderValue>, expected: &str) -> bool {
    candidate
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*"
                    || candidate == expected
                    || candidate.strip_prefix("W/") == Some(expected)
            })
        })
}

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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339::option")]
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
    #[serde(with = "time::serde::rfc3339::option")]
    catalog_seeded_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
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
    #[serde(with = "time::serde::rfc3339::option")]
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
    eligibility_ref: Option<String>,
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
    eligibility_ref: Option<String>,
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
    #[serde(with = "time::serde::rfc3339")]
    opens_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    closes_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    draw_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct RewardDrawAdminView {
    id: Uuid,
    slug: String,
    name: String,
    prize_kind: String,
    eligibility_kind: String,
    eligibility_ref: Option<String>,
    event_slug: Option<String>,
    status: String,
    winner_count: i32,
    run_count: i64,
    selected_winners: i64,
    proof_count: i64,
    can_delete: bool,
    #[serde(with = "time::serde::rfc3339")]
    opens_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    closes_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    draw_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeletedRewardDrawView {
    id: Uuid,
    slug: String,
    deleted: bool,
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
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

// Public, cacheable merch availability. It fails closed until the staged
// feature flag is enabled, while the Virya site can keep rendering static
// product cards and degrade only the small availability block.

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmMerchOrderRequest {
    stripe_session_id: String,
    inventory_reservation_id: Uuid,
    buyer_email: Option<String>,
    event_id: Option<Uuid>,
    fulfillment_mode: String,
    currency: String,
    amount_gross_minor: i64,
    goods_gross_minor: i64,
    shipping_gross_minor: i64,
    #[serde(with = "time::serde::rfc3339")]
    confirmed_at: OffsetDateTime,
}

pub async fn confirm_merch_order(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ConfirmMerchOrderRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let valid_mode = matches!(
        payload.fulfillment_mode.as_str(),
        "inpost" | "event_pickup" | "none"
    );
    let valid_event = (payload.fulfillment_mode == "event_pickup") == payload.event_id.is_some();
    let valid_currency = payload.currency.len() == 3
        && payload
            .currency
            .bytes()
            .all(|byte| byte.is_ascii_uppercase());
    if payload.stripe_session_id.trim().is_empty()
        || payload.stripe_session_id.len() > 255
        || payload
            .buyer_email
            .as_ref()
            .is_some_and(|email| email.len() > 320 || !email.contains('@'))
        || !valid_mode
        || !valid_event
        || !valid_currency
        || payload.amount_gross_minor < 0
        || payload.goods_gross_minor < 0
        || payload.shipping_gross_minor < 0
    {
        return CommerceError::Invalid.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let input = crowdrelay_infra::commerce::ConfirmedMerchOrderInput {
        workspace_id,
        stripe_session_id: payload.stripe_session_id,
        inventory_reservation_id: payload.inventory_reservation_id,
        buyer_email: payload.buyer_email,
        event_id: payload.event_id,
        fulfillment_mode: payload.fulfillment_mode,
        currency: payload.currency,
        amount_gross_minor: payload.amount_gross_minor,
        goods_gross_minor: payload.goods_gross_minor,
        shipping_gross_minor: payload.shipping_gross_minor,
        confirmed_at: payload.confirmed_at,
    };
    match timeout(
        state.ticketing.operation_timeout(),
        crowdrelay_infra::commerce::record_confirmed_merch_order(&state.database, &input),
    )
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(crowdrelay_infra::commerce::RecordMerchOrderError::ReservationNotCommitted))
        | Ok(Err(crowdrelay_infra::commerce::RecordMerchOrderError::Conflict)) => {
            CommerceError::Conflict.response(request_id_value)
        }
        Ok(Err(crowdrelay_infra::commerce::RecordMerchOrderError::Database)) | Err(_) => {
            CommerceError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn event_merch_summary(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match timeout(
        state.ticketing.operation_timeout(),
        crowdrelay_infra::commerce::event_merch_summary(&state.database, workspace_id, event_id),
    )
    .await
    {
        Ok(Ok(summary)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(summary),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::error!(%error, %event_id, "event merch summary failed");
            CommerceError::Unavailable.response(request_id_value)
        }
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

// Physical sections compile into this module through `include!`.
// This preserves the established API and item visibility while keeping
// high-risk domains small enough to review and profile independently.
include!("commerce/handlers.rs");
include!("commerce/inventory.rs");
include!("commerce/campaigns.rs");
include!("commerce/validation.rs");
include!("commerce/tests.rs");
