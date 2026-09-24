//! First-party ticket inventory, Stripe reconciliation, and paid admission issuance.
//!
//! Stripe remains the payment authority and its signature is verified by the
//! Virya server endpoint. This module owns the durable inventory hold and only
//! accepts payment transitions through a separately authenticated service route.
//! A completed payment creates ordinary claimed `admission_passes`, so every
//! admission source shares one gate redemption path.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::{EventSlug, NormalizedEmail, WorkspaceId};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, request_id,
    security::{bearer_sha256, bearer_sha256_matches_either},
    ticket_qr::encode_ticket_qr,
};

type HmacSha256 = Hmac<Sha256>;

const PRIVATE_NO_STORE: &str = "private, no-store";
const PUBLIC_REVALIDATE: &str = "public, max-age=5, s-maxage=10, stale-while-revalidate=15";
const CHECKOUT_TOKEN_CONTEXT: &[u8] = b"crowdrelay/ticket-order-checkout-token/v1\0";
const MAX_TICKET_TYPES: usize = 24;
const MAX_ORDER_LINES: usize = 10;
const MAX_NAME_CHARS: usize = 160;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_INVOICE_TEXT_CHARS: usize = 240;
const DELIVERY_RESEND_COOLDOWN_SECONDS: i64 = 300;

/// Database and authentication material used by ticketing routes.
#[derive(Clone)]
pub struct TicketingState {
    workspace_id: WorkspaceId,
    pool: PgPool,
    operation_timeout: Duration,
    admin_api_key_sha256: Option<[u8; 32]>,
    staff_api_key_sha256: Option<[u8; 32]>,
    previous_admin_api_key_sha256: Option<[u8; 32]>,
    previous_staff_api_key_sha256: Option<[u8; 32]>,
    commerce_api_key_sha256: Option<[u8; 32]>,
    previous_commerce_api_key_sha256: Option<[u8; 32]>,
    checkout_token_key: Option<[u8; 32]>,
}

