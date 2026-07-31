//! Administrative ticket accounting for Polish monthly sales reporting.
//!
//! The API exposes a preview, an immutable finalized WEW snapshot, a universal
//! semicolon-delimited CSV, and separate invoice-request data. It deliberately
//! does not pretend to submit documents to KSeF or Saldeo; those systems remain
//! the accounting system of record.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Date, Duration, Month, OffsetDateTime};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_DOCUMENT_NUMBER_CHARS: usize = 100;
const MAX_PROFILE_TEXT_CHARS: usize = 240;

#[derive(Clone, Debug, Deserialize)]
pub struct AccountingMonthQuery {
    month: String,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureAccountingProfileRequest {
    seller_name: String,
    tax_id: String,
    regon: Option<String>,
    address_line1: String,
    postal_code: String,
    city: String,
    #[serde(default = "default_country_code")]
    country_code: String,
    #[serde(default = "default_document_prefix")]
    document_prefix: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizeAccountingDocumentRequest {
    month: String,
    #[serde(default = "default_currency")]
    currency: String,
    document_number: String,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingProfileView {
    seller_name: String,
    tax_id: String,
    regon: Option<String>,
    address_line1: String,
    postal_code: String,
    city: String,
    country_code: String,
    document_prefix: String,
    updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingSaleLine {
    event_id: Uuid,
    event_title: String,
    event_starts_at: OffsetDateTime,
    ticket_type_slug: String,
    ticket_type_name: String,
    quantity: i64,
    unit_gross_minor: i64,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
    vat_rate_basis_points: i32,
    currency: String,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingAdjustmentLine {
    event_id: Uuid,
    event_title: String,
    event_starts_at: OffsetDateTime,
    entry_kind: String,
    entry_count: i64,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
    vat_rate_basis_points: i32,
    stripe_fee_minor: i64,
    stripe_net_minor: i64,
    currency: String,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct InvoiceRequestView {
    order_id: Uuid,
    order_reference: String,
    paid_at: OffsetDateTime,
    event_title: String,
    buyer_type: String,
    company_name: Option<String>,
    tax_id: Option<String>,
    full_name: Option<String>,
    address_line1: String,
    postal_code: String,
    city: String,
    country_code: String,
    buyer_email: String,
    currency: String,
    status: String,
    amount_gross_minor: i64,
    amount_refunded_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
    vat_rate_basis_points: i32,
    refunded_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingTotals {
    gross_minor: i64,
    net_minor: i64,
    vat_minor: i64,
    stripe_fee_minor: i64,
    stripe_net_minor: i64,
    sale_entry_count: i64,
    refund_entry_count: i64,
    balance_entry_count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountingPreview {
    period_start: Date,
    period_end: Date,
    currency: String,
    suggested_document_number: String,
    profile: AccountingProfileView,
    sales: Vec<AccountingSaleLine>,
    adjustments: Vec<AccountingAdjustmentLine>,
    totals: AccountingTotals,
    commerce_totals: AccountingTotals,
    invoice_request_count: usize,
    finalized_document: Option<AccountingDocumentSummary>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingDocumentSummary {
    id: Uuid,
    period_start: Date,
    period_end: Date,
    document_number: String,
    currency: String,
    gross_minor: i64,
    net_minor: i64,
    vat_minor: i64,
    stripe_fee_minor: i64,
    stripe_net_minor: i64,
    finalized_at: OffsetDateTime,
}

#[derive(Clone, Debug, FromRow)]
#[allow(dead_code)]
struct AccountingDocumentRow {
    id: Uuid,
    period_start: Date,
    period_end: Date,
    document_number: String,
    currency: String,
    gross_minor: i64,
    net_minor: i64,
    vat_minor: i64,
    stripe_fee_minor: i64,
    stripe_net_minor: i64,
    snapshot: Value,
    finalized_at: OffsetDateTime,
}

pub async fn get_profile(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    match timeout(state.ticketing.operation_timeout(), load_profile(&state)).await {
        Ok(Ok(profile)) => private_json(StatusCode::OK, profile),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn configure_profile(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ConfigureAccountingProfileRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(profile) = normalize_profile(payload) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout(),
        upsert_profile(&state, profile),
    )
    .await
    {
        Ok(Ok(profile)) => private_json(StatusCode::OK, profile),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn preview_ticket_sales(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Query<AccountingMonthQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Some(period) = AccountingPeriod::parse(&query.month, &query.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(3),
        build_preview(&state, period),
    )
    .await
    {
        Ok(Ok(preview)) => private_json(StatusCode::OK, preview),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn finalize_ticket_sales(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FinalizeAccountingDocumentRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(period) = AccountingPeriod::parse(&payload.month, &payload.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(document_number) = clean_text(&payload.document_number, MAX_DOCUMENT_NUMBER_CHARS)
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(4),
        finalize_document(&state, period, document_number),
    )
    .await
    {
        Ok(Ok(document)) => private_json(StatusCode::CREATED, document),
        Ok(Err(AccountingError::Conflict)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn list_invoice_requests(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Query<AccountingMonthQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Some(period) = AccountingPeriod::parse(&query.month, &query.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(2),
        load_invoice_requests(&state, period),
    )
    .await
    {
        Ok(Ok(items)) => private_json(StatusCode::OK, json!({ "items": items })),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn download_accounting_csv(
    State(state): State<crate::AppState>,
    Path(document_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let document_id = match Uuid::parse_str(&document_id) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let document = match timeout(
        state.ticketing.operation_timeout(),
        load_document(&state, document_id),
    )
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(AccountingError::NotFound)) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Ok(Err(_)) | Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let csv = snapshot_csv(&document.snapshot);
    let filename = format!(
        "{}-{}.csv",
        sanitize_filename(&document.document_number),
        document.currency
    );
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"ticket-sales.csv\""));
    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (CONTENT_DISPOSITION, disposition),
        ],
        csv,
    )
        .into_response()
}

fn private_json(status: StatusCode, payload: impl Serialize) -> Response {
    (
        status,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(payload),
    )
        .into_response()
}

#[derive(Clone, Copy)]
struct AccountingPeriod {
    start: Date,
    end: Date,
    next_start: Date,
    currency: [u8; 3],
}

impl AccountingPeriod {
    fn parse(month: &str, currency: &str) -> Option<Self> {
        let (year, month) = month.trim().split_once('-')?;
        let year = year.parse::<i32>().ok()?;
        let month_number = month.parse::<u8>().ok()?;
        let month = Month::try_from(month_number).ok()?;
        let start = Date::from_calendar_date(year, month, 1).ok()?;
        let (next_year, next_month) = if month == Month::December {
            (year.checked_add(1)?, Month::January)
        } else {
            (year, Month::try_from(month_number.checked_add(1)?).ok()?)
        };
        let next_start = Date::from_calendar_date(next_year, next_month, 1).ok()?;
        let end = next_start - Duration::days(1);
        let currency = currency.trim().to_ascii_uppercase();
        let bytes: [u8; 3] = currency.as_bytes().try_into().ok()?;
        if !bytes.iter().all(|byte| byte.is_ascii_uppercase()) {
            return None;
        }
        Some(Self {
            start,
            end,
            next_start,
            currency: bytes,
        })
    }

    fn currency(self) -> String {
        self.currency.iter().map(|byte| char::from(*byte)).collect()
    }
}

fn default_currency() -> String {
    "PLN".to_owned()
}
fn default_country_code() -> String {
    "PL".to_owned()
}
fn default_document_prefix() -> String {
    "WEW/BILETY".to_owned()
}

fn normalize_profile(
    request: ConfigureAccountingProfileRequest,
) -> Option<ConfigureAccountingProfileRequest> {
    let regon = match request.regon {
        Some(value) => Some(clean_text(&value, 32)?),
        None => None,
    };
    Some(ConfigureAccountingProfileRequest {
        seller_name: clean_text(&request.seller_name, 200)?,
        tax_id: clean_text(&request.tax_id, 32)?,
        regon,
        address_line1: clean_text(&request.address_line1, MAX_PROFILE_TEXT_CHARS)?,
        postal_code: clean_text(&request.postal_code, 32)?,
        city: clean_text(&request.city, 120)?,
        country_code: normalize_country_code(&request.country_code)?,
        document_prefix: clean_text(&request.document_prefix, 64)?,
    })
}

fn clean_text(value: &str, maximum_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > maximum_chars
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_owned())
}

fn normalize_country_code(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_uppercase();
    (value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_uppercase())).then_some(value)
}

async fn load_profile(state: &crate::AppState) -> Result<AccountingProfileView, AccountingError> {
    sqlx::query_as::<_, AccountingProfileView>(
        r#"
        SELECT seller_name, tax_id, regon, address_line1, postal_code, city,
               country_code::text AS country_code, document_prefix, updated_at
        FROM ticket_accounting_profiles
        WHERE workspace_id = $1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)?
    .ok_or(AccountingError::NotFound)
}

async fn upsert_profile(
    state: &crate::AppState,
    profile: ConfigureAccountingProfileRequest,
) -> Result<AccountingProfileView, AccountingError> {
    sqlx::query_as::<_, AccountingProfileView>(
        r#"
        INSERT INTO ticket_accounting_profiles (
            workspace_id, seller_name, tax_id, regon, address_line1,
            postal_code, city, country_code, document_prefix
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (workspace_id) DO UPDATE
        SET seller_name = EXCLUDED.seller_name,
            tax_id = EXCLUDED.tax_id,
            regon = EXCLUDED.regon,
            address_line1 = EXCLUDED.address_line1,
            postal_code = EXCLUDED.postal_code,
            city = EXCLUDED.city,
            country_code = EXCLUDED.country_code,
            document_prefix = EXCLUDED.document_prefix
        RETURNING seller_name, tax_id, regon, address_line1, postal_code, city,
                  country_code::text AS country_code, document_prefix, updated_at
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(profile.seller_name)
    .bind(profile.tax_id)
    .bind(profile.regon)
    .bind(profile.address_line1)
    .bind(profile.postal_code)
    .bind(profile.city)
    .bind(profile.country_code)
    .bind(profile.document_prefix)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn build_preview(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<AccountingPreview, AccountingError> {
    let profile = load_profile(state).await?;
    let currency = period.currency();
    let sales = load_sales(state, period).await?;
    let adjustments = load_adjustments(state, period).await?;
    let totals = load_totals(state, period, true).await?;
    let commerce_totals = load_totals(state, period, false).await?;
    let invoice_request_count = invoice_request_count(state, period).await?;
    let finalized_document = load_document_for_period(state, period).await?;
    let suggested_document_number = format!(
        "{}/{:02}/{:04}",
        profile.document_prefix,
        u8::from(period.start.month()),
        period.start.year()
    );
    Ok(AccountingPreview {
        period_start: period.start,
        period_end: period.end,
        currency,
        suggested_document_number,
        profile,
        sales,
        adjustments,
        totals,
        commerce_totals,
        invoice_request_count,
        finalized_document,
    })
}

async fn load_sales(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Vec<AccountingSaleLine>, AccountingError> {
    sqlx::query_as::<_, AccountingSaleLine>(
        r#"
        SELECT sale.event_id, event.title AS event_title, event.starts_at AS event_starts_at,
               ticket_type.slug AS ticket_type_slug, ticket_type.name AS ticket_type_name,
               sum(item.quantity)::bigint AS quantity,
               item.unit_gross_minor,
               sum(item.total_gross_minor)::bigint AS amount_gross_minor,
               sum(item.total_net_minor)::bigint AS amount_net_minor,
               sum(item.total_vat_minor)::bigint AS amount_vat_minor,
               orders.vat_rate_basis_points,
               orders.currency::text AS currency
        FROM ticket_orders AS orders
        JOIN ticket_sales AS sale ON sale.workspace_id = orders.workspace_id AND sale.id = orders.ticket_sale_id
        JOIN events AS event ON event.workspace_id = sale.workspace_id AND event.id = sale.event_id
        JOIN ticket_order_items AS item ON item.workspace_id = orders.workspace_id AND item.ticket_order_id = orders.id
        JOIN ticket_types AS ticket_type ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id
        WHERE orders.workspace_id = $1
          AND orders.paid_at >= $2::date
          AND orders.paid_at < $3::date
          AND orders.currency = $4
          AND NOT orders.invoice_requested
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
        GROUP BY sale.event_id, event.title, event.starts_at, ticket_type.slug,
                 ticket_type.name, item.unit_gross_minor, orders.vat_rate_basis_points,
                 orders.currency
        ORDER BY event.starts_at, event.title, ticket_type.name, item.unit_gross_minor
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn load_adjustments(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Vec<AccountingAdjustmentLine>, AccountingError> {
    sqlx::query_as::<_, AccountingAdjustmentLine>(
        r#"
        SELECT entry.event_id, event.title AS event_title, event.starts_at AS event_starts_at,
               entry.entry_kind, count(*)::bigint AS entry_count,
               sum(entry.amount_gross_minor)::bigint AS amount_gross_minor,
               sum(entry.amount_net_minor)::bigint AS amount_net_minor,
               sum(entry.amount_vat_minor)::bigint AS amount_vat_minor,
               entry.vat_rate_basis_points,
               COALESCE(sum(entry.stripe_fee_minor), 0)::bigint AS stripe_fee_minor,
               COALESCE(sum(entry.stripe_net_minor), 0)::bigint AS stripe_net_minor,
               entry.currency::text AS currency
        FROM ticket_accounting_entries AS entry
        JOIN ticket_orders AS orders
          ON orders.workspace_id = entry.workspace_id
         AND orders.id = entry.ticket_order_id
        JOIN events AS event ON event.workspace_id = entry.workspace_id AND event.id = entry.event_id
        WHERE entry.workspace_id = $1
          AND entry.occurred_at >= $2::date
          AND entry.occurred_at < $3::date
          AND entry.currency = $4
          AND NOT orders.invoice_requested
          AND entry.entry_kind = 'refund'
        GROUP BY entry.event_id, event.title, event.starts_at, entry.entry_kind, entry.vat_rate_basis_points, entry.currency
        ORDER BY event.starts_at, event.title, entry.entry_kind
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn load_totals(
    state: &crate::AppState,
    period: AccountingPeriod,
    wew_only: bool,
) -> Result<AccountingTotals, AccountingError> {
    sqlx::query_as::<_, AccountingTotals>(
        r#"
        SELECT COALESCE(sum(entry.amount_gross_minor), 0)::bigint AS gross_minor,
               COALESCE(sum(entry.amount_net_minor), 0)::bigint AS net_minor,
               COALESCE(sum(entry.amount_vat_minor), 0)::bigint AS vat_minor,
               COALESCE(sum(entry.stripe_fee_minor), 0)::bigint AS stripe_fee_minor,
               COALESCE(sum(entry.stripe_net_minor), 0)::bigint AS stripe_net_minor,
               count(*) FILTER (WHERE entry.entry_kind = 'sale')::bigint AS sale_entry_count,
               count(*) FILTER (WHERE entry.entry_kind = 'refund')::bigint AS refund_entry_count,
               count(*) FILTER (WHERE entry.stripe_balance_transaction_id IS NOT NULL)::bigint AS balance_entry_count
        FROM ticket_accounting_entries AS entry
        JOIN ticket_orders AS orders
          ON orders.workspace_id = entry.workspace_id
         AND orders.id = entry.ticket_order_id
        WHERE entry.workspace_id = $1
          AND entry.occurred_at >= $2::date
          AND entry.occurred_at < $3::date
          AND entry.currency = $4
          AND (NOT $5 OR NOT orders.invoice_requested)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .bind(wew_only)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn invoice_request_count(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<usize, AccountingError> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM ticket_orders
        WHERE workspace_id = $1 AND invoice_requested
          AND paid_at >= $2::date AND paid_at < $3::date AND currency = $4
          AND status IN ('paid', 'partially_refunded', 'refunded')
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)?;
    usize::try_from(count).map_err(|_| AccountingError::Unavailable)
}

async fn load_invoice_requests(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Vec<InvoiceRequestView>, AccountingError> {
    sqlx::query_as::<_, InvoiceRequestView>(
        r#"
        SELECT orders.id AS order_id, orders.public_reference AS order_reference,
               orders.paid_at, event.title AS event_title,
               orders.invoice_buyer_type AS buyer_type,
               orders.invoice_company_name AS company_name,
               orders.invoice_tax_id AS tax_id,
               orders.invoice_full_name AS full_name,
               orders.invoice_address_line1 AS address_line1,
               orders.invoice_postal_code AS postal_code,
               orders.invoice_city AS city,
               orders.invoice_country_code::text AS country_code,
               orders.buyer_email,
               orders.currency::text AS currency, orders.status,
               orders.amount_gross_minor, orders.amount_refunded_minor,
               orders.amount_net_minor, orders.amount_vat_minor,
               orders.vat_rate_basis_points, orders.refunded_at
        FROM ticket_orders AS orders
        JOIN ticket_sales AS sale ON sale.workspace_id = orders.workspace_id AND sale.id = orders.ticket_sale_id
        JOIN events AS event ON event.workspace_id = sale.workspace_id AND event.id = sale.event_id
        WHERE orders.workspace_id = $1 AND orders.invoice_requested
          AND orders.paid_at >= $2::date AND orders.paid_at < $3::date
          AND orders.currency = $4
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
        ORDER BY orders.paid_at, orders.id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn load_document_for_period(
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Option<AccountingDocumentSummary>, AccountingError> {
    sqlx::query_as::<_, AccountingDocumentSummary>(
        r#"
        SELECT id, period_start, period_end, document_number,
               currency::text AS currency, gross_minor, net_minor, vat_minor,
               stripe_fee_minor, stripe_net_minor, finalized_at
        FROM ticket_accounting_documents
        WHERE workspace_id = $1 AND period_start = $2 AND period_end = $3 AND currency = $4
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.end)
    .bind(period.currency())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn finalize_document(
    state: &crate::AppState,
    period: AccountingPeriod,
    document_number: String,
) -> Result<AccountingDocumentSummary, AccountingError> {
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(|_| AccountingError::Unavailable)?;
    configure_transaction(&mut transaction, state).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "ticket-accounting:{}:{}:{}",
            state.ticketing.workspace_id(),
            period.start,
            period.currency()
        ))
        .execute(&mut *transaction)
        .await
        .map_err(|_| AccountingError::Unavailable)?;
    if load_document_for_period_tx(&mut transaction, state, period)
        .await?
        .is_some()
    {
        return Err(AccountingError::Conflict);
    }
    let preview = build_preview_tx(&mut transaction, state, period).await?;
    if preview.totals.sale_entry_count == 0 && preview.totals.refund_entry_count == 0 {
        return Err(AccountingError::NotFound);
    }
    let snapshot = serde_json::to_value(&preview).map_err(|_| AccountingError::Unavailable)?;
    let document = sqlx::query_as::<_, AccountingDocumentSummary>(
        r#"
        INSERT INTO ticket_accounting_documents (
            workspace_id, period_start, period_end, document_number, currency,
            gross_minor, net_minor, vat_minor, stripe_fee_minor, stripe_net_minor,
            snapshot
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        RETURNING id, period_start, period_end, document_number,
                  currency::text AS currency, gross_minor, net_minor, vat_minor,
                  stripe_fee_minor, stripe_net_minor, finalized_at
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.end)
    .bind(&document_number)
    .bind(period.currency())
    .bind(preview.totals.gross_minor)
    .bind(preview.totals.net_minor)
    .bind(preview.totals.vat_minor)
    .bind(preview.totals.stripe_fee_minor)
    .bind(preview.totals.stripe_net_minor)
    .bind(snapshot)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| {
        if is_unique_violation(&error) {
            AccountingError::Conflict
        } else {
            AccountingError::Unavailable
        }
    })?;
    transaction
        .commit()
        .await
        .map_err(|_| AccountingError::Unavailable)?;
    Ok(document)
}

async fn build_preview_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<AccountingPreview, AccountingError> {
    let profile = sqlx::query_as::<_, AccountingProfileView>(
        "SELECT seller_name, tax_id, regon, address_line1, postal_code, city, country_code::text AS country_code, document_prefix, updated_at FROM ticket_accounting_profiles WHERE workspace_id = $1",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| AccountingError::Unavailable)?
    .ok_or(AccountingError::NotFound)?;
    let sales = load_sales_tx(transaction, state, period).await?;
    let adjustments = load_adjustments_tx(transaction, state, period).await?;
    let totals = load_totals_tx(transaction, state, period, true).await?;
    let commerce_totals = load_totals_tx(transaction, state, period, false).await?;
    let invoice_request_count = invoice_request_count_tx(transaction, state, period).await?;
    let suggested_document_number = format!(
        "{}/{:02}/{:04}",
        profile.document_prefix,
        u8::from(period.start.month()),
        period.start.year()
    );
    Ok(AccountingPreview {
        period_start: period.start,
        period_end: period.end,
        currency: period.currency(),
        suggested_document_number,
        profile,
        sales,
        adjustments,
        totals,
        commerce_totals,
        invoice_request_count,
        finalized_document: None,
    })
}

async fn load_sales_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Vec<AccountingSaleLine>, AccountingError> {
    sqlx::query_as::<_, AccountingSaleLine>(
        "SELECT sale.event_id, event.title AS event_title, event.starts_at AS event_starts_at, ticket_type.slug AS ticket_type_slug, ticket_type.name AS ticket_type_name, sum(item.quantity)::bigint AS quantity, item.unit_gross_minor, sum(item.total_gross_minor)::bigint AS amount_gross_minor, sum(item.total_net_minor)::bigint AS amount_net_minor, sum(item.total_vat_minor)::bigint AS amount_vat_minor, orders.vat_rate_basis_points, orders.currency::text AS currency FROM ticket_orders AS orders JOIN ticket_sales AS sale ON sale.workspace_id = orders.workspace_id AND sale.id = orders.ticket_sale_id JOIN events AS event ON event.workspace_id = sale.workspace_id AND event.id = sale.event_id JOIN ticket_order_items AS item ON item.workspace_id = orders.workspace_id AND item.ticket_order_id = orders.id JOIN ticket_types AS ticket_type ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id WHERE orders.workspace_id = $1 AND orders.paid_at >= $2::date AND orders.paid_at < $3::date AND orders.currency = $4 AND NOT orders.invoice_requested AND orders.status IN ('paid','partially_refunded','refunded') GROUP BY sale.event_id,event.title,event.starts_at,ticket_type.slug,ticket_type.name,item.unit_gross_minor,orders.vat_rate_basis_points,orders.currency ORDER BY event.starts_at,event.title,ticket_type.name,item.unit_gross_minor",
    ).bind(state.ticketing.workspace_id().into_uuid()).bind(period.start).bind(period.next_start).bind(period.currency()).fetch_all(&mut **transaction).await.map_err(|_| AccountingError::Unavailable)
}
async fn load_adjustments_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Vec<AccountingAdjustmentLine>, AccountingError> {
    sqlx::query_as::<_, AccountingAdjustmentLine>(
        r#"
        SELECT entry.event_id, event.title AS event_title, event.starts_at AS event_starts_at,
               entry.entry_kind, count(*)::bigint AS entry_count,
               sum(entry.amount_gross_minor)::bigint AS amount_gross_minor,
               sum(entry.amount_net_minor)::bigint AS amount_net_minor,
               sum(entry.amount_vat_minor)::bigint AS amount_vat_minor,
               entry.vat_rate_basis_points,
               COALESCE(sum(entry.stripe_fee_minor), 0)::bigint AS stripe_fee_minor,
               COALESCE(sum(entry.stripe_net_minor), 0)::bigint AS stripe_net_minor,
               entry.currency::text AS currency
        FROM ticket_accounting_entries AS entry
        JOIN ticket_orders AS orders
          ON orders.workspace_id = entry.workspace_id
         AND orders.id = entry.ticket_order_id
        JOIN events AS event
          ON event.workspace_id = entry.workspace_id
         AND event.id = entry.event_id
        WHERE entry.workspace_id = $1
          AND entry.occurred_at >= $2::date
          AND entry.occurred_at < $3::date
          AND entry.currency = $4
          AND NOT orders.invoice_requested
          AND entry.entry_kind = 'refund'
        GROUP BY entry.event_id, event.title, event.starts_at, entry.entry_kind,
                 entry.vat_rate_basis_points, entry.currency
        ORDER BY event.starts_at, event.title, entry.entry_kind
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn load_totals_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
    wew_only: bool,
) -> Result<AccountingTotals, AccountingError> {
    sqlx::query_as::<_, AccountingTotals>(
        r#"
        SELECT COALESCE(sum(entry.amount_gross_minor), 0)::bigint AS gross_minor,
               COALESCE(sum(entry.amount_net_minor), 0)::bigint AS net_minor,
               COALESCE(sum(entry.amount_vat_minor), 0)::bigint AS vat_minor,
               COALESCE(sum(entry.stripe_fee_minor), 0)::bigint AS stripe_fee_minor,
               COALESCE(sum(entry.stripe_net_minor), 0)::bigint AS stripe_net_minor,
               count(*) FILTER (WHERE entry.entry_kind = 'sale')::bigint AS sale_entry_count,
               count(*) FILTER (WHERE entry.entry_kind = 'refund')::bigint AS refund_entry_count,
               count(*) FILTER (WHERE entry.stripe_balance_transaction_id IS NOT NULL)::bigint AS balance_entry_count
        FROM ticket_accounting_entries AS entry
        JOIN ticket_orders AS orders
          ON orders.workspace_id = entry.workspace_id
         AND orders.id = entry.ticket_order_id
        WHERE entry.workspace_id = $1
          AND entry.occurred_at >= $2::date
          AND entry.occurred_at < $3::date
          AND entry.currency = $4
          AND (NOT $5 OR NOT orders.invoice_requested)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(period.start)
    .bind(period.next_start)
    .bind(period.currency())
    .bind(wew_only)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| AccountingError::Unavailable)
}

async fn invoice_request_count_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<usize, AccountingError> {
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM ticket_orders WHERE workspace_id=$1 AND invoice_requested AND paid_at >= $2::date AND paid_at < $3::date AND currency=$4 AND status IN ('paid','partially_refunded','refunded')").bind(state.ticketing.workspace_id().into_uuid()).bind(period.start).bind(period.next_start).bind(period.currency()).fetch_one(&mut **transaction).await.map_err(|_| AccountingError::Unavailable)?;
    usize::try_from(count).map_err(|_| AccountingError::Unavailable)
}
async fn load_document_for_period_tx(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    period: AccountingPeriod,
) -> Result<Option<Uuid>, AccountingError> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM ticket_accounting_documents WHERE workspace_id=$1 AND period_start=$2 AND period_end=$3 AND currency=$4 FOR UPDATE").bind(state.ticketing.workspace_id().into_uuid()).bind(period.start).bind(period.end).bind(period.currency()).fetch_optional(&mut **transaction).await.map_err(|_| AccountingError::Unavailable)
}

async fn load_document(
    state: &crate::AppState,
    document_id: Uuid,
) -> Result<AccountingDocumentRow, AccountingError> {
    sqlx::query_as::<_, AccountingDocumentRow>(
        "SELECT id,period_start,period_end,document_number,currency::text AS currency,gross_minor,net_minor,vat_minor,stripe_fee_minor,stripe_net_minor,snapshot,finalized_at FROM ticket_accounting_documents WHERE workspace_id=$1 AND id=$2",
    ).bind(state.ticketing.workspace_id().into_uuid()).bind(document_id).fetch_optional(state.ticketing.pool()).await.map_err(|_| AccountingError::Unavailable)?.ok_or(AccountingError::NotFound)
}

fn snapshot_csv(snapshot: &Value) -> String {
    let mut output = String::from(
        "\u{feff}Rodzaj;Data wydarzenia;Wydarzenie;Typ biletu;Ilość;Cena jednostkowa brutto;Netto;VAT;Brutto;Stawka VAT;Waluta\r\n",
    );
    if let Some(sales) = snapshot.get("sales").and_then(Value::as_array) {
        for line in sales {
            csv_row(&mut output, "Sprzedaż", line, true);
        }
    }
    if let Some(adjustments) = snapshot.get("adjustments").and_then(Value::as_array) {
        for line in adjustments {
            csv_row(&mut output, "Zwrot", line, false);
        }
    }
    output
}

fn csv_row(output: &mut String, kind: &str, line: &Value, sale: bool) {
    let date = line
        .get("event_starts_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let event = line
        .get("event_title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ticket_type = if sale {
        line.get("ticket_type_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        "Korekta / zwrot"
    };
    let quantity = if sale {
        line.get("quantity")
            .and_then(Value::as_i64)
            .map(|v| v.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let unit = if sale {
        line.get("unit_gross_minor")
            .and_then(Value::as_i64)
            .map(format_minor)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let net = line
        .get("amount_net_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let vat = line
        .get("amount_vat_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let gross = line
        .get("amount_gross_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let rate = line
        .get("vat_rate_basis_points")
        .and_then(Value::as_i64)
        .map(|v| format!("{:.2}%", v as f64 / 100.0))
        .unwrap_or_default();
    let currency = line
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let values = [
        kind.to_owned(),
        date.to_owned(),
        event.to_owned(),
        ticket_type.to_owned(),
        quantity,
        unit,
        net,
        vat,
        gross,
        rate,
        currency.to_owned(),
    ];
    output.push_str(
        &values
            .iter()
            .map(|value| csv_escape(value))
            .collect::<Vec<_>>()
            .join(";"),
    );
    output.push_str("\r\n");
}

fn format_minor(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let absolute = value.unsigned_abs();
    format!("{sign}{},{:02}", absolute / 100, absolute % 100)
}
fn csv_escape(value: &str) -> String {
    if value
        .chars()
        .any(|character| matches!(character, ';' | '"' | '\r' | '\n'))
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
fn sanitize_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            out.push(character);
        } else {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_owned()
}
fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|value| value.code())
        .is_some_and(|code| code.as_ref() == "23505")
}
async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), AccountingError> {
    sqlx::query(
        "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$2,true)",
    )
    .bind(format!(
        "{}ms",
        state.ticketing.operation_timeout().as_millis()
    ))
    .bind(format!("{}ms", state.ticketing.lock_timeout().as_millis()))
    .execute(&mut **transaction)
    .await
    .map_err(|_| AccountingError::Unavailable)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum AccountingError {
    NotFound,
    Conflict,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_period_parses_calendar_boundaries() {
        let december = AccountingPeriod::parse("2026-12", "pln").expect("valid period");
        assert_eq!(december.start.to_string(), "2026-12-01");
        assert_eq!(december.end.to_string(), "2026-12-31");
        assert_eq!(december.next_start.to_string(), "2027-01-01");
        assert_eq!(december.currency(), "PLN");

        assert!(AccountingPeriod::parse("2026-13", "PLN").is_none());
        assert!(AccountingPeriod::parse("2026-07", "PL12").is_none());
    }

    #[test]
    fn csv_values_are_safe_for_semicolon_imports() {
        assert_eq!(csv_escape("Virya; Wrocław"), "\"Virya; Wrocław\"");
        assert_eq!(
            csv_escape("A \"quoted\" title"),
            "\"A \"\"quoted\"\" title\""
        );
        assert_eq!(format_minor(-12_345), "-123,45");
    }

    #[test]
    fn accounting_filename_is_portable() {
        assert_eq!(
            sanitize_filename("WEW/BILETY/07/2026"),
            "WEW-BILETY-07-2026"
        );
    }
}
