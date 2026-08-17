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
