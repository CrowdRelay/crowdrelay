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
            // Parse while the claim transaction is still open. A DB/Rust enum
            // drift must never commit a whole batch as `processing` and strand the
            // valid rows behind stale-recovery. Quarantine only the unsupported row.
            let mut claimed = Vec::with_capacity(rows.len());
            for row in rows {
                let measurement_id = row.id;
                match claimed_measurement(row) {
                    Ok(measurement) => claimed.push(measurement),
                    Err(_) => {
                        sqlx::query(
                            r#"
                            UPDATE viryaos_autopilot_measurements
                            SET status='failed', finished_at=$3,
                                last_error_kind='unsupported_measurement_kind'
                            WHERE workspace_id=$1 AND id=$2 AND status='processing'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(measurement_id)
                        .bind(now)
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
                }
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(claimed)
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
                AutopilotMeasurementKind::BookingReply7d => sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT CASE WHEN EXISTS (
                        SELECT 1 FROM viryaos_booking_interactions
                        WHERE workspace_id=$1 AND target_id=$2 AND direction='inbound'
                          AND occurred_at >= $3 AND occurred_at < $3 + INTERVAL '7 days'
                    ) THEN 1.0 ELSE 0.0 END
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?,
                AutopilotMeasurementKind::OutreachReply7d => sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT CASE WHEN EXISTS (
                        SELECT 1 FROM viryaos_outreach_interactions
                        WHERE workspace_id=$1 AND target_id=$2 AND direction='inbound'
                          AND occurred_at >= $3 AND occurred_at < $3 + INTERVAL '7 days'
                    ) THEN 1.0 ELSE 0.0 END
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?,
                AutopilotMeasurementKind::AudienceTicketRevenue72h => sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COALESCE(SUM(ticket_order.amount_gross_minor),0)::double precision
                    FROM ticket_orders ticket_order
                    JOIN ticket_sales sale
                      ON sale.workspace_id=ticket_order.workspace_id AND sale.id=ticket_order.ticket_sale_id
                    WHERE ticket_order.workspace_id=$1 AND sale.event_id=$2
                      AND ticket_order.status IN ('paid','partially_refunded','refunded')
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
                AutopilotMeasurementKind::ShowTicketRevenue7d => sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COALESCE(SUM(ticket_order.amount_gross_minor),0)::double precision
                    FROM ticket_orders AS ticket_order
                    JOIN ticket_sales AS sale
                      ON sale.workspace_id=ticket_order.workspace_id
                     AND sale.id=ticket_order.ticket_sale_id
                    WHERE ticket_order.workspace_id=$1 AND sale.event_id=$2
                      AND ticket_order.status IN ('paid','partially_refunded','refunded')
                      AND ticket_order.paid_at >= $3
                      AND ticket_order.paid_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?,
                AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COALESCE(SUM(attributed_clicks),0)::double precision
                        FROM viryaos_show_growth_surfaces
                        WHERE workspace_id=$1 AND event_id=$2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                AutopilotMeasurementKind::ShowGrowthAttributedTicketOrders7d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COALESCE(SUM(attributed_ticket_orders),0)::double precision
                        FROM viryaos_show_growth_surfaces
                        WHERE workspace_id=$1 AND event_id=$2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                AutopilotMeasurementKind::GrassrootsActivationReplies14d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision
                        FROM viryaos_grassroots_activations
                        WHERE workspace_id=$1 AND event_id=$2
                          AND reply_recorded_at >= $3
                          AND reply_recorded_at < $3 + INTERVAL '14 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                // Fan growth after an agent dispatch: count new fans created
                // in the 14-day window after the action finished. The
                // subject_id is the action_id (which maps to the
                // agent_service_tasks row via metadata->>'action_id'). We
                // count all new fans in the workspace because agent
                // intelligence gathering has indirect, diffuse effects — a
                // reddit scan doesn't create a specific fan, it creates the
                // conditions for fans to find the band.
                AutopilotMeasurementKind::AgentRunFanGrowth14d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision FROM fans
                        WHERE workspace_id = $1
                          AND created_at >= $2
                          AND created_at < $2 + INTERVAL '14 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                // Incremental fan growth (North Star): new fans in the
                // 14-day post-action window minus the counterfactual
                // (pre-action daily rate × 14, stored as baseline_value).
                // Returns max(0, observed - counterfactual) — the
                // incremental fans attributable to the action.
                AutopilotMeasurementKind::IncrementalFanGrowth14d => {
                    let observed = sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision FROM fans
                        WHERE workspace_id = $1
                          AND created_at >= $2
                          AND created_at < $2 + INTERVAL '14 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
                    // The baseline_value stores the pre-action daily fan
                    // arrival rate. Counterfactual = rate × 14 days.
                    let counterfactual = measurement.baseline_value * 14.0;
                    (observed - counterfactual).max(0.0)
                }
                // Signal install growth after an agent dispatch: count new
                // active push endpoints in the 7-day window. A push endpoint
                // is a fan who installed Signal and opted in for push.
                AutopilotMeasurementKind::AgentRunSignalInstalls7d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision
                        FROM fan_push_endpoints
                        WHERE workspace_id = $1
                          AND invalidated_at IS NULL
                          AND created_at >= $2
                          AND created_at < $2 + INTERVAL '7 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                // Community engagement after a community.engage dispatch:
                // aggregate the latest metrics for posts to this target.
                // The subject_id is the outreach target_id. We sum the
                // scores of all community posts linked to this target in
                // the 7-day window — a higher score means the post
                // resonated with the community.
                AutopilotMeasurementKind::AgentRunCommunityEngagement7d => {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COALESCE(SUM(latest.score), 0)::double precision
                        FROM (
                            SELECT DISTINCT ON (cpm.community_post_id)
                                cpm.score
                            FROM community_post_metrics cpm
                            JOIN community_posts cp ON cp.id = cpm.community_post_id
                            WHERE cp.workspace_id = $1
                              AND cp.target_id = $2
                              AND cp.posted_at >= $3
                              AND cp.posted_at < $3 + INTERVAL '7 days'
                            ORDER BY cpm.community_post_id, cpm.measured_at DESC
                        ) AS latest
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
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
            // Bridge: resolve the dispatch prediction with the observed
            // outcome. The brain records predictions in
            // viryaos_dispatch_predictions before dispatch; the measurement
            // system writes outcomes to viryaos_autopilot_outcomes. Without
            // this bridge, the prediction's observed_new_fans /
            // resolved_at columns are never populated, and the causal model
            // learns from an empty dataset every cycle.
            //
            // We map measurement kinds to prediction columns:
            //   agent_run_fan_growth_14d      → observed_new_fans
            //   incremental_fan_growth_14d    → observed_new_fans (preferred)
            //   agent_run_signal_installs_7d  → observed_signal_installs
            //
            // The evidence view (viryaos_brain_evidence) also joins these
            // tables, so even if this bridge misses a row, the view
            // provides the join. But updating the prediction row directly
            // is more efficient for the brain's read path.
            match measurement.kind {
                AutopilotMeasurementKind::AgentRunFanGrowth14d
                | AutopilotMeasurementKind::IncrementalFanGrowth14d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        SET observed_new_fans = $3,
                            resolved_at = COALESCE(resolved_at, $4)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND resolved_at IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotMeasurementKind::AgentRunSignalInstalls7d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        SET observed_signal_installs = $3,
                            resolved_at = COALESCE(resolved_at, $4)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND resolved_at IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                _ => {}
            }
            if effect.assessment == EffectAssessment::Worsened {
                let demoted_context = sqlx::query_scalar::<_, String>(
                    r#"
                    WITH action_context AS (
                        SELECT context
                        FROM viryaos_autopilot_actions
                        WHERE workspace_id=$1 AND id=$2
                    ), latest_per_action AS (
                        SELECT DISTINCT ON (outcome.action_id)
                               outcome.action_id, outcome.effect_assessment,
                               outcome.observed_at, outcome.id
                        FROM viryaos_autopilot_outcomes outcome
                        JOIN viryaos_autopilot_actions action
                          ON action.workspace_id=outcome.workspace_id AND action.id=outcome.action_id
                        JOIN action_context ON action.context=action_context.context
                        WHERE outcome.workspace_id=$1 AND outcome.measurement_id IS NOT NULL
                        ORDER BY outcome.action_id, outcome.observed_at DESC, outcome.id DESC
                    ), recent AS (
                        SELECT effect_assessment
                        FROM latest_per_action
                        ORDER BY observed_at DESC, id DESC
                        LIMIT 2
                    ), qualifies AS (
                        SELECT count(*)=2 AND bool_and(effect_assessment='worsened') AS should_guard
                        FROM recent
                    )
                    UPDATE viryaos_autopilot_policies policy
                    SET autonomy_level='require_approval',
                        guarded_until=$3 + INTERVAL '7 days',
                        guardrail_reason='two_consecutive_worsened_effects',
                        version=version+1
                    FROM action_context, qualifies
                    WHERE policy.workspace_id=$1
                      AND policy.context=action_context.context
                      AND policy.enabled
                      AND policy.autonomy_level='bounded_auto'
                      AND qualifies.should_guard
                    RETURNING policy.context
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .bind(now)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if let Some(context) = demoted_context {
                    sqlx::query(
                        r#"
                        INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,max_attempts)
                        VALUES ($1,'crowdrelay.autopilot.authority_demoted',1,
                            jsonb_build_object(
                                'context',$2::text,
                                'reason','two_consecutive_worsened_effects',
                                'guarded_until',$3::timestamptz + INTERVAL '7 days'
                            ),12)
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(context)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
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

    async fn refresh_treatment_effects(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<(), RepositoryError> {
        super::operations::growth_intelligence::compute_and_store_treatment_effects(
            self,
            workspace_id,
        )
        .await
    }
}
