pub(super) async fn schedule_effect_measurement(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let mut plans = Vec::with_capacity(4);
    match payload {
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
            plans.push((
                AutopilotMeasurementKind::TicketRevenue72h,
                ticket_type_id.into_uuid(),
                baseline,
                now + time::Duration::hours(72),
            ));
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
            plans.push((
                AutopilotMeasurementKind::MerchGrossProxy7d,
                product_id.into_uuid(),
                baseline,
                now + time::Duration::days(7),
            ));
        }
        AutopilotActionPayload::RequestPromotionBudgetChange {
            campaign_id,
            roas_basis_points,
            ..
        } => plans.push((
            AutopilotMeasurementKind::PromotionRoas7d,
            campaign_id.into_uuid(),
            f64::from(*roas_basis_points),
            now + time::Duration::days(7),
        )),
        AutopilotActionPayload::RequestBookingOutreach { target_id, .. } => plans.push((
            AutopilotMeasurementKind::BookingReply7d,
            target_id.into_uuid(),
            0.0,
            now + time::Duration::days(7),
        )),
        AutopilotActionPayload::RequestOutreach { target_id, .. } => plans.push((
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
            plans.push((
                AutopilotMeasurementKind::AudienceTicketRevenue72h,
                event_id.into_uuid(),
                baseline,
                now + time::Duration::hours(72),
            ));
        }
        AutopilotActionPayload::RaiseGrowthOpportunity { .. } => {
            // No measurement is scheduled yet. Measuring a raised finding means
            // comparing the series' own later velocity against the baseline it
            // was raised from, which needs a growth-metric measurement kind that
            // does not exist yet (see Phase 5 in docs/GROWTH_OS_PLAN.md).
            // Scheduling one of the existing kinds here would attribute a
            // ticket or merch movement to an analysis step, which is exactly
            // the kind of invented causality this system must not produce.
        }
        AutopilotActionPayload::IssueReferralCode { .. } => {
            // Nothing to measure. The code either exists or it does not, and
            // whether anybody uses it is measured as a qualified referral
            // against the fan, not against the act of minting it.
        }
        AutopilotActionPayload::RaiseGrowthDebt { .. } => {
            // Same reasoning as the raised growth opportunity above, one step
            // further: debt is measured by the work getting done, and the
            // signal that it did lives in the owning table (an interaction
            // recorded, a surface published, a milestone completed), not in a
            // ticket or merch movement. Phase 5 adds the measurement kind that
            // can read those honestly.
        }
        AutopilotActionPayload::RequestShowGrowth { event_id, lever, .. } => {
            use crowdrelay_domain::show_growth::ShowGrowthLever;

            if !matches!(
                lever,
                ShowGrowthLever::MerchBuyerOffer | ShowGrowthLever::PostShowMerchFollowUp
            ) {
                let baseline = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COALESCE(SUM(ticket_order.amount_gross_minor),0)::double precision
                    FROM ticket_orders AS ticket_order
                    JOIN ticket_sales AS sale
                      ON sale.workspace_id=ticket_order.workspace_id
                     AND sale.id=ticket_order.ticket_sale_id
                    WHERE ticket_order.workspace_id=$1
                      AND sale.event_id=$2
                      AND ticket_order.status IN ('paid','partially_refunded','refunded')
                      AND ticket_order.paid_at >= $3 - INTERVAL '7 days'
                      AND ticket_order.paid_at < $3
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(event_id.into_uuid())
                .bind(now)
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                plans.push((
                    AutopilotMeasurementKind::ShowTicketRevenue7d,
                    event_id.into_uuid(),
                    baseline,
                    now + time::Duration::days(7),
                ));
            }

            // External show-growth execution writes durable, cumulative provider
            // receipts. Snapshot those counters at action completion and compare the
            // same counters after seven days; this measures only growth after the action.
            if !lever.is_first_party_campaign() {
                let (baseline_clicks, baseline_orders) = sqlx::query_as::<_, (f64, f64)>(
                    r#"
                    SELECT
                        COALESCE(SUM(attributed_clicks),0)::double precision,
                        COALESCE(SUM(attributed_ticket_orders),0)::double precision
                    FROM viryaos_show_growth_surfaces
                    WHERE workspace_id=$1 AND event_id=$2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(event_id.into_uuid())
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                plans.push((
                    AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d,
                    event_id.into_uuid(),
                    baseline_clicks,
                    now + time::Duration::days(7),
                ));
                plans.push((
                    AutopilotMeasurementKind::ShowGrowthAttributedTicketOrders7d,
                    event_id.into_uuid(),
                    baseline_orders,
                    now + time::Duration::days(7),
                ));
            }

            // A reply is only a reply when the executor records the explicit
            // reply_received signal; sent/delivered/introduced states are not inferred.
            if matches!(
                lever,
                ShowGrowthLever::PartnerCrossPromo | ShowGrowthLever::GrassrootsSceneRelay
            ) {
                plans.push((
                    AutopilotMeasurementKind::GrassrootsActivationReplies14d,
                    event_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(14),
                ));
            }
        }
        AutopilotActionPayload::ChangeTicketCapacity { .. }
        | AutopilotActionPayload::RequestFanLifecycleMessage { .. }
        | AutopilotActionPayload::RequestMerchReorder { .. }
        | AutopilotActionPayload::RequestMerchBundle { .. }
        | AutopilotActionPayload::RequestContentArtifact { .. }
        | AutopilotActionPayload::RequestBeaconDiscovery { .. }
        | AutopilotActionPayload::RequestOutreachDiscovery { .. }
        | AutopilotActionPayload::RequestBookingTargetDiscovery { .. }
        | AutopilotActionPayload::RequestBeaconInviteBatch { .. }
        | AutopilotActionPayload::RequestBeaconOutreach { .. }
        | AutopilotActionPayload::AdjustExperiment { .. }
        | AutopilotActionPayload::CompleteShowTask { .. }
        | AutopilotActionPayload::EscalateShowTask { .. }
        | AutopilotActionPayload::ExecuteReleaseMilestone { .. }
        | AutopilotActionPayload::ApplyLiveOpportunity { .. }
        // A verification measures nothing; it decides whether anything may be
        // counted at all. Its result lands on the placement row.
        | AutopilotActionPayload::VerifyPlaylistPlacement { .. }
        // A reminder measures nothing either. Whether the pitch was submitted
        // is a thing only a human can report.
        | AutopilotActionPayload::EscalateEditorialPitch { .. }
        // A negotiation's effect is the booking, and the booking is measured
        // where it belongs: Phase 7's predicted cost against the settled one.
        // A seventy-two hour window after a counter measures nothing.
        | AutopilotActionPayload::CounterLiveOpportunityTerms { .. }
        | AutopilotActionPayload::AcceptLiveOpportunityTerms { .. }
        | AutopilotActionPayload::PrepareFundingPackage { .. }
        | AutopilotActionPayload::SubmitFundingApplication { .. }
        // A play's effect is the play's, not one send's: a tracker count moves
        // because a campaign ran, and attributing it to whichever message
        // happened to be last would be a number that reads as attribution and
        // is not. Phase 14 measures the play against its own pre-play baseline.
        | AutopilotActionPayload::RunPlayStep { .. }
        | AutopilotActionPayload::SendTeamAssignmentEmail { .. } => {}
    }

    for (kind, subject_id, baseline_value, due_at) in plans {
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
    }
    Ok(())
}
pub(super) async fn record_execution_outcome(
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
        AutopilotActionPayload::RequestBeaconDiscovery { .. } => ("beacon_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestOutreachDiscovery { .. } => ("outreach_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => ("booking_target_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestBeaconOutreach { .. } => ("beacon_outreach_requested", 1.0, None),
        AutopilotActionPayload::RequestBeaconInviteBatch { requested_count, .. } => {
            ("beacon_invite_batch_requested", f64::from(*requested_count), None)
        }
        AutopilotActionPayload::RaiseGrowthOpportunity {
            deviation_basis_points,
            ..
        } => (
            "growth_opportunity_raised",
            f64::from(*deviation_basis_points),
            None,
        ),
        AutopilotActionPayload::IssueReferralCode { .. } => ("referral_code_issued", 1.0, None),
        AutopilotActionPayload::RunPlayStep { step_index, .. } => {
            ("play_step_dispatched", f64::from(*step_index), None)
        }
        AutopilotActionPayload::RaiseGrowthDebt {
            overdue_basis_points,
            ..
        } => (
            "growth_debt_raised",
            f64::from(*overdue_basis_points),
            None,
        ),
        AutopilotActionPayload::RequestShowGrowth { .. } => ("show_growth_lever_requested", 1.0, None),
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
        AutopilotActionPayload::VerifyPlaylistPlacement { checkpoint, .. } => {
            ("playlist_placement_checked", f64::from(*checkpoint), None)
        }
        AutopilotActionPayload::EscalateEditorialPitch { .. } => {
            ("editorial_pitch_escalated", 1.0, None)
        }
        // The number worth recording is the money asked for or taken. A round
        // count would say how hard the agent pushed and nothing about whether
        // the push was worth making.
        AutopilotActionPayload::CounterLiveOpportunityTerms { ask_minor, .. } => {
            ("live_opportunity_counter_minor", *ask_minor as f64, None)
        }
        AutopilotActionPayload::AcceptLiveOpportunityTerms { fee_minor, .. } => {
            ("live_opportunity_accepted_minor", *fee_minor as f64, None)
        }
        AutopilotActionPayload::PrepareFundingPackage { .. } => ("funding_package_requested",1.0,None),
        AutopilotActionPayload::SubmitFundingApplication { .. } => ("funding_submission_requested",1.0,None),
        AutopilotActionPayload::SendTeamAssignmentEmail { .. } => {
            ("team_assignment_email_requested", 1.0, None)
        }
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

pub(super) const fn payload_requires_executor(payload: &AutopilotActionPayload) -> bool {
    match payload {
        AutopilotActionPayload::RequestShowGrowth { lever, .. } => !lever.is_first_party_campaign(),
        _ => matches!(
            payload,
            AutopilotActionPayload::RequestFanLifecycleMessage { .. }
                | AutopilotActionPayload::RequestMerchReorder { .. }
                | AutopilotActionPayload::RequestBookingOutreach { .. }
                | AutopilotActionPayload::RequestMerchBundle { .. }
                | AutopilotActionPayload::RequestOutreach { .. }
                | AutopilotActionPayload::RequestBeaconDiscovery { .. }
                | AutopilotActionPayload::RequestOutreachDiscovery { .. }
                | AutopilotActionPayload::RequestBeaconInviteBatch { .. }
                | AutopilotActionPayload::RequestBeaconOutreach { .. }
                | AutopilotActionPayload::RequestContentArtifact { .. }
                | AutopilotActionPayload::EscalateShowTask { .. }
                | AutopilotActionPayload::RequestPromotionBudgetChange { .. }
                | AutopilotActionPayload::ApplyLiveOpportunity { .. }
                | AutopilotActionPayload::VerifyPlaylistPlacement { .. }
                | AutopilotActionPayload::CounterLiveOpportunityTerms { .. }
                | AutopilotActionPayload::AcceptLiveOpportunityTerms { .. }
                | AutopilotActionPayload::PrepareFundingPackage { .. }
                | AutopilotActionPayload::SubmitFundingApplication { .. }
                | AutopilotActionPayload::RunPlayStep { .. }
                | AutopilotActionPayload::SendTeamAssignmentEmail { .. }
        ),
    }
}

/// The capability an action will need before it is claimed, so work behind a
/// gated executor can be parked instead of claimed, attempted and burned.
///
/// `None` means the action is executed entirely inside CrowdRelay and no
/// executor is involved. The strings here are the same ones
/// `executor_capability_for_event` derives at emission time; a contract test
/// keeps the two from drifting.
pub(in crate::autopilot) fn executor_capability_for_payload(
    payload: &AutopilotActionPayload,
) -> Option<&'static str> {
    if !payload_requires_executor(payload) {
        return None;
    }
    Some(match payload {
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => "fan.lifecycle.message",
        AutopilotActionPayload::RequestMerchReorder { .. } => "merch.reorder",
        AutopilotActionPayload::RequestBookingOutreach { .. } => "booking.outreach",
        AutopilotActionPayload::RequestMerchBundle { .. } => "merch.bundle",
        AutopilotActionPayload::RequestOutreach { .. } => "outreach.send",
        AutopilotActionPayload::RequestBeaconDiscovery { .. } => "beacon.discovery",
        AutopilotActionPayload::RequestOutreachDiscovery { .. } => "outreach.discovery",
        AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => "booking.discovery",
        AutopilotActionPayload::RequestBeaconOutreach { .. } => "beacon.outreach",
        AutopilotActionPayload::RequestBeaconInviteBatch { .. } => "beacon.invite_batch",
        AutopilotActionPayload::RequestShowGrowth { .. } => "show.growth",
        AutopilotActionPayload::RequestContentArtifact { .. } => "content.artifact",
        AutopilotActionPayload::EscalateShowTask { .. } => "show.escalation",
        AutopilotActionPayload::RequestPromotionBudgetChange { .. } => "promotion.budget",
        AutopilotActionPayload::ApplyLiveOpportunity { .. } => "opportunity.application",
        AutopilotActionPayload::VerifyPlaylistPlacement { .. } => "playlist.verify",
        AutopilotActionPayload::CounterLiveOpportunityTerms { .. } => "opportunity.terms",
        AutopilotActionPayload::AcceptLiveOpportunityTerms { .. } => "opportunity.terms",
        AutopilotActionPayload::PrepareFundingPackage { .. } => "funding.package",
        AutopilotActionPayload::SubmitFundingApplication { .. } => "funding.submit",
        AutopilotActionPayload::RunPlayStep { .. } => "play.step",
        AutopilotActionPayload::SendTeamAssignmentEmail { .. } => "team.email",
        // `payload_requires_executor` is the authority on which variants reach
        // this point; anything else executes without one.
        _ => return None,
    })
}

fn executor_capability_for_event(event_type: &str) -> &'static str {
    match event_type {
        "crowdrelay.fan_lifecycle.message_requested" => "fan.lifecycle.message",
        "crowdrelay.merch.reorder_requested" => "merch.reorder",
        "crowdrelay.booking.outreach_requested" => "booking.outreach",
        "crowdrelay.merch.bundle_requested" => "merch.bundle",
        "crowdrelay.outreach.requested" => "outreach.send",
        "crowdrelay.beacon.discovery_requested" => "beacon.discovery",
        "crowdrelay.outreach.discovery_requested" => "outreach.discovery",
        "crowdrelay.booking.target_discovery_requested" => "booking.discovery",
        "crowdrelay.beacon.outreach_requested" => "beacon.outreach",
        "crowdrelay.beacon.invite_batch_requested" => "beacon.invite_batch",
        "crowdrelay.beacon.release_delivery_confirmation_requested" => "beacon.release.mail",
        "crowdrelay.beacon.network_discovery_requested" => "beacon.network.discovery",
        "crowdrelay.beacon.invite_delivery_requested" => "beacon.network.invite",
        "crowdrelay.show_growth.requested" => "show.growth",
        "crowdrelay.content.artifact_requested" => "content.artifact",
        "crowdrelay.show.task_attention_required" => "show.escalation",
        "crowdrelay.ops.status_changed" => "ops.alert",
        "crowdrelay.promotion.budget_change_requested" => "promotion.budget",
        "crowdrelay.opportunity.application_requested" => "opportunity.application",
        // One capability for both moves. An executor that can write to a
        // promoter can write either message, and splitting them would let a
        // workspace advertise the ability to accept without the ability to
        // counter — which is the wrong half to have.
        "crowdrelay.playlist.placement_check_requested" => "playlist.verify",
        "crowdrelay.opportunity.terms_countered" => "opportunity.terms",
        "crowdrelay.opportunity.terms_accepted" => "opportunity.terms",
        "crowdrelay.funding.package_requested" => "funding.package",
        "crowdrelay.funding.submission_requested" => "funding.submit",
        "crowdrelay.calendar.upsert_requested" => "calendar.upsert",
        "crowdrelay.play.step_requested" => "play.step",
        "crowdrelay.team.assignment_email_requested" => "team.email",
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

/// Strict capability gate for new external features. Unlike the backwards-
/// compatible gate above, absence of the registry is unavailable: a task must
/// never be committed unless an active executor explicitly advertises it.
/// Whether a capability is currently advertised, with no logging and no error.
///
/// The strict version is right at the moment an action is about to need a
/// capability. It is wrong for a scheduled sweep that merely *might* need one:
/// a capability an operator has deliberately gated off is a steady state, not a
/// fault, and treating it as an error makes a healthy system report a failing
/// cycle every sixty seconds forever.
/// Whether any executor has registered at all. A workspace with no registry is
/// one where nothing has ever advertised anything, and gating there would park
/// every action forever; the same fail-open rule `ensure_executor_capability`
/// applies.
pub(in crate::autopilot) async fn executor_registry_is_active(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

/// Non-failing probe for callers that treat a missing executor as a soft
/// skip (best-effort notifications) instead of refusing the operation.
pub(in crate::autopilot) async fn executor_capability_available(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
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
    .map_err(map_sqlx)
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
