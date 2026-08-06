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

// Public, cacheable merch availability. It fails closed until the staged
// feature flag is enabled, while the Virya site can keep rendering static
// product cards and degrade only the small availability block.

// Physical sections compile into this module through `include!`.
// This preserves the established API and item visibility while keeping
// high-risk domains small enough to review and profile independently.
include!("commerce/handlers.rs");
include!("commerce/inventory.rs");
include!("commerce/campaigns.rs");
include!("commerce/validation.rs");
include!("commerce/tests.rs");
