//! Split PostgreSQL Autopilot adapter implementation.

mod readiness;

use super::*;
use readiness::{
    measured_evidence_quality, observable_community, refresh_evidence_readiness,
    refresh_experiment_readiness,
};

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
                          AND measured_at >= $3
                          AND measured_at < $3 + INTERVAL '7 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
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
                          AND measured_at >= $3
                          AND measured_at < $3 + INTERVAL '7 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.subject_id)
                    .bind(measurement.action_finished_at)
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
                          AND status != 'suppressed'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                // Incremental fan growth (North Star): difference-in-
                // differences (DiD) estimate. New fans in the 14-day post-
                // action window minus the counterfactual (pre-action daily
                // rate from a matched 14-day window × 14, stored as
                // baseline_value).
                //
                // COMMUNITY-LEVEL MEASUREMENT via fan_provenance_events:
                // When the experiment assignment's unit_kind is
                // TargetCommunity, we count DISTINCT fans from provenance
                // events attributed to that community (event_kind =
                // 'conversion', fan_id IS NOT NULL). This gives a
                // community-level outcome that matches the experimental
                // unit — the core requirement for valid causal inference.
                //
                // PROVENANCE ≠ CAUSALITY. Community-attributed conversion
                // is an outcome signal. The incremental causal effect
                // still requires treatment/control comparison via the
                // experiment design. When provenance is missing or
                // insufficient, we fall back to workspace-level DiD and
                // downgrade evidence quality to MatchedQuasiExperiment.
                //
                // Allows negative values — the brain must be able to learn
                // that an action *harmed* fan growth (e.g. a community post
                // that alienated the audience). The treatment-effect
                // posterior supports negative τ via `update_signed`.
                AutopilotMeasurementKind::IncrementalFanGrowth14d => {
                    let community =
                        observable_community(&self.pool, workspace_id, measurement.action_id)
                            .await?;
                    let observed = if let Some(handle) = &community {
                        // Community-level outcome: fans whose conversion was
                        // attributed to this community's smart link inside the
                        // window. The counterfactual is scoped to the same
                        // community by `record_measurement_plans`, so both
                        // sides of the subtraction count the same kind of
                        // thing over the same width of time.
                        sqlx::query_scalar::<_, f64>(
                            r#"
                            SELECT COUNT(DISTINCT fan_id)::double precision
                            FROM fan_provenance_events
                            WHERE workspace_id = $1
                              AND community = $2
                              AND event_kind = 'conversion'
                              AND fan_id IS NOT NULL
                              AND occurred_at >= $3
                              AND occurred_at < $3 + INTERVAL '14 days'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(handle)
                        .bind(measurement.action_finished_at)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(map_sqlx)?
                    } else {
                        sqlx::query_scalar::<_, f64>(
                            r#"
                            SELECT COUNT(*)::double precision FROM fans
                            WHERE workspace_id = $1
                              AND created_at >= $2
                              AND created_at < $2 + INTERVAL '14 days'
                              AND status != 'suppressed'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(measurement.action_finished_at)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(map_sqlx)?
                    };
                    observed - measurement.counterfactual_value()
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
                          AND active = true
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
                // Durable fan growth (Y30): fans created in the 14-day
                // post-action window that are still active 30 days after
                // creation. This is the true North Star — fans that stick,
                // not just fans that sign up.
                //
                // COMMUNITY-LEVEL MEASUREMENT via fan_provenance_events:
                // When the experiment assignment's unit_kind is
                // TargetCommunity, we count DISTINCT fans from provenance
                // events with event_kind = 'durability' attributed to that
                // community. This gives a community-level durable outcome
                // that matches the experimental unit.
                //
                // The measurement is incremental: it subtracts the
                // counterfactual (baseline daily rate × 14) so Y30 is a
                // causal incremental outcome, not a raw count. Allows
                // negative values — the brain must learn when actions
                // produce *non-durable* fans.
                //
                // SQL fix: the second status check was `!= 'suppressed'`
                // (same as the first) instead of `= 'active'`. This meant
                // the query never actually verified the fan was still
                // active — it only checked not-suppressed twice.
                AutopilotMeasurementKind::DurableFanGrowth30d => {
                    let community =
                        observable_community(&self.pool, workspace_id, measurement.action_id)
                            .await?;
                    let observed = if let Some(handle) = &community {
                        // Durability is a state of the converted fan, not a
                        // separate event: the fans this community converted
                        // inside the window who are still active thirty days
                        // after they arrived. Reading it from the conversion
                        // ledger joined to the fan keeps one writer for the
                        // provenance chain instead of requiring a second one
                        // to stamp a durability event that nothing emits.
                        sqlx::query_scalar::<_, f64>(
                            r#"
                            SELECT COUNT(DISTINCT fan.id)::double precision
                            FROM fan_provenance_events AS conversion
                            JOIN fans AS fan
                              ON fan.workspace_id = conversion.workspace_id
                             AND fan.id = conversion.fan_id
                            WHERE conversion.workspace_id = $1
                              AND conversion.community = $2
                              AND conversion.event_kind = 'conversion'
                              AND conversion.occurred_at >= $3
                              AND conversion.occurred_at < $3 + INTERVAL '14 days'
                              AND fan.created_at + INTERVAL '30 days' <= now()
                              AND fan.status = 'active'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(handle)
                        .bind(measurement.action_finished_at)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(map_sqlx)?
                    } else {
                        sqlx::query_scalar::<_, f64>(
                            r#"
                            SELECT COUNT(*)::double precision FROM fans
                            WHERE workspace_id = $1
                              AND created_at >= $2
                              AND created_at < $2 + INTERVAL '14 days'
                              AND created_at + INTERVAL '30 days' <= now()
                              AND status = 'active'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(measurement.action_finished_at)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(map_sqlx)?
                    };
                    observed - measurement.counterfactual_value()
                }
                // Scanner discovery quality: counts new outreach targets
                // discovered in the 14-day post-action window. The scanner's
                // proximal outcome is discovery, not fan growth — measuring
                // it on workspace-wide fan count would credit it for fans
                // acquired by other workers.
                AutopilotMeasurementKind::ScannerDiscoveryQuality14d => {
                    // Targets this dispatch found, and no others.
                    //
                    // `execute_agent_run` stamps the action id into the
                    // task's metadata, so the chain is exact:
                    //   action -> agent_service_tasks.metadata->>'action_id'
                    //          -> agent_outreach_targets.source_task_id
                    //
                    // Counting every row in the window instead credited a
                    // scanner with everything the workspace discovered:
                    // production holds 85 targets, 28 of them written by the
                    // promotion sweep from the audience graph, which no
                    // scanner found. Counting only rows with a
                    // `source_task_id` fixed that but still pooled every run
                    // of the same template, so two scanner dispatches a
                    // fortnight apart shared their discoveries and each was
                    // measured on the other's work. The join settles it.
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision
                        FROM agent_outreach_targets AS target
                        JOIN agent_service_tasks AS task
                          ON task.id = target.source_task_id
                        WHERE target.workspace_id = $1
                          AND target.created_at >= $2
                          AND target.created_at < $2 + INTERVAL '14 days'
                          AND task.metadata->>'action_id' = $3
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .bind(measurement.action_id.into_uuid().to_string())
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
                // Strategist insight quality: counts campaign insights
                // produced in the 14-day post-action window. The
                // strategist's proximal outcome is intelligence production,
                // not fan growth.
                AutopilotMeasurementKind::StrategistInsightQuality14d => {
                    // Insights this dispatch produced, and no others.
                    //
                    // `campaign_insight` is not the strategist's alone —
                    // production has fifteen from `growth-strategist` and
                    // four from `campaign-analysis` — and counting all
                    // nineteen let a campaign-analysis run raise the
                    // strategist's measured quality without the strategist
                    // doing anything. Filtering by template fixed that and
                    // still pooled every strategist run together.
                    //
                    // The action id in the task metadata is the exact link,
                    // and it makes the template filter redundant: a task
                    // started by this action is this action's task whatever
                    // template it ran.
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision
                        FROM agent_outcomes AS outcome
                        JOIN agent_service_tasks AS task ON task.id = outcome.task_id
                        WHERE outcome.workspace_id = $1
                          AND outcome.kind = 'campaign_insight'
                          AND outcome.created_at >= $2
                          AND outcome.created_at < $2 + INTERVAL '14 days'
                          AND task.metadata->>'action_id' = $3
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .bind(measurement.action_id.into_uuid().to_string())
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                }
            };
            if observed.is_finite() {
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
            if !observed_value.is_finite() {
                return Err(RepositoryError::Unexpected);
            }
            // Resolved before the transaction opens, from rows another writer
            // committed. `Some` here means the observation above came from the
            // community ledger rather than the workspace fallback, which is
            // what the evidence quality has to reflect.
            let community = observable_community(&self.pool, workspace_id, measurement.action_id)
                .await
                .ok()
                .flatten();
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let metric_key = format!("effect.{}", measurement.kind.as_str());
            let assessment = effect_assessment_str(effect.assessment);
            let outcome_inserted = sqlx::query(
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
            if outcome_inserted.rows_affected() == 0 {
                // The action row is missing — the outcome INSERT...SELECT
                // produced no rows. Fail the measurement instead of marking
                // it succeeded with no outcome recorded.
                return Err(RepositoryError::NotFound);
            }
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
                AutopilotMeasurementKind::AgentRunFanGrowth14d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        -- Each measurement writes its own column and nothing
                        -- else. `resolved_at` is set by
                        -- `refresh_evidence_readiness` once the queue is empty,
                        -- because readiness is a fact about the whole set of
                        -- outcomes and no single measurement can speak for it.
                        SET observed_new_fans = COALESCE(observed_new_fans, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_new_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // Also update the growth evidence table.
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
                        SET observed_fans = COALESCE(observed_fans, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // IncrementalFanGrowth14d is the counterfactual-adjusted
                // value. It is available to the brain via the evidence
                // view's observed_incremental_fans column, so we don't
                // write it to observed_new_fans (which holds the raw
                // count only).
                AutopilotMeasurementKind::IncrementalFanGrowth14d => {
                    let evidence_quality = measured_evidence_quality(
                        &mut transaction,
                        workspace_id,
                        measurement,
                        community.as_deref(),
                    )
                    .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
                        SET observed_incremental_fans = COALESCE(observed_incremental_fans, $3),
                            evidence_quality = $4
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_incremental_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(evidence_quality)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotMeasurementKind::AgentRunSignalInstalls7d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        SET observed_signal_installs = COALESCE(observed_signal_installs, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_signal_installs IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // Nothing else. The seven-day signal measurement has no
                    // column on the evidence row and must not close it either
                    // — Y14 is a week away and Y30 a month past that.
                }
                // DurableFanGrowth30d writes the durable fan count to the
                // growth evidence table's durable_fans_30d column.
                AutopilotMeasurementKind::DurableFanGrowth30d => {
                    let evidence_quality = measured_evidence_quality(
                        &mut transaction,
                        workspace_id,
                        measurement,
                        community.as_deref(),
                    )
                    .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
                        SET durable_fans_30d = COALESCE(durable_fans_30d, $3),
                            evidence_quality = $4
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND durable_fans_30d IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(evidence_quality)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // Scanner/strategist proximal outcomes have no column on the
                // evidence row — the observed value lives in the outcome table
                // and these workers acquire no fans. Readiness below closes
                // their evidence once their queue is empty, same as everyone
                // else's.
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
            // Enqueue attribution request for fan-growth measurements.
            // The attribution worker discovers competing actions, runs
            // the CreditAllocator, and writes credited entries to the
            // credit ledger. This is durable — if the transaction
            // commits, the attribution will eventually happen.
            if matches!(
                measurement.kind,
                AutopilotMeasurementKind::AgentRunFanGrowth14d
                    | AutopilotMeasurementKind::IncrementalFanGrowth14d
                    | AutopilotMeasurementKind::DurableFanGrowth30d
            ) {
                let _ = sqlx::query(
                    r#"
                    INSERT INTO viryaos_attribution_requests
                        (workspace_id, measurement_id, action_id, attribution_version)
                    VALUES ($1, $2, $3, 1)
                    ON CONFLICT (measurement_id, attribution_version) DO NOTHING
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            // Look up the experiment assignment for this action to
            // evaluate contamination over the full measurement window.
            // CONTAMINATION IS EVALUATED OVER THE FULL WINDOW — not just
            // assignment time. A clean assignment can become contaminated
            // later if concurrent treatment actions occur on the same unit.
            let experiment_info: Option<(uuid::Uuid, String, time::OffsetDateTime)> =
                if matches!(
                    measurement.kind,
                    AutopilotMeasurementKind::AgentRunFanGrowth14d
                        | AutopilotMeasurementKind::IncrementalFanGrowth14d
                        | AutopilotMeasurementKind::DurableFanGrowth30d
                ) {
                    sqlx::query_as::<_, (sqlx::types::Uuid, String, time::OffsetDateTime)>(
                        r#"
                        SELECT experiment_uuid, unit_id, assigned_at
                        FROM viryaos_experiment_assignments
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND experiment_uuid IS NOT NULL
                        LIMIT 1
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
                } else {
                    None
                };
            // Contamination is established inside this transaction, with the
            // outcome it qualifies. It used to run after the commit with its
            // result dropped, so a failure left the assignment's
            // `final_contamination` NULL while the evidence row went on saying
            // `randomized_holdout` — an unbacked claim of clean causal
            // evidence that nothing downstream could detect. Committed
            // together, the outcome and what is known about its cleanliness
            // cannot disagree; and a failure here rolls the outcome back so
            // the measurement is retried rather than half-recorded.
            if let Some((exp_uuid, unit_id, assigned_at)) = experiment_info {
                super::operations::experiment_assignments::evaluate_contamination(
                    &mut transaction,
                    workspace_id,
                    exp_uuid,
                    &unit_id,
                    assigned_at,
                    now,
                )
                .await?;
                // The control arm is measured in the same breath. A control
                // unit is never dispatched, so nothing schedules a measurement
                // for it, so its evidence would sit unresolved forever while
                // the treatment rows resolved around it — leaving the learner
                // with treatment-only data under an intent-to-treat label.
                super::operations::experiment_assignments::resolve_control_evidence(
                    &mut transaction,
                    workspace_id,
                    exp_uuid,
                    now,
                )
                .await?;
                // Readiness for the whole experiment, not just this action. The
                // control arm may have resolved a moment ago in this very
                // transaction, releasing treated rows that finished their own
                // measurements days earlier and have been waiting for it.
                refresh_experiment_readiness(&mut transaction, workspace_id, exp_uuid, now).await?;
            }
            // The queue decides readiness, and it decides it after this
            // measurement has been marked terminal above, so an action whose
            // last outcome just landed closes here and one still waiting on a
            // fourteen- or forty-four-day window does not. Runs after the
            // control sweep so a treated row released by it is not held for
            // another cycle.
            refresh_evidence_readiness(&mut transaction, workspace_id, measurement.action_id, now)
                .await?;
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
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let action_id: Option<uuid::Uuid> = sqlx::query_scalar::<_, uuid::Uuid>(
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
                RETURNING action_id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement_id.into_uuid())
            .bind(now)
            .bind(retryable)
            .bind(error_kind)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // An outcome that will never arrive must not hold the evidence
            // open forever. Readiness re-checks the queue: if this was the
            // last thing outstanding, the row closes with that outcome's
            // column still NULL, and the learner skips what it never learned.
            if let Some(action_id) = action_id {
                refresh_evidence_readiness(
                    &mut transaction,
                    workspace_id,
                    AutopilotActionId::from(action_id),
                    now,
                )
                .await?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}
