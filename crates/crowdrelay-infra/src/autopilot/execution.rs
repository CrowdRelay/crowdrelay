use crowdrelay_domain::worker_template::{TemplateAudience, WorkerTemplate};


/// Whether a social content draft has an honest tracked-click rail.
///
/// Social-post outcomes on Facebook/X no longer depend on the model remembering
/// a CTA: the social executor deterministically binds an owned-site fallback.
/// Instagram still needs an explicit CTA to preserve the old contract; without
/// one, a caption URL is not treated as a clickable acquisition surface.
fn social_content_funnel_planned(
    template_id: Option<&str>,
    draft: &serde_json::Value,
) -> bool {
    let platform = draft.get("platform").and_then(serde_json::Value::as_str);
    let has_cta = draft
        .get("cta_url")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|url| !url.trim().is_empty());
    match platform {
        Some("facebook" | "x") if template_id == Some("social-post") => true,
        Some("instagram" | "facebook" | "x") => has_cta,
        _ => false,
    }
}

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
        AutopilotActionPayload::RequestSourceCampaign { .. } => {
            // The drop email's two honest questions: did this drop's fan-out
            // grow fans, and did the email cost subscribers? Growth is keyed
            // to the action — the attributed-fans read follows the action's
            // trace lineage, so the sibling lanes' tracked posts count for
            // it the same way they count for a channel draft (the known
            // cross-template double count that comment in
            // `attributed_fans.rs` already owns). The unsubscribe measure
            // binds to the campaign row itself, resolved through the
            // dispatch event execution just wrote.
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(3),
            ));
            let campaign_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT campaign.id
                FROM communication_campaigns AS campaign
                JOIN outbox_events AS dispatch
                  ON dispatch.workspace_id = campaign.workspace_id
                 AND dispatch.id = campaign.dispatch_event_id
                WHERE campaign.workspace_id = $1
                  AND dispatch.action_id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            if let Some(campaign_id) = campaign_id {
                plans.push((
                    AutopilotMeasurementKind::CampaignUnsubscribe7d,
                    campaign_id,
                    0.0,
                    now + time::Duration::days(7),
                ));
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
        // Named Beacon outreach is the relationship lane's real experiment.
        // Schedule only after its executor-success receipt, so a delivered
        // message with no response becomes valid negative evidence while a
        // send that never left never teaches the Brain that the relationship
        // failed. Link outcomes are action-owned and therefore cannot be
        // borrowed from another Beacon or another show.
        AutopilotActionPayload::RequestBeaconOutreach { beacon_id, .. } => {
            plans.push((
                AutopilotMeasurementKind::BeaconOutreachReplyQuality14d,
                beacon_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            let has_tracked_cta = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM smart_links
                    WHERE workspace_id=$1 AND action_id=$2 AND active
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            if has_tracked_cta {
                plans.push((
                    AutopilotMeasurementKind::BeaconOutreachUniqueVisitors14d,
                    beacon_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(14),
                ));
                // Fan value uses the canonical North-Star learner rather than
                // a Beacon-only counter. The action-owned smart link makes
                // attribution exact, and the generic fan observer feeds the
                // same Y3/Y14/Y30 posteriors every other acquisition lane uses.
                plans.push((
                    AutopilotMeasurementKind::IncrementalFanGrowth3d,
                    beacon_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(3),
                ));
                plans.push((
                    AutopilotMeasurementKind::IncrementalFanGrowth14d,
                    beacon_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(14),
                ));
                plans.push((
                    AutopilotMeasurementKind::DurableFanGrowth30d,
                    beacon_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(44),
                ));
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
            // The fan kinds count fans traced to the action; their
            // counterfactual is zero by construction, so no pre-period rate
            // is read (`counts_attributed_fans`).
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                source_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                source_id.into_uuid(),
                0.0,
                now + time::Duration::days(3),
            ));
            plans.push((
                AutopilotMeasurementKind::DurableFanGrowth30d,
                source_id.into_uuid(),
                0.0,
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
        | AutopilotActionPayload::RequestOutreachTarget { .. }
        // The wave's effect is confirmations landing as `fan.confirmed`
        // events — the staged → pending → confirmed → engaged ladder is the
        // readout. Scheduling a metric against the approval would attribute
        // fan joins to a click, which is the inflation the measurement
        // layer exists to refuse.
        | AutopilotActionPayload::RunArchivePromoteWave { .. } => {}
        // A fan lifecycle message (welcome, re-engagement, referral invite)
        // is externally dispatched and requires a terminal executor receipt.
        // Its measurement is scheduled when that receipt arrives, via
        // `apply_success_side_effects` → `schedule_effect_measurement`.
        //
        // Welcome v2 observes binary deliberate activation after the success
        // receipt. Legacy templates keep their event-count metric unchanged.
        // A provider receipt does not prove inbox delivery or causal lift.
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => {
            // Re-bind fan_id from the reference — the { .. } pattern keeps
            // the contract test happy while still extracting the subject.
            if let AutopilotActionPayload::RequestFanLifecycleMessage {
                fan_id, template_key, ..
            } = payload {
                let kind = if template_key == WELCOME_V2_TEMPLATE {
                    AutopilotMeasurementKind::FanLifecycleActivation7d
                } else {
                    AutopilotMeasurementKind::FanLifecycleEngagement7d
                };
                plans.push((
                    kind,
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
            let baseline_installs =
                pre_action_signal_installs(transaction, workspace_id, now, 7).await?;
            let baseline_installs_1d =
                pre_action_signal_installs(transaction, workspace_id, now, 1).await?;
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
                baseline_installs_1d,
                now + time::Duration::days(1),
            ));
        }
        // Agent dispatches: measure whether the worker's intelligence
        // gathering actually grew fans. The baseline is the non-suppressed
        // fans that arrived in the matched window before dispatch; the
        // observation counts the same in the window after it. This closes the learning loop: the
        // brain can retire workers that consistently produce no growth and
        // shorten the cadence of workers that do.
        AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
            // Reward alignment: measure each worker on its proximal outcome,
            // not workspace-wide fan growth. The scanner discovers
            // communities, the strategist produces insights — neither
            // acquires fans directly. Measuring them on fan growth would
            // credit them for fans acquired by other workers (credit
            // leakage + polluted posteriors).
            // Reach classification already exists in WorkerTemplate; use it
            // instead of maintaining another partial list here. The old list
            // forgot fanbase-scout and strategy-consult, so two intelligence
            // workers were measured as if they directly acquired fans.
            let known_template = WorkerTemplate::parse(template_id);
            let is_relationship_research = template_id == "contact-researcher";
            let is_intelligence = is_relationship_research
                || known_template.is_some_and(|template| {
                    template.audience() == TemplateAudience::Intelligence
                });
            let is_discovery_intelligence = known_template.is_some_and(|template| {
                matches!(
                    template,
                    WorkerTemplate::RedditScanner
                        | WorkerTemplate::TelegramScanner
                        | WorkerTemplate::MetalArchivesScanner
                        | WorkerTemplate::BandcampScanner
                        | WorkerTemplate::FanbaseScout
                )
            });
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
            if !is_intelligence {
                // Direct-action workers (community-engager, social-post,
                // signal-inviter, press-pitch) can acquire fans directly, so
                // they are measured on the fans traced to them — 14 days and
                // an early 3-day checkpoint. No baseline: a traced count's
                // counterfactual is zero (`counts_attributed_fans`).
                plans.push((
                    AutopilotMeasurementKind::AgentRunFanGrowth14d,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(14),
                ));
                plans.push((
                    AutopilotMeasurementKind::AgentRunFanGrowth3d,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(3),
                ));
            // North Star: the fans traced to this dispatch at 14 days, an
            // early 3-day read, and at 44 days those still active 30 days
            // after arriving. It used to be a difference-in-differences
            // against the workspace's pre-period arrival rate, which credited
            // every dispatch with every arrival; see `attributed_fans`.
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(3),
            ));
            plans.push((
                AutopilotMeasurementKind::DurableFanGrowth30d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(44),
            ));
            let baseline_installs =
                pre_action_signal_installs(transaction, workspace_id, now, 7).await?;
            let baseline_installs_1d =
                pre_action_signal_installs(transaction, workspace_id, now, 1).await?;
            plans.push((
                AutopilotMeasurementKind::AgentRunSignalInstalls7d,
                action_id.into_uuid(),
                baseline_installs,
                now + time::Duration::days(7),
            ));
            // Fast checkpoint: 1-day signal installs. The brain gets
            // next-cycle feedback on whether the worker moved fans toward
            // Signal within 24 hours, not a week. Its baseline is the one day
            // of installs before the dispatch, matched to its own width.
            plans.push((
                AutopilotMeasurementKind::SignalInstalls1d,
                action_id.into_uuid(),
                baseline_installs_1d,
                now + time::Duration::days(1),
            ));
            } else if !is_relationship_research {
                // Intelligence workers are judged on the thing they actually
                // produce. Discovery workers (including fanbase-scout) are
                // discovery; advisory workers (growth-strategist and
                // strategy-consult) are insights. Neither receives fan credit
                // or fan blame for downstream actions taken by somebody else.
                let kind = if is_discovery_intelligence {
                    AutopilotMeasurementKind::ScannerDiscoveryQuality14d
                } else {
                    AutopilotMeasurementKind::StrategistInsightQuality14d
                };
                plans.push((
                    kind,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(14),
                ));
                let fast_kind = if is_discovery_intelligence {
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
            // contact-research gets only AgentRunOutcomeQuality1h above.
            // Its durable product is a validated PersonalHook written minutes
            // later. Giving it a 3/14/44-day fan-growth score would punish
            // internal learning for not contacting anybody and teach the Brain
            // exactly the wrong lesson.
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
            // North Star: the post exists to grow fans, so it is measured on
            // the fans traced to its own link. The window anchor is dispatch
            // time for now; `anchor_measurements_to_publication` re-anchors
            // to `community_posts.posted_at` when the post lands, and a post
            // that never goes live with a tracked link is abandoned
            // (`no_tracked_link`) — the absence of an outcome is not an
            // outcome of zero.
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(3),
            ));
            // Fast, action-bound funnel signal. Community posts carry their
            // own tracked smart link, so seven days is enough to learn which
            // rooms drive traffic and actual owned-fan acquisition while the
            // causal Y14/Y30 estimands are still maturing. Both observers fail
            // closed with `no_tracked_link` when the post never lands with a
            // measurable link — absence of instrumentation is not a zero.
            for kind in [
                AutopilotMeasurementKind::ContentLinkClicks7d,
                AutopilotMeasurementKind::ContentFanAcquisition7d,
            ] {
                plans.push((
                    kind,
                    action_id.into_uuid(),
                    0.0,
                    now + time::Duration::days(7),
                ));
            }
            // No AgentRunOutcomeQuality1h here: that measure links through
            // `agent_service_tasks.metadata->>'action_id'`, and community
            // engagement creates no task row — the community_posts row is
            // its receipt. A measure that can only ever read 0 would teach
            // the brain the intervention failed when nothing failed at all.
        }
        // Agent content (social/telegram/discord posts): measure whether the
        // published content actually grew fans — the fans traced to its own
        // link (`attributed_fans`). The content action materializes the draft
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
        AutopilotActionPayload::RequestAgentContent {
            template_id, draft, ..
        } => {
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth14d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(14),
            ));
            plans.push((
                AutopilotMeasurementKind::IncrementalFanGrowth3d,
                action_id.into_uuid(),
                0.0,
                now + time::Duration::days(3),
            ));
            // What the post's own tracked link did. An explicit CTA keeps the
            // old Instagram/Facebook/X path. New social-post Facebook/X drafts
            // are also measurable without model CTA because the executor
            // guarantees a tenant-owned fallback before publication.
            if social_content_funnel_planned(template_id.as_deref(), draft) {
                for kind in [
                    AutopilotMeasurementKind::ContentLinkClicks7d,
                    AutopilotMeasurementKind::ContentFanAcquisition7d,
                ] {
                    plans.push((
                        kind,
                        action_id.into_uuid(),
                        0.0,
                        now + time::Duration::days(7),
                    ));
                }
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
                for kind in [
                    AutopilotMeasurementKind::ContentLinkClicks7d,
                    AutopilotMeasurementKind::ContentFanAcquisition7d,
                ] {
                    plans.push((
                        kind,
                        action_id.into_uuid(),
                        0.0,
                        now + time::Duration::days(7),
                    ));
                }
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

#[cfg(test)]
mod owned_social_measurement_tests {
    use super::social_content_funnel_planned;
    use serde_json::json;

    #[test]
    fn social_post_fallback_plans_only_honest_clickable_rails() {
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"facebook","text":"hello"})
        ));
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"x","text":"hello"})
        ));
        assert!(!social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"instagram","text":"hello"})
        ));
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({
                "platform":"instagram",
                "text":"hello",
                "cta_url":"https://band.example/signal/"
            })
        ));
        assert!(!social_content_funnel_planned(
            Some("press-pitch"),
            &json!({"platform":"facebook","text":"hello"})
        ));
    }
}
