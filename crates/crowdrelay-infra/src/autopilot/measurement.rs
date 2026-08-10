//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

#[async_trait]
impl AutopilotMeasurementRepository for PostgresAutopilotRepository {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout)
                .await?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = 'failed', finished_at = $2, last_error_kind = 'stale_retry_exhausted'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count >= 3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = 'pending', available_at = $2, started_at = NULL,
                    last_error_kind = 'stale_processing_recovered'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count < 3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let rows = sqlx::query_as::<_, ClaimedMeasurementRow>(
                r#"
                WITH selected AS (
                    SELECT id
                    FROM viryaos_autopilot_measurements
                    WHERE workspace_id = $1
                      AND status = 'pending'
                      AND due_at <= $2
                      AND available_at <= $2
                      AND attempt_count < 3
                    ORDER BY due_at, id
                    FOR UPDATE SKIP LOCKED
                    LIMIT $3
                )
                UPDATE viryaos_autopilot_measurements AS measurement
                SET status = 'processing',
                    attempt_count = measurement.attempt_count + 1,
                    started_at = $2,
                    finished_at = NULL,
                    last_error_kind = NULL
                FROM selected
                WHERE measurement.id = selected.id
                RETURNING measurement.id, measurement.action_id, measurement.measurement_kind,
                          measurement.subject_id, measurement.baseline_value,
                          measurement.action_finished_at,
                          measurement.attempt_count AS attempt_number
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(i64::from(limit.min(100)))
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            rows.into_iter().map(claimed_measurement).collect()
        })
        .await
    }

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError> {
        self.bounded(async {
            let observed = match measurement.kind {
                AutopilotMeasurementKind::TicketRevenue72h => sqlx::query_scalar::<_, f64>(
                    r#"
                        SELECT COALESCE(SUM(item.total_gross_minor), 0)::double precision
                        FROM ticket_order_items AS item
                        JOIN ticket_orders AS ticket_order
                          ON ticket_order.workspace_id = item.workspace_id
                         AND ticket_order.id = item.ticket_order_id
                        WHERE item.workspace_id = $1
                          AND item.ticket_type_id = $2
                          AND ticket_order.status = 'paid'
                          AND ticket_order.paid_at >= $3
                          AND ticket_order.paid_at < $3 + INTERVAL '72 hours'
                        "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?,
                AutopilotMeasurementKind::MerchGrossProxy7d => {
                    let units = sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COALESCE(-SUM(ledger.delta) FILTER (
                            WHERE ledger.movement_kind = 'sale'
                              AND ledger.occurred_at >= $3
                              AND ledger.occurred_at < $3 + INTERVAL '7 days'
                        ), 0)::double precision
                        FROM merch_variants AS variant
                        LEFT JOIN inventory_ledger AS ledger
                          ON ledger.workspace_id = variant.workspace_id
                         AND ledger.variant_id = variant.id
                        WHERE variant.workspace_id = $1
                          AND variant.product_id = $2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
                    let to_minor = sqlx::query_scalar::<_, i64>(
                        r#"
                        SELECT (payload ->> 'to_minor')::bigint
                        FROM viryaos_autopilot_actions
                        WHERE workspace_id = $1 AND id = $2
                          AND action_kind = 'merch.price.change'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::Unexpected)?;
                    units * (to_minor as f64)
                }
                AutopilotMeasurementKind::PromotionRoas7d => {
                    // Use a state observation captured only after the complete
                    // post-change seven-day window. Earlier rolling snapshots mix
                    // pre-action spend into the result and are not valid evidence.
                    let values = sqlx::query_as::<_, (i64, i64)>(
                        r#"
                        SELECT spend_last_7d_minor, attributed_revenue_last_7d_minor
                        FROM viryaos_promotion_campaign_states
                        WHERE workspace_id = $1
                          AND id = $2
                          AND observed_at >= $3 + INTERVAL '7 days'
                          AND observed_at <= $4
                        ORDER BY observed_at DESC
                        LIMIT 1
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
                    .bind(now)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::Unavailable)?;
                    if values.0 <= 0 {
                        0.0
                    } else {
                        (values.1 as f64 / values.0 as f64) * 10_000.0
                    }
                }
            };
            if observed.is_finite() && observed >= 0.0 {
                Ok(observed)
            } else {
                Err(RepositoryError::Unexpected)
            }
        })
        .await
    }

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            if !observed_value.is_finite() || observed_value < 0.0 {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout)
                .await?;
            let metric_key = format!("effect.{}", measurement.kind.as_str());
            let assessment = effect_assessment_str(effect.assessment);
            sqlx::query(
                r#"
                INSERT INTO viryaos_autopilot_outcomes (
                    workspace_id, decision_id, action_id, measurement_id, metric_key,
                    observed_value, baseline_value, effect_assessment, delta_basis_points,
                    metadata, observed_at
                )
                SELECT $1, action.decision_id, action.id, $3, $4, $5, $6, $7, $8, $9, $10
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = $1 AND action.id = $2
                ON CONFLICT (workspace_id, measurement_id)
                    WHERE measurement_id IS NOT NULL DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.action_id.into_uuid())
            .bind(measurement.id.into_uuid())
            .bind(metric_key)
            .bind(observed_value)
            .bind(measurement.baseline_value)
            .bind(assessment)
            .bind(effect.delta_basis_points)
            .bind(json!({
                "measurement_kind": measurement.kind.as_str(),
            }))
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let updated = sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = 'succeeded', finished_at = $3, last_error_kind = NULL
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }

    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement_id: AutopilotMeasurementId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = CASE WHEN $4 AND attempt_count < 3 THEN 'pending' ELSE 'failed' END,
                    available_at = CASE
                        WHEN $4 AND attempt_count < 3 THEN $3 + INTERVAL '30 minutes'
                        ELSE available_at
                    END,
                    started_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE started_at END,
                    finished_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE $3 END,
                    last_error_kind = $5
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement_id.into_uuid())
            .bind(now)
            .bind(retryable)
            .bind(error_kind)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}
