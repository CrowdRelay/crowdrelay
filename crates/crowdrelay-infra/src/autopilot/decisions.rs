//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

#[async_trait]
impl AutopilotDecisionRepository for PostgresAutopilotRepository {
    async fn load_policies(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AutopilotPolicy>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PolicyRow>(
                r#"
                SELECT context, enabled, autonomy_level,
                       minimum_confidence_basis_points, max_actions_24h, config, version,
                       guarded_until, guardrail_reason
                FROM viryaos_autopilot_policies
                WHERE workspace_id = $1
                ORDER BY context
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(parse_policy).collect()
        })
        .await
    }

    async fn load_ticket_yield_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<TicketYieldSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, TicketSnapshotRow>(
                r#"
                SELECT
                    ticket_type.id AS ticket_type_id,
                    ticket_type.price_gross_minor AS current_price_minor,
                    COALESCE(SUM(order_item.quantity) FILTER (
                        WHERE ticket_order.status = 'paid'
                    ), 0)::bigint AS paid_quantity,
                    COALESCE(ticket_type.capacity, ticket_sale.capacity)::bigint AS capacity,
                    ticket_sale.capacity::bigint AS sale_capacity,
                    COALESCE(SUM(order_item.quantity) FILTER (
                        WHERE ticket_order.status = 'paid'
                          AND ticket_order.paid_at >= $2 - INTERVAL '72 hours'
                    ), 0)::bigint AS paid_last_72h,
                    GREATEST(
                        0,
                        CEIL(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 86400.0)
                    )::bigint AS days_to_event,
                    last_change.finished_at AS last_price_change_at,
                    last_capacity_change.finished_at AS last_capacity_change_at,
                    allocation.minimum_capacity AS allocation_minimum_capacity,
                    allocation.maximum_capacity AS allocation_maximum_capacity,
                    allocation.step_capacity AS allocation_step_capacity,
                    allocation.version AS allocation_guardrail_version
                FROM ticket_types AS ticket_type
                JOIN ticket_sales AS ticket_sale
                  ON ticket_sale.workspace_id = ticket_type.workspace_id
                 AND ticket_sale.id = ticket_type.ticket_sale_id
                JOIN events AS event
                  ON event.workspace_id = ticket_sale.workspace_id
                 AND event.id = ticket_sale.event_id
                LEFT JOIN viryaos_ticket_type_allocation_guardrails AS allocation
                  ON allocation.workspace_id = ticket_type.workspace_id
                 AND allocation.ticket_type_id = ticket_type.id
                LEFT JOIN ticket_order_items AS order_item
                  ON order_item.workspace_id = ticket_type.workspace_id
                 AND order_item.ticket_type_id = ticket_type.id
                LEFT JOIN ticket_orders AS ticket_order
                  ON ticket_order.workspace_id = order_item.workspace_id
                 AND ticket_order.id = order_item.ticket_order_id
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = ticket_type.workspace_id
                      AND action.subject_id = ticket_type.id
                      AND action.action_kind = 'ticket.price.change'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_change ON true
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = ticket_type.workspace_id
                      AND action.subject_id = ticket_type.id
                      AND action.action_kind = 'ticket.capacity.change'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_capacity_change ON true
                WHERE ticket_type.workspace_id = $1
                  AND ticket_type.active
                  AND ticket_sale.active
                  AND event.status = 'published'
                  AND ticket_sale.sales_open_at <= $2
                  AND ticket_sale.sales_close_at > $2
                  AND event.starts_at > $2
                  AND (
                      ticket_type.capacity IS NOT NULL
                      OR 1 = (
                          SELECT COUNT(*)
                          FROM ticket_types AS sibling
                          WHERE sibling.workspace_id = ticket_type.workspace_id
                            AND sibling.ticket_sale_id = ticket_type.ticket_sale_id
                            AND sibling.active
                      )
                  )
                GROUP BY ticket_type.id, ticket_type.price_gross_minor,
                         ticket_type.capacity, ticket_sale.capacity,
                         event.starts_at, last_change.finished_at,
                         last_capacity_change.finished_at,
                         allocation.minimum_capacity, allocation.maximum_capacity,
                         allocation.step_capacity, allocation.version
                ORDER BY event.starts_at, ticket_type.sort_order, ticket_type.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(ticket_snapshot).collect()
        })
        .await
    }

    async fn load_fan_lifecycle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FanLifecycleSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, LifecycleSnapshotRow>(
                r#"
                SELECT
                    fan.id AS fan_id,
                    fan.status = 'active' AS active,
                    COALESCE(latest_consent.granted, false) AS marketing_consent,
                    fan.created_at,
                    synesthesia.completed_at AS synesthesia_completed_at,
                    GREATEST(
                        lifecycle_touch.finished_at,
                        campaign_touch.touch_at
                    ) AS last_marketing_touch_at,
                    EXISTS (
                        SELECT 1
                        FROM ticket_orders AS ticket_order
                        WHERE ticket_order.workspace_id = fan.workspace_id
                          AND ticket_order.buyer_email = fan.normalized_email
                          AND ticket_order.status IN ('paid', 'partially_refunded')
                    ) AS has_paid_ticket,
                    (
                        SELECT max(ticket_order.paid_at)
                        FROM ticket_orders AS ticket_order
                        WHERE ticket_order.workspace_id = fan.workspace_id
                          AND ticket_order.buyer_email = fan.normalized_email
                          AND ticket_order.status IN ('paid', 'partially_refunded')
                    ) AS last_paid_ticket_at,
                    (
                        SELECT max(interest.created_at)
                        FROM event_interests AS interest
                        WHERE interest.workspace_id = fan.workspace_id
                          AND interest.fan_id = fan.id
                    ) AS last_event_interest_at
                FROM fans AS fan
                LEFT JOIN LATERAL (
                    SELECT consent.granted
                    FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                    ORDER BY consent.recorded_at DESC, consent.id DESC
                    LIMIT 1
                ) AS latest_consent ON true
                LEFT JOIN LATERAL (
                    SELECT run.completed_at
                    FROM synesthesia_reward_entries AS entry
                    JOIN synesthesia_runs AS run
                      ON run.workspace_id = entry.workspace_id
                     AND run.id = entry.run_id
                    WHERE entry.workspace_id = fan.workspace_id
                      AND entry.fan_id = fan.id
                      AND run.completed_at IS NOT NULL
                    ORDER BY run.completed_at DESC, run.id DESC
                    LIMIT 1
                ) AS synesthesia ON true
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = fan.workspace_id
                      AND action.subject_id = fan.id
                      AND action.action_kind = 'fan.lifecycle.message.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS lifecycle_touch ON true
                LEFT JOIN LATERAL (
                    SELECT COALESCE(
                        campaign.completed_at,
                        campaign.scheduled_at,
                        recipient.snapshotted_at
                    ) AS touch_at
                    FROM communication_campaign_recipients AS recipient
                    JOIN communication_campaigns AS campaign
                      ON campaign.workspace_id = recipient.workspace_id
                     AND campaign.id = recipient.campaign_id
                    WHERE recipient.workspace_id = fan.workspace_id
                      AND recipient.fan_id = fan.id
                      AND campaign.status IN ('scheduled', 'completed')
                    ORDER BY COALESCE(
                        campaign.completed_at,
                        campaign.scheduled_at,
                        recipient.snapshotted_at
                    ) DESC, campaign.id DESC
                    LIMIT 1
                ) AS campaign_touch ON true
                WHERE fan.workspace_id = $1
                  AND fan.status = 'active'
                  AND (synesthesia.completed_at IS NULL OR synesthesia.completed_at <= $2)
                ORDER BY fan.created_at, fan.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(lifecycle_snapshot).collect()
        })
        .await
    }

    async fn load_event_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<EventCampaignSnapshot>, RepositoryError> {
        self.bounded(operations::load_event_campaign_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_merch_inventory_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchInventorySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, MerchSnapshotRow>(
                r#"
                WITH stock AS (
                    SELECT
                        ledger.variant_id,
                        COALESCE(SUM(ledger.delta), 0)::bigint AS on_hand,
                        COALESCE(-SUM(ledger.delta) FILTER (
                            WHERE ledger.movement_kind = 'sale'
                              AND ledger.occurred_at >= $2 - INTERVAL '30 days'
                        ), 0)::bigint AS sold_30d
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = $1
                    GROUP BY ledger.variant_id
                ), reservations AS (
                    SELECT
                        item.variant_id,
                        COALESCE(SUM(item.quantity), 0)::bigint AS reserved
                    FROM inventory_reservation_items AS item
                    JOIN inventory_reservations AS reservation
                      ON reservation.workspace_id = item.workspace_id
                     AND reservation.id = item.reservation_id
                    WHERE item.workspace_id = $1
                      AND reservation.status = 'active'
                      AND (reservation.expires_at IS NULL OR reservation.expires_at > $2)
                    GROUP BY item.variant_id
                )
                SELECT
                    variant.id AS variant_id,
                    GREATEST(
                        COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0),
                        0
                    )::bigint AS available_quantity,
                    GREATEST(COALESCE(stock.sold_30d, 0), 0)::bigint AS sold_last_30d,
                    EXISTS (
                        SELECT 1
                        FROM viryaos_autopilot_actions AS action
                        WHERE action.workspace_id = variant.workspace_id
                          AND action.subject_id = variant.id
                          AND action.action_kind = 'merch.reorder.request'
                          AND action.status IN ('awaiting_approval', 'queued', 'processing')
                    ) AS reorder_in_flight,
                    last_reorder.finished_at AS last_reorder_at
                FROM merch_variants AS variant
                JOIN merch_products AS product
                  ON product.workspace_id = variant.workspace_id
                 AND product.id = variant.product_id
                LEFT JOIN stock ON stock.variant_id = variant.id
                LEFT JOIN reservations ON reservations.variant_id = variant.id
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = variant.workspace_id
                      AND action.subject_id = variant.id
                      AND action.action_kind = 'merch.reorder.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_reorder ON true
                WHERE variant.workspace_id = $1
                  AND variant.active
                  AND product.active
                ORDER BY product.slug, variant.sku, variant.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(merch_snapshot).collect()
        })
        .await
    }

    async fn load_merch_price_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchPriceSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, MerchPriceSnapshotRow>(
                r#"
                WITH stock AS (
                    SELECT
                        variant.product_id,
                        COALESCE(SUM(ledger.delta), 0)::bigint AS on_hand,
                        COALESCE(-SUM(ledger.delta) FILTER (
                            WHERE ledger.movement_kind = 'sale'
                              AND ledger.occurred_at >= $2 - INTERVAL '7 days'
                        ), 0)::bigint AS sold_7d,
                        COALESCE(-SUM(ledger.delta) FILTER (
                            WHERE ledger.movement_kind = 'sale'
                              AND ledger.occurred_at >= $2 - INTERVAL '30 days'
                        ), 0)::bigint AS sold_30d
                    FROM merch_variants AS variant
                    LEFT JOIN inventory_ledger AS ledger
                      ON ledger.workspace_id = variant.workspace_id
                     AND ledger.variant_id = variant.id
                    WHERE variant.workspace_id = $1
                      AND variant.active
                    GROUP BY variant.product_id
                ), reservations AS (
                    SELECT
                        variant.product_id,
                        COALESCE(SUM(item.quantity), 0)::bigint AS reserved
                    FROM inventory_reservation_items AS item
                    JOIN inventory_reservations AS reservation
                      ON reservation.workspace_id = item.workspace_id
                     AND reservation.id = item.reservation_id
                    JOIN merch_variants AS variant
                      ON variant.workspace_id = item.workspace_id
                     AND variant.id = item.variant_id
                    WHERE item.workspace_id = $1
                      AND reservation.status = 'active'
                      AND (reservation.expires_at IS NULL OR reservation.expires_at > $2)
                    GROUP BY variant.product_id
                )
                SELECT
                    product.id AS product_id,
                    product.price_gross_minor AS current_price_minor,
                    economics.minimum_price_minor,
                    economics.maximum_price_minor,
                    economics.unit_cost_minor,
                    economics.version AS economics_version,
                    GREATEST(
                        COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0),
                        0
                    )::bigint AS available_quantity,
                    GREATEST(COALESCE(stock.sold_7d, 0), 0)::bigint AS sold_last_7d,
                    GREATEST(COALESCE(stock.sold_30d, 0), 0)::bigint AS sold_last_30d,
                    last_change.finished_at AS last_price_change_at
                FROM merch_products AS product
                JOIN viryaos_merch_product_economics AS economics
                  ON economics.workspace_id = product.workspace_id
                 AND economics.product_id = product.id
                LEFT JOIN stock ON stock.product_id = product.id
                LEFT JOIN reservations ON reservations.product_id = product.id
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = product.workspace_id
                      AND action.subject_id = product.id
                      AND action.action_kind = 'merch.price.change'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_change ON true
                WHERE product.workspace_id = $1
                  AND product.active
                  AND product.public
                ORDER BY product.slug, product.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(merch_price_snapshot).collect()
        })
        .await
    }

    async fn load_merch_bundle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchBundleSnapshot>, RepositoryError> {
        self.bounded(operations::load_merch_bundle_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_city_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<CityOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, BookingSnapshotRow>(
                r#"
                SELECT
                    city.id AS city_id,
                    COALESCE(city_aggregate.confirmed_fan_count, 0)::bigint AS active_fans,
                    COALESCE(growth.new_fans_30d, 0)::bigint AS new_fans_30d,
                    COALESCE(interests.event_interests, 0)::bigint AS event_interests,
                    COALESCE(area.area_claims, 0)::bigint AS area_claims,
                    CASE
                        WHEN last_show.starts_at IS NULL THEN NULL
                        ELSE GREATEST(
                            0,
                            FLOOR(EXTRACT(EPOCH FROM ($2 - last_show.starts_at)) / 2629800.0)
                        )::bigint
                    END AS months_since_last_show,
                    EXISTS (
                        SELECT 1
                        FROM viryaos_autopilot_actions AS action
                        WHERE action.workspace_id = $1
                          AND action.subject_id = city.id
                          AND action.action_kind = 'booking.outreach.request'
                          AND action.status IN ('awaiting_approval', 'queued', 'processing')
                    ) AS outreach_in_flight,
                    last_outreach.finished_at AS last_outreach_at
                FROM cities AS city
                JOIN city_aggregates AS city_aggregate
                  ON city_aggregate.workspace_id = $1
                 AND city_aggregate.city_id = city.id
                LEFT JOIN LATERAL (
                    SELECT COUNT(DISTINCT interest.fan_id)::bigint AS new_fans_30d
                    FROM fan_city_interests AS interest
                    JOIN fans AS fan
                      ON fan.workspace_id = interest.workspace_id
                     AND fan.id = interest.fan_id
                    WHERE interest.workspace_id = $1
                      AND interest.city_id = city.id
                      AND fan.status = 'active'
                      AND fan.created_at >= $2 - INTERVAL '30 days'
                ) AS growth ON true
                LEFT JOIN LATERAL (
                    SELECT COUNT(*)::bigint AS event_interests
                    FROM event_interests AS event_interest
                    JOIN events AS event
                      ON event.workspace_id = event_interest.workspace_id
                     AND event.id = event_interest.event_id
                    WHERE event_interest.workspace_id = $1
                      AND event.city_id = city.id
                      AND event_interest.created_at >= $2 - INTERVAL '180 days'
                ) AS interests ON true
                LEFT JOIN LATERAL (
                    SELECT COUNT(*)::bigint AS area_claims
                    FROM area_claims AS claim
                    JOIN area_drops AS drop
                      ON drop.workspace_id = claim.workspace_id
                     AND drop.id = claim.drop_id
                    WHERE claim.workspace_id = $1
                      AND drop.city_id = city.id
                      AND claim.claimed_at >= $2 - INTERVAL '365 days'
                ) AS area ON true
                LEFT JOIN LATERAL (
                    SELECT event.starts_at
                    FROM events AS event
                    WHERE event.workspace_id = $1
                      AND event.city_id = city.id
                      AND event.status IN ('published', 'completed')
                      AND event.starts_at < $2
                    ORDER BY event.starts_at DESC, event.id DESC
                    LIMIT 1
                ) AS last_show ON true
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = $1
                      AND action.subject_id = city.id
                      AND action.action_kind = 'booking.outreach.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_outreach ON true
                WHERE city_aggregate.workspace_id = $1
                ORDER BY city_aggregate.confirmed_fan_count DESC, city.name, city.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let city_ids = rows.iter().map(|row| row.city_id).collect::<Vec<_>>();
            let market_rows = if city_ids.is_empty() {
                Vec::new()
            } else {
                sqlx::query_as::<_, MarketSignalRow>(
                    r#"
                    SELECT city_id, signal_kind, score_basis_points, confidence_basis_points,
                           observed_at, expires_at
                    FROM viryaos_city_market_signals
                    WHERE workspace_id = $1
                      AND city_id = ANY($2)
                      AND observed_at <= $3
                      AND expires_at > $3
                    ORDER BY city_id, signal_kind, source, id
                    LIMIT 5000
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&city_ids)
                .bind(now)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?
            };
            let mut signals_by_city: HashMap<Uuid, Vec<CityMarketSignal>> = HashMap::new();
            for row in market_rows {
                signals_by_city
                    .entry(row.city_id)
                    .or_default()
                    .push(market_signal(row)?);
            }
            rows.into_iter()
                .map(|row| {
                    let market_evidence = aggregate_city_market_evidence(
                        signals_by_city.remove(&row.city_id).unwrap_or_default(),
                        now,
                    );
                    booking_snapshot(row, market_evidence)
                })
                .collect()
        })
        .await
    }

    async fn load_booking_target_snapshots(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError> {
        self.bounded(async {
            sqlx::query_as::<_, BookingTargetRow>(
                r#"
                SELECT target.id AS target_id, target.city_id, target.target_kind,
                       target.display_name, target.capacity, target.version, target.active, target.accepts_booking, target.priority,
                       target.relationship_score,
                       EXISTS (
                           SELECT 1
                           FROM viryaos_autopilot_actions AS action
                           WHERE action.workspace_id = target.workspace_id
                             AND action.action_kind = 'booking.outreach.request'
                             AND action.status IN ('awaiting_approval', 'queued', 'processing')
                             AND action.payload ->> 'target_id' = target.id::text
                       ) AS outreach_in_flight,
                       target.last_outreach_at,
                       COALESCE((SELECT count(*)::integer FROM viryaos_booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='outbound' AND interaction.phase='followup'),0) AS followup_count,
                       COALESCE((SELECT interaction.disposition FROM viryaos_booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='inbound' AND interaction.phase='reply'
                         ORDER BY interaction.occurred_at DESC,interaction.id DESC LIMIT 1),'none') AS last_reply_disposition
                FROM viryaos_booking_targets AS target
                WHERE target.workspace_id = $1
                ORDER BY target.city_id, target.priority DESC,
                         target.relationship_score DESC, target.id
                LIMIT 2000
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(booking_target_snapshot)
            .collect()
        })
        .await
    }

    async fn load_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
        self.bounded(operations::load_outreach_snapshots(self, workspace_id, now))
            .await
    }

    async fn load_content_supply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError> {
        self.bounded(operations::load_content_supply_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_experiment_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ExperimentSnapshot>, RepositoryError> {
        self.bounded(operations::load_experiment_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_show_task_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowTaskSnapshot>, RepositoryError> {
        self.bounded(operations::load_show_task_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_promotion_performance_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PromotionPerformanceSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PromotionSnapshotRow>(
                r#"
                SELECT
                    state.id AS campaign_id,
                    state.current_daily_budget_minor,
                    state.minimum_daily_budget_minor,
                    state.maximum_daily_budget_minor,
                    state.spend_last_7d_minor,
                    state.attributed_revenue_last_7d_minor,
                    SUM(state.current_daily_budget_minor) OVER (
                        PARTITION BY state.workspace_id, state.currency
                    ) AS workspace_daily_budget_minor,
                    SUM(state.spend_month_to_date_minor) OVER (
                        PARTITION BY state.workspace_id, state.currency
                    ) AS workspace_spend_month_to_date_minor,
                    guardrail.maximum_total_daily_budget_minor AS workspace_maximum_daily_budget_minor,
                    guardrail.maximum_monthly_spend_minor AS workspace_maximum_monthly_spend_minor,
                    CASE
                        WHEN event.starts_at IS NULL THEN 365::bigint
                        ELSE GREATEST(0, CEIL(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 86400.0))::bigint
                    END AS days_to_event,
                    state.active,
                    COALESCE(last_change.finished_at, state.last_budget_change_at) AS last_budget_change_at,
                    state.observed_at,
                    state.expires_at
                FROM viryaos_promotion_campaign_states AS state
                LEFT JOIN events AS event
                  ON event.workspace_id = state.workspace_id
                 AND event.id = state.event_id
                LEFT JOIN viryaos_promotion_budget_guardrails AS guardrail
                  ON guardrail.workspace_id = state.workspace_id
                 AND guardrail.currency = state.currency
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = state.workspace_id
                      AND action.subject_id = state.id
                      AND action.action_kind = 'promotion.budget_change.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_change ON true
                WHERE state.workspace_id = $1
                  AND state.active
                  AND state.expires_at > $2
                  AND (event.id IS NULL OR event.starts_at > $2)
                ORDER BY state.observed_at DESC, state.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(promotion_snapshot).collect()
        })
        .await
    }

    async fn load_release_plan_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ReleasePlanSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, ReleaseSnapshotRow>(r#"
                SELECT plan.id AS release_id, plan.title, plan.release_at, plan.active,
                       plan.assets_ready, plan.communication_enabled, plan.press_enabled,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='seed_calendar') calendar_seeded,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='announcement') announcement_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='start_press') press_started,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='fan_warmup') fan_warmup_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='countdown') countdown_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='release_day') release_day_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='sustain') sustain_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='wrap') wrap_sent
                FROM viryaos_release_plans plan
                WHERE plan.workspace_id=$1 AND plan.active
                  AND plan.release_at BETWEEN $2 - INTERVAL '30 days' AND $2 + INTERVAL '180 days'
                ORDER BY plan.release_at, plan.id
                LIMIT $3
            "#)
            .bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            Ok(rows.into_iter().map(|row| ReleasePlanSnapshot {
                release_id: ReleasePlanId::from_uuid(row.release_id), title: row.title,
                release_at: row.release_at, active: row.active, assets_ready: row.assets_ready,
                communication_enabled: row.communication_enabled, press_enabled: row.press_enabled,
                history: ReleaseMilestoneHistory {
                    calendar_seeded: row.calendar_seeded, announcement_sent: row.announcement_sent,
                    press_started: row.press_started, fan_warmup_sent: row.fan_warmup_sent,
                    countdown_sent: row.countdown_sent, release_day_sent: row.release_day_sent,
                    sustain_sent: row.sustain_sent, wrap_sent: row.wrap_sent,
                },
            }).collect())
        }).await
    }

    async fn load_live_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows=sqlx::query_as::<_,LiveOpportunityRow>(r#"
                SELECT opportunity.id opportunity_id, opportunity.opportunity_kind,
                       opportunity.status NOT IN ('submitted','replied','won','lost','dismissed') active,
                       opportunity.verified_destination, opportunity.contact_email, opportunity.metadata,
                       opportunity.fit_basis_points, opportunity.reputation_basis_points,
                       opportunity.confidence_basis_points, opportunity.expected_fee_minor,
                       opportunity.estimated_cost_minor, opportunity.application_fee_minor,
                       opportunity.requires_contract, opportunity.exclusive, opportunity.deadline,
                       opportunity.status, opportunity.event_starts_at, opportunity.travel_band,
                       (
                           SELECT COUNT(*)
                           FROM events event
                           WHERE event.workspace_id=opportunity.workspace_id
                             AND event.status IN ('published','completed')
                             AND EXTRACT(YEAR FROM event.starts_at)=EXTRACT(
                                 YEAR FROM COALESCE(opportunity.event_starts_at,$2)
                             )
                       ) committed_shows_year,
                       COALESCE((manager.value->>'annual_target')::integer,15) annual_target,
                       COALESCE((manager.value->>'annual_stretch')::integer,20) annual_stretch,
                       COALESCE((manager.value->>'stretch_minimum_score_basis_points')::integer,9000)
                           stretch_minimum_score_basis_points,
                       COALESCE((manager.value->>'far_shot_minimum_score_basis_points')::integer,9000)
                           far_shot_minimum_score_basis_points,
                       COALESCE((manager.value->>'prefer_weekend_one_shots')::boolean,true)
                           prefer_weekend_one_shots
                FROM viryaos_team_opportunities opportunity
                LEFT JOIN viryaos_manager_config manager
                  ON manager.workspace_id=opportunity.workspace_id
                 AND manager.config_key='booking_policy'
                WHERE opportunity.workspace_id=$1
                  AND opportunity.opportunity_kind IN ('festival','showcase','review_contest','support_slot')
                  AND opportunity.eligible
                  AND opportunity.status IN ('new','prepared','awaiting_approval')
                  AND (opportunity.deadline IS NULL OR opportunity.deadline>$2)
                ORDER BY opportunity.deadline NULLS LAST,
                         opportunity.fit_basis_points DESC, opportunity.id
                LIMIT $3
            "#).bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
              .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            rows.into_iter().map(|row| {
                let kind=match row.opportunity_kind.as_str(){
                    "festival"=>LiveOpportunityKind::Festival,"showcase"=>LiveOpportunityKind::Showcase,
                    "review_contest"=>LiveOpportunityKind::ReviewContest,"support_slot"=>LiveOpportunityKind::SupportSlot,
                    _=>return Err(RepositoryError::Unexpected),
                };
                let travel_band=match row.travel_band.as_deref(){
                    Some("poland")=>Some(LiveTravelBand::Poland),
                    Some("east_germany")=>Some(LiveTravelBand::EastGermany),
                    Some("czechia_slovakia")=>Some(LiveTravelBand::CzechiaSlovakia),
                    Some("far_shot")=>Some(LiveTravelBand::FarShot),
                    None=>None,
                    Some(_)=>return Err(RepositoryError::Unexpected),
                };
                Ok(LiveOpportunitySnapshot{
                    opportunity_id:TeamOpportunityId::from_uuid(row.opportunity_id),kind,active:row.active,
                    verified_destination:row.verified_destination,
                    auto_submission_capable: (row.contact_email.as_ref().is_some_and(|email| !email.trim().is_empty())
                        || row.metadata.get("submission_adapter").and_then(serde_json::Value::as_str).is_some_and(|value| value=="email"))
                        && !row.metadata.get("discovery").and_then(|value|value.get("fee_unverified")).and_then(serde_json::Value::as_bool).unwrap_or(false)
                        && !row.metadata.get("discovery").and_then(|value|value.get("terms_unverified")).and_then(serde_json::Value::as_bool).unwrap_or(false),
                    fit_basis_points:u16::try_from(row.fit_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    reputation_basis_points:u16::try_from(row.reputation_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    evidence_confidence:parse_confidence(row.confidence_basis_points)?,
                    expected_fee_minor:row.expected_fee_minor, estimated_cost_minor:row.estimated_cost_minor,
                    application_fee_minor:row.application_fee_minor, requires_contract:row.requires_contract,
                    exclusive:row.exclusive, deadline:row.deadline, event_starts_at:row.event_starts_at,
                    travel_band, committed_shows_year:u16::try_from(row.committed_shows_year).unwrap_or(u16::MAX),
                    annual_target:u16::try_from(row.annual_target).map_err(|_|RepositoryError::Unexpected)?,
                    annual_stretch:u16::try_from(row.annual_stretch).map_err(|_|RepositoryError::Unexpected)?,
                    stretch_minimum_score_basis_points:u16::try_from(row.stretch_minimum_score_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    far_shot_minimum_score_basis_points:u16::try_from(row.far_shot_minimum_score_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    prefer_weekend_one_shots:row.prefer_weekend_one_shots,
                    already_applied:matches!(row.status.as_str(),"submitted"|"replied"|"won"|"lost"),
                })
            }).collect()
        }).await
    }

    async fn load_funding_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FundingOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows=sqlx::query_as::<_,TeamOpportunityRow>(r#"
                SELECT id opportunity_id, opportunity_kind, status NOT IN ('submitted','won','lost','dismissed') active,
                       verified_destination,contact_email,metadata,fit_basis_points,reputation_basis_points,confidence_basis_points,
                       expected_fee_minor,estimated_cost_minor,application_fee_minor,requires_contract,exclusive,eligible,
                       funding_amount_minor,own_contribution_minor,deadline,package_status,status
                FROM viryaos_team_opportunities
                WHERE workspace_id=$1 AND opportunity_kind='funding' AND eligible
                  AND status IN ('new','prepared','awaiting_approval') AND deadline>$2
                ORDER BY deadline, funding_amount_minor DESC, id LIMIT $3
            "#).bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
              .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            rows.into_iter().map(|row| Ok(FundingOpportunitySnapshot{
                opportunity_id:TeamOpportunityId::from_uuid(row.opportunity_id),active:row.active,eligible:row.eligible,
                evidence_confidence:parse_confidence(row.confidence_basis_points)?,
                fit_basis_points:u16::try_from(row.fit_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                amount_minor:row.funding_amount_minor,own_contribution_minor:row.own_contribution_minor,
                deadline:row.deadline.ok_or(RepositoryError::Unexpected)?,package_prepared:row.package_status=="ready",
                submitted:matches!(row.status.as_str(),"submitted"|"won"|"lost"),
            })).collect()
        }).await
    }

    async fn load_beacon_discovery_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError> {
        self.bounded(operations::load_beacon_discovery_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_beacon_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError> {
        self.bounded(operations::load_beacon_campaign_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_show_growth_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError> {
        self.bounded(operations::load_show_growth_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout)
                .await?;
            if matches!(
                candidate.disposition,
                PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute
            ) {
                let max_actions_24h = sqlx::query_scalar::<_, i32>(
                    r#"
                    SELECT max_actions_24h
                    FROM viryaos_autopilot_policies
                    WHERE workspace_id = $1 AND context = $2 AND enabled
                    FOR UPDATE
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                let actions_24h = sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT COUNT(*)::bigint
                    FROM viryaos_autopilot_actions
                    WHERE workspace_id = $1
                      AND context = $2
                      AND created_at >= now() - INTERVAL '24 hours'
                      AND status <> 'cancelled'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if actions_24h >= i64::from(max_actions_24h) {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: false,
                        action_created: false,
                        quota_throttled: true,
                    });
                }
            }
            let decision_id = Uuid::now_v7();
            let action_json =
                serde_json::to_value(&candidate.action).map_err(|_| RepositoryError::Unexpected)?;
            let inserted_decision = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_decisions (
                    id, workspace_id, decision_key, context, subject_kind, subject_id,
                    decision_kind, confidence_basis_points, disposition, reason,
                    input_snapshot, policy_snapshot, recommendation
                )
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                ON CONFLICT (workspace_id, decision_key) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(decision_id)
            .bind(workspace_id.into_uuid())
            .bind(&candidate.decision_key)
            .bind(candidate.context.as_str())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(candidate.decision_kind)
            .bind(i32::from(candidate.confidence.basis_points()))
            .bind(disposition_str(candidate.disposition))
            .bind(candidate.reason)
            .bind(&candidate.input_snapshot)
            .bind(&candidate.policy_snapshot)
            .bind(&action_json)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let Some(decision_id) = inserted_decision else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence::default());
            };

            let status = match candidate.disposition {
                PolicyDisposition::RequireApproval => Some("awaiting_approval"),
                PolicyDisposition::AutoExecute => Some("queued"),
                PolicyDisposition::ObserveOnly
                | PolicyDisposition::RecommendOnly
                | PolicyDisposition::Deny => None,
            };
            let Some(status) = status else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                });
            };

            let action_id = Uuid::now_v7();
            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_actions (
                    id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    approved_at, approved_by, approval_expires_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,
                    CASE WHEN $10 = 'queued' THEN now() ELSE NULL END,
                    CASE WHEN $10 = 'queued' THEN 'policy:bounded_auto' ELSE NULL END,
                    CASE WHEN $10 = 'awaiting_approval' THEN now() + INTERVAL '72 hours' ELSE NULL END
                )
                ON CONFLICT DO NOTHING
                RETURNING id
                "#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(candidate.context.as_str())
            .bind(candidate.action.action_kind())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(&candidate.action_idempotency_key)
            .bind(action_json)
            .bind(status)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if inserted.is_some() && status == "awaiting_approval" {
                sqlx::query(
                    r#"
                    INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts)
                    VALUES (
                        $1, 'viryaos.autopilot.approval_requested', 1,
                        jsonb_build_object(
                            'action_id', $2::uuid,
                            'context', $3::text,
                            'action_kind', $4::text,
                            'subject_kind', $5::text,
                            'subject_id', $6::uuid,
                            'reason', $7::text,
                            'confidence_basis_points', $8::integer,
                            'approval_expires_at', now() + INTERVAL '72 hours'
                        ),
                        12
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id)
                .bind(candidate.context.as_str())
                .bind(candidate.action.action_kind())
                .bind(candidate.subject.kind())
                .bind(candidate.subject.uuid())
                .bind(candidate.reason)
                .bind(i32::from(candidate.confidence.basis_points()))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(CandidatePersistence {
                decision_created: true,
                action_created: inserted.is_some(),
                quota_throttled: false,
            })
        })
        .await
    }
}
