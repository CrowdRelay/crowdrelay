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
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, request_id, security::bearer_sha256_matches,
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
    lock_timeout: Duration,
    admin_api_key_sha256: Option<[u8; 32]>,
    staff_api_key_sha256: Option<[u8; 32]>,
    commerce_api_key_sha256: Option<[u8; 32]>,
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
        lock_timeout: Duration,
        admin_api_key_sha256: Option<[u8; 32]>,
        staff_api_key_sha256: Option<[u8; 32]>,
        commerce_api_key_sha256: Option<[u8; 32]>,
        checkout_token_key: Option<[u8; 32]>,
    ) -> Self {
        Self {
            workspace_id,
            pool,
            operation_timeout,
            lock_timeout,
            admin_api_key_sha256,
            staff_api_key_sha256,
            commerce_api_key_sha256,
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

    pub(crate) fn lock_timeout(&self) -> Duration {
        self.lock_timeout
    }

    pub(crate) fn commerce_authorized(&self, headers: &HeaderMap) -> bool {
        bearer_sha256_matches(headers, self.commerce_api_key_sha256)
    }

    pub(crate) fn admin_authorized(&self, headers: &HeaderMap) -> bool {
        bearer_sha256_matches(headers, self.admin_api_key_sha256)
    }

    pub(crate) fn operator_authorized(&self, headers: &HeaderMap) -> bool {
        self.admin_authorized(headers) || bearer_sha256_matches(headers, self.staff_api_key_sha256)
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

/// Returns the currently configured public ticket offer for an event.
pub async fn public_sale(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let future = load_sale_view(&state.ticketing, event_slug.as_str(), false);
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PUBLIC_REVALIDATE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Creates or updates an event ticket sale and its price tiers.
pub async fn configure_sale(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ConfigureTicketSaleRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.ticketing.admin_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let ticket_types = match normalize_ticket_types(&payload) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if !valid_sale_configuration(&payload) {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = configure_sale_inner(
        &state.ticketing,
        event_slug.as_str(),
        &payload,
        &ticket_types,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(())) => match timeout(
            state.ticketing.operation_timeout,
            load_sale_view(&state.ticketing, event_slug.as_str(), true),
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
            Err(_) => TicketingError::Unavailable.response(request_id_value),
        },
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns operational ticketing totals and recent orders for one event.
pub async fn admin_overview(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.operator_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let future = load_admin_overview(&state.ticketing, event_slug.as_str());
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Atomically reserves ticket capacity before a Stripe Checkout Session exists.
pub async fn reserve_order(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ReserveTicketOrderRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let reservation = match normalize_reservation(event_slug.as_str(), payload) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Some(checkout_token_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket checkout token key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let request_id_text = trusted_request_id(&headers);
    let future = reserve_order_inner(
        &state.ticketing,
        event_slug.as_str(),
        &idempotency_key,
        request_id_text.as_deref(),
        &reservation,
        &checkout_token_key,
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(result)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns a private order view when the caller presents its checkout token.
pub async fn order_status(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let future = load_order_by_token(&state.ticketing, order_id, token);
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns the private ticket wallet, including durable QR credentials.
pub async fn order_wallet(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket wallet signing key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let future = load_ticket_wallet(&state.ticketing, order_id, token, &signing_key);
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Queues an idempotent re-delivery of the ticket wallet to the buyer e-mail.
pub async fn request_delivery(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket wallet signing key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let request_id_text = trusted_request_id(&headers);
    let future = request_ticket_delivery(
        &state.ticketing,
        order_id,
        token,
        &idempotency_key,
        request_id_text.as_deref(),
        &signing_key,
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(3), future).await {
        Ok(Ok(view)) => (
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

async fn configure_sale_inner(
    state: &TicketingState,
    event_slug: &str,
    request: &ConfigureTicketSaleRequest,
    ticket_types: &[NormalizedTicketType],
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;

    let (event_id, event_starts_at) = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
        r#"
        SELECT id, starts_at
        FROM events
        WHERE workspace_id = $1
          AND slug = $2
          AND status = 'published'
          AND starts_at > now()
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?
    .ok_or(TicketingError::NotFound)?;
    if request.sales_close_at > event_starts_at {
        return Err(TicketingError::Invalid);
    }

    let existing_sale = sqlx::query_as::<_, ExistingSaleRow>(
        r#"
        SELECT id, admission_pool_id
        FROM ticket_sales
        WHERE workspace_id = $1 AND event_id = $2
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let pool_id = if let Some(sale) = &existing_sale {
        let (issued_count, reserved_count) = sqlx::query_as::<_, (i32, i32)>(
            r#"
            SELECT issued_count, reserved_count
            FROM admission_pools
            WHERE workspace_id = $1 AND id = $2
            FOR UPDATE
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale.admission_pool_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
        if issued_count.saturating_add(reserved_count) > request.capacity {
            return Err(TicketingError::Conflict);
        }
        let updated_pool = sqlx::query(
            r#"
            UPDATE admission_pools
            SET capacity = $3, active = $4, name = 'Paid tickets'
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale.admission_pool_id)
        .bind(request.capacity)
        .bind(request.active)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
        if updated_pool.rows_affected() != 1 {
            return Err(TicketingError::Unexpected);
        }
        sale.admission_pool_id
    } else {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO admission_pools (
                workspace_id, event_id, slug, name, capacity, active
            ) VALUES ($1, $2, 'paid-tickets', 'Paid tickets', $3, $4)
            ON CONFLICT (workspace_id, event_id, slug) DO UPDATE
            SET capacity = EXCLUDED.capacity,
                active = EXCLUDED.active,
                name = EXCLUDED.name
            WHERE admission_pools.issued_count + admission_pools.reserved_count
                <= EXCLUDED.capacity
            RETURNING id
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(event_id)
        .bind(request.capacity)
        .bind(request.active)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::Conflict)?
    };

    let sale_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO ticket_sales (
            workspace_id, event_id, admission_pool_id, currency,
            vat_rate_basis_points, capacity, max_per_order, hold_seconds,
            sales_open_at, sales_close_at, active
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (workspace_id, event_id) DO UPDATE
        SET admission_pool_id = EXCLUDED.admission_pool_id,
            currency = EXCLUDED.currency,
            vat_rate_basis_points = EXCLUDED.vat_rate_basis_points,
            capacity = EXCLUDED.capacity,
            max_per_order = EXCLUDED.max_per_order,
            hold_seconds = EXCLUDED.hold_seconds,
            sales_open_at = EXCLUDED.sales_open_at,
            sales_close_at = EXCLUDED.sales_close_at,
            active = EXCLUDED.active
        RETURNING id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_id)
    .bind(pool_id)
    .bind(request.currency.trim().to_ascii_uppercase())
    .bind(request.vat_rate_basis_points)
    .bind(request.capacity)
    .bind(request.max_per_order)
    .bind(request.hold_seconds)
    .bind(request.sales_open_at)
    .bind(request.sales_close_at)
    .bind(request.active)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let configured_slugs: Vec<String> = ticket_types.iter().map(|item| item.slug.clone()).collect();
    sqlx::query(
        r#"
        UPDATE ticket_types
        SET active = false
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND NOT (slug = ANY($3))
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .bind(&configured_slugs)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    for ticket_type in ticket_types {
        sqlx::query(
            r#"
            INSERT INTO ticket_types (
                workspace_id, ticket_sale_id, slug, name, description,
                price_gross_minor, capacity, sort_order, active
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (workspace_id, ticket_sale_id, slug) DO UPDATE
            SET name = EXCLUDED.name,
                description = EXCLUDED.description,
                price_gross_minor = EXCLUDED.price_gross_minor,
                capacity = EXCLUDED.capacity,
                sort_order = EXCLUDED.sort_order,
                active = EXCLUDED.active
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale_id)
        .bind(&ticket_type.slug)
        .bind(&ticket_type.name)
        .bind(&ticket_type.description)
        .bind(ticket_type.price_gross_minor)
        .bind(ticket_type.capacity)
        .bind(ticket_type.sort_order)
        .bind(ticket_type.active)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    }

    let inventory = active_type_inventory(&mut transaction, state.workspace_id, sale_id).await?;
    let configured_rows = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1 AND ticket_sale_id = $2
        FOR SHARE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    for ticket_type in &configured_rows {
        let Some(capacity) = ticket_type.capacity else {
            continue;
        };
        let committed = inventory
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default()
            .committed()?;
        if committed > i64::from(capacity) {
            return Err(TicketingError::Conflict);
        }
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_sale.configured",
        "ticket_sale",
        sale_id,
        request_id_value,
        json!({
            "event_id": event_id,
            "capacity": request.capacity,
            "currency": request.currency.trim().to_ascii_uppercase(),
            "vat_rate_basis_points": request.vat_rate_basis_points,
            "ticket_type_count": ticket_types.len(),
            "active": request.active,
        }),
    )
    .await?;

    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(())
}

async fn reserve_order_inner(
    state: &TicketingState,
    event_slug: &str,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    reservation: &NormalizedReservation,
    checkout_token_key: &[u8; 32],
) -> Result<TicketReservationResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;

    if let Some(existing) =
        load_order_row_by_reservation_key(&mut transaction, state.workspace_id, idempotency_key)
            .await?
    {
        if existing.event_slug != event_slug
            || existing.request_hash.as_slice() != reservation.request_hash.as_slice()
        {
            return Err(TicketingError::Conflict);
        }
        let checkout_token =
            derive_checkout_token(checkout_token_key, existing.id, &existing.reservation_key)?;
        let order = load_order_view_for_row(&mut transaction, state.workspace_id, existing).await?;
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(TicketReservationResponse {
            checkout_token,
            order,
        });
    }

    let sale = lock_sale(&mut transaction, state.workspace_id, event_slug).await?;
    let now = OffsetDateTime::now_utc();
    if sale.event_status != "published"
        || now >= sale.starts_at
        || !sale.active
        || now < sale.sales_open_at
        || now >= sale.sales_close_at
    {
        return Err(TicketingError::Conflict);
    }
    if reservation.total_quantity > sale.max_per_order {
        return Err(TicketingError::Invalid);
    }

    let expired_quantity = expire_active_reservations(
        &mut transaction,
        state.workspace_id,
        sale.id,
        sale.admission_pool_id,
    )
    .await?;
    let current_reserved_count = sale
        .reserved_count
        .checked_sub(expired_quantity)
        .ok_or(TicketingError::Unexpected)?;
    if sale
        .issued_count
        .saturating_add(current_reserved_count)
        .saturating_add(reservation.total_quantity)
        > sale.capacity
    {
        return Err(TicketingError::Conflict);
    }

    let slugs: Vec<String> = reservation
        .items
        .iter()
        .map(|(slug, _)| slug.clone())
        .collect();
    let ticket_type_rows = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND slug = ANY($3)
        ORDER BY sort_order, id
        FOR SHARE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(&slugs)
    .fetch_all(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if ticket_type_rows.len() != reservation.items.len()
        || ticket_type_rows.iter().any(|item| !item.active)
    {
        return Err(TicketingError::NotFound);
    }

    let inventory_by_type =
        active_type_inventory(&mut transaction, state.workspace_id, sale.id).await?;
    let quantity_by_slug: HashMap<&str, i32> = reservation
        .items
        .iter()
        .map(|(slug, quantity)| (slug.as_str(), *quantity))
        .collect();

    let mut prepared = Vec::with_capacity(ticket_type_rows.len());
    let mut amount_gross_minor = 0_i64;
    let mut amount_net_minor = 0_i64;
    let mut amount_vat_minor = 0_i64;
    for ticket_type in ticket_type_rows {
        let quantity = quantity_by_slug
            .get(ticket_type.slug.as_str())
            .copied()
            .ok_or(TicketingError::Invalid)?;
        let committed = inventory_by_type
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default()
            .committed()?;
        let requested_commitment = committed
            .checked_add(i64::from(quantity))
            .ok_or(TicketingError::Unexpected)?;
        if ticket_type
            .capacity
            .is_some_and(|capacity| requested_commitment > i64::from(capacity))
        {
            return Err(TicketingError::Conflict);
        }
        let unit_gross_minor = ticket_type.price_gross_minor;
        let (unit_net_minor, unit_vat_minor) =
            split_gross(unit_gross_minor, sale.vat_rate_basis_points)?;
        let total_gross_minor = unit_gross_minor
            .checked_mul(i64::from(quantity))
            .ok_or(TicketingError::Invalid)?;
        let (total_net_minor, total_vat_minor) =
            split_gross(total_gross_minor, sale.vat_rate_basis_points)?;
        amount_gross_minor = amount_gross_minor
            .checked_add(total_gross_minor)
            .ok_or(TicketingError::Invalid)?;
        amount_net_minor = amount_net_minor
            .checked_add(total_net_minor)
            .ok_or(TicketingError::Invalid)?;
        amount_vat_minor = amount_vat_minor
            .checked_add(total_vat_minor)
            .ok_or(TicketingError::Invalid)?;
        prepared.push(PreparedOrderItem {
            id: Uuid::now_v7(),
            ticket_type,
            quantity,
            unit_gross_minor,
            unit_net_minor,
            unit_vat_minor,
            total_gross_minor,
            total_net_minor,
            total_vat_minor,
        });
    }
    if amount_net_minor + amount_vat_minor != amount_gross_minor {
        return Err(TicketingError::Unexpected);
    }

    let hold_expires_at = now
        .checked_add(time::Duration::seconds(i64::from(sale.hold_seconds)))
        .ok_or(TicketingError::Unexpected)?;
    let hard_close_at = sale.sales_close_at.min(sale.starts_at);
    if hold_expires_at > hard_close_at {
        return Err(TicketingError::Conflict);
    }

    let order_id = Uuid::now_v7();
    let public_reference = order_public_reference(order_id);
    let checkout_token = derive_checkout_token(checkout_token_key, order_id, idempotency_key)?;
    let checkout_token_hash: [u8; 32] = Sha256::digest(checkout_token.as_bytes()).into();
    let expires_at = hold_expires_at;

    let invoice = reservation.invoice_details.as_ref();
    sqlx::query(
        r#"
        INSERT INTO ticket_orders (
            id, workspace_id, ticket_sale_id, public_reference, status,
            buyer_email, buyer_name, buyer_locale,
            invoice_buyer_type, invoice_company_name, invoice_tax_id,
            invoice_full_name, invoice_address_line1, invoice_postal_code,
            invoice_city, invoice_country_code,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, invoice_requested, reservation_key,
            request_hash, checkout_token_hash, expires_at
        ) VALUES (
            $1, $2, $3, $4, 'reserved', $5, $6, $7,
            $8, $9, $10, $11, $12, $13, $14, $15,
            $16, $17, $18, $19, $20, $21, $22, $23, $24, $25
        )
        "#,
    )
    .bind(order_id)
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(&public_reference)
    .bind(reservation.buyer_email.as_str())
    .bind(&reservation.buyer_name)
    .bind(&reservation.buyer_locale)
    .bind(invoice.map(|value| value.buyer_type.as_str()))
    .bind(invoice.and_then(|value| value.company_name.as_deref()))
    .bind(invoice.and_then(|value| value.tax_id.as_deref()))
    .bind(invoice.and_then(|value| value.full_name.as_deref()))
    .bind(invoice.map(|value| value.address_line1.as_str()))
    .bind(invoice.map(|value| value.postal_code.as_str()))
    .bind(invoice.map(|value| value.city.as_str()))
    .bind(invoice.map(|value| value.country_code.as_str()))
    .bind(&sale.currency)
    .bind(amount_gross_minor)
    .bind(amount_net_minor)
    .bind(amount_vat_minor)
    .bind(sale.vat_rate_basis_points)
    .bind(reservation.invoice_requested)
    .bind(idempotency_key)
    .bind(reservation.request_hash.as_slice())
    .bind(checkout_token_hash.as_slice())
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    for item in &prepared {
        sqlx::query(
            r#"
            INSERT INTO ticket_order_items (
                id, workspace_id, ticket_order_id, ticket_type_id, quantity,
                unit_gross_minor, unit_net_minor, unit_vat_minor,
                total_gross_minor, total_net_minor, total_vat_minor
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(item.id)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(item.ticket_type.id)
        .bind(item.quantity)
        .bind(item.unit_gross_minor)
        .bind(item.unit_net_minor)
        .bind(item.unit_vat_minor)
        .bind(item.total_gross_minor)
        .bind(item.total_net_minor)
        .bind(item.total_vat_minor)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    }

    let reserved = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count + $3
        WHERE workspace_id = $1 AND id = $2
          AND issued_count + reserved_count + $3 <= capacity
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.admission_pool_id)
    .bind(reservation.total_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if reserved.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_order.reserved",
        "ticket_order",
        order_id,
        request_id_value,
        json!({
            "event_id": sale.event_id,
            "quantity": reservation.total_quantity,
            "amount_gross_minor": amount_gross_minor,
            "currency": sale.currency,
            "expires_at": expires_at,
        }),
    )
    .await?;

    let order_row = load_order_row_by_id(&mut transaction, state.workspace_id, order_id).await?;
    let order = load_order_view_for_row(&mut transaction, state.workspace_id, order_row).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(TicketReservationResponse {
        checkout_token,
        order,
    })
}

/// Binds a reserved order to exactly one Stripe Checkout Session.
pub async fn bind_stripe_checkout(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<BindStripeCheckoutRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.ticketing.commerce_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if payload.checkout_token.len() != 64
        || !payload
            .checkout_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !valid_stripe_id(&payload.stripe_checkout_session_id, "cs_")
    {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = bind_stripe_checkout_inner(
        &state.ticketing,
        order_id,
        &payload,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Cancels an unpaid order and releases its shared admission reservation.
pub async fn cancel_order(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<CancelTicketOrderRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.ticketing.commerce_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if payload.checkout_token.len() != 64
        || !payload
            .checkout_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || clean_text(&payload.reason, 160).is_none()
    {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = cancel_order_inner(
        &state.ticketing,
        order_id,
        &payload,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(order)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(order),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Applies one verified Stripe event to a ticket order.
pub async fn stripe_event(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<StripeTicketEventRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.ticketing.commerce_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if !valid_stripe_event(&payload) {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = stripe_event_inner(&state.ticketing, &payload, request_id_text.as_deref());
    match timeout(state.ticketing.operation_timeout.saturating_mul(3), future).await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

async fn bind_stripe_checkout_inner(
    state: &TicketingState,
    order_id: Uuid,
    request: &BindStripeCheckoutRequest,
    request_id_value: Option<&str>,
) -> Result<StripeCheckoutBindingResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;

    let token_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT checkout_token_hash = digest($3, 'sha256')
        FROM ticket_orders
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(&request.checkout_token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }

    if let Some(existing_session) = &order.stripe_checkout_session_id {
        if existing_session != &request.stripe_checkout_session_id {
            return Err(TicketingError::Conflict);
        }
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(StripeCheckoutBindingResponse {
            order_id: order.id,
            public_reference: order.public_reference,
            stripe_checkout_session_id: existing_session.clone(),
            currency: order.currency,
            amount_gross_minor: order.amount_gross_minor,
            expires_at: order.expires_at,
        });
    }

    let now = OffsetDateTime::now_utc();
    if order.status != "reserved" || order.expires_at <= now {
        if order.status == "reserved" && order.expires_at <= now {
            release_order_reservation(&mut transaction, state.workspace_id, &order, "expired")
                .await?;
            transaction.commit().await.map_err(TicketingError::sqlx)?;
        }
        return Err(TicketingError::Conflict);
    }
    if request.stripe_expires_at < now || request.stripe_expires_at > order.expires_at {
        return Err(TicketingError::Invalid);
    }

    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = 'checkout_created',
            stripe_checkout_session_id = $3,
            expires_at = $4
        WHERE workspace_id = $1 AND id = $2
          AND status = 'reserved'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_checkout_session_id)
    .bind(request.stripe_expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_order.checkout_bound",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "stripe_checkout_session_id": request.stripe_checkout_session_id,
            "expires_at": request.stripe_expires_at,
        }),
    )
    .await?;

    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(StripeCheckoutBindingResponse {
        order_id: order.id,
        public_reference: order.public_reference,
        stripe_checkout_session_id: request.stripe_checkout_session_id.clone(),
        currency: order.currency,
        amount_gross_minor: order.amount_gross_minor,
        expires_at: request.stripe_expires_at,
    })
}

async fn cancel_order_inner(
    state: &TicketingState,
    order_id: Uuid,
    request: &CancelTicketOrderRequest,
    request_id_value: Option<&str>,
) -> Result<TicketOrderView, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    let token_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT checkout_token_hash = digest($3, 'sha256')
        FROM ticket_orders
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.checkout_token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    if matches!(order.status.as_str(), "reserved" | "checkout_created") {
        release_order_reservation(&mut transaction, state.workspace_id, &order, "cancelled")
            .await?;
        append_audit(
            &mut transaction,
            state.workspace_id,
            "service",
            "ticket_order.cancelled",
            "ticket_order",
            order.id,
            request_id_value,
            json!({ "reason": request.reason.trim() }),
        )
        .await?;
    }
    let updated = load_order_row_by_id(&mut transaction, state.workspace_id, order.id).await?;
    let view = load_order_view_for_row(&mut transaction, state.workspace_id, updated).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(view)
}

async fn stripe_event_inner(
    state: &TicketingState,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<StripeTicketEventResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    let order = if let Some(session_id) = request.stripe_checkout_session_id.as_deref() {
        sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_STRIPE_SESSION_FOR_UPDATE)
            .bind(state.workspace_id.into_uuid())
            .bind(session_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TicketingError::sqlx)?
    } else if let Some(payment_intent_id) = request.stripe_payment_intent_id.as_deref() {
        sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_PAYMENT_INTENT_FOR_UPDATE)
            .bind(state.workspace_id.into_uuid())
            .bind(payment_intent_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TicketingError::sqlx)?
    } else {
        None
    }
    .ok_or(TicketingError::NotFound)?;

    let payload = serde_json::to_vec(request).map_err(|_| TicketingError::Invalid)?;
    let payload_hash: [u8; 32] = Sha256::digest(&payload).into();
    let existing_hash = sqlx::query_scalar::<_, Vec<u8>>(
        r#"
        SELECT payload_hash
        FROM ticket_stripe_events
        WHERE workspace_id = $1 AND stripe_event_id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(&request.stripe_event_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if let Some(existing_hash) = existing_hash {
        if existing_hash.as_slice() != payload_hash.as_slice() {
            return Err(TicketingError::Conflict);
        }
        let view = load_order_view_for_row(&mut transaction, state.workspace_id, order).await?;
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(StripeTicketEventResponse {
            received: true,
            duplicate: true,
            order: view,
        });
    }

    match request.event_type.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            process_paid_order(&mut transaction, state, &order, request, request_id_value).await?;
        }
        "checkout.session.expired" | "checkout.session.async_payment_failed" => {
            release_unpaid_order(&mut transaction, state, &order, request, request_id_value)
                .await?;
        }
        "charge.refunded" | "refund.created" | "refund.updated" => {
            process_refund(&mut transaction, state, &order, request, request_id_value).await?;
        }
        _ => return Err(TicketingError::Invalid),
    }

    sqlx::query(
        r#"
        INSERT INTO ticket_stripe_events (
            workspace_id, ticket_order_id, stripe_event_id, event_type, payload_hash
        ) VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_event_id)
    .bind(&request.event_type)
    .bind(payload_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let updated = load_order_row_by_id(&mut transaction, state.workspace_id, order.id).await?;
    let view = load_order_view_for_row(&mut transaction, state.workspace_id, updated).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(StripeTicketEventResponse {
        received: true,
        duplicate: false,
        order: view,
    })
}

async fn process_paid_order(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        if order.stripe_payment_intent_id.as_deref() != request.stripe_payment_intent_id.as_deref()
        {
            return Err(TicketingError::Conflict);
        }
        return Ok(());
    }
    if order.status != "checkout_created" {
        return Err(TicketingError::Conflict);
    }
    let stripe_checkout_session_id = request
        .stripe_checkout_session_id
        .as_deref()
        .ok_or(TicketingError::Invalid)?;
    let payment_status = request.payment_status.as_deref().unwrap_or_default();
    if !matches!(payment_status, "paid" | "no_payment_required") {
        if request.event_type == "checkout.session.completed" {
            return Ok(());
        }
        return Err(TicketingError::Conflict);
    }
    let event_currency = request.currency.as_deref().map(str::to_ascii_uppercase);
    if request.amount_total_minor != Some(order.amount_gross_minor)
        || event_currency.as_deref() != Some(order.currency.as_str())
    {
        return Err(TicketingError::Conflict);
    }
    if order.amount_gross_minor > 0 && request.stripe_payment_intent_id.is_none() {
        return Err(TicketingError::Conflict);
    }
    if request
        .customer_email
        .as_deref()
        .is_some_and(|email| !email.eq_ignore_ascii_case(&order.buyer_email))
    {
        return Err(TicketingError::Conflict);
    }

    let pool = sqlx::query_as::<_, PoolCapacityRow>(
        r#"
        SELECT capacity, issued_count, reserved_count
        FROM admission_pools
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let items = load_order_items(transaction, state.workspace_id, order.id).await?;
    let ticket_count: i32 = items.iter().try_fold(0_i32, |total, item| {
        total
            .checked_add(item.quantity)
            .ok_or(TicketingError::Unexpected)
    })?;
    if pool.reserved_count < ticket_count || pool.issued_count + ticket_count > pool.capacity {
        return Err(TicketingError::Conflict);
    }

    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO fans (workspace_id, normalized_email, display_name, status)
        VALUES ($1, $2, $3, 'active')
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE
        SET display_name = COALESCE(fans.display_name, EXCLUDED.display_name)
        RETURNING id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(&order.buyer_email)
    .bind(&order.buyer_name)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let claim_expires_at = order
        .ends_at
        .unwrap_or_else(|| order.starts_at + time::Duration::hours(12))
        + time::Duration::days(1);
    let issued_rows = sqlx::query_as::<_, IssuedPaidTicketRow>(
        r#"
        WITH expanded AS (
            SELECT
                item.id AS order_item_id,
                ticket_type.slug AS ticket_type_slug,
                ticket_type.name AS ticket_type_name,
                generate_series(1, item.quantity) AS sequence
            FROM ticket_order_items AS item
            JOIN ticket_types AS ticket_type
              ON ticket_type.workspace_id = item.workspace_id
             AND ticket_type.id = item.ticket_type_id
            WHERE item.workspace_id = $1
              AND item.ticket_order_id = $2
        ), inserted AS (
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, claim_token_consumed_at, status,
                claimed_at, ticket_order_item_id, ticket_sequence,
                holder_name, holder_email
            )
            SELECT
                gen_random_uuid(), $1, $3, $4, $5, 'paid',
                'VIRYA-' || upper(encode(gen_random_bytes(16), 'hex')),
                NULL, $6, now(), 'claimed', now(), expanded.order_item_id,
                expanded.sequence, $7, $8
            FROM expanded
            ORDER BY expanded.order_item_id, expanded.sequence
            RETURNING
                id AS pass_id,
                ticket_order_item_id AS order_item_id,
                ticket_sequence AS sequence,
                public_reference
        )
        SELECT
            inserted.pass_id,
            inserted.order_item_id,
            expanded.ticket_type_slug,
            expanded.ticket_type_name,
            inserted.sequence,
            inserted.public_reference
        FROM inserted
        JOIN expanded
          ON expanded.order_item_id = inserted.order_item_id
         AND expanded.sequence = inserted.sequence
        ORDER BY inserted.order_item_id, inserted.sequence
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(order.event_id)
    .bind(order.admission_pool_id)
    .bind(fan_id)
    .bind(claim_expires_at)
    .bind(&order.buyer_name)
    .bind(&order.buyer_email)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if issued_rows.len() != usize::try_from(ticket_count).map_err(|_| TicketingError::Unexpected)? {
        return Err(TicketingError::Unexpected);
    }
    let mut issued = Vec::with_capacity(issued_rows.len());
    for ticket in issued_rows {
        issued.push(json!({
            "pass_id": ticket.pass_id,
            "order_item_id": ticket.order_item_id,
            "ticket_type_slug": ticket.ticket_type_slug,
            "ticket_type_name": ticket.ticket_type_name,
            "sequence": ticket.sequence,
            "public_reference": ticket.public_reference,
        }));
    }

    let transferred = sqlx::query(
        r#"
        UPDATE admission_pools
        SET issued_count = issued_count + $3,
            reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
          AND issued_count + $3 <= capacity
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .bind(ticket_count)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if transferred.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    let paid_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = 'paid',
            stripe_payment_intent_id = $3,
            paid_at = $4
        WHERE workspace_id = $1 AND id = $2
          AND status = 'checkout_created'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_payment_intent_id)
    .bind(request.occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if paid_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    insert_accounting_entry(
        transaction,
        state,
        order,
        request,
        "sale",
        order.amount_gross_minor,
        order.amount_net_minor,
        order.amount_vat_minor,
    )
    .await?;

    let token_key = state
        .checkout_token_key
        .ok_or(TicketingError::Unavailable)?;
    let checkout_token = derive_checkout_token(&token_key, order.id, &order.reservation_key)?;
    let qr_not_before = ticket_qr_not_before(order);
    let qr_expires_at = ticket_qr_expires_at(order)?;
    for ticket in &mut issued {
        let pass_id = ticket
            .get("pass_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(TicketingError::Unexpected)?;
        let reference = ticket
            .get("public_reference")
            .and_then(Value::as_str)
            .ok_or(TicketingError::Unexpected)?;
        let qr_token = encode_ticket_qr(
            pass_id,
            order.event_id,
            reference,
            qr_not_before.unix_timestamp(),
            qr_expires_at.unix_timestamp(),
            &token_key,
        )
        .map_err(|_| TicketingError::Unexpected)?;
        let Some(object) = ticket.as_object_mut() else {
            return Err(TicketingError::Unexpected);
        };
        object.insert("qr_token".to_owned(), Value::String(qr_token));
        object.insert("qr_not_before".to_owned(), json!(qr_not_before));
        object.insert("qr_expires_at".to_owned(), json!(qr_expires_at));
    }

    append_outbox(
        transaction,
        state.workspace_id,
        "ticket.order.paid",
        request_id_value,
        json!({
            "order_id": order.id,
            "order_reference": order.public_reference,
            "event_id": order.event_id,
            "event_slug": order.event_slug,
            "event_title": order.event_title,
            "venue": order.venue,
            "timezone": order.timezone,
            "starts_at": order.starts_at,
            "buyer_email": order.buyer_email,
            "buyer_name": order.buyer_name,
            "buyer_locale": order.buyer_locale,
            "checkout_token": checkout_token,
            "invoice": invoice_payload(order),
            "currency": order.currency,
            "amount_gross_minor": order.amount_gross_minor,
            "amount_net_minor": order.amount_net_minor,
            "amount_vat_minor": order.amount_vat_minor,
            "vat_rate_basis_points": order.vat_rate_basis_points,
            "invoice_requested": order.invoice_requested,
            "stripe_checkout_session_id": stripe_checkout_session_id,
            "stripe_payment_intent_id": request.stripe_payment_intent_id,
            "tickets": issued,
        }),
    )
    .await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.paid",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "ticket_count": ticket_count,
            "amount_gross_minor": order.amount_gross_minor,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    Ok(())
}

async fn release_unpaid_order(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Ok(());
    }
    if !matches!(order.status.as_str(), "reserved" | "checkout_created") {
        return Ok(());
    }
    let status = if request.event_type == "checkout.session.async_payment_failed" {
        "payment_failed"
    } else {
        "expired"
    };
    release_order_reservation(transaction, state.workspace_id, order, status).await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.released",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "status": status,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    Ok(())
}

async fn process_refund(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if !matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    let refunded = request
        .amount_refunded_minor
        .ok_or(TicketingError::Invalid)?;
    if refunded < order.amount_refunded_minor || refunded > order.amount_gross_minor {
        return Err(TicketingError::Conflict);
    }
    if refunded == order.amount_refunded_minor {
        return Ok(());
    }
    let full = refunded == order.amount_gross_minor;
    let revoked = if full {
        let changed = sqlx::query(
            r#"
            UPDATE admission_passes AS pass
            SET status = 'revoked'
            FROM ticket_order_items AS item
            WHERE item.workspace_id = pass.workspace_id
              AND item.id = pass.ticket_order_item_id
              AND item.workspace_id = $1
              AND item.ticket_order_id = $2
              AND pass.status IN ('issued', 'claimed')
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(order.id)
        .execute(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .rows_affected();
        let revoked = i32::try_from(changed).map_err(|_| TicketingError::Unexpected)?;
        if revoked > 0 {
            let updated_pool = sqlx::query(
                r#"
                UPDATE admission_pools
                SET issued_count = issued_count - $3
                WHERE workspace_id = $1 AND id = $2
                  AND issued_count >= $3
                "#,
            )
            .bind(state.workspace_id.into_uuid())
            .bind(order.admission_pool_id)
            .bind(revoked)
            .execute(&mut **transaction)
            .await
            .map_err(TicketingError::sqlx)?;
            if updated_pool.rows_affected() != 1 {
                return Err(TicketingError::Unexpected);
            }
        }
        revoked
    } else {
        0
    };

    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = $3,
            amount_refunded_minor = $4,
            refunded_at = CASE WHEN $5 THEN $6 ELSE refunded_at END
        WHERE workspace_id = $1 AND id = $2
          AND status IN ('paid', 'partially_refunded', 'refunded')
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(if full {
        "refunded"
    } else {
        "partially_refunded"
    })
    .bind(refunded)
    .bind(full)
    .bind(request.occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    let refund_gross_minor = refunded
        .checked_sub(order.amount_refunded_minor)
        .ok_or(TicketingError::Unexpected)?;
    let previously_refunded_net_minor = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(-sum(amount_net_minor), 0)::bigint
        FROM ticket_accounting_entries
        WHERE workspace_id = $1
          AND ticket_order_id = $2
          AND entry_kind = 'refund'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let target_refunded_net_minor = if full {
        order.amount_net_minor
    } else {
        proportional_minor(order.amount_net_minor, refunded, order.amount_gross_minor)?
    };
    let refund_net_minor = target_refunded_net_minor
        .checked_sub(previously_refunded_net_minor)
        .ok_or(TicketingError::Unexpected)?;
    let refund_vat_minor = refund_gross_minor
        .checked_sub(refund_net_minor)
        .ok_or(TicketingError::Unexpected)?;
    insert_accounting_entry(
        transaction,
        state,
        order,
        request,
        "refund",
        -refund_gross_minor,
        -refund_net_minor,
        -refund_vat_minor,
    )
    .await?;

    append_outbox(
        transaction,
        state.workspace_id,
        "ticket.order.refund_recorded",
        request_id_value,
        json!({
            "order_id": order.id,
            "order_reference": order.public_reference,
            "event_id": order.event_id,
            "buyer_email": order.buyer_email,
            "amount_gross_minor": order.amount_gross_minor,
            "amount_refunded_minor": refunded,
            "currency": order.currency,
            "full_refund": full,
            "revoked_ticket_count": revoked,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.refund_recorded",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "amount_refunded_minor": refunded,
            "full_refund": full,
            "revoked_ticket_count": revoked,
        }),
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_accounting_entry(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    entry_kind: &str,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
) -> Result<(), TicketingError> {
    let balance_values_are_consistent = match (request.stripe_fee_minor, request.stripe_net_minor) {
        (Some(fee), Some(net)) => amount_gross_minor
            .checked_sub(fee)
            .is_some_and(|expected| expected == net),
        (None, None) => true,
        _ => false,
    };
    if !balance_values_are_consistent {
        return Err(TicketingError::Conflict);
    }
    sqlx::query(
        r#"
        INSERT INTO ticket_accounting_entries (
            workspace_id, ticket_order_id, event_id, stripe_event_id,
            entry_kind, occurred_at, currency, vat_rate_basis_points,
            amount_gross_minor, amount_net_minor, amount_vat_minor,
            stripe_balance_transaction_id, stripe_fee_minor, stripe_net_minor,
            stripe_reporting_category
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            $9, $10, $11, $12, $13, $14, $15
        )
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(order.event_id)
    .bind(&request.stripe_event_id)
    .bind(entry_kind)
    .bind(request.occurred_at)
    .bind(&order.currency)
    .bind(order.vat_rate_basis_points)
    .bind(amount_gross_minor)
    .bind(amount_net_minor)
    .bind(amount_vat_minor)
    .bind(&request.stripe_balance_transaction_id)
    .bind(request.stripe_fee_minor)
    .bind(request.stripe_net_minor)
    .bind(&request.stripe_reporting_category)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

fn proportional_minor(
    component_total: i64,
    cumulative_gross: i64,
    gross_total: i64,
) -> Result<i64, TicketingError> {
    if component_total < 0 || cumulative_gross < 0 || gross_total <= 0 {
        return Err(TicketingError::Unexpected);
    }
    let numerator = i128::from(component_total)
        .checked_mul(i128::from(cumulative_gross))
        .ok_or(TicketingError::Unexpected)?;
    let rounded = numerator
        .checked_add(i128::from(gross_total / 2))
        .ok_or(TicketingError::Unexpected)?
        / i128::from(gross_total);
    i64::try_from(rounded).map_err(|_| TicketingError::Unexpected)
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct ExistingSaleRow {
    id: Uuid,
    admission_pool_id: Uuid,
}

#[derive(Debug, FromRow)]
struct PoolCapacityRow {
    capacity: i32,
    issued_count: i32,
    reserved_count: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InventorySnapshot {
    sold: i32,
    reserved: i32,
    available: i32,
}

fn inventory_snapshot(
    capacity: i32,
    sold: i32,
    reserved: i32,
) -> Result<InventorySnapshot, TicketingError> {
    if capacity < 0 || sold < 0 || reserved < 0 {
        return Err(TicketingError::Unexpected);
    }
    let committed = sold
        .checked_add(reserved)
        .ok_or(TicketingError::Unexpected)?;
    if committed > capacity {
        return Err(TicketingError::Unexpected);
    }
    Ok(InventorySnapshot {
        sold,
        reserved,
        available: capacity - committed,
    })
}

#[derive(Debug, FromRow)]
struct OverviewTotalsRow {
    reserved_orders: i64,
    checkout_created_orders: i64,
    reserved_tickets: i64,
    paid_orders: i64,
    paid_tickets: i64,
    gross_sales_minor: i64,
    refunded_minor: i64,
}

const SALE_ROW_QUERY: &str = r#"
    SELECT
        sale.id,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.status AS event_status,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at,
        sale.currency::text AS currency,
        sale.vat_rate_basis_points,
        pool.capacity AS capacity,
        pool.issued_count,
        pool.reserved_count,
        sale.max_per_order,
        sale.hold_seconds,
        sale.sales_open_at,
        sale.sales_close_at,
        sale.active
    FROM ticket_sales AS sale
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    JOIN admission_pools AS pool
      ON pool.workspace_id = sale.workspace_id
     AND pool.id = sale.admission_pool_id
    WHERE sale.workspace_id = $1
      AND event.slug = $2
"#;

const ORDER_ROW_BASE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
"#;

const ORDER_ROW_BY_ID_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1 AND orders.id = $2
    FOR UPDATE OF orders
"#;

const ORDER_ROW_BY_STRIPE_SESSION_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1
      AND orders.stripe_checkout_session_id = $2
    FOR UPDATE OF orders
"#;

const ORDER_ROW_BY_PAYMENT_INTENT_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1
      AND orders.stripe_payment_intent_id = $2
    FOR UPDATE OF orders
"#;

async fn lock_sale(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_slug: &str,
) -> Result<SaleRow, TicketingError> {
    let query = format!("{SALE_ROW_QUERY} FOR UPDATE OF sale, pool");
    sqlx::query_as::<_, SaleRow>(&query)
        .bind(workspace_id.into_uuid())
        .bind(event_slug)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn cleanup_expired_reservations(
    state: &TicketingState,
    event_slug: &str,
) -> Result<(), TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    let sale = lock_sale(&mut transaction, state.workspace_id, event_slug).await?;
    expire_active_reservations(
        &mut transaction,
        state.workspace_id,
        sale.id,
        sale.admission_pool_id,
    )
    .await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(())
}

async fn load_sale_view(
    state: &TicketingState,
    event_slug: &str,
    include_inactive: bool,
) -> Result<TicketSaleView, TicketingError> {
    cleanup_expired_reservations(state, event_slug).await?;
    let sale = sqlx::query_as::<_, SaleRow>(SALE_ROW_QUERY)
        .bind(state.workspace_id.into_uuid())
        .bind(event_slug)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    if !include_inactive
        && (!sale.active
            || sale.event_status != "published"
            || sale.starts_at <= OffsetDateTime::now_utc())
    {
        return Err(TicketingError::NotFound);
    }
    let ticket_types = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND ($3 OR active)
        ORDER BY sort_order, id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(include_inactive)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    build_sale_view(&state.pool, state.workspace_id, sale, ticket_types).await
}

async fn build_sale_view(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    sale: SaleRow,
    ticket_types: Vec<TicketTypeRow>,
) -> Result<TicketSaleView, TicketingError> {
    let sale_inventory = inventory_snapshot(sale.capacity, sale.issued_count, sale.reserved_count)?;
    let inventory_by_type = active_type_inventory_pool(pool, workspace_id, sale.id).await?;
    let sale_remaining = i64::from(sale_inventory.available);
    let mut type_views = Vec::with_capacity(ticket_types.len());
    for ticket_type in ticket_types {
        let inventory = inventory_by_type
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default();
        let committed = inventory.committed()?;
        let type_remaining = match ticket_type.capacity {
            Some(capacity) => {
                let capacity = i64::from(capacity);
                if committed > capacity {
                    return Err(TicketingError::Unexpected);
                }
                capacity - committed
            }
            None => sale_remaining,
        };
        let available = sale_remaining.min(type_remaining).min(i64::from(i32::MAX));
        type_views.push(TicketTypeView {
            id: ticket_type.id,
            slug: ticket_type.slug,
            name: ticket_type.name,
            description: ticket_type.description,
            price_gross_minor: ticket_type.price_gross_minor,
            capacity: ticket_type.capacity,
            sold: i32::try_from(inventory.sold).map_err(|_| TicketingError::Unexpected)?,
            reserved: i32::try_from(inventory.reserved).map_err(|_| TicketingError::Unexpected)?,
            available: i32::try_from(available).map_err(|_| TicketingError::Unexpected)?,
            sort_order: ticket_type.sort_order,
            active: ticket_type.active,
        });
    }
    let now = OffsetDateTime::now_utc();
    let hard_close_at = sale.sales_close_at.min(sale.starts_at);
    let latest_checkout_at = hard_close_at
        .checked_sub(time::Duration::seconds(i64::from(sale.hold_seconds)))
        .unwrap_or(sale.sales_open_at);
    let sales_state = if sale.event_status != "published" || now >= sale.starts_at {
        "event_unavailable"
    } else if !sale.active {
        "inactive"
    } else if now < sale.sales_open_at {
        "upcoming"
    } else if now > latest_checkout_at {
        "closed"
    } else if sale_inventory.available == 0 {
        "sold_out"
    } else {
        "open"
    };
    Ok(TicketSaleView {
        event_id: sale.event_id,
        event_slug: sale.event_slug,
        event_title: sale.event_title,
        event_status: sale.event_status,
        venue: sale.venue,
        timezone: sale.timezone,
        starts_at: sale.starts_at,
        currency: sale.currency,
        vat_rate_basis_points: sale.vat_rate_basis_points,
        capacity: sale.capacity,
        sold: sale_inventory.sold,
        reserved: sale_inventory.reserved,
        available: sale_inventory.available,
        max_per_order: sale.max_per_order,
        sales_open_at: sale.sales_open_at,
        sales_close_at: sale.sales_close_at,
        active: sale.active,
        sales_state,
        ticket_types: type_views,
    })
}

async fn load_admin_overview(
    state: &TicketingState,
    event_slug: &str,
) -> Result<AdminTicketingOverview, TicketingError> {
    let sale = load_sale_view(state, event_slug, true).await?;
    let sale_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT sale.id
        FROM ticket_sales AS sale
        JOIN events AS event
          ON event.workspace_id = sale.workspace_id
         AND event.id = sale.event_id
        WHERE sale.workspace_id = $1 AND event.slug = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_one(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let totals = sqlx::query_as::<_, OverviewTotalsRow>(
        r#"
        WITH order_totals AS (
            SELECT
                orders.id,
                orders.status,
                orders.expires_at,
                orders.amount_gross_minor,
                orders.amount_refunded_minor,
                COALESCE(sum(item.quantity), 0)::bigint AS ticket_count
            FROM ticket_orders AS orders
            LEFT JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE orders.workspace_id = $1
              AND orders.ticket_sale_id = $2
            GROUP BY
                orders.id,
                orders.status,
                orders.expires_at,
                orders.amount_gross_minor,
                orders.amount_refunded_minor
        )
        SELECT
            count(*) FILTER (
                WHERE status IN ('reserved', 'checkout_created')
                  AND expires_at > now()
            )::bigint AS reserved_orders,
            count(*) FILTER (
                WHERE status = 'checkout_created'
                  AND expires_at > now()
            )::bigint AS checkout_created_orders,
            COALESCE(sum(ticket_count) FILTER (
                WHERE status IN ('reserved', 'checkout_created')
                  AND expires_at > now()
            ), 0)::bigint AS reserved_tickets,
            count(*) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            )::bigint AS paid_orders,
            COALESCE(sum(ticket_count) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            ), 0)::bigint AS paid_tickets,
            COALESCE(sum(amount_gross_minor) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            ), 0)::bigint AS gross_sales_minor,
            COALESCE(sum(amount_refunded_minor), 0)::bigint AS refunded_minor
        FROM order_totals
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_one(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let recent_rows = sqlx::query_as::<_, OrderRow>(&format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.ticket_sale_id = $2 ORDER BY orders.created_at DESC, orders.id DESC LIMIT 50"
    ))
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let mut recent_orders = Vec::with_capacity(recent_rows.len());
    for row in recent_rows {
        recent_orders.push(load_order_view_pool(&state.pool, state.workspace_id, row).await?);
    }
    Ok(AdminTicketingOverview {
        sale,
        reserved_orders: totals.reserved_orders,
        checkout_created_orders: totals.checkout_created_orders,
        reserved_tickets: totals.reserved_tickets,
        paid_orders: totals.paid_orders,
        paid_tickets: totals.paid_tickets,
        gross_sales_minor: totals.gross_sales_minor,
        refunded_minor: totals.refunded_minor,
        recent_orders,
    })
}

async fn load_order_by_token(
    state: &TicketingState,
    order_id: Uuid,
    checkout_token: &str,
) -> Result<TicketOrderView, TicketingError> {
    if !valid_checkout_token(checkout_token) {
        return Err(TicketingError::NotFound);
    }
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.id = $2 AND orders.checkout_token_hash = digest($3, 'sha256')"
    );
    let row = sqlx::query_as::<_, OrderRow>(&query)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(checkout_token)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    load_order_view_pool(&state.pool, state.workspace_id, row).await
}

async fn load_order_row_by_reservation_key(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    reservation_key: &str,
) -> Result<Option<OrderRow>, TicketingError> {
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.reservation_key = $2 FOR UPDATE OF orders"
    );
    sqlx::query_as::<_, OrderRow>(&query)
        .bind(workspace_id.into_uuid())
        .bind(reservation_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)
}

async fn load_order_row_by_id(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<OrderRow, TicketingError> {
    sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn expire_active_reservations(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
    admission_pool_id: Uuid,
) -> Result<i32, TicketingError> {
    let released_quantity = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expired_orders AS MATERIALIZED (
            SELECT id
            FROM ticket_orders
            WHERE workspace_id = $1
              AND ticket_sale_id = $2
              AND status IN ('reserved', 'checkout_created')
              AND expires_at <= now()
            FOR UPDATE
        ),
        released_orders AS (
            UPDATE ticket_orders AS orders
            SET status = 'expired', released_at = now()
            FROM expired_orders
            WHERE orders.workspace_id = $1
              AND orders.id = expired_orders.id
            RETURNING orders.id
        )
        SELECT COALESCE(sum(item.quantity), 0)::bigint
        FROM released_orders
        JOIN ticket_order_items AS item
          ON item.workspace_id = $1
         AND item.ticket_order_id = released_orders.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let released_quantity =
        i32::try_from(released_quantity).map_err(|_| TicketingError::Unexpected)?;
    if released_quantity == 0 {
        return Ok(0);
    }
    let released = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(admission_pool_id)
    .bind(released_quantity)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if released.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    Ok(released_quantity)
}

async fn order_ticket_count(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<i32, TicketingError> {
    let quantity = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(sum(quantity), 0)::bigint
        FROM ticket_order_items
        WHERE workspace_id = $1 AND ticket_order_id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let quantity = i32::try_from(quantity).map_err(|_| TicketingError::Unexpected)?;
    if quantity <= 0 {
        return Err(TicketingError::Unexpected);
    }
    Ok(quantity)
}

async fn release_order_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order: &OrderRow,
    status: &str,
) -> Result<(), TicketingError> {
    if !matches!(status, "expired" | "cancelled" | "payment_failed") {
        return Err(TicketingError::Unexpected);
    }
    let quantity = order_ticket_count(transaction, workspace_id, order.id).await?;
    let released = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .bind(quantity)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if released.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = $3, released_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND status IN ('reserved', 'checkout_created')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order.id)
    .bind(status)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }
    Ok(())
}

async fn load_order_items(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<OrderItemRow>, TicketingError> {
    sqlx::query_as::<_, OrderItemRow>(
        r#"
        SELECT
            item.id,
            item.ticket_type_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            item.quantity,
            item.unit_gross_minor,
            item.unit_net_minor,
            item.unit_vat_minor,
            item.total_gross_minor,
            item.total_net_minor,
            item.total_vat_minor
        FROM ticket_order_items AS item
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_order_items_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<OrderItemRow>, TicketingError> {
    sqlx::query_as::<_, OrderItemRow>(
        r#"
        SELECT
            item.id,
            item.ticket_type_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            item.quantity,
            item.unit_gross_minor,
            item.unit_net_minor,
            item.unit_vat_minor,
            item.total_gross_minor,
            item.total_net_minor,
            item.total_vat_minor
        FROM ticket_order_items AS item
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_issued_tickets(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<IssuedTicketRow>, TicketingError> {
    sqlx::query_as::<_, IssuedTicketRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id
         AND item.id = pass.ticket_order_item_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_issued_tickets_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<IssuedTicketRow>, TicketingError> {
    sqlx::query_as::<_, IssuedTicketRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id
         AND item.id = pass.ticket_order_item_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_order_view_for_row(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    row: OrderRow,
) -> Result<TicketOrderView, TicketingError> {
    let items = load_order_items(transaction, workspace_id, row.id).await?;
    let tickets = load_issued_tickets(transaction, workspace_id, row.id).await?;
    Ok(order_view(row, items, tickets))
}

async fn load_order_view_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    row: OrderRow,
) -> Result<TicketOrderView, TicketingError> {
    let items = load_order_items_pool(pool, workspace_id, row.id).await?;
    let tickets = load_issued_tickets_pool(pool, workspace_id, row.id).await?;
    Ok(order_view(row, items, tickets))
}

fn order_view(
    row: OrderRow,
    items: Vec<OrderItemRow>,
    tickets: Vec<IssuedTicketRow>,
) -> TicketOrderView {
    TicketOrderView {
        order_id: row.id,
        public_reference: row.public_reference,
        event_slug: row.event_slug,
        event_title: row.event_title,
        venue: row.venue,
        timezone: row.timezone,
        starts_at: row.starts_at,
        status: row.status,
        buyer_email_masked: mask_email(&row.buyer_email),
        buyer_name: row.buyer_name,
        currency: row.currency,
        amount_gross_minor: row.amount_gross_minor,
        amount_net_minor: row.amount_net_minor,
        amount_vat_minor: row.amount_vat_minor,
        amount_refunded_minor: row.amount_refunded_minor,
        vat_rate_basis_points: row.vat_rate_basis_points,
        invoice_requested: row.invoice_requested,
        expires_at: row.expires_at,
        paid_at: row.paid_at,
        refunded_at: row.refunded_at,
        items: items
            .into_iter()
            .map(|item| TicketOrderItemView {
                id: item.id,
                ticket_type_slug: item.ticket_type_slug,
                ticket_type_name: item.ticket_type_name,
                quantity: item.quantity,
                unit_gross_minor: item.unit_gross_minor,
                unit_net_minor: item.unit_net_minor,
                unit_vat_minor: item.unit_vat_minor,
                total_gross_minor: item.total_gross_minor,
                total_net_minor: item.total_net_minor,
                total_vat_minor: item.total_vat_minor,
            })
            .collect(),
        tickets: tickets
            .into_iter()
            .map(|ticket| IssuedTicketView {
                pass_id: ticket.pass_id,
                order_item_id: ticket.order_item_id,
                sequence: ticket.sequence,
                public_reference: ticket.public_reference,
                status: ticket.status,
                holder_name: ticket.holder_name,
                holder_email_masked: mask_email(&ticket.holder_email),
                redeemed_at: ticket.redeemed_at,
            })
            .collect(),
    }
}

const TYPE_INVENTORY_QUERY: &str = r#"
    WITH inventory AS (
        SELECT
            item.ticket_type_id,
            item.quantity::bigint AS reserved,
            0::bigint AS sold
        FROM ticket_order_items AS item
        JOIN ticket_orders AS orders
          ON orders.workspace_id = item.workspace_id
         AND orders.id = item.ticket_order_id
        WHERE orders.workspace_id = $1
          AND orders.ticket_sale_id = $2
          AND (
              orders.status IN ('reserved', 'checkout_created')
              AND orders.expires_at > now()
          )

        UNION ALL

        SELECT
            item.ticket_type_id,
            0::bigint AS reserved,
            count(pass.id)::bigint AS sold
        FROM ticket_order_items AS item
        JOIN ticket_orders AS orders
          ON orders.workspace_id = item.workspace_id
         AND orders.id = item.ticket_order_id
        JOIN admission_passes AS pass
          ON pass.workspace_id = item.workspace_id
         AND pass.ticket_order_item_id = item.id
        WHERE orders.workspace_id = $1
          AND orders.ticket_sale_id = $2
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
          AND pass.status IN ('issued', 'claimed', 'redeemed')
        GROUP BY item.ticket_type_id
    )
    SELECT
        ticket_type_id,
        COALESCE(sum(reserved), 0)::bigint AS reserved,
        COALESCE(sum(sold), 0)::bigint AS sold
    FROM inventory
    GROUP BY ticket_type_id
"#;

fn collect_type_inventory(
    rows: Vec<TypeInventoryRow>,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let mut inventory = HashMap::with_capacity(rows.len());
    for row in rows {
        if row.reserved < 0 || row.sold < 0 {
            return Err(TicketingError::Unexpected);
        }
        inventory.insert(
            row.ticket_type_id,
            TypeInventory {
                reserved: row.reserved,
                sold: row.sold,
            },
        );
    }
    Ok(inventory)
}

async fn active_type_inventory(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let rows = sqlx::query_as::<_, TypeInventoryRow>(TYPE_INVENTORY_QUERY)
        .bind(workspace_id.into_uuid())
        .bind(sale_id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    collect_type_inventory(rows)
}

async fn active_type_inventory_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let rows = sqlx::query_as::<_, TypeInventoryRow>(TYPE_INVENTORY_QUERY)
        .bind(workspace_id.into_uuid())
        .bind(sale_id)
        .fetch_all(pool)
        .await
        .map_err(TicketingError::sqlx)?;
    collect_type_inventory(rows)
}

fn valid_sale_configuration(request: &ConfigureTicketSaleRequest) -> bool {
    let currency = request.currency.trim();
    currency.len() == 3
        && currency.bytes().all(|byte| byte.is_ascii_alphabetic())
        && (0..=10_000).contains(&request.vat_rate_basis_points)
        && (1..=1_000_000).contains(&request.capacity)
        && (1..=100).contains(&request.max_per_order)
        && (2_100..=86_400).contains(&request.hold_seconds)
        && request.sales_open_at < request.sales_close_at
        && !request.ticket_types.is_empty()
        && request.ticket_types.len() <= MAX_TICKET_TYPES
}

fn normalize_ticket_types(
    request: &ConfigureTicketSaleRequest,
) -> Result<Vec<NormalizedTicketType>, TicketingError> {
    let mut slugs = HashSet::with_capacity(request.ticket_types.len());
    let mut normalized = Vec::with_capacity(request.ticket_types.len());
    for ticket_type in &request.ticket_types {
        let slug = EventSlug::parse(ticket_type.slug.trim())
            .map_err(|_| TicketingError::Invalid)?
            .as_str()
            .to_owned();
        if !slugs.insert(slug.clone()) {
            return Err(TicketingError::Invalid);
        }
        let name = clean_text(&ticket_type.name, MAX_NAME_CHARS).ok_or(TicketingError::Invalid)?;
        let description = match ticket_type.description.as_deref() {
            Some(value) => {
                Some(clean_text(value, MAX_DESCRIPTION_CHARS).ok_or(TicketingError::Invalid)?)
            }
            None => None,
        };
        if !(1..=1_000_000_000).contains(&ticket_type.price_gross_minor)
            || ticket_type
                .capacity
                .is_some_and(|capacity| !(1..=request.capacity).contains(&capacity))
            || !(-100_000..=100_000).contains(&ticket_type.sort_order)
        {
            return Err(TicketingError::Invalid);
        }
        normalized.push(NormalizedTicketType {
            slug,
            name,
            description,
            price_gross_minor: ticket_type.price_gross_minor,
            capacity: ticket_type.capacity,
            sort_order: ticket_type.sort_order,
            active: ticket_type.active,
        });
    }
    Ok(normalized)
}

fn normalize_reservation(
    event_slug: &str,
    request: ReserveTicketOrderRequest,
) -> Result<NormalizedReservation, TicketingError> {
    if request.items.is_empty() || request.items.len() > MAX_ORDER_LINES {
        return Err(TicketingError::Invalid);
    }
    let buyer_email =
        NormalizedEmail::parse(request.buyer_email).map_err(|_| TicketingError::Invalid)?;
    let buyer_name = match request.buyer_name.as_deref() {
        Some(value) => Some(clean_text(value, MAX_NAME_CHARS).ok_or(TicketingError::Invalid)?),
        None => None,
    };
    let buyer_locale = match request.buyer_locale.trim().to_ascii_lowercase().as_str() {
        "pl" => "pl".to_owned(),
        "en" => "en".to_owned(),
        _ => return Err(TicketingError::Invalid),
    };
    let invoice_details =
        normalize_invoice_details(request.invoice_requested, request.invoice_details)?;
    let mut quantities = BTreeMap::<String, i32>::new();
    for item in request.items {
        if !(1..=100).contains(&item.quantity) {
            return Err(TicketingError::Invalid);
        }
        let slug = EventSlug::parse(item.ticket_type_slug.trim())
            .map_err(|_| TicketingError::Invalid)?
            .as_str()
            .to_owned();
        let entry = quantities.entry(slug).or_insert(0);
        *entry = entry
            .checked_add(item.quantity)
            .ok_or(TicketingError::Invalid)?;
    }
    let total_quantity = quantities.values().try_fold(0_i32, |total, quantity| {
        total.checked_add(*quantity).ok_or(TicketingError::Invalid)
    })?;
    let items: Vec<(String, i32)> = quantities.into_iter().collect();
    let request_hash: [u8; 32] = Sha256::digest(
        serde_json::to_vec(&json!({
            "event_slug": event_slug,
            "buyer_email": buyer_email.as_str(),
            "buyer_name": buyer_name.as_deref(),
            "buyer_locale": &buyer_locale,
            "invoice_requested": request.invoice_requested,
            "invoice_details": &invoice_details,
            "items": &items,
        }))
        .map_err(|_| TicketingError::Invalid)?,
    )
    .into();
    Ok(NormalizedReservation {
        buyer_email,
        buyer_name,
        buyer_locale,
        invoice_requested: request.invoice_requested,
        invoice_details,
        items,
        total_quantity,
        request_hash,
    })
}

fn normalize_invoice_details(
    invoice_requested: bool,
    details: Option<InvoiceDetailsRequest>,
) -> Result<Option<InvoiceDetailsRequest>, TicketingError> {
    if !invoice_requested {
        return details
            .is_none()
            .then_some(None)
            .ok_or(TicketingError::Invalid);
    }
    let details = details.ok_or(TicketingError::Invalid)?;
    let buyer_type = match details.buyer_type.trim().to_ascii_lowercase().as_str() {
        "individual" => "individual".to_owned(),
        "company" => "company".to_owned(),
        _ => return Err(TicketingError::Invalid),
    };
    let address_line1 = clean_text(&details.address_line1, MAX_INVOICE_TEXT_CHARS)
        .ok_or(TicketingError::Invalid)?;
    let postal_code = clean_text(&details.postal_code, 32).ok_or(TicketingError::Invalid)?;
    let city = clean_text(&details.city, 120).ok_or(TicketingError::Invalid)?;
    let country_code = details.country_code.trim().to_ascii_uppercase();
    if country_code.len() != 2 || !country_code.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err(TicketingError::Invalid);
    }
    let (company_name, tax_id, full_name) = if buyer_type == "company" {
        (
            Some(
                clean_text(details.company_name.as_deref().unwrap_or_default(), 200)
                    .ok_or(TicketingError::Invalid)?,
            ),
            Some(
                clean_text(details.tax_id.as_deref().unwrap_or_default(), 32)
                    .ok_or(TicketingError::Invalid)?,
            ),
            None,
        )
    } else {
        (
            None,
            None,
            Some(
                clean_text(details.full_name.as_deref().unwrap_or_default(), 200)
                    .ok_or(TicketingError::Invalid)?,
            ),
        )
    };
    Ok(Some(InvoiceDetailsRequest {
        buyer_type,
        company_name,
        tax_id,
        full_name,
        address_line1,
        postal_code,
        city,
        country_code,
    }))
}

fn split_gross(gross_minor: i64, vat_rate_basis_points: i32) -> Result<(i64, i64), TicketingError> {
    if gross_minor < 0 || !(0..=10_000).contains(&vat_rate_basis_points) {
        return Err(TicketingError::Invalid);
    }
    let divisor = i128::from(10_000 + vat_rate_basis_points);
    let numerator = i128::from(gross_minor)
        .checked_mul(10_000)
        .ok_or(TicketingError::Invalid)?;
    let net = numerator
        .checked_add(divisor / 2)
        .ok_or(TicketingError::Invalid)?
        / divisor;
    let net = i64::try_from(net).map_err(|_| TicketingError::Invalid)?;
    let vat = gross_minor
        .checked_sub(net)
        .ok_or(TicketingError::Invalid)?;
    Ok((net, vat))
}

fn derive_checkout_token(
    key: &[u8; 32],
    order_id: Uuid,
    reservation_key: &str,
) -> Result<String, TicketingError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| TicketingError::Unexpected)?;
    mac.update(CHECKOUT_TOKEN_CONTEXT);
    mac.update(order_id.as_bytes());
    mac.update(reservation_key.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn order_public_reference(order_id: Uuid) -> String {
    let suffix: String = order_id
        .simple()
        .to_string()
        .chars()
        .skip(16)
        .collect::<String>()
        .to_ascii_uppercase();
    format!("VRY-ORD-{suffix}")
}

fn valid_stripe_event(request: &StripeTicketEventRequest) -> bool {
    let checkout_event = matches!(
        request.event_type.as_str(),
        "checkout.session.completed"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.expired"
            | "checkout.session.async_payment_failed"
    );
    let refund_event = matches!(
        request.event_type.as_str(),
        "charge.refunded" | "refund.created" | "refund.updated"
    );
    (checkout_event || refund_event)
        && valid_stripe_id(&request.stripe_event_id, "evt_")
        && request
            .stripe_checkout_session_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "cs_"))
        && request
            .stripe_payment_intent_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "pi_"))
        && (!checkout_event || request.stripe_checkout_session_id.is_some())
        && (!refund_event || request.stripe_payment_intent_id.is_some())
        && request.amount_total_minor.is_none_or(|value| value >= 0)
        && request.amount_refunded_minor.is_none_or(|value| value >= 0)
        && request.currency.as_deref().is_none_or(|value| {
            value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
        && request
            .stripe_balance_transaction_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "txn_"))
        && (request.stripe_fee_minor.is_some() == request.stripe_net_minor.is_some())
        && (request.stripe_balance_transaction_id.is_some() == request.stripe_fee_minor.is_some())
        && request
            .stripe_reporting_category
            .as_deref()
            .is_none_or(|value| {
                !value.trim().is_empty()
                    && value.len() <= 80
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
}

fn valid_checkout_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_stripe_id(value: &str, prefix: &str) -> bool {
    value.len() <= 255
        && value.starts_with(prefix)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn clean_text(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= max_chars).then(|| value.to_owned())
}

fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "***".to_owned();
    };
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{domain}")
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(&IDEMPOTENCY_KEY)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return None;
    }
    Some(value.to_owned())
}

fn trusted_request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn ticket_qr_not_before(order: &OrderRow) -> OffsetDateTime {
    order
        .doors_at
        .unwrap_or(order.starts_at)
        .saturating_sub(time::Duration::hours(6))
}

fn ticket_qr_expires_at(order: &OrderRow) -> Result<OffsetDateTime, TicketingError> {
    order
        .ends_at
        .unwrap_or_else(|| order.starts_at + time::Duration::hours(12))
        .checked_add(time::Duration::hours(24))
        .ok_or(TicketingError::Unexpected)
}

fn invoice_payload(order: &OrderRow) -> Value {
    if !order.invoice_requested {
        return Value::Null;
    }
    json!({
        "buyer_type": order.invoice_buyer_type,
        "company_name": order.invoice_company_name,
        "tax_id": order.invoice_tax_id,
        "full_name": order.invoice_full_name,
        "address_line1": order.invoice_address_line1,
        "postal_code": order.invoice_postal_code,
        "city": order.invoice_city,
        "country_code": order.invoice_country_code,
    })
}

async fn load_ticket_wallet(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
    signing_key: &[u8; 32],
) -> Result<TicketWalletView, TicketingError> {
    let order = load_order_row_by_token_pool(state, order_id, token).await?;
    build_ticket_wallet_pool(state, order, signing_key).await
}

async fn load_order_row_by_token_pool(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
) -> Result<OrderRow, TicketingError> {
    if !valid_checkout_token(token) {
        return Err(TicketingError::NotFound);
    }
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.id = $2 AND orders.checkout_token_hash = digest($3, 'sha256')"
    );
    sqlx::query_as::<_, OrderRow>(&query)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(token)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn build_ticket_wallet_pool(
    state: &TicketingState,
    order: OrderRow,
    signing_key: &[u8; 32],
) -> Result<TicketWalletView, TicketingError> {
    if !matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    let order_view =
        load_order_view_pool(&state.pool, state.workspace_id, clone_order_row(&order)).await?;
    let rows = sqlx::query_as::<_, TicketWalletRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id
         AND item.id = pass.ticket_order_item_id
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id, pass.ticket_sequence
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let qr_not_before = ticket_qr_not_before(&order);
    let qr_expires_at = ticket_qr_expires_at(&order)?;
    let tickets = rows
        .into_iter()
        .map(|row| {
            let qr_token = if row.status == "claimed" {
                Some(
                    encode_ticket_qr(
                        row.pass_id,
                        order.event_id,
                        &row.public_reference,
                        qr_not_before.unix_timestamp(),
                        qr_expires_at.unix_timestamp(),
                        signing_key,
                    )
                    .map_err(|_| TicketingError::Unexpected)?,
                )
            } else {
                None
            };
            Ok(TicketWalletPassView {
                pass_id: row.pass_id,
                order_item_id: row.order_item_id,
                ticket_type_slug: row.ticket_type_slug,
                ticket_type_name: row.ticket_type_name,
                sequence: row.sequence,
                public_reference: row.public_reference,
                status: row.status,
                holder_name: row.holder_name,
                holder_email_masked: mask_email(&row.holder_email),
                redeemed_at: row.redeemed_at,
                qr_token,
                qr_not_before,
                qr_expires_at,
            })
        })
        .collect::<Result<Vec<_>, TicketingError>>()?;
    Ok(TicketWalletView {
        order: order_view,
        tickets,
    })
}

async fn request_ticket_delivery(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    signing_key: &[u8; 32],
) -> Result<TicketDeliveryRequestResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    if !valid_checkout_token(token) {
        return Err(TicketingError::NotFound);
    }
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    let token_matches = sqlx::query_scalar::<_, bool>(
        "SELECT checkout_token_hash = digest($3, 'sha256') FROM ticket_orders WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }
    if !matches!(order.status.as_str(), "paid" | "partially_refunded") {
        return Err(TicketingError::Conflict);
    }
    let request_hash: [u8; 32] =
        Sha256::digest(format!("{order_id}:{idempotency_key}").as_bytes()).into();
    let existing = sqlx::query_as::<_, (Vec<u8>, OffsetDateTime)>(
        "SELECT request_hash, created_at FROM ticket_delivery_requests WHERE workspace_id = $1 AND idempotency_key = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(idempotency_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if let Some((existing_hash, created_at)) = existing {
        if existing_hash.as_slice() != request_hash.as_slice() {
            return Err(TicketingError::Conflict);
        }
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(TicketDeliveryRequestResponse {
            accepted: true,
            duplicate: true,
            requested_at: created_at,
        });
    }
    let last_requested = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "SELECT last_delivery_requested_at FROM ticket_orders WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let now = OffsetDateTime::now_utc();
    if last_requested
        .is_some_and(|value| (now - value).whole_seconds() < DELIVERY_RESEND_COOLDOWN_SECONDS)
    {
        return Err(TicketingError::Conflict);
    }
    sqlx::query(
        "INSERT INTO ticket_delivery_requests (workspace_id, ticket_order_id, idempotency_key, request_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(idempotency_key)
    .bind(request_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let updated = sqlx::query(
        "UPDATE ticket_orders SET last_delivery_requested_at = $3, delivery_request_count = delivery_request_count + 1 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(now)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    let rows = load_wallet_rows(&mut transaction, state.workspace_id, order_id).await?;
    let qr_not_before = ticket_qr_not_before(&order);
    let qr_expires_at = ticket_qr_expires_at(&order)?;
    let tickets = wallet_rows_payload(
        rows,
        order.event_id,
        qr_not_before,
        qr_expires_at,
        signing_key,
    )?;
    append_outbox(
        &mut transaction,
        state.workspace_id,
        "ticket.order.delivery_requested",
        request_id_value,
        json!({
            "order_id": order.id,
            "order_reference": order.public_reference,
            "event_id": order.event_id,
            "event_slug": order.event_slug,
            "event_title": order.event_title,
            "venue": order.venue,
            "timezone": order.timezone,
            "starts_at": order.starts_at,
            "buyer_email": order.buyer_email,
            "buyer_name": order.buyer_name,
            "buyer_locale": order.buyer_locale,
            "checkout_token": token,
            "tickets": tickets,
        }),
    )
    .await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(TicketDeliveryRequestResponse {
        accepted: true,
        duplicate: false,
        requested_at: now,
    })
}

async fn load_wallet_rows(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<TicketWalletRow>, TicketingError> {
    sqlx::query_as::<_, TicketWalletRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id AND item.id = pass.ticket_order_item_id
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

fn wallet_rows_payload(
    rows: Vec<TicketWalletRow>,
    event_id: Uuid,
    qr_not_before: OffsetDateTime,
    qr_expires_at: OffsetDateTime,
    signing_key: &[u8; 32],
) -> Result<Vec<Value>, TicketingError> {
    rows.into_iter()
        .map(|row| {
            let qr_token = (row.status == "claimed")
                .then(|| {
                    encode_ticket_qr(
                        row.pass_id,
                        event_id,
                        &row.public_reference,
                        qr_not_before.unix_timestamp(),
                        qr_expires_at.unix_timestamp(),
                        signing_key,
                    )
                })
                .transpose()
                .map_err(|_| TicketingError::Unexpected)?;
            Ok(json!({
                "pass_id": row.pass_id,
                "order_item_id": row.order_item_id,
                "ticket_type_slug": row.ticket_type_slug,
                "ticket_type_name": row.ticket_type_name,
                "sequence": row.sequence,
                "public_reference": row.public_reference,
                "status": row.status,
                "holder_name": row.holder_name,
                "holder_email": row.holder_email,
                "redeemed_at": row.redeemed_at,
                "qr_token": qr_token,
                "qr_not_before": qr_not_before,
                "qr_expires_at": qr_expires_at,
            }))
        })
        .collect()
}

fn clone_order_row(row: &OrderRow) -> OrderRow {
    OrderRow {
        id: row.id,
        ticket_sale_id: row.ticket_sale_id,
        public_reference: row.public_reference.clone(),
        status: row.status.clone(),
        buyer_email: row.buyer_email.clone(),
        buyer_name: row.buyer_name.clone(),
        buyer_locale: row.buyer_locale.clone(),
        invoice_buyer_type: row.invoice_buyer_type.clone(),
        invoice_company_name: row.invoice_company_name.clone(),
        invoice_tax_id: row.invoice_tax_id.clone(),
        invoice_full_name: row.invoice_full_name.clone(),
        invoice_address_line1: row.invoice_address_line1.clone(),
        invoice_postal_code: row.invoice_postal_code.clone(),
        invoice_city: row.invoice_city.clone(),
        invoice_country_code: row.invoice_country_code.clone(),
        currency: row.currency.clone(),
        amount_gross_minor: row.amount_gross_minor,
        amount_net_minor: row.amount_net_minor,
        amount_vat_minor: row.amount_vat_minor,
        amount_refunded_minor: row.amount_refunded_minor,
        vat_rate_basis_points: row.vat_rate_basis_points,
        invoice_requested: row.invoice_requested,
        reservation_key: row.reservation_key.clone(),
        request_hash: row.request_hash.clone(),
        expires_at: row.expires_at,
        stripe_checkout_session_id: row.stripe_checkout_session_id.clone(),
        stripe_payment_intent_id: row.stripe_payment_intent_id.clone(),
        paid_at: row.paid_at,
        refunded_at: row.refunded_at,
        event_id: row.event_id,
        admission_pool_id: row.admission_pool_id,
        event_slug: row.event_slug.clone(),
        event_title: row.event_title.clone(),
        venue: row.venue.clone(),
        timezone: row.timezone.clone(),
        starts_at: row.starts_at,
        doors_at: row.doors_at,
        ends_at: row.ends_at,
    }
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
) -> Result<(), TicketingError> {
    let statement_ms = duration_milliseconds(state.operation_timeout)?;
    let lock_ms = duration_milliseconds(state.lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<u128, TicketingError> {
    let milliseconds = value.as_millis();
    if milliseconds == 0 || milliseconds > 2_147_483_647_u128 {
        return Err(TicketingError::Unexpected);
    }
    Ok(milliseconds)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id_value: Option<&str>,
    payload: Value,
) -> Result<(), TicketingError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        ) VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .bind(request_id_value)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn append_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    actor_kind: &str,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    request_id_value: Option<&str>,
    metadata: Value,
) -> Result<(), TicketingError> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id,
            request_id, metadata
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(actor_kind)
    .bind(action)
    .bind(target_type)
    .bind(target_id.to_string())
    .bind(request_id_value)
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_snapshot_separates_sales_holds_and_availability() -> Result<(), TicketingError> {
        assert_eq!(
            inventory_snapshot(100, 12, 3)?,
            InventorySnapshot {
                sold: 12,
                reserved: 3,
                available: 85,
            }
        );
        Ok(())
    }

    #[test]
    fn inventory_snapshot_rejects_negative_or_overcommitted_state() {
        assert!(inventory_snapshot(-1, 0, 0).is_err());
        assert!(inventory_snapshot(100, -1, 0).is_err());
        assert!(inventory_snapshot(100, 0, -1).is_err());
        assert!(inventory_snapshot(10, 8, 3).is_err());
    }

    #[test]
    fn type_inventory_checks_commitment_overflow() {
        assert_eq!(
            TypeInventory {
                reserved: 2,
                sold: 7,
            }
            .committed(),
            Ok(9)
        );
        assert_eq!(
            TypeInventory {
                reserved: i64::MAX,
                sold: 1,
            }
            .committed(),
            Err(TicketingError::Unexpected)
        );
    }

    #[test]
    fn email_masking_never_exposes_the_local_part() {
        assert_eq!(mask_email("wojciech@gmail.com"), "w***@gmail.com");
        assert_eq!(mask_email("a@example.org"), "a***@example.org");
        assert_eq!(mask_email("invalid"), "***");
    }

    #[test]
    fn splits_vat_inclusive_price_with_half_up_rounding() {
        assert_eq!(split_gross(5_000, 800), Ok((4_630, 370)));
        assert_eq!(split_gross(1_000, 800), Ok((926, 74)));
        assert_eq!(split_gross(0, 800), Ok((0, 0)));
    }

    #[test]
    fn checkout_token_is_deterministic_and_context_bound() -> Result<(), TicketingError> {
        let key = [7_u8; 32];
        let order = Uuid::now_v7();
        let first = derive_checkout_token(&key, order, "reservation-1")?;
        assert_eq!(first, derive_checkout_token(&key, order, "reservation-1")?);
        assert_ne!(first, derive_checkout_token(&key, order, "reservation-2")?);
        assert_eq!(first.len(), 64);
        Ok(())
    }

    #[test]
    fn checkout_token_validation_is_strict() {
        assert!(valid_checkout_token(&"a".repeat(64)));
        assert!(valid_checkout_token(&"F".repeat(64)));
        assert!(!valid_checkout_token(&"a".repeat(63)));
        assert!(!valid_checkout_token(&"a".repeat(65)));
        assert!(!valid_checkout_token(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn order_reference_does_not_expose_full_uuid() {
        let order = Uuid::now_v7();
        let reference = order_public_reference(order);
        assert!(reference.starts_with("VRY-ORD-"));
        assert_eq!(reference.len(), 24);
        assert_eq!(reference.matches('-').count(), 2);
    }

    #[test]
    fn stripe_identifiers_are_strictly_bounded() {
        assert!(valid_stripe_id("cs_test_123ABC", "cs_"));
        assert!(!valid_stripe_id("pi_123", "cs_"));
        assert!(!valid_stripe_id("cs_bad/value", "cs_"));
    }
}
