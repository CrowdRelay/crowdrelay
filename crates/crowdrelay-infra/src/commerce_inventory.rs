//! PostgreSQL adapter for the `CommerceInventoryRepository` port.
//!
//! Moves all SQL write operations (stocktake, inventory activation) out of the
//! API layer. The API layer retains pure validation (normalize, hash, text
//! bounds) and response formatting; this adapter owns every INSERT/UPDATE
//! against `inventory_activation_state`, `inventory_stocktakes`,
//! `inventory_stocktake_items`, `inventory_ledger`, and
//! `ecosystem_feature_flags`.

use async_trait::async_trait;
use crowdrelay_application::{
    CommerceInventoryError, CommerceInventoryRepository, InventoryActivationState,
    MarkInventoryReadyCommand, MarkInventoryReadyResult, StocktakeCommand, StocktakeItemResult,
    StocktakeResult,
};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

/// Tenant-scoped PostgreSQL commerce inventory repository.
#[derive(Clone)]
pub struct PostgresCommerceInventoryRepository {
    pool: PgPool,
}

impl PostgresCommerceInventoryRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CommerceInventoryRepository for PostgresCommerceInventoryRepository {
    async fn stocktake(
        &self,
        command: &StocktakeCommand,
    ) -> Result<StocktakeResult, CommerceInventoryError> {
        let workspace_id = command.workspace_id;
        let mut transaction = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "stocktake begin transaction failed");
            CommerceInventoryError::Unavailable
        })?;

        sqlx::query(
            "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
        )
        .bind(workspace_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "stocktake ensure activation row failed");
            CommerceInventoryError::Unavailable
        })?;

        if let Some(existing) = sqlx::query_as::<_, ExistingStocktake>(
            r#"
            SELECT id, request_hash, created_at
            FROM inventory_stocktakes
            WHERE workspace_id = $1 AND idempotency_key = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "stocktake lookup existing failed");
            CommerceInventoryError::Unavailable
        })? {
            if existing.request_hash != command.request_hash {
                return Err(CommerceInventoryError::Conflict);
            }
            let items =
                load_stocktake_items_tx(&mut transaction, workspace_id, existing.id).await?;
            transaction.commit().await.map_err(|error| {
                tracing::warn!(%error, "stocktake replay commit failed");
                CommerceInventoryError::Unavailable
            })?;
            return Ok(StocktakeResult {
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
        .bind(&command.idempotency_key)
        .bind(&command.request_hash)
        .bind(command.actor_id.as_deref())
        .bind(command.reason.as_deref())
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "stocktake insert header failed");
            CommerceInventoryError::Unavailable
        })?;

        for item in &command.items {
            let availability =
                lock_variant_availability_tx(&mut transaction, workspace_id, &item.sku).await?;
            if !availability.sell_without_stock && i64::from(item.on_hand) < availability.reserved {
                return Err(CommerceInventoryError::Conflict);
            }
            let delta_i64 = i64::from(item.on_hand).saturating_sub(availability.on_hand);
            let delta = i32::try_from(delta_i64).map_err(|_| CommerceInventoryError::Invalid)?;
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
                .bind(command.actor_id.as_deref())
                .bind(
                    command
                        .reason
                        .as_deref()
                        .unwrap_or("exact physical stocktake"),
                )
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "stocktake ledger insert failed");
                    CommerceInventoryError::Unavailable
                })?;
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
            .map_err(|error| {
                tracing::warn!(%error, "stocktake item insert failed");
                CommerceInventoryError::Unavailable
            })?;
        }

        let items = load_stocktake_items_tx(&mut transaction, workspace_id, stocktake_id).await?;
        transaction.commit().await.map_err(|error| {
            tracing::warn!(%error, "stocktake commit failed");
            CommerceInventoryError::Unavailable
        })?;
        Ok(StocktakeResult {
            id: stocktake_id,
            replayed: false,
            created_at,
            items,
        })
    }

    async fn mark_inventory_ready(
        &self,
        command: &MarkInventoryReadyCommand,
    ) -> Result<MarkInventoryReadyResult, CommerceInventoryError> {
        let workspace_id = command.workspace_id;
        let mut transaction = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready begin transaction failed");
            CommerceInventoryError::Unavailable
        })?;

        sqlx::query(
            "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
        )
        .bind(workspace_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready ensure activation row failed");
            CommerceInventoryError::Unavailable
        })?;

        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM inventory_activation_state WHERE workspace_id = $1 FOR UPDATE",
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready lock status failed");
            CommerceInventoryError::Unavailable
        })?;

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
            .map_err(|error| {
                tracing::warn!(%error, "mark_inventory_ready lock variants failed");
                CommerceInventoryError::Unavailable
            })?;

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
            .map_err(|error| {
                tracing::warn!(%error, "mark_inventory_ready missing count failed");
                CommerceInventoryError::Unavailable
            })?;

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
            .map_err(|error| {
                tracing::warn!(%error, "mark_inventory_ready active count failed");
                CommerceInventoryError::Unavailable
            })?;

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
            .map_err(|error| {
                tracing::warn!(%error, "mark_inventory_ready invalid availability failed");
                CommerceInventoryError::Unavailable
            })?;

            if active_count == 0 || missing_count > 0 || invalid_availability > 0 {
                return Err(CommerceInventoryError::Conflict);
            }

            sqlx::query(
                r#"
                UPDATE inventory_activation_state
                SET status = 'ready', ready_at = now(), ready_by = $2, version = version + 1
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id)
            .bind(&command.actor_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "mark_inventory_ready update status failed");
                CommerceInventoryError::Unavailable
            })?;
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
        .bind(command.request_id.as_deref())
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready feature flags insert failed");
            CommerceInventoryError::Unavailable
        })?;

        let row = sqlx::query_as::<_, InventoryActivationRow>(
            r#"
            SELECT status, ready_at, ready_by, version
            FROM inventory_activation_state
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready load activation failed");
            CommerceInventoryError::Unavailable
        })?;

        transaction.commit().await.map_err(|error| {
            tracing::warn!(%error, "mark_inventory_ready commit failed");
            CommerceInventoryError::Unavailable
        })?;

        let enabled_feature_flags = vec![
            "merch_inventory_enabled".to_owned(),
            "merch_inventory_writes_enabled".to_owned(),
            "reward_campaigns_enabled".to_owned(),
        ];

        Ok(MarkInventoryReadyResult {
            activation: InventoryActivationState {
                status: row.status,
                ready_at: row.ready_at,
                ready_by: row.ready_by,
                version: i32::try_from(row.version).map_err(|_| CommerceInventoryError::Invalid)?,
            },
            enabled_feature_flags,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ExistingStocktake {
    id: Uuid,
    request_hash: Vec<u8>,
    created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct VariantAvailability {
    id: Uuid,
    #[allow(dead_code)]
    product_name: String,
    #[allow(dead_code)]
    sku: String,
    sell_without_stock: bool,
    on_hand: i64,
    reserved: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct StocktakeItemRow {
    sku: String,
    label: String,
    target_on_hand: i32,
    on_hand_before: i64,
    reserved_at_apply: i64,
    applied_delta: i32,
    available_quantity: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct InventoryActivationRow {
    status: String,
    ready_at: Option<OffsetDateTime>,
    ready_by: Option<String>,
    version: i64,
}

async fn lock_variant_availability_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    sku: &str,
) -> Result<VariantAvailability, CommerceInventoryError> {
    sqlx::query_as::<_, VariantAvailability>(
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
    .map_err(|error| {
        tracing::warn!(%error, "lock_variant_availability_tx failed");
        CommerceInventoryError::Unavailable
    })?
    .ok_or(CommerceInventoryError::NotFound)
}

async fn load_stocktake_items_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    stocktake_id: Uuid,
) -> Result<Vec<StocktakeItemResult>, CommerceInventoryError> {
    let rows = sqlx::query_as::<_, StocktakeItemRow>(
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
    .map_err(|error| {
        tracing::warn!(%error, "load_stocktake_items_tx failed");
        CommerceInventoryError::Unavailable
    })?;
    Ok(rows
        .into_iter()
        .map(|row| StocktakeItemResult {
            sku: row.sku,
            label: row.label,
            target_on_hand: row.target_on_hand,
            on_hand_before: row.on_hand_before,
            reserved_at_apply: row.reserved_at_apply,
            applied_delta: row.applied_delta,
            available_quantity: row.available_quantity,
        })
        .collect())
}
