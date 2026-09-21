//! Delayed effect measurement: the queue, its kinds, and how an observation
//! becomes a classified effect.
//!
//! Split out of `ports.rs` because this is one coherent surface — the shape of
//! a claimed measurement, what each kind means, and the single function that
//! turns an observed number into a finding. Keeping it together makes the one
//! distinction that matters here visible in a single screen: some kinds report
//! a level and some report an effect, and the two must never be classified the
//! same way.

use crate::RepositoryError;
use async_trait::async_trait;
use crowdrelay_domain::performance::{
    EffectAssessment, EffectDirection, EffectResult, assess_effect, assess_signed_effect,
};
use crowdrelay_domain::{AutopilotActionId, AutopilotMeasurementId, WorkspaceId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotMeasurementKind {
    TicketRevenue72h,
    MerchGrossProxy7d,
    PromotionRoas7d,
    BookingReply7d,
    OutreachReply7d,
    AudienceTicketRevenue72h,
    ShowTicketRevenue7d,
    ShowGrowthSurfaceClicks7d,
    ShowGrowthAttributedTicketOrders7d,
    GrassrootsActivationReplies14d,
    /// Fan count delta in the 14 days after an agent dispatch. Measures
    /// whether the worker's intelligence gathering actually aggregated
    /// new fans into the fanbase.
    AgentRunFanGrowth14d,
    /// Incremental fan growth: new fans in the 14-day post-action window
    /// minus the counterfactual (pre-action daily rate × 14). This is the
    /// North Star metric — it measures causal uplift, not just correlation.
    /// The baseline_value stores the pre-action daily fan arrival rate.
    IncrementalFanGrowth14d,
    /// The same counterfactual estimate over three days instead of fourteen.
    ///
    /// The strategy posterior learns from incremental fans and from nothing
    /// else, and `IncrementalFanGrowth14d` was the only kind that produced
    /// them — so no strategy belief could move until fourteen days after a
    /// dispatch, on a brain that had been alive for four. This is the same
    /// difference-in-differences arithmetic against a matched three-day
    /// pre-period.
    ///
    /// It is a weaker estimate and is treated as one. Three days of arrivals
    /// is a noisier sample than fourteen, and it cannot see an effect that
    /// takes a week to appear. It is recorded in its own column so it never
    /// displaces the fourteen-day number, and the learner prefers the
    /// fourteen-day one wherever it exists.
    IncrementalFanGrowth3d,
    /// Signal install delta in the 7 days after an agent dispatch. Measures
    /// whether the worker's output moved fans toward the Signal app (growth).
    AgentRunSignalInstalls7d,
    /// Community engagement metric delta in the 7 days after a community
    /// engagement dispatch. Measures whether the posts produced meaningful
    /// engagement (upvotes, comments) rather than just existing.
    AgentRunCommunityEngagement7d,
    /// Durable fan growth 30 days after the measurement window. Counts fans
    /// created in the 14-day post-action window that are still active 30
    /// days after creation (not suppressed, not deleted). This is the true
    /// North Star — fans that stick, not just fans that sign up.
    DurableFanGrowth30d,
    /// Scanner discovery quality: counts the number of new outreach targets
    /// discovered by a reddit-scanner dispatch in the 14-day post-action
    /// window. Measures the scanner's proximal outcome (discovery) rather
    /// than workspace-wide fan growth — the scanner doesn't acquire fans,
    /// it finds communities.
    ScannerDiscoveryQuality14d,
    /// Strategist insight quality: counts the number of campaign insights
    /// produced by a growth-strategist dispatch in the 14-day post-action
    /// window. Measures the strategist's proximal outcome (insight
    /// production) rather than workspace-wide fan growth.
    StrategistInsightQuality14d,
    /// Fan engagement in the 7 days after a lifecycle message (welcome,
    /// re-engagement, referral invite). Counts ticket orders, Signal push
    /// endpoint creations, and referral redemptions by the specific fan
    /// who received the message. The baseline is 0 — lifecycle messages
    /// target new or dormant fans who haven't engaged yet. This is the
    /// per-fan outcome signal that closes the learning loop for lifecycle
    /// messaging: the brain learns which message templates actually move
    /// individual fans to action.
    FanLifecycleEngagement7d,
    /// Early fan-growth checkpoint 3 days after an agent dispatch. This is
    /// the fastest feedback signal for the learning loop — the brain can
    /// learn from a 3-day partial observation while waiting for the full
    /// 14-day and 30-day measurements. Mirrors Kern's 1-day checkpoint
    /// settling: intermediate observations update the posterior with
    /// downweighted evidence quality, so the brain gets next-cycle
    /// feedback instead of waiting weeks for the final outcome.
    AgentRunFanGrowth3d,
    /// Worker reliability checkpoint 1 hour after an agent dispatch. The
    /// fastest feedback signal possible: did the worker produce a valid,
    /// grounded, processed outcome? Binary — 1 if processed, 0 if rejected
    /// or no outcome landed. This closes the loop on worker quality within
    /// an hour, not days. The brain learns which templates produce reliable
    /// output and which fail grounding checks, before any downstream effect
    /// is measurable.
    AgentRunOutcomeQuality1h,
    /// Scanner discovery checkpoint 1 hour after a scanner dispatch. The
    /// scanner discovers communities and targets immediately — its proximal
    /// outcome is available within minutes, not 14 days. This fast checkpoint
    /// lets the brain learn scanner quality within an hour. The 14-day
    /// measurement remains for downstream engagement, but the proximal
    /// discovery count is the fast feedback signal.
    ScannerDiscoveryQuality1h,
    /// Strategist insight checkpoint 1 hour after a strategist dispatch. The
    /// strategist produces campaign insights immediately — its proximal
    /// outcome is available within minutes, not 14 days. Same reasoning as
    /// the scanner: the 14-day measurement stays for downstream value, but
    /// the insight count is the fast feedback signal.
    StrategistInsightQuality1h,
    /// Signal install checkpoint 1 day after an agent dispatch. Faster than
    /// the 7-day window — the brain gets next-cycle feedback on whether the
    /// worker moved fans toward Signal within 24 hours, not a week.
    SignalInstalls1d,
    /// Whether a booking-agent approach got an answer in the 30 days after
    /// dispatch. An agent decides on a season's timescale, not a pitch's
    /// week — thirty days is the window where a reply is still the
    /// approach's answer rather than the season's news. The observation
    /// counts inbound `reply` interactions on the agent since the send.
    BookingAgentReply30d,
    /// Whether the audience actually showed up: redeemed admission passes
    /// over passes valid for entry, observed fourteen days after the event.
    /// Selling a ticket and filling the room are different outcomes, and a
    /// lever that moves only the first is a different lever than one that
    /// moves both. The subject is the event, not the action's target.
    ShowAttendanceRate14d,
    /// Fans acquired through the release's own campaign links in the 14 days
    /// after a milestone ran — `fan_acquisition_events` joined through the
    /// campaign every milestone ensures before it sends. The subject is the
    /// release plan, so each rung answers for its own window.
    ReleaseBoundAcquisition14d,
    /// Clicks on the release's tracked link in the 14 days after a milestone
    /// ran. The link exists before anything shares it — a milestone that
    /// moved nobody reads its own zero.
    ReleaseLinkClicks14d,
    /// Release-acquired fans who bought a ticket inside the same window —
    /// the conversion half of "the release grew the fanbase": joining the
    /// acquisition row to a paid order by the same address.
    ReleaseFanConversion14d,
    /// The release's own metric series — video views, listens — lifted over
    /// the pre-release baseline: the post-release delta minus the delta of
    /// the fourteen days before it. A signed effect, not a level: a release
    /// can genuinely move its channels backwards. Scheduled only when a
    /// `release_plan` series exists to read; the milestone's other
    /// measurements report without it.
    ReleaseChannelLift14d,
    /// Delivered recipients who bought a ticket inside fourteen days —
    /// the conversion an email to existing fans can actually produce. The
    /// order must postdate that fan's delivery receipt, or a purchase that
    /// predated the send would count as its work.
    CampaignTicketConversion14d,
    /// The unsubscribe rate among delivered recipients in the seven days
    /// after their receipt — consent withdrawals recorded after the fan's
    /// own delivery over the count the campaign reached. A harm metric:
    /// more is worse, and the measurement answers for what the send cost,
    /// not only what it earned.
    CampaignUnsubscribe7d,
}

impl AutopilotMeasurementKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketRevenue72h => "ticket_revenue_72h",
            Self::MerchGrossProxy7d => "merch_gross_proxy_7d",
            Self::PromotionRoas7d => "promotion_roas_7d",
            Self::BookingReply7d => "booking_reply_7d",
            Self::OutreachReply7d => "outreach_reply_7d",
            Self::AudienceTicketRevenue72h => "audience_ticket_revenue_72h",
            Self::ShowTicketRevenue7d => "show_ticket_revenue_7d",
            Self::ShowGrowthSurfaceClicks7d => "show_growth_surface_clicks_7d",
            Self::ShowGrowthAttributedTicketOrders7d => "show_growth_attributed_ticket_orders_7d",
            Self::GrassrootsActivationReplies14d => "grassroots_activation_replies_14d",
            Self::AgentRunFanGrowth14d => "agent_run_fan_growth_14d",
            Self::IncrementalFanGrowth14d => "incremental_fan_growth_14d",
            Self::IncrementalFanGrowth3d => "incremental_fan_growth_3d",
            Self::AgentRunSignalInstalls7d => "agent_run_signal_installs_7d",
            Self::AgentRunCommunityEngagement7d => "agent_run_community_engagement_7d",
            Self::DurableFanGrowth30d => "durable_fan_growth_30d",
            Self::ScannerDiscoveryQuality14d => "scanner_discovery_quality_14d",
            Self::StrategistInsightQuality14d => "strategist_insight_quality_14d",
            Self::FanLifecycleEngagement7d => "fan_lifecycle_engagement_7d",
            Self::AgentRunFanGrowth3d => "agent_run_fan_growth_3d",
            Self::AgentRunOutcomeQuality1h => "agent_run_outcome_quality_1h",
            Self::ScannerDiscoveryQuality1h => "scanner_discovery_quality_1h",
            Self::StrategistInsightQuality1h => "strategist_insight_quality_1h",
            Self::SignalInstalls1d => "signal_installs_1d",
            Self::BookingAgentReply30d => "booking_agent_reply_30d",
            Self::ShowAttendanceRate14d => "show_attendance_rate_14d",
            Self::ReleaseBoundAcquisition14d => "release_bound_acquisition_14d",
            Self::ReleaseLinkClicks14d => "release_link_clicks_14d",
            Self::ReleaseFanConversion14d => "release_fan_conversion_14d",
            Self::ReleaseChannelLift14d => "release_channel_lift_14d",
            Self::CampaignTicketConversion14d => "campaign_ticket_conversion_14d",
            Self::CampaignUnsubscribe7d => "campaign_unsubscribe_7d",
        }
    }

    #[must_use]
    pub const fn direction(self) -> EffectDirection {
        match self {
            // The one kind where more is worse: an unsubscribe is a fan
            // spent, and the classification has to say so or a costly send
            // would read as a success.
            Self::CampaignUnsubscribe7d => EffectDirection::LowerIsBetter,
            _ => EffectDirection::HigherIsBetter,
        }
    }

    /// The key under which this kind's observed value lands in
    /// `viryaos_growth_evidence.observed_metrics`, or `None` when the kind
    /// already has its own learner.
    ///
    /// `None` is not "not learned" — the fan-growth and Signal-install kinds
    /// return `None` because they write typed columns that dedicated
    /// posteriors consume (`observed_fans`, `observed_incremental_fans`,
    /// `durable_fans_30d`, `observed_signal_installs`). Duplicating them into
    /// the map would let one observation reach two learners and count twice.
    /// Everything else returns a key: the value a measurement produces is
    /// evidence, and evidence that only reaches the outcomes table is stored,
    /// not learned.
    ///
    /// Keys are stable identifiers in the metric-posterior vocabulary — a
    /// kind's key never changes once shipped, and kinds that observe the
    /// same quantity at different windows or scopes get different keys (a
    /// one-hour discovery count and a fourteen-day one are different
    /// measurements of different things).
    #[must_use]
    pub const fn learnable_metric_key(self) -> Option<&'static str> {
        match self {
            Self::TicketRevenue72h => Some("ticket_revenue_minor"),
            Self::MerchGrossProxy7d => Some("merch_gross_minor"),
            Self::PromotionRoas7d => Some("promotion_roas_bps"),
            Self::BookingReply7d => Some("booking_replies"),
            Self::BookingAgentReply30d => Some("booking_agent_replies"),
            Self::OutreachReply7d => Some("outreach_replies"),
            Self::AudienceTicketRevenue72h => Some("audience_ticket_revenue_minor"),
            Self::ShowTicketRevenue7d => Some("show_ticket_revenue_minor"),
            Self::ShowGrowthSurfaceClicks7d => Some("show_growth_clicks"),
            Self::ShowGrowthAttributedTicketOrders7d => Some("show_growth_ticket_orders"),
            Self::ShowAttendanceRate14d => Some("show_attendance_rate"),
            Self::ReleaseBoundAcquisition14d => Some("release_acquisitions"),
            Self::ReleaseLinkClicks14d => Some("release_link_clicks"),
            Self::ReleaseFanConversion14d => Some("release_fan_conversions"),
            Self::ReleaseChannelLift14d => Some("release_channel_lift"),
            Self::CampaignTicketConversion14d => Some("campaign_ticket_conversions"),
            Self::CampaignUnsubscribe7d => Some("campaign_unsubscribe_rate"),
            Self::GrassrootsActivationReplies14d => Some("activation_replies"),
            Self::AgentRunCommunityEngagement7d => Some("engagement_score"),
            Self::FanLifecycleEngagement7d => Some("lifecycle_engagement_events"),
            Self::ScannerDiscoveryQuality14d => Some("scanner_discoveries"),
            Self::StrategistInsightQuality14d => Some("strategist_insights"),
            Self::ScannerDiscoveryQuality1h => Some("scanner_discoveries_1h"),
            Self::StrategistInsightQuality1h => Some("strategist_insights_1h"),
            Self::AgentRunOutcomeQuality1h => Some("outcome_quality_1h"),
            Self::AgentRunFanGrowth14d
            | Self::AgentRunFanGrowth3d
            | Self::IncrementalFanGrowth14d
            | Self::IncrementalFanGrowth3d
            | Self::DurableFanGrowth30d
            | Self::AgentRunSignalInstalls7d
            | Self::SignalInstalls1d => None,
        }
    }

    /// Whether the observed value is an effect rather than a level.
    ///
    /// A signed kind has already had its counterfactual subtracted, so a
    /// negative reading is a result — the action did worse than doing nothing
    /// — and not a malformed measurement. Generic measurement code must ask
    /// this before classifying, because [`assess_effect`] refuses negative
    /// levels and would otherwise turn every harmful outcome into a
    /// repository error and retry it away.
    #[must_use]
    pub const fn is_signed_effect(self) -> bool {
        matches!(
            self,
            Self::IncrementalFanGrowth14d
                | Self::IncrementalFanGrowth3d
                | Self::DurableFanGrowth30d
                // The lift observation is post-window delta minus pre-window
                // delta — already signed, already a difference. Classifying
                // it as a level would refuse the negative reading a release
                // that moved backwards earns.
                | Self::ReleaseChannelLift14d
        )
    }

    /// Why a measurement was abandoned when its dispatch never reached anyone.
    ///
    /// Recorded as the measurement's `last_error_kind`, which is bounded at 96
    /// characters — so this is a short kind, not a sentence. It lives here
    /// rather than beside the query that raises it because the worker records
    /// it and the repository raises it, and neither should own the other's
    /// vocabulary.
    pub const NEVER_PUBLISHED: &'static str = "dispatch_never_published";

    /// Why an event-bound measurement was abandoned: the show was cancelled,
    /// so its outcome is unobservable rather than zero. A cancelled event's
    /// zero attendance would otherwise land in evidence as the lever's
    /// fault.
    pub const EVENT_CANCELLED: &'static str = "event_cancelled";

    /// Why an attendance measurement was abandoned: the event had no
    /// admission passes at all, meaning ticketing did not run through the
    /// platform and there is no attendance signal to read. Absence of a
    /// denominator is not a rate of zero.
    pub const NO_ISSUED_PASSES: &'static str = "no_issued_passes";

    /// Why a release-funnel measurement was abandoned: the release had no
    /// campaign — the milestone executor only creates one when the plan
    /// carries a listenable link. Without it the funnel arms return a real
    /// zero that was never measured, and "nothing was instrumented" is not
    /// "nobody came".
    pub const NO_RELEASE_LINK: &'static str = "no_release_link";

    /// Why a release channel-lift measurement was abandoned: the release had
    /// series, but none of them anchored both the pre and post windows — a
    /// feed that stalled, or a series too young to have a baseline. The lift
    /// is unobservable rather than zero.
    pub const NO_RELEASE_SERIES_DATA: &'static str = "no_release_series_data";

    /// Whether the kind's `subject_id` is an `events.id` — the kinds whose
    /// observation is a fact about a show. A cancelled show has no outcome
    /// to observe, and observing one anyway would write a zero that the
    /// learner would read as the action's fault.
    #[must_use]
    pub const fn subject_is_event(self) -> bool {
        matches!(
            self,
            Self::AudienceTicketRevenue72h
                | Self::ShowTicketRevenue7d
                | Self::ShowGrowthSurfaceClicks7d
                | Self::ShowGrowthAttributedTicketOrders7d
                | Self::GrassrootsActivationReplies14d
                | Self::ShowAttendanceRate14d
        )
    }

    /// Whether this kind measures what an outbound post did to an audience.
    ///
    /// A dispatch that produced only a draft has no such outcome. Every
    /// outbound channel drafts and waits for an operator — Reddit is read-only
    /// by policy, Telegram, Discord and social default to manual — so the
    /// action succeeds, the measurement comes due on schedule, and it observes
    /// the fans a post nobody published did not attract. That zero is the
    /// absence of an outcome, not an outcome of zero, and the brain cannot
    /// tell them apart: it learns the template does not work and moves the
    /// dispatch budget away from the channel that would have worked.
    ///
    /// The kinds listed here are abandoned rather than recorded when the
    /// dispatch never reached anyone. Everything else is deliberately absent:
    ///
    /// - Ticket, merch and promotion kinds measure a price or budget change
    ///   that took effect regardless of any post.
    /// - Reply kinds measure email the outbox actually delivered.
    /// - Scanner and strategist quality kinds measure work done inside the
    ///   system — targets discovered, insights written — which is real whether
    ///   or not anything was ever published.
    /// - `AgentRunOutcomeQuality1h` asks whether the worker produced usable
    ///   output at all. A draft is usable output; that question is answered by
    ///   the dispatch, not by the operator's backlog.
    #[must_use]
    pub const fn measures_outbound_reach(self) -> bool {
        matches!(
            self,
            Self::AgentRunFanGrowth3d
                | Self::AgentRunFanGrowth14d
                | Self::IncrementalFanGrowth14d
                | Self::IncrementalFanGrowth3d
                | Self::DurableFanGrowth30d
                | Self::AgentRunSignalInstalls7d
                | Self::SignalInstalls1d
                | Self::AgentRunCommunityEngagement7d
        )
    }

    /// Whether this kind counts events through the release's campaign —
    /// the tracked link the milestone executor ensured. A release plan with
    /// no listenable URL gets no campaign, and these arms would answer a
    /// hard zero for a funnel that was never instrumented.
    #[must_use]
    pub const fn measures_release_funnel(self) -> bool {
        matches!(
            self,
            Self::ReleaseBoundAcquisition14d
                | Self::ReleaseLinkClicks14d
                | Self::ReleaseFanConversion14d
        )
    }

    /// Whether this kind measures what a communication campaign did to the
    /// fans it was sent to. The delivery ledger is the reach contract: a
    /// campaign with no `delivered` receipt reached nobody, and observing
    /// anyway would write the zeros of an email that never left as the
    /// campaign's fault — the same failure `measures_outbound_reach` guards
    /// for posts.
    #[must_use]
    pub const fn measures_email_campaign(self) -> bool {
        matches!(
            self,
            Self::CampaignTicketConversion14d | Self::CampaignUnsubscribe7d
        )
    }

    /// Days of counterfactual the stored `baseline_value` rate covers.
    ///
    /// The observation subtracts `baseline_value × window` and the
    /// classification divides by the same quantity, so the window lives here
    /// rather than being spelled out at both call sites where the two could
    /// drift apart.
    #[must_use]
    pub const fn counterfactual_window_days(self) -> f64 {
        match self {
            Self::IncrementalFanGrowth14d | Self::DurableFanGrowth30d => 14.0,
            Self::IncrementalFanGrowth3d => 3.0,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClaimedAutopilotMeasurement {
    pub id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub kind: AutopilotMeasurementKind,
    pub subject_id: uuid::Uuid,
    pub baseline_value: f64,
    pub action_finished_at: OffsetDateTime,
    pub attempt_number: u32,
}

impl ClaimedAutopilotMeasurement {
    /// The counterfactual this measurement's effect was taken against.
    ///
    /// `baseline_value` holds a daily rate for signed kinds; the observation
    /// subtracts this product and the classification divides by it. Zero for
    /// every other kind, which compare against a level instead.
    #[must_use]
    pub fn counterfactual_value(&self) -> f64 {
        self.baseline_value * self.kind.counterfactual_window_days()
    }
}

#[async_trait]
pub trait AutopilotMeasurementRepository: Send + Sync {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError>;

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError>;

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement_id: AutopilotMeasurementId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

#[must_use]
pub fn assess_measurement_effect(
    measurement: &ClaimedAutopilotMeasurement,
    observed_value: f64,
) -> Option<EffectResult> {
    if measurement.kind == AutopilotMeasurementKind::CampaignUnsubscribe7d {
        // The observed value is a rate — withdrawals over delivered — and a
        // send's audience carries baseline churn: a fraction of a percent
        // would have left anyway. Below that floor the honest verdict is
        // Neutral, not Worsened — otherwise every send that cost one fan in
        // two hundred would read as harm and feed the demotion guard. The
        // rate itself still lands in `observed_metrics` for the posterior.
        let result = assess_effect(0.0, observed_value, EffectDirection::LowerIsBetter, 500)?;
        return Some(EffectResult {
            assessment: if observed_value < 0.005 {
                EffectAssessment::Neutral
            } else {
                result.assessment
            },
            delta_basis_points: result.delta_basis_points,
        });
    }
    if measurement.kind == AutopilotMeasurementKind::ReleaseChannelLift14d {
        // The lift is a delta between two channel counts, so the signed
        // assessor classifies it against zero — where any nonzero value
        // saturates the basis-point scale. A video idling at −1 view is feed
        // noise, not the release moving backwards, and letting it read
        // Worsened hands the autonomy-demotion guard a verdict instrumentation
        // never earned. Under a handful of units the honest answer is Neutral;
        // the raw lift still lands in `observed_metrics` for the posterior.
        let result = assess_signed_effect(measurement.counterfactual_value(), observed_value, 500)?;
        return Some(EffectResult {
            assessment: if observed_value.abs() < 5.0 {
                EffectAssessment::Neutral
            } else {
                result.assessment
            },
            delta_basis_points: result.delta_basis_points,
        });
    }
    if measurement.kind.is_signed_effect() {
        // The observation is already an effect. Classify it against zero and
        // express it against the counterfactual it was measured against.
        return assess_signed_effect(measurement.counterfactual_value(), observed_value, 500);
    }
    assess_effect(
        measurement.baseline_value,
        observed_value,
        measurement.kind.direction(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claimed(kind: AutopilotMeasurementKind) -> ClaimedAutopilotMeasurement {
        ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(uuid::Uuid::now_v7()),
            kind,
            subject_id: uuid::Uuid::now_v7(),
            baseline_value: 0.0,
            action_finished_at: OffsetDateTime::now_utc(),
            attempt_number: 1,
        }
    }

    /// A channel lift inside feed noise classifies Neutral rather than
    /// Worsened — a −1-view wobble must not reach the autonomy-demotion
    /// guard as if the release moved its own audience backwards.
    #[test]
    fn release_channel_lift_reads_small_deltas_as_noise() {
        let measurement = claimed(AutopilotMeasurementKind::ReleaseChannelLift14d);
        for observed in [-4.0, -1.0, 0.0, 3.0, 4.9] {
            let result =
                assess_measurement_effect(&measurement, observed).expect("lift assessment");
            assert_eq!(
                result.assessment,
                EffectAssessment::Neutral,
                "lift {observed} should classify Neutral"
            );
        }
    }

    /// A lift beyond the noise floor keeps its real verdict — the floor is
    /// a noise guard, not a sponge that soaks every regression.
    #[test]
    fn release_channel_lift_keeps_real_verdicts() {
        let measurement = claimed(AutopilotMeasurementKind::ReleaseChannelLift14d);
        let worsened = assess_measurement_effect(&measurement, -40.0).expect("worsened");
        assert_eq!(worsened.assessment, EffectAssessment::Worsened);
        let improved = assess_measurement_effect(&measurement, 40.0).expect("improved");
        assert_eq!(improved.assessment, EffectAssessment::Improved);
    }

    /// A send's unsubscribe rate under half a percent is baseline churn, not
    /// harm the send caused — without the floor every send that cost one fan
    /// in a few hundred would read Worsened and feed the demotion guard.
    #[test]
    fn campaign_unsubscribe_reads_baseline_churn_as_neutral() {
        let measurement = claimed(AutopilotMeasurementKind::CampaignUnsubscribe7d);
        for rate in [0.0, 0.001, 0.0049] {
            let result =
                assess_measurement_effect(&measurement, rate).expect("unsubscribe assessment");
            assert_eq!(
                result.assessment,
                EffectAssessment::Neutral,
                "rate {rate} should classify Neutral"
            );
        }
        let harmful = assess_measurement_effect(&measurement, 0.05).expect("harmful");
        assert_eq!(harmful.assessment, EffectAssessment::Worsened);
    }
}
