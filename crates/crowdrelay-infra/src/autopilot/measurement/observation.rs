//! Reading the world for one due measurement.
//!
//! Split out of the adapter so both stay inside the source-size ratchet, and
//! because this is one job with one shape: every arm answers "what happened in
//! the window this measurement covers", and returns a number.
//!
//! Two rules hold across every arm. A kind that reports an *effect* subtracts
//! its counterfactual here and may return a negative — the brain has to be
//! able to learn that an action did harm. A kind that reports a *level* never
//! does. `AutopilotMeasurementKind::is_signed_effect` is which is which, and
//! the classification downstream reads the same flag, so the two cannot drift
//! into disagreeing about what a negative number means.

use super::super::*;
use super::{dispatch_reached_an_audience, observable_community};

/// Observes one claimed measurement.
pub(super) async fn observe(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<f64, RepositoryError> {
    // A dispatch whose post is still a draft has no outcome to
    // observe. Measuring it anyway records the fans an unpublished
    // post did not attract as a real zero, and the brain reads that as
    // the template failing — see
    // `AutopilotMeasurementKind::measures_outbound_reach`.
    //
    // Abandoned rather than postponed. A postponed measurement holds
    // its evidence row open forever if nobody ever publishes, and
    // `resolved_at` never stamps; a failed one is terminal, the
    // horizon stays NULL, and the learner skips it. That is the honest
    // reading of "we tried and could not find out" — the same rule
    // `observable_community` already applies to one kind on one
    // channel.
    if measurement.kind.measures_outbound_reach()
        && !dispatch_reached_an_audience(pool, workspace_id, measurement.action_id).await?
    {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NEVER_PUBLISHED,
        ));
    }
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
            .fetch_one(pool)
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
                .fetch_one(pool)
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
                .fetch_optional(pool)
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
                .fetch_optional(pool)
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
                ) THEN 1.0::double precision ELSE 0.0::double precision END
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.subject_id)
            .bind(measurement.action_finished_at)
            .fetch_one(pool)
            .await
            .map_err(map_sqlx)?,
            AutopilotMeasurementKind::OutreachReply7d => sqlx::query_scalar::<_, f64>(
                r#"
                SELECT CASE WHEN EXISTS (
                    SELECT 1 FROM viryaos_outreach_interactions
                    WHERE workspace_id=$1 AND target_id=$2 AND direction='inbound'
                      AND occurred_at >= $3 AND occurred_at < $3 + INTERVAL '7 days'
                ) THEN 1.0::double precision ELSE 0.0::double precision END
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.subject_id)
            .bind(measurement.action_finished_at)
            .fetch_one(pool)
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
            .fetch_one(pool)
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
            .fetch_one(pool)
            .await
            .map_err(map_sqlx)?,
            AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COALESCE(SUM(attributed_clicks),0)::double precision
                    FROM viryaos_show_growth_surfaces
                    WHERE workspace_id=$1 AND event_id=$2
                      AND updated_at >= $3
                      AND updated_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            AutopilotMeasurementKind::ShowGrowthAttributedTicketOrders7d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COALESCE(SUM(attributed_ticket_orders),0)::double precision
                    FROM viryaos_show_growth_surfaces
                    WHERE workspace_id=$1 AND event_id=$2
                      AND updated_at >= $3
                      AND updated_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
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
                .fetch_one(pool)
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
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Early 3-day checkpoint — same query as the 14-day
            // measurement but with a 3-day window. This is the
            // fastest feedback signal for the learning loop.
            AutopilotMeasurementKind::AgentRunFanGrowth3d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision FROM fans
                    WHERE workspace_id = $1
                      AND created_at >= $2
                      AND created_at < $2 + INTERVAL '3 days'
                      AND status != 'suppressed'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
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
                    observable_community(pool, workspace_id, measurement.action_id)
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
                    .fetch_one(pool)
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
                    .fetch_one(pool)
                    .await
                    .map_err(map_sqlx)?
                };
                observed - measurement.counterfactual_value()
            }
            // The same difference-in-differences estimate over three days.
            //
            // Identical arithmetic to the fourteen-day arm above, at the
            // same two levels — the community when the unit is one, the
            // workspace otherwise — so the two differ only in the width of
            // the window and never in what they mean. The counterfactual
            // is `baseline_value × 3`, scaled by `counterfactual_window_days`.
            //
            // Weaker on purpose and treated as weaker downstream: three
            // days of arrivals is a noisier sample, and an effect that
            // takes a week to show up is invisible here. It exists so a
            // strategy belief can move at three days instead of fourteen,
            // not because it is the better number.
            AutopilotMeasurementKind::IncrementalFanGrowth3d => {
                let community =
                    observable_community(pool, workspace_id, measurement.action_id)
                        .await?;
                let observed = if let Some(handle) = &community {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(DISTINCT fan_id)::double precision
                        FROM fan_provenance_events
                        WHERE workspace_id = $1
                          AND community = $2
                          AND event_kind = 'conversion'
                          AND fan_id IS NOT NULL
                          AND occurred_at >= $3
                          AND occurred_at < $3 + INTERVAL '3 days'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(handle)
                    .bind(measurement.action_finished_at)
                    .fetch_one(pool)
                    .await
                    .map_err(map_sqlx)?
                } else {
                    sqlx::query_scalar::<_, f64>(
                        r#"
                        SELECT COUNT(*)::double precision FROM fans
                        WHERE workspace_id = $1
                          AND created_at >= $2
                          AND created_at < $2 + INTERVAL '3 days'
                          AND status != 'suppressed'
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_finished_at)
                    .fetch_one(pool)
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
                .fetch_one(pool)
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
                .fetch_one(pool)
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
                    observable_community(pool, workspace_id, measurement.action_id)
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
                    .fetch_one(pool)
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
                    .fetch_one(pool)
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
                .fetch_one(pool)
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
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Fan lifecycle engagement: count per-fan engagement events
            // in the 7-day window after the lifecycle message was
            // confirmed delivered. The subject_id is the fan_id.
            //
            // Engagement is counted as any of:
            //   - A redeemed admission pass (fan attended a show)
            //   - A Signal push endpoint creation (fan installed Signal)
            //   - A referral attribution (fan was referred by someone)
            //
            // Each is a distinct signal that the message moved the fan
            // from passive to active. The baseline is 0 — lifecycle
            // messages target new or dormant fans. A positive observed
            // value means the message worked; 0 means it didn't.
            AutopilotMeasurementKind::FanLifecycleEngagement7d => {
                let admission_passes = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM admission_passes
                    WHERE workspace_id = $1
                      AND fan_id = $2
                      AND status = 'redeemed'
                      AND redeemed_at >= $3
                      AND redeemed_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?;
                let push_endpoints = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM fan_push_endpoints
                    WHERE workspace_id = $1
                      AND fan_id = $2
                      AND created_at >= $3
                      AND created_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?;
                let referral_attributions = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM referral_attributions
                    WHERE workspace_id = $1
                      AND referred_fan_id = $2
                      AND accepted_at >= $3
                      AND accepted_at < $3 + INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?;
                admission_passes + push_endpoints + referral_attributions
            }
            // Fast feedback: 1-hour outcome quality checkpoint. Did the
            // agent produce a valid, processed (not rejected) outcome?
            // Binary: 1 if at least one processed outcome exists for
            // this action, 0 otherwise. The brain learns within an hour
            // whether a worker is producing valid output or failing
            // grounding checks.
            AutopilotMeasurementKind::AgentRunOutcomeQuality1h => {
                // The casts are load-bearing. Postgres types a bare `1.0`
                // literal as NUMERIC, and sqlx decodes this column into `f64`,
                // which is FLOAT8 — so without them every attempt failed with
                // "mismatched types; Rust type `f64` (as SQL type `FLOAT8`) is
                // not compatible with SQL type `NUMERIC`". It compiled, linted
                // and unit-tested clean, because nothing here checks SQL types
                // until a real server answers.
                //
                // In production that meant this measurement could never
                // resolve: it was retried, failed the same way each time, and
                // degraded the cycle that attempted it. The one-hour outcome
                // quality check is the brain's fastest feedback signal on
                // whether a dispatched worker produced anything usable, so the
                // fast half of the learning loop was dead while the slow half
                // looked fine.
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT CASE WHEN EXISTS (
                        SELECT 1 FROM agent_outcomes
                        WHERE workspace_id = $1
                          AND processed_action_id = $2
                          AND status = 'processed'
                          AND created_at >= $3
                          AND created_at < $3 + INTERVAL '1 hour'
                    ) THEN 1.0::double precision ELSE 0.0::double precision END
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Fast checkpoint: scanner discovery count in 1 hour. Same
            // query as the 14-day measurement but with a 1-hour window.
            // The scanner discovers targets immediately — this gives
            // the brain next-cycle feedback on scanner quality.
            AutopilotMeasurementKind::ScannerDiscoveryQuality1h => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM agent_outreach_targets AS target
                    JOIN agent_service_tasks AS task
                      ON task.id = target.source_task_id
                    WHERE target.workspace_id = $1
                      AND target.created_at >= $2
                      AND target.created_at < $2 + INTERVAL '1 hour'
                      AND task.metadata->>'action_id' = $3
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_finished_at)
                .bind(measurement.action_id.into_uuid().to_string())
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Fast checkpoint: strategist insight count in 1 hour. Same
            // query as the 14-day measurement but with a 1-hour window.
            AutopilotMeasurementKind::StrategistInsightQuality1h => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM agent_outcomes AS outcome
                    JOIN agent_service_tasks AS task ON task.id = outcome.task_id
                    WHERE outcome.workspace_id = $1
                      AND outcome.kind = 'campaign_insight'
                      AND outcome.created_at >= $2
                      AND outcome.created_at < $2 + INTERVAL '1 hour'
                      AND task.metadata->>'action_id' = $3
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_finished_at)
                .bind(measurement.action_id.into_uuid().to_string())
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Fast checkpoint: signal installs in 1 day. Same query as
            // the 7-day measurement but with a 1-day window.
            AutopilotMeasurementKind::SignalInstalls1d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM fan_push_endpoints
                    WHERE workspace_id = $1
                      AND active = true
                      AND invalidated_at IS NULL
                      AND created_at >= $2
                      AND created_at < $2 + INTERVAL '1 day'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
        };
    if observed.is_finite() {
        Ok(observed)
    } else {
        Err(RepositoryError::Unexpected)
    }
}
