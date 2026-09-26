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
            let baseline =
                ticket_revenue_baseline_72h(transaction, workspace_id, ticket_type_id, now).await?;
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
        // One measurement per booking desk written to, same as a gig batch —
        // a reply (or a silence) belongs to its target, not to the letter.
        AutopilotActionPayload::RequestBookingOutreach {
            target_id,
            additional_recipients,
            ..
        } => {
            plans.push((
                AutopilotMeasurementKind::BookingReply7d,
                target_id.into_uuid(),
                0.0,
                now + time::Duration::days(7),
            ));
            for (extra_id, _) in additional_recipients {
                plans.push((
                    AutopilotMeasurementKind::BookingReply7d,
                    extra_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
        }
        // One measurement per promoter written to, under the same kind a
        // single booking approach uses. The question a reply answers is about
        // that promoter, not about the batch — and 4G.5 asks which kind of
        // evidence predicts a booking, which it cannot do if three promoters'
        // silence arrives as one row.
        AutopilotActionPayload::RequestGigOutreach { recipients, .. } => {
            for recipient in recipients {
                plans.push((
                    AutopilotMeasurementKind::BookingReply7d,
                    recipient.target_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
        }
        AutopilotActionPayload::RequestOutreach { target_id, .. }
        // An approach is measured the way a pitch is: did the contact write
        // back inside the week. The kind it is approached about lives on
        // the target, so the reply count needs no new measurement kind.
        // A reply measures the same counter on the same subject.
        | AutopilotActionPayload::RequestRepresentationApproach { target_id, .. }
        | AutopilotActionPayload::RequestOutreachReply { target_id, .. } => plans.push((
            AutopilotMeasurementKind::OutreachReply7d,
            target_id.into_uuid(),
            0.0,
            now + time::Duration::days(7),
        )),
        // An agent decides on a season's timescale — thirty days is where a
        // reply is still this application's answer, and the reply lives on
        // the agent's own interaction ledger, not the outreach targets'.
        // Approach and reply measure the same counter on the same subject:
        // did the conversation continue.
        AutopilotActionPayload::RequestBookingAgentApproach { agent_id, .. }
        | AutopilotActionPayload::RequestBookingAgentReply { agent_id, .. } => plans.push((
            AutopilotMeasurementKind::BookingAgentReply30d,
            agent_id.into_uuid(),
            0.0,
            now + time::Duration::days(30),
        )),
        // Each agent in the wave measures on their own ledger — a reply
        // answers that agent's ask, not the batch's.
        AutopilotActionPayload::RequestBookingAgentApproachWave { approaches, .. } => {
            for approach in approaches {
                plans.push(wave_reply_measurement(approach, now));
            }
        }
        AutopilotActionPayload::RequestAudienceCampaign { event_id, .. } => {
            let baseline =
                audience_ticket_revenue_baseline_72h(transaction, workspace_id, event_id, now)
                    .await?;
            plans.push((
                AutopilotMeasurementKind::AudienceTicketRevenue72h,
                event_id.into_uuid(),
                baseline,
                now + time::Duration::hours(72),
            ));
            schedule_attendance(
                transaction,
                workspace_id,
                event_id.into_uuid(),
                &mut plans,
            )
            .await?;
            // The send itself answers too: the emission row the executor just
            // wrote binds this action to the communication campaign, and the
            // delivery ledger will say whether the fans it reached bought a
            // ticket or withdrew consent. Audience campaigns carry no tracked
            // link, so the link-bound kinds stay unscheduled rather than
            // observing a funnel that was never instrumented.
            // `fetch_optional`: the emission insert is ON CONFLICT DO NOTHING
            // on (workspace, event, phase) — a row bound to an earlier action
            // survives a re-execution, and a miss must skip measurement, not
            // fail the whole dispatch transaction.
            let campaign_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT communication_campaign_id
                FROM campaign_lifecycle_emissions
                WHERE workspace_id=$1 AND action_id=$2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            if let Some(campaign_id) = campaign_id {
                for (kind, days) in [
                    (AutopilotMeasurementKind::CampaignTicketConversion14d, 14),
                    (AutopilotMeasurementKind::CampaignUnsubscribe7d, 7),
                ] {
                    plans.push((kind, campaign_id, 0.0, now + time::Duration::days(days)));
                }
            }
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
        AutopilotActionPayload::RaiseDeclineAdvisory { .. } => {
            // Whether parking the room moved anything is a question the
            // provenance ledger answers on its own — conversions either
            // start appearing from other rooms or they do not. Scheduling
            // a fan metric against the approval date would attribute a
            // number to a non-action.
        }
        AutopilotActionPayload::RaiseContentSuggestion { .. }
        | AutopilotActionPayload::RaiseContentArc { .. } => {
            // No plan here either, for the strongest reason of the three:
            // approving a suggestion publishes nothing — the band commits to
            // making the beat. Measuring fan growth from the approval date
            // would attribute a number to a decision, not to content. The
            // suggestion's own outcome ledger is the receipt: when the band
            // reports done or done-differently, `results` carries the
            // measured reach and the learning loop reads it there.
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
                    FROM show_growth_surfaces
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

            schedule_attendance(
                transaction,
                workspace_id,
                event_id.into_uuid(),
                &mut plans,
            )
            .await?;
        }
        // A milestone that talks to an audience answers for what the audience
        // did next. Calendar seeding and the parked editorial pitch reach
        // nobody, so they schedule nothing; every outward rung gets the
        // release's own three counters — acquisitions bound to its campaign,
        // clicks on the tracked link the executor guaranteed exists, and the
        // release-acquired fans who converted to a paid order. The channel
        // lift joins only when a series on the release exists to read; a
        // release with no declared series measures its audience effects and
        // records no channel claim at all.
        AutopilotActionPayload::ExecuteReleaseMilestone {
            release_id,
            milestone,
            ..
        } => {
            use crowdrelay_domain::release_autopilot::ReleaseMilestone;

            if !matches!(
                milestone,
                ReleaseMilestone::SeedCalendar | ReleaseMilestone::EditorialPitch
            ) {
                for kind in [
                    AutopilotMeasurementKind::ReleaseBoundAcquisition14d,
                    AutopilotMeasurementKind::ReleaseLinkClicks14d,
                    AutopilotMeasurementKind::ReleaseFanConversion14d,
                ] {
                    plans.push((
                        kind,
                        release_id.into_uuid(),
                        0.0,
                        now + time::Duration::days(14),
                    ));
                }
                // The send itself answers separately from the release it
                // served: the communication campaign the executor just
                // created carries its own conversion and unsubscribe
                // outcomes. `fetch_optional` — an outward milestone that
                // sends no email (StartPress seeds outreach rows only) has
                // no campaign row, and a miss must skip measurement, not
                // fail the whole dispatch transaction.
                let campaign = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    SELECT id FROM communication_campaigns
                    WHERE workspace_id=$1 AND slug=$2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(format!(
                    "crowdrelay-release-{release_id}-{}",
                    operations::release_milestone_str(*milestone)
                ))
                .fetch_optional(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                if let Some(campaign_id) = campaign {
                    for (kind, days) in [
                        (AutopilotMeasurementKind::CampaignTicketConversion14d, 14),
                        (AutopilotMeasurementKind::CampaignUnsubscribe7d, 7),
                    ] {
                        plans.push((
                            kind,
                            campaign_id,
                            0.0,
                            now + time::Duration::days(days),
                        ));
                    }
                }
                let series_declared = sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS(
                        SELECT 1 FROM growth_metric_series
                        WHERE workspace_id=$1 AND subject_kind='release_plan'
                          AND subject_id=$2 AND active
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(release_id.into_uuid())
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                if series_declared {
                    plans.push((
                        AutopilotMeasurementKind::ReleaseChannelLift14d,
                        release_id.into_uuid(),
                        0.0,
                        now + time::Duration::days(14),
                    ));
                }
            }
        }
        // A produced artifact is content the brain asked for and an audience
        // now sees — the request half of the loop closed at the receipt, and
        // the answer half is whether fans moved afterward. It sat in the
        // do-nothing arm below: a shipped video left no measurement behind,
        // so `suggestion_outcomes.results` carried operator-reported reach
        // that nothing reads back into a belief.
        //
        // The window anchors at the executor-confirmed production time — the
        // receipt's occurred_at — which is the same convention every other
        // executor-backed action uses: producing the artifact and putting it
        // in front of people is the executor's one job. Anchoring at request
        // time would open the observation window before anyone could have
        // seen the thing. The subject is the content source the artifact was
        // made from, which is also the entity read models can point at.
        AutopilotActionPayload::RequestContentArtifact { source_id, .. } => {
            let (pre_action_daily_rate, pre_action_durable_daily_rate) =
                fan_growth_baselines(transaction, workspace_id, action_id, now).await?;
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                source_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                source_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(3),
            ));
            plans.push((
                AutopilotMeasurementKind::DurableFanGrowth30d,
                source_id.into_uuid(),
                pre_action_durable_daily_rate,
                now + time::Duration::days(44),
            ));
            // Did the produced artifact reach an audience — posts filed
            // against this source inside the week after production. The
            // request was executor-gated, so this measurement only exists
            // once the receipt confirmed the artifact exists; its zero then
            // means produced-and-never-posted, which is the real verdict.
            plans.push((
                AutopilotMeasurementKind::ArtifactOutcome7d,
                source_id.into_uuid(),
                0.0,
                now + time::Duration::days(7),
            ));
        }
        AutopilotActionPayload::ChangeTicketCapacity { .. }
        | AutopilotActionPayload::RequestMerchReorder { .. }
        | AutopilotActionPayload::RequestMerchBundle { .. }
        | AutopilotActionPayload::RequestBeaconDiscovery { .. }
        | AutopilotActionPayload::RequestOutreachDiscovery { .. }
        | AutopilotActionPayload::RequestBookingTargetDiscovery { .. }
        | AutopilotActionPayload::RequestBeaconInviteBatch { .. }
        | AutopilotActionPayload::RequestBeaconOutreach { .. }
        | AutopilotActionPayload::AdjustExperiment { .. }
        | AutopilotActionPayload::CompleteShowTask { .. }
        | AutopilotActionPayload::EscalateShowTask { .. }
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
        // The report's effect is the negotiation it unblocks, measured where
        // the negotiation is measured — not a send receipt for an email.
        | AutopilotActionPayload::IssueCounterpartyReport { .. }
        | AutopilotActionPayload::PrepareFundingPackage { .. }
        | AutopilotActionPayload::SubmitFundingApplication { .. }
        // An invitation is measured by whether the person joined, not by
        // whether they replied — most will simply click or not. That evidence
        // arrives as a fan row against an address the band already knew, which
        // no existing measurement kind describes, so nothing is planned here
        // rather than a reply window this letter never asks for.
        | AutopilotActionPayload::RequestLatarnikInvite { .. }
        // A play's effect is the play's, not one send's: a tracker count moves
        // because a campaign ran, and attributing it to whichever message
        // happened to be last would be a number that reads as attribution and
        // is not. Phase 14 measures the play against its own pre-play baseline.
        | AutopilotActionPayload::RunPlayStep { .. }
        // The fix's effect is the listing it completes — measured at the play
        // level with the sweep that found the gap, not as a send to anybody.
        | AutopilotActionPayload::SetEventTicketUrl { .. }
        | AutopilotActionPayload::SendTeamAssignmentEmail { .. }
        | AutopilotActionPayload::RequestOutreachTarget { .. } => {}
        // A fan lifecycle message (welcome, re-engagement, referral invite)
        // is externally dispatched and requires a terminal executor receipt.
        // Its measurement is scheduled when that receipt arrives, via
        // `apply_success_side_effects` → `schedule_effect_measurement`.
        //
        // The measurement counts per-fan engagement events (ticket orders,
        // Signal push endpoint creations, referral redemptions) in the 7-day
        // window after the message was confirmed delivered. The baseline is 0
        // — lifecycle messages target new or dormant fans who haven't
        // engaged yet. This closes the learning loop: the brain learns which
        // message templates actually move individual fans to action.
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => {
            // Re-bind fan_id from the reference — the { .. } pattern keeps
            // the contract test happy while still extracting the subject.
            if let AutopilotActionPayload::RequestFanLifecycleMessage { fan_id, .. } = payload {
                plans.push((
                    AutopilotMeasurementKind::FanLifecycleEngagement7d,
                    fan_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
        }
        // A Signal push exists to put the app in someone's hand, so measure
        // exactly that: installs in the week after it went out.
        //
        // This sat in the bundled do-nothing arm above with 25 other variants,
        // which is how production reached 108 succeeded actions and 9 measured
        // ones. The metric and its observer already existed — only the
        // scheduling was missing, so the brain kept pushing and never learned
        // whether any of it worked.
        //
        // Baseline is the pre-period install rate: new endpoints created in
        // the 7 days *before* the push. The observer counts new endpoints in
        // the 7 days *after*. The effect is then a pre/post comparison —
        // did the push accelerate installs beyond the baseline rate?
        AutopilotActionPayload::RequestSignalPush { .. } => {
            let baseline_installs = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COUNT(*)::double precision
                FROM fan_push_endpoints
                WHERE workspace_id = $1
                  AND active = true
                  AND invalidated_at IS NULL
                  AND created_at >= $2 - INTERVAL '7 days'
                  AND created_at < $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            plans.push((
                AutopilotMeasurementKind::AgentRunSignalInstalls7d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(7),
            ));
            // Fast checkpoints: 1h outcome quality + 1d signal installs.
            plans.push((
                AutopilotMeasurementKind::AgentRunOutcomeQuality1h,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::hours(1),
            ));
            plans.push((
                AutopilotMeasurementKind::SignalInstalls1d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(1),
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
            // Fast feedback: 1-hour outcome quality checkpoint for EVERY
            // agent dispatch. The brain learns whether the worker produced
            // a valid, grounded, processed outcome within an hour — not days.
            // This is the fastest possible learning signal: did the worker
            // do its job at all? Binary: 1 if processed, 0 if rejected or no
            // outcome. The baseline is 0 because no outcome existed before.
            plans.push((
                AutopilotMeasurementKind::AgentRunOutcomeQuality1h,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::hours(1),
            ));
            if !is_scanner && !is_strategist {
                // Direct-action workers: measure fan growth (the existing
                // path). These workers (community-engager, social-post,
                // signal-inviter, press-pitch) can directly acquire fans.
                // Baseline is the pre-period fan arrival rate: new fans in
                // the 14 days *before* the dispatch. The observer counts new
                // fans in the 14 days *after*. The effect is then a pre/post
                // comparison — did the worker accelerate fan growth beyond
                // the baseline rate?
                let baseline_fans = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision FROM fans
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
                    AutopilotMeasurementKind::AgentRunFanGrowth14d,
                    action_id.into_uuid(),
                    baseline_fans,
                    now + time::Duration::days(14),
                ));
                // Early 3-day checkpoint — the fastest feedback signal for
                // the learning loop. The brain can learn from this partial
                // observation while waiting for the full 14-day and 30-day
                // measurements. Mirrors Kern's 1-day checkpoint settling.
                //
                // The baseline must match the observation window length: a
                // 3-day post-period needs a 3-day pre-period baseline, not
                // the 14-day baseline used by the 14-day measurement. Using
                // the 14-day count here would always produce a large negative
                // delta (3 days of arrivals vs 14 days of arrivals) and
                // corrupt the early learning signal.
                let baseline_fans_3d = sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision FROM fans
                    WHERE workspace_id = $1
                      AND created_at >= $2 - INTERVAL '3 days'
                      AND created_at < $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?;
                plans.push((
                    AutopilotMeasurementKind::AgentRunFanGrowth3d,
                    action_id.into_uuid(),
                    baseline_fans_3d,
                    now + time::Duration::days(3),
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
            // The baseline counts fans the same way Y14 counts them:
            // everything that arrived and was not suppressed. Counting all
            // fans regardless of status would compare an outcome that
            // excludes suppressed arrivals against a counterfactual that
            // includes them.
            // When the experimental unit is a community, the outcome will be
            // read from that community's own conversion ledger. A workspace
            // arrival rate is then the wrong counterfactual by both population
            // and unit: it subtracts everything the whole workspace acquired
            // from what one subreddit sent, which makes a community that
            // performed exactly at its own baseline look like it destroyed a
            // fortnight of growth. Same ledger, same width, same community.
            let (pre_action_daily_rate, pre_action_durable_daily_rate) =
                fan_growth_baselines(transaction, workspace_id, action_id, now).await?;
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(14),
            ));
            // The same counterfactual, eleven days earlier. Shares the
            // pre-action daily rate: the baseline is a rate, and the window it
            // is multiplied by lives on the kind, so one reading serves both
            // widths and the two cannot disagree about what the world looked
            // like before the dispatch.
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(3),
            ));
            // Y30 durable fan growth (North Star): fans created in the
            // 14-day post-action window that are still active 30 days
            // after creation. The measurement window is 44 days (14-day
            // observation + 30-day durability check).
            //
            // Y30 needs its own baseline, and this is the whole point of the
            // query below. It used to reuse `pre_action_daily_rate`, which
            // counts every arrival — while the Y30 outcome counts only the
            // arrivals that were still active thirty days later. Subtracting
            // a counterfactual built from all arrivals from an outcome built
            // from durable ones biases Y30 negative by exactly the churn
            // rate: a workspace where ten fans arrive per fortnight and six
            // stick would measure roughly -4 durable fans for an action that
            // performed precisely at baseline. The brain would have learned
            // that every action it takes is harmful.
            //
            // The matched comparison has to be aged, not just filtered: fans
            // from the last fourteen days cannot have a thirty-day durability
            // outcome yet. So the baseline window is the fourteen days ending
            // thirty days ago — same width, old enough to have been observed.
            plans.push((
                AutopilotMeasurementKind::DurableFanGrowth30d,
                action_id.into_uuid(),
                pre_action_durable_daily_rate,
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
            // Fast checkpoint: 1-day signal installs. The brain gets
            // next-cycle feedback on whether the worker moved fans toward
            // Signal within 24 hours, not a week. Same baseline as the 7d
            // measurement — the pre-action total install count.
            plans.push((
                AutopilotMeasurementKind::SignalInstalls1d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(1),
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
                // Fast checkpoint: 1-hour proximal outcome. The scanner
                // discovers targets and the strategist produces insights
                // within minutes — waiting 14 days for the proximal count
                // is absurd. The 14-day measurement stays for downstream
                // engagement, but this gives the brain next-cycle feedback
                // on worker quality.
                let fast_kind = if is_scanner {
                    AutopilotMeasurementKind::ScannerDiscoveryQuality1h
                } else {
                    AutopilotMeasurementKind::StrategistInsightQuality1h
                };
                plans.push((
                    fast_kind,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::hours(1),
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
            // North Star: the post exists to grow fans, so the action carries
            // the same incremental counterfactual every publishing action
            // does — community-scoped when the experiment unit is one, the
            // workspace rate otherwise. The window anchor is dispatch time
            // for now; `anchor_measurements_to_publication` re-anchors to
            // `community_posts.posted_at` when the post actually lands, and
            // `measures_outbound_reach` abandons the measurement entirely if
            // it never does — the absence of an outcome is not an outcome
            // of zero.
            let community =
                measurement_unit_community(transaction, workspace_id, action_id).await?;
            let pre_action_daily_rate = if let Some(handle) = &community {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(DISTINCT fan_id)::double precision / 14.0
                    FROM fan_provenance_events
                    WHERE workspace_id = $1
                      AND community = $3
                      AND event_kind = 'conversion'
                      AND fan_id IS NOT NULL
                      AND occurred_at >= $2::timestamptz - INTERVAL '14 days'
                      AND occurred_at < $2::timestamptz
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .bind(handle)
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision / 14.0 FROM fans
                    WHERE workspace_id = $1
                      AND created_at >= $2::timestamptz - INTERVAL '14 days'
                      AND created_at < $2::timestamptz
                      AND status != 'suppressed'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .fetch_one(&mut **transaction)
                .await
                .map_err(map_sqlx)?
            };
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(3),
            ));
            // No AgentRunOutcomeQuality1h here: that measure links through
            // `agent_service_tasks.metadata->>'action_id'`, and community
            // engagement creates no task row — the community_posts row is
            // its receipt. A measure that can only ever read 0 would teach
            // the brain the intervention failed when nothing failed at all.
        }
        // Agent content (social/telegram/discord posts): measure whether the
        // published content actually grew fans. The baseline is the pre-action
        // daily fan arrival rate (same counterfactual as RequestAgentRun's
        // IncrementalFanGrowth14d). The content action materializes the draft
        // and triggers the post; the measurement closes the learning loop by
        // observing whether fans actually arrived in the 14-day window after.
        //
        // The executors (social_post_executor, telegram_executor,
        // discord_executor) are internal workers that poll for `succeeded`
        // actions and create post table rows. They don't file execution
        // reports — the post tables are their receipts. This measurement is
        // scheduled at dispatch (first-party path) so the brain learns even
        // when the post is still awaiting manual publication. When the
        // operator registers the manual post URL, the measurement window is
        // re-anchored to the actual publication time.
        AutopilotActionPayload::RequestAgentContent { draft, .. } => {
            let pre_action_daily_rate = sqlx::query_scalar::<_, f64>(
                r#"
                SELECT COUNT(*)::double precision / 14.0 FROM fans
                WHERE workspace_id = $1
                  AND created_at >= $2::timestamptz - INTERVAL '14 days'
                  AND created_at < $2::timestamptz
                  AND status != 'suppressed'
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
            // The same counterfactual, eleven days earlier. Shares the
            // pre-action daily rate: the baseline is a rate, and the window it
            // is multiplied by lives on the kind, so one reading serves both
            // widths and the two cannot disagree about what the world looked
            // like before the dispatch.
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                pre_action_daily_rate,
                now + time::Duration::days(3),
            ));
            // What the post's own tracked link did — clicks on the `/l/`
            // redirect the social executor mints for the draft's `cta_url`,
            // joined through `social_posts.smart_link_id`. Scheduled only when
            // the draft names a link for a platform the social executor
            // claims; a draft with no link has nothing to attribute, and a
            // telegram/discord draft's links ride untracked in its text, so
            // scheduling there would produce a structural zero the learner
            // would read as the content failing.
            let linkable_platform = draft
                .get("platform")
                .and_then(|p| p.as_str())
                .is_some_and(|p| matches!(p, "instagram" | "facebook" | "x"));
            let has_cta = draft
                .get("cta_url")
                .and_then(|u| u.as_str())
                .is_some_and(|u| !u.trim().is_empty());
            if linkable_platform && has_cta {
                plans.push((
                    AutopilotMeasurementKind::ContentLinkClicks7d,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
            // No AgentRunOutcomeQuality1h here either: the drafting task's
            // outcome links to the action that requested the draft, not to
            // this content action — the join can never match.
        }
        // The weekly join-ask (§5): clicks on the tracked link the social
        // executor mints for `cta_url`, seven days out. No fan-growth
        // counterfactual — the ask targets existing followers, so a
        // workspace-level fan delta would read archive-import windfalls as
        // the post's doing (the mismeasurement F1's briefing split prevents).
        // Signups surface on the briefing line instead of a measurement row.
        AutopilotActionPayload::PublishJoinAsk { platform, .. } => {
            if matches!(platform.as_str(), "instagram" | "facebook" | "x" | "telegram") {
                plans.push((
                    AutopilotMeasurementKind::ContentLinkClicks7d,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
        }
    }

    for (kind, subject_id, baseline_value, due_at) in plans {
        if !baseline_value.is_finite() || baseline_value < 0.0 {
            return Err(RepositoryError::Unexpected);
        }
        sqlx::query(
            r#"
            INSERT INTO autopilot_measurements (
                id, workspace_id, action_id, measurement_kind, subject_id,
                action_finished_at, baseline_value, due_at, available_at,
                trace_id
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,
                (SELECT trace_id FROM autopilot_actions WHERE id = $3)
            )
            ON CONFLICT (workspace_id, action_id, measurement_kind, subject_id) DO NOTHING
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

