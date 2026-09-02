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
        | AutopilotActionPayload::SendTeamAssignmentEmail { .. }
        | AutopilotActionPayload::RequestAgentContent { .. } => {}
        // A Signal push exists to put the app in someone's hand, so measure
        // exactly that: installs in the week after it went out.
        //
        // This sat in the bundled do-nothing arm above with 25 other variants,
        // which is how production reached 108 succeeded actions and 9 measured
        // ones. The metric and its observer already existed — only the
        // scheduling was missing, so the brain kept pushing and never learned
        // whether any of it worked.
        //
        // Baseline is the current install count, matching how an agent-run
        // dispatch schedules the same kind: the observer counts endpoints
        // created inside the window, so the delta is what moved.
        AutopilotActionPayload::RequestSignalPush { .. } => {
            let baseline_installs = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COUNT(*)::double precision
                FROM fan_push_endpoints
                WHERE workspace_id = $1 AND invalidated_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            plans.push((
                AutopilotMeasurementKind::AgentRunSignalInstalls7d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(7),
            ));
        }
        // Agent dispatches: measure whether the worker's intelligence
        // gathering actually grew fans. The baseline is the fan count at
        // dispatch time; the observation counts new fans in the 14-day
        // window after the dispatch. This closes the learning loop: the
        // brain can retire workers that consistently produce no growth and
        // shorten the cadence of workers that do.
        AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
            // Reward alignment: measure each worker on its proximal outcome,
            // not workspace-wide fan growth. The scanner discovers
            // communities, the strategist produces insights — neither
            // acquires fans directly. Measuring them on fan growth would
            // credit them for fans acquired by other workers (credit
            // leakage + polluted posteriors).
            let is_scanner = template_id == "reddit-scanner"
                || template_id == "telegram-scanner"
                || template_id == "metal-archives-scanner"
                || template_id == "bandcamp-scanner";
            let is_strategist = template_id == "growth-strategist";
            if !is_scanner && !is_strategist {
                // Direct-action workers: measure fan growth (the existing
                // path). These workers (community-engager, social-post,
                // signal-inviter, press-pitch) can directly acquire fans.
                let baseline_fans = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision FROM fans
                    WHERE workspace_id = $1
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                plans.push((
                    AutopilotMeasurementKind::AgentRunFanGrowth14d,
                    action_id.into_uuid(),
                    baseline_fans,
                    now + time::Duration::days(14),
                ));
            // North Star: incremental fan growth with a difference-in-
            // differences (DiD) counterfactual. The baseline is the pre-
            // action daily fan arrival rate computed from a matched 14-day
            // window (the same length as the observation window). This is
            // a quasi-experimental counterfactual: the 14-day pre-period
            // is the "control" and the 14-day post-period is the
            // "treatment". The DiD estimate is:
            //
            //   τ = (fans_post - fans_pre) = observed - (rate × 14)
            //
            // Using a matched 14-day window (instead of the previous 30-day
            // average) makes the counterfactual more robust to time-varying
            // trends: if fan growth was already declining before the action,
            // the 30-day average would overstate the counterfactual and
            // understate the treatment effect. The 14-day matched window
            // captures the most recent trend.
            //
            // The evidence quality is `Observational` — this is a
            // quasi-experimental estimate, not a randomized experiment.
            // The treatment-effect posterior weights it accordingly.
            let pre_action_daily_rate = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COUNT(*)::double precision / 14.0 FROM fans
                WHERE workspace_id = $1
                  AND created_at >= $2 - INTERVAL '14 days'
                  AND created_at < $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(14),
            ));
            // Y30 durable fan growth (North Star): fans created in the
            // 14-day post-action window that are still active 30 days
            // after creation. The measurement window is 44 days (14-day
            // observation + 30-day durability check). The baseline is
            // the same pre-action daily rate, so Y30 is incremental —
            // not a raw count. Previously this measurement kind existed
            // but was never scheduled, so the Y30 treatment-effect
            // posterior was never updated (conf_y30 was always 0).
            plans.push((
                AutopilotMeasurementKind::DurableFanGrowth30d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(44),
            ));
            let baseline_installs = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COUNT(*)::double precision
                FROM fan_push_endpoints
                WHERE workspace_id = $1 AND invalidated_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            plans.push((
                AutopilotMeasurementKind::AgentRunSignalInstalls7d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(7),
            ));
            } else {
                // Scanner/strategist: measure proximal outcome, not fan
                // growth. The scanner discovers communities, the
                // strategist produces insights — neither acquires fans.
                let kind = if is_scanner {
                    AutopilotMeasurementKind::ScannerDiscoveryQuality14d
                } else {
                    AutopilotMeasurementKind::StrategistInsightQuality14d
                };
                plans.push((
                    kind,
                    action_id.into_uuid(),
                    0.0, // baseline: no targets/insights existed before
                    now + time::Duration::days(14),
                ));
            }
        }
        // Community engagement: measure whether the posts produced
        // meaningful engagement (upvotes, comments) rather than just
        // existing. The baseline is zero — the posts didn't exist before.
        AutopilotActionPayload::RequestCommunityEngagement { target_id, .. } => {
            plans.push((
                AutopilotMeasurementKind::AgentRunCommunityEngagement7d,
                *target_id,
                0.0,
                now + time::Duration::days(7),
            ));
        }
    }

    for (kind, subject_id, baseline_value, due_at) in plans {
        if !baseline_value.is_finite() || baseline_value < 0.0 {
            return Err(RepositoryError::Unexpected);
        }
        sqlx::query(
            r#"
            INSERT INTO viryaos_autopilot_measurements (
                id, workspace_id, action_id, measurement_kind, subject_id,
                action_finished_at, baseline_value, due_at, available_at,
                trace_id
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,
                (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $3)
            )
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
        AutopilotActionPayload::RequestAgentContent { .. } => {
            ("agent_content_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestAgentRun { .. } => {
            ("agent_run_dispatched", 1.0, None)
        }
        AutopilotActionPayload::RequestCommunityEngagement { .. } => {
            ("community_engagement_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestSignalPush { .. } => {
            ("signal_push_requested", 1.0, None)
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

/// Whether this payload is executed by an external executor that must file
/// a terminal execution receipt. Public for the worker's receipt
/// reconciliation sweep, which flags dispatched actions whose receipts
/// never arrived.
pub const fn payload_requires_executor(payload: &AutopilotActionPayload) -> bool {
    match payload {
        // CanonicalLinkSetup is a pure first-party DB write (smart_links), so
        // it must not be gated behind an executor capability. is_first_party
        // covers both communication campaigns and the canonical-link write.
        AutopilotActionPayload::RequestShowGrowth { lever, .. } => !lever.is_first_party(),
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
                | AutopilotActionPayload::RequestAgentContent { .. }
                | AutopilotActionPayload::RequestCommunityEngagement { .. }
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
        AutopilotActionPayload::RequestAgentContent { .. } => "agent.content",
        AutopilotActionPayload::RequestCommunityEngagement { .. } => "community.engage",
        // `payload_requires_executor` is the authority on which variants reach
        // this point; anything else executes without one.
        _ => return None,
    })
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
    // Propagate trace context (trace_id, causation_id) from the autopilot
    // action onto the outbox event so the trace spine stays continuous from
    // decision → action → outbox delivery. The action_id is already a bind
    // parameter; we join to fetch its trace columns inside the same CTE so
    // the outbox insert and the emission insert commit atomically.
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH action_trace AS (
            SELECT trace_id, causation_id
            FROM viryaos_autopilot_actions
            WHERE id = $2
        ), emission AS (
            INSERT INTO viryaos_autopilot_action_emissions (
                workspace_id, action_id, emission_key, outbox_event_id
            ) VALUES ($1,$2,$3,$4)
            ON CONFLICT (workspace_id, emission_key) DO NOTHING
            RETURNING outbox_event_id
        ), outbox AS (
            INSERT INTO outbox_events (
                id, workspace_id, event_type, event_version, payload,
                request_id, max_attempts, trace_id, causation_id, action_id
            )
            SELECT $4,$1,$5,$6,$7,$3,12,at.trace_id,at.causation_id,$2
            FROM emission CROSS JOIN action_trace at
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
