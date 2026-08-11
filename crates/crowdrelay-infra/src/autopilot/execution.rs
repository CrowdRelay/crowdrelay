async fn schedule_effect_measurement(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let plan = match payload {
        AutopilotActionPayload::ChangeTicketPrice { ticket_type_id, .. } => {
            let baseline = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COALESCE(SUM(item.total_gross_minor), 0)::double precision
                FROM ticket_order_items AS item
                JOIN ticket_orders AS ticket_order
                  ON ticket_order.workspace_id = item.workspace_id
                 AND ticket_order.id = item.ticket_order_id
                WHERE item.workspace_id = $1
                  AND item.ticket_type_id = $2
                  AND ticket_order.status = 'paid'
                  AND ticket_order.paid_at >= $3 - INTERVAL '72 hours'
                  AND ticket_order.paid_at < $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(ticket_type_id.into_uuid())
            .bind(now)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            Some((
                AutopilotMeasurementKind::TicketRevenue72h,
                ticket_type_id.into_uuid(),
                baseline,
                now + time::Duration::hours(72),
            ))
        }
        AutopilotActionPayload::ChangeMerchPrice {
            product_id,
            from_minor,
            ..
        } => {
            // Inventory is the authoritative first-party sales signal today. Until
            // checkout-level net revenue attribution exists, learn from a clearly
            // named gross-list-price proxy instead of pretending units == success.
            let baseline_units = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COALESCE(-SUM(ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.occurred_at >= $3 - INTERVAL '7 days'
                      AND ledger.occurred_at < $3
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
            .bind(product_id.into_uuid())
            .bind(now)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            let baseline = baseline_units * (*from_minor as f64);
            Some((
                AutopilotMeasurementKind::MerchGrossProxy7d,
                product_id.into_uuid(),
                baseline,
                now + time::Duration::days(7),
            ))
        }
        AutopilotActionPayload::RequestPromotionBudgetChange {
            campaign_id,
            roas_basis_points,
            ..
        } => Some((
            AutopilotMeasurementKind::PromotionRoas7d,
            campaign_id.into_uuid(),
            f64::from(*roas_basis_points),
            now + time::Duration::days(7),
        )),
        AutopilotActionPayload::RequestBookingOutreach { target_id, .. } => Some((
            AutopilotMeasurementKind::BookingReply7d,
            target_id.into_uuid(),
            0.0,
            now + time::Duration::days(7),
        )),
        AutopilotActionPayload::RequestOutreach { target_id, .. } => Some((
            AutopilotMeasurementKind::OutreachReply7d,
            target_id.into_uuid(),
            0.0,
            now + time::Duration::days(7),
        )),
        AutopilotActionPayload::RequestAudienceCampaign { event_id, .. } => {
            let baseline = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COALESCE(SUM(ticket_order.amount_gross_minor),0)::double precision
                FROM ticket_orders ticket_order
                JOIN ticket_sales sale
                  ON sale.workspace_id=ticket_order.workspace_id AND sale.id=ticket_order.ticket_sale_id
                WHERE ticket_order.workspace_id=$1 AND sale.event_id=$2
                  AND ticket_order.status IN ('paid','partially_refunded','refunded')
                  AND ticket_order.paid_at >= $3 - INTERVAL '72 hours'
                  AND ticket_order.paid_at < $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .bind(now)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            Some((
                AutopilotMeasurementKind::AudienceTicketRevenue72h,
                event_id.into_uuid(),
                baseline,
                now + time::Duration::hours(72),
            ))
        }
        AutopilotActionPayload::ChangeTicketCapacity { .. }
        | AutopilotActionPayload::RequestFanLifecycleMessage { .. }
        | AutopilotActionPayload::RequestMerchReorder { .. }
        | AutopilotActionPayload::RequestMerchBundle { .. }
        | AutopilotActionPayload::RequestContentArtifact { .. }
        | AutopilotActionPayload::AdjustExperiment { .. }
        | AutopilotActionPayload::CompleteShowTask { .. }
        | AutopilotActionPayload::EscalateShowTask { .. }
        | AutopilotActionPayload::ExecuteReleaseMilestone { .. }
        | AutopilotActionPayload::ApplyLiveOpportunity { .. }
        | AutopilotActionPayload::PrepareFundingPackage { .. }
        | AutopilotActionPayload::SubmitFundingApplication { .. } => None,
    };
    let Some((kind, subject_id, baseline_value, due_at)) = plan else {
        return Ok(());
    };
    if !baseline_value.is_finite() || baseline_value < 0.0 {
        return Err(RepositoryError::Unexpected);
    }
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_measurements (
            id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8)
        ON CONFLICT (workspace_id, action_id, measurement_kind) DO NOTHING
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(kind.as_str())
    .bind(subject_id)
    .bind(now)
    .bind(baseline_value)
    .bind(due_at)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
async fn record_execution_outcome(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let (metric_key, observed_value, baseline_value) = match payload {
        AutopilotActionPayload::ChangeTicketPrice {
            from_minor,
            to_minor,
            ..
        } => (
            "ticket_price_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::ChangeTicketCapacity {
            from_capacity,
            to_capacity,
            ..
        } => (
            "ticket_capacity",
            f64::from(*to_capacity),
            Some(f64::from(*from_capacity)),
        ),
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => {
            ("lifecycle_message_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestMerchReorder { quantity, .. } => {
            ("merch_reorder_quantity", f64::from(*quantity), None)
        }
        AutopilotActionPayload::ChangeMerchPrice {
            from_minor,
            to_minor,
            ..
        } => (
            "merch_price_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::RequestBookingOutreach { score, .. } => {
            ("booking_opportunity_score", f64::from(*score), None)
        }
        AutopilotActionPayload::RequestAudienceCampaign { .. } => {
            ("audience_campaign_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestMerchBundle {
            bundle_price_minor, ..
        } => ("merch_bundle_price_minor", *bundle_price_minor as f64, None),
        AutopilotActionPayload::RequestOutreach { .. } => ("outreach_requested", 1.0, None),
        AutopilotActionPayload::RequestContentArtifact { .. } => {
            ("content_artifact_requested", 1.0, None)
        }
        AutopilotActionPayload::AdjustExperiment { complete, .. } => (
            if *complete {
                "experiment_completed"
            } else {
                "experiment_allocation_changed"
            },
            1.0,
            None,
        ),
        AutopilotActionPayload::CompleteShowTask { .. } => ("show_task_completed", 1.0, None),
        AutopilotActionPayload::EscalateShowTask { .. } => ("show_task_escalated", 1.0, None),
        AutopilotActionPayload::RequestPromotionBudgetChange {
            from_minor,
            to_minor,
            ..
        } => (
            "promotion_daily_budget_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::ExecuteReleaseMilestone { .. } => ("release_milestone_executed",1.0,None),
        AutopilotActionPayload::ApplyLiveOpportunity { score, .. } => ("live_opportunity_score",f64::from(*score),None),
        AutopilotActionPayload::PrepareFundingPackage { .. } => ("funding_package_requested",1.0,None),
        AutopilotActionPayload::SubmitFundingApplication { .. } => ("funding_submission_requested",1.0,None),
    };
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_outcomes (
            workspace_id, decision_id, action_id, metric_key,
            observed_value, baseline_value, observed_at
        )
        SELECT $1, action.decision_id, action.id, $3, $4, $5, $6
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = $1 AND action.id = $2
        ON CONFLICT (workspace_id, action_id, metric_key)
            WHERE action_id IS NOT NULL DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(metric_key)
    .bind(observed_value)
    .bind(baseline_value)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

async fn execute_ticket_price_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    ticket_type_id: TicketTypeId,
    from_minor: i64,
    to_minor: i64,
) -> Result<(), RepositoryError> {
    let updated = sqlx::query(
        r#"
        UPDATE ticket_types
        SET price_gross_minor = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND price_gross_minor = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .bind(from_minor)
    .bind(to_minor)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT price_gross_minor FROM ticket_types WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    match current {
        Some(current) if current == to_minor => Ok(()),
        Some(_) => Err(RepositoryError::Conflict),
        None => Err(RepositoryError::NotFound),
    }
}

async fn execute_ticket_capacity_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    ticket_type_id: TicketTypeId,
    from_capacity: u32,
    to_capacity: u32,
    expected_guardrail_version: i64,
) -> Result<(), RepositoryError> {
    if to_capacity <= from_capacity {
        return Err(RepositoryError::Conflict);
    }
    let row = sqlx::query_as::<_, (Option<i32>, i32, i32, i32, i64)>(
        r#"
        SELECT ticket_type.capacity, ticket_sale.capacity,
               guardrail.minimum_capacity, guardrail.maximum_capacity, guardrail.version
        FROM ticket_types AS ticket_type
        JOIN ticket_sales AS ticket_sale
          ON ticket_sale.workspace_id = ticket_type.workspace_id
         AND ticket_sale.id = ticket_type.ticket_sale_id
        JOIN viryaos_ticket_type_allocation_guardrails AS guardrail
          ON guardrail.workspace_id = ticket_type.workspace_id
         AND guardrail.ticket_type_id = ticket_type.id
        WHERE ticket_type.workspace_id = $1
          AND ticket_type.id = $2
          AND ticket_type.active
          AND ticket_sale.active
        FOR UPDATE OF ticket_type, guardrail
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let current_capacity = row.0.ok_or(RepositoryError::Conflict)?;
    let from_i32 = i32::try_from(from_capacity).map_err(|_| RepositoryError::Unexpected)?;
    let to_i32 = i32::try_from(to_capacity).map_err(|_| RepositoryError::Unexpected)?;
    if row.4 != expected_guardrail_version
        || current_capacity != from_i32
        || to_i32 > row.1
        || from_i32 < row.2
        || to_i32 > row.3
    {
        return Err(RepositoryError::Conflict);
    }
    let committed = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(item.quantity), 0)::bigint
        FROM ticket_order_items AS item
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = item.workspace_id
         AND ticket_order.id = item.ticket_order_id
        WHERE item.workspace_id = $1
          AND item.ticket_type_id = $2
          AND ticket_order.status IN ('reserved', 'paid')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if committed > i64::from(to_i32) {
        return Err(RepositoryError::Conflict);
    }
    let updated = sqlx::query(
        r#"
        UPDATE ticket_types
        SET capacity = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND capacity = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .bind(from_i32)
    .bind(to_i32)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}

async fn execute_merch_price_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    product_id: MerchProductId,
    from_minor: i64,
    to_minor: i64,
    expected_economics_version: i64,
) -> Result<(), RepositoryError> {
    let guardrails = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT minimum_price_minor, maximum_price_minor, version
        FROM viryaos_merch_product_economics
        WHERE workspace_id = $1 AND product_id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    if guardrails.2 != expected_economics_version
        || to_minor < guardrails.0
        || to_minor > guardrails.1
    {
        return Err(RepositoryError::Conflict);
    }
    let updated = sqlx::query(
        r#"
        UPDATE merch_products
        SET price_gross_minor = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND public
          AND price_gross_minor = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .bind(from_minor)
    .bind(to_minor)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT price_gross_minor FROM merch_products WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    match current {
        Some(current) if current == to_minor => Ok(()),
        Some(_) => Err(RepositoryError::Conflict),
        None => Err(RepositoryError::NotFound),
    }
}

async fn lock_booking_target_for_execution(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    city_id: CityId,
    target_id: BookingTargetId,
    expected_version: i64,
) -> Result<(String, String, String), RepositoryError> {
    sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT target_kind, display_name, contact_email
        FROM viryaos_booking_targets
        WHERE workspace_id = $1
          AND id = $2
          AND city_id = $3
          AND version = $4
          AND active
          AND accepts_booking
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(city_id.into_uuid())
    .bind(expected_version)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)
}

async fn ensure_promotion_state_current(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    campaign_id: PromotionCampaignId,
    expected_budget_minor: i64,
    proposed_budget_minor: i64,
) -> Result<(), RepositoryError> {
    let current = sqlx::query_as::<_, (i64, String)>(
        r#"
        SELECT current_daily_budget_minor, currency
        FROM viryaos_promotion_campaign_states
        WHERE workspace_id = $1 AND id = $2 AND active AND expires_at > now()
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)?;
    if current.0 != expected_budget_minor {
        return Err(RepositoryError::Conflict);
    }
    if proposed_budget_minor <= expected_budget_minor {
        return Ok(());
    }

    let guardrail = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT maximum_total_daily_budget_minor, maximum_monthly_spend_minor
        FROM viryaos_promotion_budget_guardrails
        WHERE workspace_id = $1 AND currency = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    let (daily_budget_minor, month_to_date_minor) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT
            COALESCE(SUM(current_daily_budget_minor), 0)::bigint,
            COALESCE(SUM(spend_month_to_date_minor), 0)::bigint
        FROM viryaos_promotion_campaign_states
        WHERE workspace_id = $1
          AND currency = $2
          AND active
          AND expires_at > now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let reserved_delta_minor = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(daily_delta_minor), 0)::bigint
        FROM viryaos_promotion_budget_reservations
        WHERE workspace_id = $1 AND currency = $2 AND expires_at > now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let delta = proposed_budget_minor
        .checked_sub(expected_budget_minor)
        .ok_or(RepositoryError::Unexpected)?;
    let projected_daily = daily_budget_minor
        .checked_add(reserved_delta_minor)
        .and_then(|value| value.checked_add(delta))
        .ok_or(RepositoryError::Unexpected)?;
    if projected_daily > guardrail.0 || month_to_date_minor >= guardrail.1 {
        return Err(RepositoryError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO viryaos_promotion_budget_reservations (
            workspace_id, action_id, campaign_id, currency, daily_delta_minor, expires_at
        ) VALUES ($1,$2,$3,$4,$5,now() + interval '24 hours')
        ON CONFLICT (workspace_id, action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .bind(&current.1)
    .bind(delta)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

async fn ensure_marketing_eligible(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<(), RepositoryError> {
    let eligible = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM fans AS fan
            JOIN LATERAL (
                SELECT consent.granted
                FROM fan_consents AS consent
                WHERE consent.workspace_id = fan.workspace_id
                  AND consent.fan_id = fan.id
                  AND consent.purpose = 'marketing'
                ORDER BY consent.recorded_at DESC, consent.id DESC
                LIMIT 1
            ) AS latest_consent ON latest_consent.granted
            WHERE fan.workspace_id = $1
              AND fan.id = $2
              AND fan.status = 'active'
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if eligible {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}

fn executor_capability_for_event(event_type: &str) -> &'static str {
    match event_type {
        "viryaos.fan_lifecycle.message_requested" => "fan.lifecycle.message",
        "viryaos.merch.reorder_requested" => "merch.reorder",
        "viryaos.booking.outreach_requested" => "booking.outreach",
        "viryaos.merch.bundle_requested" => "merch.bundle",
        "viryaos.outreach.requested" => "outreach.send",
        "viryaos.content.artifact_requested" => "content.artifact",
        "viryaos.show.task_attention_required" => "show.escalation",
        "viryaos.promotion.budget_change_requested" => "promotion.budget",
        "viryaos.opportunity.application_requested" => "opportunity.application",
        "viryaos.funding.package_requested" => "funding.package",
        "viryaos.funding.submission_requested" => "funding.submit",
        "viryaos.calendar.upsert_requested" => "calendar.upsert",
        _ => "unknown",
    }
}

pub(in crate::autopilot) async fn ensure_executor_capability(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<(), RepositoryError> {
    let registry_enabled = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if !registry_enabled {
        return Ok(());
    }
    let available = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM viryaos_executor_capabilities capability
            JOIN viryaos_executor_instances executor
              ON executor.workspace_id=capability.workspace_id
             AND executor.executor_id=capability.executor_id
            LEFT JOIN viryaos_executor_circuit_breakers breaker
              ON breaker.workspace_id=executor.workspace_id
             AND breaker.executor_id=executor.executor_id
            WHERE capability.workspace_id=$1
              AND capability.capability=$2
              AND capability.expires_at>now()
              AND executor.expires_at>now()
              AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(capability)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if available {
        Ok(())
    } else {
        Err(RepositoryError::Unavailable)
    }
}

pub(in crate::autopilot) async fn reserve_contact_window(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    context: &'static str,
    contact: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let normalized = contact.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized.len() > 320 {
        return Err(RepositoryError::Conflict);
    }
    let reserved = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO viryaos_contact_governor (
            workspace_id, normalized_contact, last_context, last_action_id,
            last_outbound_at, next_contact_after
        ) VALUES ($1,$2,$3,$4,$5,$5 + INTERVAL '7 days')
        ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
        SET last_context=EXCLUDED.last_context,
            last_action_id=EXCLUDED.last_action_id,
            last_outbound_at=EXCLUDED.last_outbound_at,
            next_contact_after=EXCLUDED.next_contact_after
        WHERE NOT viryaos_contact_governor.do_not_contact
          AND (
              viryaos_contact_governor.next_contact_after <= EXCLUDED.last_outbound_at
              OR viryaos_contact_governor.last_action_id = EXCLUDED.last_action_id
          )
        RETURNING normalized_contact
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&normalized)
    .bind(context)
    .bind(action_id.into_uuid())
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if reserved.is_some() {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}

pub(super) async fn emit_external_action(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &'static str,
    payload: Value,
) -> Result<(), RepositoryError> {
    ensure_executor_capability(
        transaction,
        workspace_id,
        executor_capability_for_event(event_type),
    )
    .await?;
    let emission_key = format!("autopilot-action:{}", action_id);
    let outbox_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH emission AS (
            INSERT INTO viryaos_autopilot_action_emissions (
                workspace_id, action_id, emission_key, outbox_event_id
            ) VALUES ($1,$2,$3,$4)
            ON CONFLICT (workspace_id, emission_key) DO NOTHING
            RETURNING outbox_event_id
        ), outbox AS (
            INSERT INTO outbox_events (
                id, workspace_id, event_type, event_version, payload,
                request_id, max_attempts
            )
            SELECT $4,$1,$5,$6,$7,$3,12
            FROM emission
            RETURNING id
        )
        SELECT id FROM outbox
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(&emission_key)
    .bind(outbox_id)
    .bind(event_type)
    .bind(EXTERNAL_ACTION_EVENT_VERSION)
    .bind(payload)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if inserted.is_some() {
        return Ok(());
    }

    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM viryaos_autopilot_action_emissions
            WHERE workspace_id = $1 AND emission_key = $2 AND action_id = $3
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&emission_key)
    .bind(action_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}
