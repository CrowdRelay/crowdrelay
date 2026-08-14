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
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct AccountingSaleLine {
    event_id: Uuid,
    event_title: String,
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339::option")]
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
    #[serde(with = "time::serde::rfc3339")]
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
