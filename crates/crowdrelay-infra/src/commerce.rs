use serde::Serialize;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ConfirmedMerchOrderInput {
    pub workspace_id: Uuid,
    pub stripe_session_id: String,
    pub inventory_reservation_id: Uuid,
    pub buyer_email: Option<String>,
    pub event_id: Option<Uuid>,
    pub fulfillment_mode: String,
    pub currency: String,
    pub amount_gross_minor: i64,
    pub goods_gross_minor: i64,
    pub shipping_gross_minor: i64,
    pub confirmed_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordMerchOrderError {
    ReservationNotCommitted,
    Conflict,
    Database,
}

#[derive(Clone, Debug, FromRow)]
struct ExistingOrderFact {
    stripe_session_id: String,
    inventory_reservation_id: Uuid,
    fan_id: Option<Uuid>,
    event_id: Option<Uuid>,
    fulfillment_mode: String,
    currency: String,
    amount_gross_minor: i64,
    goods_gross_minor: i64,
    shipping_gross_minor: i64,
}

fn same_fact(existing: &ExistingOrderFact, input: &ConfirmedMerchOrderInput) -> bool {
    existing.stripe_session_id == input.stripe_session_id
        && existing.inventory_reservation_id == input.inventory_reservation_id
        && existing.event_id == input.event_id
        && existing.fulfillment_mode == input.fulfillment_mode
        && existing.currency == input.currency
        && existing.amount_gross_minor == input.amount_gross_minor
        && existing.goods_gross_minor == input.goods_gross_minor
        && existing.shipping_gross_minor == input.shipping_gross_minor
}

pub async fn record_confirmed_merch_order(
    pool: &PgPool,
    input: &ConfirmedMerchOrderInput,
) -> Result<(), RecordMerchOrderError> {
    let mut tx = pool.begin().await.map_err(|error| {
        tracing::error!(%error, "begin merch order fact transaction failed");
        RecordMerchOrderError::Database
    })?;

    let reservation_committed = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM inventory_reservations
            WHERE workspace_id = $1 AND id = $2 AND status = 'committed'
        )
        "#,
    )
    .bind(input.workspace_id)
    .bind(input.inventory_reservation_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(%error, "check merch order reservation failed");
        RecordMerchOrderError::Database
    })?;
    if !reservation_committed {
        return Err(RecordMerchOrderError::ReservationNotCommitted);
    }

    let fan_id = if let Some(email) = input.buyer_email.as_deref() {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM fans WHERE workspace_id = $1 AND normalized_email = lower(btrim($2)) LIMIT 1",
        )
        .bind(input.workspace_id)
        .bind(email)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            tracing::error!(%error, "resolve merch order fan failed");
            RecordMerchOrderError::Database
        })?
    } else {
        None
    };

    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO merch_order_facts (
            workspace_id, stripe_session_id, inventory_reservation_id, fan_id, event_id,
            fulfillment_mode, currency, amount_gross_minor, goods_gross_minor,
            shipping_gross_minor, confirmed_at
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(input.workspace_id)
    .bind(&input.stripe_session_id)
    .bind(input.inventory_reservation_id)
    .bind(fan_id)
    .bind(input.event_id)
    .bind(&input.fulfillment_mode)
    .bind(&input.currency)
    .bind(input.amount_gross_minor)
    .bind(input.goods_gross_minor)
    .bind(input.shipping_gross_minor)
    .bind(input.confirmed_at)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(%error, "insert merch order fact failed");
        RecordMerchOrderError::Database
    })?;

    if inserted.is_none() {
        let existing = sqlx::query_as::<_, ExistingOrderFact>(
            r#"
            SELECT stripe_session_id, inventory_reservation_id, fan_id, event_id,
                   fulfillment_mode, currency, amount_gross_minor,
                   goods_gross_minor, shipping_gross_minor
            FROM merch_order_facts
            WHERE workspace_id = $1
              AND (stripe_session_id = $2 OR inventory_reservation_id = $3)
            LIMIT 1
            "#,
        )
        .bind(input.workspace_id)
        .bind(&input.stripe_session_id)
        .bind(input.inventory_reservation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            tracing::error!(%error, "read existing merch order fact failed");
            RecordMerchOrderError::Database
        })?;
        let Some(existing) = existing else {
            return Err(RecordMerchOrderError::Conflict);
        };
        if !same_fact(&existing, input) {
            return Err(RecordMerchOrderError::Conflict);
        }
        // fan_id is enrichment, not part of the immutable commerce identity.
        // A Stripe retry may happen after the buyer has subsequently become a fan.
        // Preserve idempotency and opportunistically attach that now-known fan.
        if existing.fan_id.is_none()
            && let Some(resolved_fan_id) = fan_id
        {
            sqlx::query(
                r#"
                    UPDATE merch_order_facts
                    SET fan_id = $4
                    WHERE workspace_id = $1
                      AND stripe_session_id = $2
                      AND inventory_reservation_id = $3
                      AND fan_id IS NULL
                    "#,
            )
            .bind(input.workspace_id)
            .bind(&input.stripe_session_id)
            .bind(input.inventory_reservation_id)
            .bind(resolved_fan_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                tracing::error!(%error, "enrich merch order fan failed");
                RecordMerchOrderError::Database
            })?;
        }
    }

    tx.commit().await.map_err(|error| {
        tracing::error!(%error, "commit merch order fact transaction failed");
        RecordMerchOrderError::Database
    })
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct EventMerchPickupItem {
    pub product_name: String,
    pub variant_label: String,
    pub sku: String,
    pub quantity: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventMerchSummary {
    pub event_id: Uuid,
    pub order_count: i64,
    pub pickup_order_count: i64,
    pub pickup_unit_count: i64,
    pub gross_minor: i64,
    pub goods_minor: i64,
    pub shipping_minor: i64,
    pub currency: String,
    pub pickup_items: Vec<EventMerchPickupItem>,
}

pub async fn event_merch_summary(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
) -> Result<EventMerchSummary, sqlx::Error> {
    #[derive(FromRow)]
    struct Totals {
        order_count: i64,
        pickup_order_count: i64,
        gross_minor: i64,
        goods_minor: i64,
        shipping_minor: i64,
        currency: Option<String>,
    }
    let totals = sqlx::query_as::<_, Totals>(
        r#"
        SELECT
          count(*)::bigint AS order_count,
          count(*) FILTER (WHERE fulfillment_mode = 'event_pickup')::bigint AS pickup_order_count,
          COALESCE(sum(amount_gross_minor), 0)::bigint AS gross_minor,
          COALESCE(sum(goods_gross_minor), 0)::bigint AS goods_minor,
          COALESCE(sum(shipping_gross_minor), 0)::bigint AS shipping_minor,
          min(currency)::text AS currency
        FROM merch_order_facts
        WHERE workspace_id = $1 AND event_id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(pool)
    .await?;

    let pickup_items = sqlx::query_as::<_, EventMerchPickupItem>(
        r#"
        SELECT product.name AS product_name,
               variant.label AS variant_label,
               variant.sku,
               sum(item.quantity)::bigint AS quantity
        FROM merch_order_facts fact
        JOIN inventory_reservation_items item
          ON item.workspace_id = fact.workspace_id
         AND item.reservation_id = fact.inventory_reservation_id
        JOIN merch_variants variant
          ON variant.workspace_id = item.workspace_id AND variant.id = item.variant_id
        JOIN merch_products product
          ON product.workspace_id = variant.workspace_id AND product.id = variant.product_id
        WHERE fact.workspace_id = $1 AND fact.event_id = $2
          AND fact.fulfillment_mode = 'event_pickup'
        GROUP BY product.name, variant.label, variant.sku
        ORDER BY product.name, variant.label, variant.sku
        LIMIT 500
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(pool)
    .await?;
    let pickup_unit_count = pickup_items.iter().map(|item| item.quantity).sum();

    Ok(EventMerchSummary {
        event_id,
        order_count: totals.order_count,
        pickup_order_count: totals.pickup_order_count,
        pickup_unit_count,
        gross_minor: totals.gross_minor,
        goods_minor: totals.goods_minor,
        shipping_minor: totals.shipping_minor,
        currency: totals.currency.unwrap_or_else(|| "PLN".to_owned()),
        pickup_items,
    })
}