impl TicketingState {
    /// Creates the ticketing route state.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_id: WorkspaceId,
        pool: PgPool,
        operation_timeout: Duration,
        admin_api_key_sha256: Option<[u8; 32]>,
        staff_api_key_sha256: Option<[u8; 32]>,
        previous_admin_api_key_sha256: Option<[u8; 32]>,
        previous_staff_api_key_sha256: Option<[u8; 32]>,
        commerce_api_key_sha256: Option<[u8; 32]>,
        previous_commerce_api_key_sha256: Option<[u8; 32]>,
        checkout_token_key: Option<[u8; 32]>,
    ) -> Self {
        Self {
            workspace_id,
            pool,
            operation_timeout,
            admin_api_key_sha256,
            staff_api_key_sha256,
            previous_admin_api_key_sha256,
            previous_staff_api_key_sha256,
            commerce_api_key_sha256,
            previous_commerce_api_key_sha256,
            checkout_token_key,
        }
    }

    pub(crate) fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn operation_timeout(&self) -> Duration {
        self.operation_timeout
    }

    pub(crate) fn checkout_token_key(&self) -> Option<[u8; 32]> {
        self.checkout_token_key
    }

    pub(crate) fn commerce_authorized(&self, headers: &HeaderMap) -> bool {
        bearer_sha256_matches_either(
            headers,
            self.commerce_api_key_sha256,
            self.previous_commerce_api_key_sha256,
        )
    }

    pub(crate) fn admin_authorized(&self, headers: &HeaderMap) -> bool {
        bearer_sha256_matches_either(
            headers,
            self.admin_api_key_sha256,
            self.previous_admin_api_key_sha256,
        )
    }

    pub(crate) async fn operator_authorized(&self, headers: &HeaderMap) -> bool {
        if self.admin_authorized(headers) {
            return true;
        }
        if bearer_sha256_matches_either(
            headers,
            self.staff_api_key_sha256,
            self.previous_staff_api_key_sha256,
        ) {
            crate::http_metrics().record_legacy_static_staff_auth();
            return true;
        }
        let Some(token_hash) = bearer_sha256(headers) else {
            return false;
        };
        match sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM staff_device_sessions
                WHERE workspace_id = $1
                  AND token_hash = $2
                  AND revoked_at IS NULL
                  AND expires_at > now()
            )
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(token_hash.to_vec())
        .fetch_one(&self.pool)
        .await
        {
            Ok(authorized) => authorized,
            Err(error) => {
                tracing::warn!(%error, "staff device session lookup failed");
                false
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureTicketSaleRequest {
    currency: String,
    vat_rate_basis_points: i32,
    capacity: i32,
    max_per_order: i32,
    hold_seconds: i32,
    #[serde(with = "time::serde::rfc3339")]
    sales_open_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    sales_close_at: OffsetDateTime,
    active: bool,
    ticket_types: Vec<ConfigureTicketTypeRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureTicketTypeRequest {
    slug: String,
    name: String,
    description: Option<String>,
    price_gross_minor: i64,
    capacity: Option<i32>,
    sort_order: i32,
    active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveTicketOrderRequest {
    buyer_email: String,
    buyer_name: Option<String>,
    #[serde(default = "default_buyer_locale")]
    buyer_locale: String,
    #[serde(default)]
    invoice_requested: bool,
    invoice_details: Option<InvoiceDetailsRequest>,
    items: Vec<ReserveTicketItemRequest>,
}

fn default_buyer_locale() -> String {
    "pl".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InvoiceDetailsRequest {
    buyer_type: String,
    company_name: Option<String>,
    tax_id: Option<String>,
    full_name: Option<String>,
    address_line1: String,
    postal_code: String,
    city: String,
    country_code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReserveTicketItemRequest {
    ticket_type_slug: String,
    quantity: i32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindStripeCheckoutRequest {
    checkout_token: String,
    stripe_checkout_session_id: String,
    #[serde(with = "time::serde::rfc3339")]
    stripe_expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelTicketOrderRequest {
    checkout_token: String,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StripeTicketEventRequest {
    stripe_event_id: String,
    event_type: String,
    stripe_checkout_session_id: Option<String>,
    stripe_payment_intent_id: Option<String>,
    payment_status: Option<String>,
    amount_total_minor: Option<i64>,
    amount_refunded_minor: Option<i64>,
    currency: Option<String>,
    customer_email: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    stripe_balance_transaction_id: Option<String>,
    stripe_fee_minor: Option<i64>,
    stripe_net_minor: Option<i64>,
    stripe_reporting_category: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketSaleView {
    event_id: Uuid,
    event_slug: String,
    event_title: String,
    event_status: String,
    venue: Option<String>,
    timezone: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    currency: String,
    vat_rate_basis_points: i32,
    capacity: i32,
    sold: i32,
    reserved: i32,
    available: i32,
    max_per_order: i32,
    #[serde(with = "time::serde::rfc3339")]
    sales_open_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    sales_close_at: OffsetDateTime,
    active: bool,
    sales_state: &'static str,
    ticket_types: Vec<TicketTypeView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketTypeView {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    price_gross_minor: i64,
    capacity: Option<i32>,
    sold: i32,
    reserved: i32,
    available: i32,
    sort_order: i32,
    active: bool,
}

/// The fan-facing sale read: everything a fan needs to choose and buy a
/// ticket, and nothing else.
///
/// The admin view's `capacity`/`sold`/`reserved` are the venue's operating
/// numbers — on a public, revalidatable route they tell a scalper exactly
/// how the night is selling and tell a fan "nobody is coming" on a fresh
/// show. `availability` is the honest scarcity word the public inventory
/// bar renders instead of a raw proportion. `available` stays because
/// checkout needs a bound and "how many tickets can I still buy" is the
/// fan's own question, not an operating secret.
#[derive(Clone, Debug, Serialize)]
pub struct PublicTicketSaleView {
    event_id: Uuid,
    event_slug: String,
    event_title: String,
    event_status: String,
    venue: Option<String>,
    timezone: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    currency: String,
    vat_rate_basis_points: i32,
    /// `plenty` while stock is comfortable, `low` inside the last fifth,
    /// `sold_out` at zero — the band virya's bar renders; the exact pool
    /// size never leaves the staff surfaces.
    availability: &'static str,
    available: i32,
    max_per_order: i32,
    #[serde(with = "time::serde::rfc3339")]
    sales_open_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    sales_close_at: OffsetDateTime,
    active: bool,
    sales_state: &'static str,
    ticket_types: Vec<PublicTicketTypeView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicTicketTypeView {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    price_gross_minor: i64,
    /// Same band vocabulary as the sale — an uncapped tier draws from the
    /// sale's pool, so it inherits the sale's band rather than inventing a
    /// proportion from a denominator it does not have.
    availability: &'static str,
    available: i32,
    sort_order: i32,
    active: bool,
}

/// The scarcity word for `available` out of `capacity`. Twenty percent is
/// the same line virya's inventory bar draws for "last tickets" — the server
/// names the band so the page does not re-derive it from a leaked count.
fn availability_band(available: i64, capacity: i64) -> &'static str {
    if available <= 0 {
        "sold_out"
    } else if capacity > 0 && available * 5 <= capacity {
        "low"
    } else {
        "plenty"
    }
}

impl From<TicketSaleView> for PublicTicketSaleView {
    fn from(sale: TicketSaleView) -> Self {
        let availability = availability_band(i64::from(sale.available), i64::from(sale.capacity));
        Self {
            event_id: sale.event_id,
            event_slug: sale.event_slug,
            event_title: sale.event_title,
            event_status: sale.event_status,
            venue: sale.venue,
            timezone: sale.timezone,
            starts_at: sale.starts_at,
            currency: sale.currency,
            vat_rate_basis_points: sale.vat_rate_basis_points,
            availability,
            available: sale.available,
            max_per_order: sale.max_per_order,
            sales_open_at: sale.sales_open_at,
            sales_close_at: sale.sales_close_at,
            active: sale.active,
            sales_state: sale.sales_state,
            ticket_types: sale
                .ticket_types
                .into_iter()
                .map(|t| {
                    let band = match t.capacity {
                        Some(capacity) => {
                            availability_band(i64::from(t.available), i64::from(capacity))
                        }
                        None => availability,
                    };
                    PublicTicketTypeView {
                        id: t.id,
                        slug: t.slug,
                        name: t.name,
                        description: t.description,
                        price_gross_minor: t.price_gross_minor,
                        availability: band,
                        available: t.available,
                        sort_order: t.sort_order,
                        active: t.active,
                    }
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketOrderView {
    order_id: Uuid,
    public_reference: String,
    event_slug: String,
    event_title: String,
    venue: Option<String>,
    timezone: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    status: String,
    buyer_email_masked: String,
    buyer_name: Option<String>,
    currency: String,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
    amount_refunded_minor: i64,
    vat_rate_basis_points: i32,
    invoice_requested: bool,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    refunded_at: Option<OffsetDateTime>,
    items: Vec<TicketOrderItemView>,
    tickets: Vec<IssuedTicketView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketOrderItemView {
    id: Uuid,
    ticket_type_slug: String,
    ticket_type_name: String,
    quantity: i32,
    unit_gross_minor: i64,
    unit_net_minor: i64,
    unit_vat_minor: i64,
    total_gross_minor: i64,
    total_net_minor: i64,
    total_vat_minor: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssuedTicketView {
    pass_id: Uuid,
    order_item_id: Uuid,
    sequence: i32,
    public_reference: String,
    status: String,
    holder_name: Option<String>,
    holder_email_masked: String,
    #[serde(with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketWalletPassView {
    pass_id: Uuid,
    order_item_id: Uuid,
    ticket_type_slug: String,
    ticket_type_name: String,
    sequence: i32,
    public_reference: String,
    status: String,
    holder_name: Option<String>,
    holder_email_masked: String,
    #[serde(with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
    qr_token: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    qr_not_before: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    qr_expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketWalletView {
    order: TicketOrderView,
    tickets: Vec<TicketWalletPassView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketDeliveryRequestResponse {
    accepted: bool,
    duplicate: bool,
    #[serde(with = "time::serde::rfc3339")]
    requested_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct TicketReservationResponse {
    checkout_token: String,
    order: TicketOrderView,
}

#[derive(Debug, Serialize)]
pub struct StripeCheckoutBindingResponse {
    order_id: Uuid,
    public_reference: String,
    stripe_checkout_session_id: String,
    currency: String,
    amount_gross_minor: i64,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct StripeTicketEventResponse {
    received: bool,
    duplicate: bool,
    order: TicketOrderView,
}

#[derive(Debug, Serialize)]
pub struct AdminTicketingOverview {
    sale: TicketSaleView,
    reserved_orders: i64,
    checkout_created_orders: i64,
    reserved_tickets: i64,
    paid_orders: i64,
    paid_tickets: i64,
    gross_sales_minor: i64,
    refunded_minor: i64,
    recent_orders: Vec<TicketOrderView>,
}

#[derive(Clone, Debug)]
struct NormalizedReservation {
    buyer_email: NormalizedEmail,
    buyer_name: Option<String>,
    buyer_locale: String,
    invoice_requested: bool,
    invoice_details: Option<InvoiceDetailsRequest>,
    items: Vec<(String, i32)>,
    total_quantity: i32,
    request_hash: [u8; 32],
}

#[derive(Clone, Debug)]
struct NormalizedTicketType {
    slug: String,
    name: String,
    description: Option<String>,
    price_gross_minor: i64,
    capacity: Option<i32>,
    sort_order: i32,
    active: bool,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct SaleRow {
    id: Uuid,
    event_id: Uuid,
    admission_pool_id: Uuid,
    event_slug: String,
    event_title: String,
    event_status: String,
    venue: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
    currency: String,
    vat_rate_basis_points: i32,
    capacity: i32,
    issued_count: i32,
    reserved_count: i32,
    max_per_order: i32,
    hold_seconds: i32,
    sales_open_at: OffsetDateTime,
    sales_close_at: OffsetDateTime,
    active: bool,
}

#[derive(Clone, Debug, FromRow)]
struct TicketTypeRow {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    price_gross_minor: i64,
    capacity: Option<i32>,
    sort_order: i32,
    active: bool,
}

#[derive(Debug, FromRow)]
struct TypeInventoryRow {
    ticket_type_id: Uuid,
    reserved: i64,
    sold: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TypeInventory {
    reserved: i64,
    sold: i64,
}

impl TypeInventory {
    fn committed(self) -> Result<i64, TicketingError> {
        self.reserved
            .checked_add(self.sold)
            .ok_or(TicketingError::Unexpected)
    }
}

#[derive(Debug, FromRow)]
struct OrderRow {
    id: Uuid,
    ticket_sale_id: Uuid,
    public_reference: String,
    status: String,
    buyer_email: String,
    buyer_name: Option<String>,
    buyer_locale: String,
    invoice_buyer_type: Option<String>,
    invoice_company_name: Option<String>,
    invoice_tax_id: Option<String>,
    invoice_full_name: Option<String>,
    invoice_address_line1: Option<String>,
    invoice_postal_code: Option<String>,
    invoice_city: Option<String>,
    invoice_country_code: Option<String>,
    currency: String,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
    amount_refunded_minor: i64,
    vat_rate_basis_points: i32,
    invoice_requested: bool,
    reservation_key: String,
    request_hash: Vec<u8>,
    expires_at: OffsetDateTime,
    stripe_checkout_session_id: Option<String>,
    stripe_payment_intent_id: Option<String>,
    paid_at: Option<OffsetDateTime>,
    refunded_at: Option<OffsetDateTime>,
    event_id: Uuid,
    admission_pool_id: Uuid,
    event_slug: String,
    event_title: String,
    venue: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, FromRow)]
#[allow(dead_code)]
struct OrderItemRow {
    id: Uuid,
    ticket_type_id: Uuid,
    ticket_type_slug: String,
    ticket_type_name: String,
    quantity: i32,
    unit_gross_minor: i64,
    unit_net_minor: i64,
    unit_vat_minor: i64,
    total_gross_minor: i64,
    total_net_minor: i64,
    total_vat_minor: i64,
}

#[derive(Clone, Debug, FromRow)]
struct IssuedTicketRow {
    pass_id: Uuid,
    order_item_id: Uuid,
    sequence: i32,
    public_reference: String,
    status: String,
    holder_name: Option<String>,
    holder_email: String,
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, FromRow)]
struct IssuedPaidTicketRow {
    pass_id: Uuid,
    order_item_id: Uuid,
    ticket_type_slug: String,
    ticket_type_name: String,
    sequence: i32,
    public_reference: String,
}

#[derive(Clone, Debug, FromRow)]
struct TicketWalletRow {
    pass_id: Uuid,
    order_item_id: Uuid,
    ticket_type_slug: String,
    ticket_type_name: String,
    sequence: i32,
    public_reference: String,
    status: String,
    holder_name: Option<String>,
    holder_email: String,
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Debug)]
struct PreparedOrderItem {
    id: Uuid,
    ticket_type: TicketTypeRow,
    quantity: i32,
    unit_gross_minor: i64,
    unit_net_minor: i64,
    unit_vat_minor: i64,
    total_gross_minor: i64,
    total_net_minor: i64,
    total_vat_minor: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TicketingError {
    Invalid,
    NotFound,
    Conflict,
    Unavailable,
    Unexpected,
}

impl TicketingError {
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
        tracing::error!(error = %error, "ticketing database operation failed");
        Self::Unavailable
    }
}

// Physical sections compile into this module through `include!`.
// This preserves the established API and item visibility while keeping
// high-risk domains small enough to review and profile independently.
include!("ticketing/handlers.rs");
include!("ticketing/reservations.rs");
include!("ticketing/payments.rs");
include!("ticketing/read_model.rs");
include!("ticketing/validation.rs");
include!("ticketing/tests.rs");
