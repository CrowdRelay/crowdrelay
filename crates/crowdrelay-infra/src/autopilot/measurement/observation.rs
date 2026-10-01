//! Reading the world for one due measurement.
//!
//! Split out of the adapter so both stay inside the source-size ratchet, and
//! because this is one job with one shape: every arm answers "what happened in
//! the window this measurement covers", and returns a number.
//!
//! Two rules hold across every arm. A kind that reports an *effect* is
//! classified against zero downstream and may be negative — the brain has to
//! be able to learn that an action did harm. A kind that reports a *level*
//! never is. `AutopilotMeasurementKind::is_signed_effect` is which is which.
//! The fan kinds are effects whose counterfactual is zero by construction:
//! they count fans traced to the action (`attributed_fans`).

pub(super) mod attributed_fans;
mod campaigns;
pub(super) mod content_synergy;
pub(super) mod harm;
mod release_lift;

use super::super::*;
use super::dispatch_reached_an_audience;

/// Observes one claimed measurement.
pub(super) async fn observe(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<f64, RepositoryError> {
    observe_with_metrics(pool, workspace_id, measurement, now)
        .await
        .map(|observed| observed.value)
}

pub(super) async fn observe_with_metrics(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<crowdrelay_application::autopilot::AutopilotMeasurementObservation, RepositoryError> {
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
    // reading of "we tried and could not find out".
    if measurement.kind.measures_outbound_reach()
        && !dispatch_reached_an_audience(pool, workspace_id, measurement.action_id).await?
    {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NEVER_PUBLISHED,
        ));
    }
    // A cancelled show has no outcome: no attendance, no post-show clicks,
    // no reply window that means anything. Observing anyway would write a
    // zero the learners would read as the action failing, when the truth is
    // the event never happened. Failed terminal, like never-published.
    if measurement.kind.subject_is_event()
        && event_is_cancelled(pool, workspace_id, measurement.subject_id).await?
    {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::EVENT_CANCELLED,
        ));
    }
    // The release funnel reads through the campaign the milestone executor
    // ensured — and it only ensures one when the plan carries a listenable
    // link. Without a campaign every funnel arm returns a real zero for a
    // funnel that was never instrumented, which the posterior would learn as
    // the milestone failing. "Nothing was instrumented" is not "nobody came";
    // failed terminal, like never-published.
    if measurement.kind.measures_release_funnel()
        && !release_link_is_tracked(pool, workspace_id, measurement.subject_id).await?
    {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_RELEASE_LINK,
        ));
    }
    // A campaign with no delivered receipt reached nobody — but whether it
    // can still reach anyone depends on where the send is. `scheduled`
    // means dispatched and awaiting ledger results, so the measurement
    // retries rather than abandoning; every other state with no delivered
    // receipt is terminal: draft never sent, cancelled never ran, completed
    // reached nobody. Observing any of those writes the zeros of an email
    // that did not leave as the campaign's performance.
    if measurement.kind.measures_email_campaign() {
        match campaigns::campaign_delivery_state(pool, workspace_id, measurement.subject_id).await?
        {
            campaigns::CampaignDeliveryState::Reached => {}
            campaigns::CampaignDeliveryState::InFlight => {
                return Err(RepositoryError::Unavailable);
            }
            campaigns::CampaignDeliveryState::NeverReached => {
                return Err(RepositoryError::ConflictBecause(
                    AutopilotMeasurementKind::NEVER_PUBLISHED,
                ));
            }
        }
    }
    // `agent_service_tasks` is the agent service's table — a stack without
    // that service has no such table at all, and the joins below fail with
    // `relation does not exist`. That is not a transient miss to retry and
    // not a zero to learn: the measurement can never be read here, so it
    // abandons with a named kind, the same terminal answer never-published
    // and cancelled-event get.
    if measurement.kind.reads_agent_service() && !agent_tasks_table_exists(pool).await? {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_AGENT_SERVICE,
        ));
    }
    if measurement.kind == AutopilotMeasurementKind::ReleaseChannelLift14d {
        return release_lift::observe(pool, workspace_id, measurement, now).await;
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
                    FROM autopilot_actions
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
                    FROM promotion_campaign_states
                    WHERE workspace_id = $1
                      AND id = $2
                      AND observed_at >= $3::timestamptz + INTERVAL '7 days'
                      AND observed_at <= $4::timestamptz
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
            // A reply belongs to the last letter that preceded it, not to
            // every letter whose window it happens to fall inside. A promoter
            // written to twice in one week — a booking approach and a gig
            // proposal's letter, say — used to answer both: one reply, two
            // measurements, two successes, and a reason tally that believed
            // twice as much evidence existed as there was. The `NOT EXISTS`
            // gives the reply to whichever outbound touch was most recent
            // before it.
            AutopilotMeasurementKind::BookingReply7d => sqlx::query_scalar::<_, f64>(
                r#"
                SELECT CASE WHEN EXISTS (
                    SELECT 1 FROM booking_interactions AS reply
                    WHERE reply.workspace_id=$1 AND reply.target_id=$2
                      AND reply.direction='inbound'
                      AND reply.occurred_at >= $3
                      AND reply.occurred_at < $3 + INTERVAL '7 days'
                      AND NOT EXISTS (
                          SELECT 1 FROM booking_interactions AS newer
                          WHERE newer.workspace_id=reply.workspace_id
                            AND newer.target_id=reply.target_id
                            AND newer.direction='outbound'
                            AND newer.occurred_at > $3
                            AND newer.occurred_at <= reply.occurred_at
                      )
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
                    SELECT 1 FROM outreach_interactions
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
            // An agent's reply belongs to the season's one application, and
            // thirty days is where it is still that application's answer.
            // Same "latest letter owns the reply" rule as the booking
            // measurement — a reply that landed after a newer approach is the
            // newer approach's answer, not this one's.
            AutopilotMeasurementKind::BookingAgentReply30d => sqlx::query_scalar::<_, f64>(
                r#"
                SELECT CASE WHEN EXISTS (
                    SELECT 1 FROM booking_agent_interactions AS reply
                    WHERE reply.workspace_id=$1 AND reply.agent_id=$2
                      AND reply.direction='inbound'
                      AND reply.occurred_at >= $3
                      AND reply.occurred_at < $3 + INTERVAL '30 days'
                      AND NOT EXISTS (
                          SELECT 1 FROM booking_agent_interactions AS newer
                          WHERE newer.workspace_id=reply.workspace_id
                            AND newer.agent_id=reply.agent_id
                            AND newer.direction='outbound'
                            AND newer.occurred_at > $3
                            AND newer.occurred_at <= reply.occurred_at
                      )
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
                    FROM show_growth_surfaces
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
                    FROM show_growth_surfaces
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
                    FROM grassroots_activations
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
            // Attendance: redeemed admission passes over every pass that was
            // valid for entry — issued, claimed and expired passes could all
            // have been used or not; revoked ones were taken back and are no
            // show's fault. The rate, not the count: a forty-cap room and a
            // four-hundred-cap room answer the same question. An event with
            // no passes at all did not ticket through the platform, so the
            // measurement is abandoned rather than reported as a zero rate.
            AutopilotMeasurementKind::ShowAttendanceRate14d => {
                let (redeemed, valid): (f64, f64) = sqlx::query_as(
                    r#"
                    SELECT COUNT(*) FILTER (WHERE status = 'redeemed')::double precision,
                           COUNT(*) FILTER (
                               WHERE status IN ('issued','claimed','redeemed','expired')
                           )::double precision
                    FROM admission_passes
                    WHERE workspace_id = $1 AND event_id = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?;
                if valid <= 0.0 {
                    return Err(RepositoryError::ConflictBecause(
                        AutopilotMeasurementKind::NO_ISSUED_PASSES,
                    ));
                }
                redeemed / valid
            }
            // Fans the release's own campaign acquired in the milestone's
            // window — the acquisition row joins to the campaign the
            // milestone executor ensured before it sent, and the campaign
            // joins back to the release plan. A milestone that reached
            // nobody reads its own zero rather than the workspace's.
            AutopilotMeasurementKind::ReleaseBoundAcquisition14d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM fan_acquisition_events AS event
                    JOIN campaigns AS campaign
                      ON campaign.workspace_id=event.workspace_id
                     AND campaign.id=event.campaign_id
                    WHERE event.workspace_id=$1 AND campaign.release_plan_id=$2
                      AND event.occurred_at >= $3
                      AND event.occurred_at < $3 + INTERVAL '14 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            AutopilotMeasurementKind::ReleaseLinkClicks14d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(*)::double precision
                    FROM click_events AS click
                    JOIN campaigns AS campaign
                      ON campaign.workspace_id=click.workspace_id
                     AND campaign.id=click.campaign_id
                    WHERE click.workspace_id=$1 AND campaign.release_plan_id=$2
                      AND click.occurred_at >= $3
                      AND click.occurred_at < $3 + INTERVAL '14 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Release-acquired fans who bought a ticket in the window —
            // joined by address because orders know the buyer's email, not
            // the fan row. Paid orders only; a reserved one that lapsed is
            // not a conversion.
            AutopilotMeasurementKind::ReleaseFanConversion14d => {
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT COUNT(DISTINCT event.fan_id)::double precision
                    FROM fan_acquisition_events AS event
                    JOIN campaigns AS campaign
                      ON campaign.workspace_id=event.workspace_id
                     AND campaign.id=event.campaign_id
                    JOIN fans AS fan
                      ON fan.workspace_id=event.workspace_id
                     AND fan.id=event.fan_id
                    JOIN ticket_orders AS ticket_order
                      ON ticket_order.workspace_id=fan.workspace_id
                     AND ticket_order.buyer_email=fan.normalized_email
                    WHERE event.workspace_id=$1 AND campaign.release_plan_id=$2
                      AND event.occurred_at >= $3
                      AND event.occurred_at < $3 + INTERVAL '14 days'
                      AND ticket_order.status IN ('paid','partially_refunded')
                      AND ticket_order.paid_at >= $3
                      AND ticket_order.paid_at < $3 + INTERVAL '14 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.subject_id)
                .bind(measurement.action_finished_at)
                .fetch_one(pool)
                .await
                .map_err(map_sqlx)?
            }
            // Dimensioned release series are handled before this scalar match.
            AutopilotMeasurementKind::ReleaseChannelLift14d => return Err(RepositoryError::Unexpected),
            AutopilotMeasurementKind::CampaignTicketConversion14d => {
                campaigns::ticket_conversions(pool, workspace_id, measurement.subject_id)
                    .await?
            }
            AutopilotMeasurementKind::CampaignUnsubscribe7d => {
                campaigns::unsubscribe_rate(pool, workspace_id, measurement.subject_id).await?
            }
            // What the post's own tracked link did — never the workspace's
            // click ledger. A published post with no link to count through
            // abandons as `no_tracked_link` rather than reporting a zero it
            // was never instrumented to produce.
            AutopilotMeasurementKind::ContentLinkClicks7d => {
                content_synergy::content_link_clicks(pool, workspace_id, measurement).await?
            }
            // Fans who arrived through that exact tracked link — the content
            // North-Star outcome rather than its click proxy.
            AutopilotMeasurementKind::ContentFanAcquisition7d => {
                content_synergy::content_fan_acquisitions(pool, workspace_id, measurement).await?
            }
            // Posts filed against the artifact's content source in the week
            // after production — produced-and-never-posted is the real zero
            // this arm reports.
            AutopilotMeasurementKind::ArtifactOutcome7d => {
                content_synergy::artifact_outcome(pool, workspace_id, measurement).await?
            }
            // The fans the action earned — conversions credited to its
            // lineage's live tracked links — not every fan the workspace
            // gained in the window. See `attributed_fans`.
            AutopilotMeasurementKind::AgentRunFanGrowth14d
            | AutopilotMeasurementKind::AgentRunFanGrowth3d
            | AutopilotMeasurementKind::IncrementalFanGrowth14d
            | AutopilotMeasurementKind::IncrementalFanGrowth3d
            | AutopilotMeasurementKind::DurableFanGrowth30d => {
                attributed_fans::observe_attributed_fans(pool, workspace_id, measurement, now)
                    .await?
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
            //   - A qualified referral created by this fan
            //
            // Referral direction is load-bearing: the lifecycle message's
            // subject is the referrer we are trying to activate, never the
            // person somebody else happened to refer. Pending/rejected/
            // reversed attributions are not growth outcomes, so only a
            // qualification inside the measurement window counts.
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
                      AND referrer_fan_id = $2
                      AND status = 'qualified'
                      AND qualified_at >= $3
                      AND qualified_at < $3 + INTERVAL '7 days'
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
                //
                // The linkage is the task row, not `processed_action_id`: that
                // column names the action an outcome *produced*, while this
                // measurement asks whether the dispatched run produced an
                // outcome. The run action's id is on the task's metadata —
                // same join StrategistInsightQuality1h makes below.
                sqlx::query_scalar::<_, f64>(
                    r#"
                    SELECT CASE WHEN EXISTS (
                        SELECT 1 FROM agent_outcomes AS outcome
                        JOIN agent_service_tasks AS task ON task.id = outcome.task_id
                        WHERE outcome.workspace_id = $1
                          AND outcome.status = 'processed'
                          AND outcome.created_at >= $2
                          AND outcome.created_at < $2 + INTERVAL '1 hour'
                          AND task.metadata->>'action_id' = $3
                    ) THEN 1.0::double precision ELSE 0.0::double precision END
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_finished_at)
                .bind(measurement.action_id.into_uuid().to_string())
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
        Ok(crowdrelay_application::autopilot::AutopilotMeasurementObservation::scalar(observed))
    } else {
        Err(RepositoryError::Unexpected)
    }
}

/// Whether the agent service's task table exists on this deployment — the
/// probe every `agent_service_tasks` read makes first, because no migration
/// in this repository creates the table.
async fn agent_tasks_table_exists(pool: &sqlx::PgPool) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass('agent_service_tasks') IS NOT NULL")
        .fetch_one(pool)
        .await
        .map_err(map_sqlx)
}

/// Whether the event a measurement is bound to was cancelled. A missing row
/// is not cancelled — the subject guard asks only about terminal state, and
/// an event id that resolves to nothing fails its own observation query.
async fn event_is_cancelled(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    event_id: uuid::Uuid,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM events
            WHERE workspace_id = $1 AND id = $2 AND status = 'cancelled'
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

/// Whether the release plan has a campaign — the tracked link the milestone
/// executor ensures before it sends. The funnel measurements all join through
/// `campaigns.release_plan_id`, so when no campaign exists they observe a
/// funnel that was never instrumented.
async fn release_link_is_tracked(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    release_id: uuid::Uuid,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM campaigns
            WHERE workspace_id = $1 AND release_plan_id = $2
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(release_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}
