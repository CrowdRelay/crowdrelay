macro_rules! decision_core_reads {
    () => {
    async fn load_policies_impl(
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

    async fn load_ticket_yield_snapshots_impl(
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

    async fn load_fan_lifecycle_snapshots_impl(
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
                    ) AS last_event_interest_at,
                    (
                        SELECT count(*)
                        FROM ticket_orders AS ticket_order
                        WHERE ticket_order.workspace_id = fan.workspace_id
                          AND ticket_order.buyer_email = fan.normalized_email
                          AND ticket_order.status IN ('paid', 'partially_refunded')
                    ) AS paid_ticket_count,
                    (
                        SELECT count(*)
                        FROM referral_attributions AS referral
                        WHERE referral.workspace_id = fan.workspace_id
                          AND referral.referrer_fan_id = fan.id
                    ) AS qualified_referrals,
                    (
                        SELECT max(referral.accepted_at)
                        FROM referral_attributions AS referral
                        WHERE referral.workspace_id = fan.workspace_id
                          AND referral.referrer_fan_id = fan.id
                    ) AS last_qualified_referral_at,
                    EXISTS (
                        SELECT 1 FROM referral_codes AS code
                        WHERE code.workspace_id = fan.workspace_id
                          AND code.fan_id = fan.id
                          AND code.active
                    ) AS has_referral_code
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
                      AND NOT run.synthetic
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

    async fn load_event_campaign_snapshots_impl(
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

    async fn load_merch_inventory_snapshots_impl(
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

    async fn load_merch_price_snapshots_impl(
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

    async fn load_merch_bundle_snapshots_impl(
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
    };
}
