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
    /// re-engagement, referral invite). Counts redeemed admission passes,
    /// Signal push endpoint creations, and qualified referrals made by the
    /// specific fan
    /// who received the message. Being referred by somebody else is not the
    /// recipient's response to the message, and a pending referral is not yet
    /// a fan-growth outcome. The baseline is 0 — lifecycle messages
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
    /// Clicks on the tracked link a social post carried, in the seven days
    /// after it reached the audience. Joined through the post's own
    /// `smart_link_id` — the workspace's whole click ledger would credit
    /// the post with traffic it never drove. Scheduled only when the draft
    /// named a link to track; a post that published without one reports
    /// `no_tracked_link` rather than a zero it never earned.
    ContentLinkClicks7d,
    /// Posts filed against the artifact's content source in the seven days
    /// after the executor confirmed production. Scheduled at the success
    /// receipt — production is already a fact by the time the measurement
    /// exists — so the observed zero means produced-and-never-posted, which
    /// is a real outcome, not an unmeasurable one.
    ArtifactOutcome7d,
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
            Self::ContentLinkClicks7d => "content_link_clicks_7d",
            Self::ArtifactOutcome7d => "artifact_outcome_7d",
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
    /// `growth_evidence.observed_metrics`, or `None` when the kind
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
            Self::ContentLinkClicks7d => Some("content_link_clicks"),
            Self::ArtifactOutcome7d => Some("artifact_posts"),
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
                // Attributed fan counts are effects against a counterfactual
                // of zero — see `counts_attributed_fans`.
                | Self::AgentRunFanGrowth14d
                | Self::AgentRunFanGrowth3d
                // The lift observation is post-window delta minus pre-window
                // delta — already signed, already a difference. Classifying
                // it as a level would refuse the negative reading a release
                // that moved backwards earns.
                | Self::ReleaseChannelLift14d
        )
    }

    /// Kinds that grade the worker's own output — did it produce a usable
    /// draft, find targets, write insights — rather than anything a fan did.
    ///
    /// They are real signals about the machine, but they are not outcomes.
    /// A summary that adds them to fan and revenue verdicts reads as
    /// "23 improved" when every one of the 23 was the system approving of
    /// its own work (production, week to 2026-09-27). Readers that report
    /// outcomes must keep them apart.
    pub const PROCESS_CHECKS: [Self; 5] = [
        Self::AgentRunOutcomeQuality1h,
        Self::ScannerDiscoveryQuality1h,
        Self::ScannerDiscoveryQuality14d,
        Self::StrategistInsightQuality1h,
        Self::StrategistInsightQuality14d,
    ];

    /// The kinds whose observation is the fans traced to the action — the
    /// conversions its own live, tracked links earned — rather than every fan
    /// the workspace gained in the window.
    ///
    /// Until 2026-09-27 these five counted `fans` created in the window, so
    /// each dispatch was credited with every arrival from any cause, and
    /// overlapping dispatches were credited with the same ones. The workspace
    /// had 23 fans ever; dispatches had been credited with 145. The brain's
    /// worker ranking was learned from that. See
    /// `~/.devin/plans/ATTRIBUTED_OUTCOME_PLAN.md`.
    ///
    /// A fan cannot click a link that was never posted, so the untreated
    /// outcome is zero by construction: the traced count *is* the effect, and
    /// a lower bound — it misses anyone who saw the post and signed up
    /// without clicking. `counterfactual_window_days` is zero for all five.
    /// An action with no live tracked link anywhere in its lineage is
    /// abandoned as [`Self::NO_TRACKED_LINK`] rather than counted as zero.
    #[must_use]
    pub const fn counts_attributed_fans(self) -> bool {
        matches!(
            self,
            Self::AgentRunFanGrowth14d
                | Self::AgentRunFanGrowth3d
                | Self::IncrementalFanGrowth14d
                | Self::IncrementalFanGrowth3d
                | Self::DurableFanGrowth30d
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

    /// Why a click measurement was abandoned: the post reached the audience
    /// carrying no tracked link — the draft named a destination no `smart_links`
    /// row was minted for, or the post ran on a channel with no link column.
    /// A click count of zero there is not a result; it is a post that could
    /// not have produced one.
    pub const NO_TRACKED_LINK: &'static str = "no_tracked_link";

    /// Why an agent-run measurement was abandoned: the stack has no agent
    /// service, so the task rows the observation joins through do not exist
    /// and never will on this deployment. "Cannot ever be read" is a
    /// terminal answer, not a zero and not a retry.
    pub const NO_AGENT_SERVICE: &'static str = "no_agent_service";

    /// The named reasons a measurement is abandoned because its outcome
    /// cannot exist — nothing was published, the show was cancelled, nothing
    /// was instrumented. These are answers, not faults: the worker records
    /// them and moves on without marking the cycle degraded. Since fan
    /// outcomes became attributed (#325), `no_tracked_link` is routine, and
    /// counting it as a degraded phase turned the cycle ledger into noise.
    pub const ABANDONMENTS: [&'static str; 7] = [
        Self::NEVER_PUBLISHED,
        Self::EVENT_CANCELLED,
        Self::NO_ISSUED_PASSES,
        Self::NO_RELEASE_LINK,
        Self::NO_RELEASE_SERIES_DATA,
        Self::NO_TRACKED_LINK,
        Self::NO_AGENT_SERVICE,
    ];

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

    /// Kinds that count the whole workspace over a window instead of what
    /// the action caused: every active push endpoint created in the day (or
    /// week) after the action, against the same span before it. #325 made
    /// the fan kinds attributed; these two still are not, and at this
    /// tenant's volume the comparison is 3-then-0 noise. On 2026-09-26 three
    /// `signal_installs_1d` readings of 0 against a baseline of 3 were scored
    /// `worsened` and demoted `content_supply` to `require_approval` for a
    /// week. They stay in the ledger as context; they are not evidence
    /// about an action.
    pub const WORKSPACE_WINDOW_KINDS: [&'static str; 2] = [
        Self::SignalInstalls1d.as_str(),
        Self::AgentRunSignalInstalls7d.as_str(),
    ];

    /// See [`Self::WORKSPACE_WINDOW_KINDS`].
    #[must_use]
    pub const fn is_workspace_window(self) -> bool {
        matches!(
            self,
            Self::SignalInstalls1d | Self::AgentRunSignalInstalls7d
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
                // A post nobody published carried no link to click — the
                // zero would be the draft's fault recorded as the content's.
                | Self::ContentLinkClicks7d
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

    /// Whether the observation joins through `agent_service_tasks` — the
    /// agent service's own table. On a stack without that service the table
    /// does not exist at all, and an unguarded join aborts the measurement
    /// with a bare `relation does not exist` instead of a named abandon.
    #[must_use]
    pub const fn reads_agent_service(self) -> bool {
        matches!(
            self,
            Self::ScannerDiscoveryQuality14d
                | Self::ScannerDiscoveryQuality1h
                | Self::StrategistInsightQuality14d
                | Self::StrategistInsightQuality1h
                | Self::AgentRunOutcomeQuality1h
        )
    }

    /// Days of counterfactual the stored `baseline_value` rate covers.
    ///
    /// Zero for every kind today. The fan kinds used to subtract a workspace
    /// arrival rate × 14 or × 3; they now count attributed fans, whose
    /// counterfactual is zero by construction (`counts_attributed_fans`).
    /// Measurements scheduled before that change still carry the old rate in
    /// `baseline_value`, and this is what keeps it out of the arithmetic.
    #[must_use]
    pub const fn counterfactual_window_days(self) -> f64 {
        0.0
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
    /// When the measurement's window ends — the harm collector bounds its
    /// counts to the same window the primary metric covers, so "what the
    /// action cost" and "what the action earned" answer about the same
    /// span.
    pub due_at: OffsetDateTime,
    pub attempt_number: u32,
}

/// What the action *cost*, counted per harm source inside the measurement's
/// window. These are levels — counts of real events — never signed deltas,
/// and each lands in the evidence row's `observed_metrics` under its
/// `harm:*` key when the measurement resolves, terminal failure included.
/// Every `harm:` key is lower-is-better by definition; the prefix is the
/// vocabulary.
///
/// Attribution is per action: fans the action's sends actually reached,
/// outreach targets the action actually wrote to, the event the action
/// actually promoted. A harm event that cannot be tied to the action does
/// not appear here — workspace-level harm is a different question.
#[derive(Clone, Copy, Debug, Default)]
pub struct HarmObservation {
    /// Consent withdrawals by fans the action contacted — one withdrawal is
    /// one fan lost. Counted per fan, not per consent row: a fan who
    /// rescinds marketing and notification consent after one send is still
    /// one fan.
    pub unsubscribes: f64,
    /// Spam complaints filed against outreach targets this action wrote to.
    /// Reputation damage, not a countable fan — this feeds the worsened
    /// classification and the autonomy guard, never `harm_fans`.
    pub complaints: f64,
    /// Refund ledger entries against the event this action promoted —
    /// money returned is a broken promise, a constraint signal rather than
    /// a fan count.
    pub refunds: f64,
    /// Fans the action contacted who were suppressed (account deleted)
    /// inside the window. Same identity conversion as unsubscribes: one
    /// suppression is one fan lost.
    pub fan_suppressions: f64,
    /// The promoted event was cancelled — 0 or 1, event subjects only. A
    /// cancelled show is harm to the audience relationship even though its
    /// attendance outcome is unobservable.
    pub show_cancellations: f64,
    /// How many distinct fans the action's sends reached — the unsubscribe
    /// denominator, not a harm key. Written nowhere; it exists so the
    /// assessment override can tell churn from harm.
    pub contacted: f64,
}

/// Baseline marketing churn, as a fraction of the audience reached —
/// the same `0.005` floor `CampaignUnsubscribe7d` applies to its observed
/// rate. A send that lost fewer withdrawals than the floor would have lost
/// anyway did not earn a Worsened verdict for them.
const UNSUBSCRIBE_CHURN_FLOOR: f64 = 0.005;

impl HarmObservation {
    /// Any harm observed at all.
    #[must_use]
    pub fn any(&self) -> bool {
        self.unsubscribes > 0.0
            || self.complaints > 0.0
            || self.refunds > 0.0
            || self.fan_suppressions > 0.0
            || self.show_cancellations > 0.0
    }

    /// The harm the assessment override acts on. Every source counts raw
    /// except unsubscribes, which carry baseline churn — the audience would
    /// have lost a fraction with no send at all. Under the floor a
    /// withdrawal is churn, not harm this action earned; above it, the
    /// excess is real. Suppressions, complaints, refunds and cancellations
    /// have no baseline excuse and count at face value.
    #[must_use]
    pub fn actionable(&self) -> bool {
        let unsubscribe_harm =
            self.unsubscribes >= (self.contacted * UNSUBSCRIBE_CHURN_FLOOR).max(1.0);
        unsubscribe_harm
            || self.complaints > 0.0
            || self.refunds > 0.0
            || self.fan_suppressions > 0.0
            || self.show_cancellations > 0.0
    }

    /// The fan-equivalent loss — the only harm that enters
    /// `DecisionValue::harm_fans`. Identity conversion: a withdrawal and a
    /// suppression are each exactly one fan gone. Complaints, refunds and
    /// cancellations are deliberately absent: they are constraint inputs
    /// (Standing worsened, the autonomy guard), and pricing reputation in
    /// fans would invent an exchange rate nobody measured.
    #[must_use]
    pub fn fan_equivalent_loss(&self) -> f64 {
        self.unsubscribes + self.fan_suppressions
    }

    /// The `harm:*` entries for the `observed_metrics` merge — all five,
    /// zeros included. A clean send is evidence too: a posterior that only
    /// ever saw harmful actions would overstate the rate.
    #[must_use]
    pub fn entries(&self) -> [(&'static str, f64); 5] {
        [
            ("harm:unsubscribes", self.unsubscribes),
            ("harm:complaints", self.complaints),
            ("harm:refunds", self.refunds),
            ("harm:fan_suppressions", self.fan_suppressions),
            ("harm:show_cancellations", self.show_cancellations),
        ]
    }
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

    /// Counts the harm events attributable to the measurement's action in
    /// its window — the cost side of the ledger the value observation alone
    /// does not see. Best called before `observe_measurement`: harm exists
    /// whether or not the primary metric is observable, and a cancelled
    /// event's measurement abandons while its harm is still real.
    async fn observe_action_harm(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<HarmObservation, RepositoryError>;

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Fails a measurement, retryable or terminal. `harm` rides along so a
    /// terminal failure merges its `harm:*` keys in the same transaction
    /// that resolves readiness — the evidence row closes with the harm it
    /// observed already on it, and no replay delta can slip between the
    /// two writes. On a retryable miss the row stays `pending` and the
    /// merge is skipped: the retried completion owns it.
    ///
    /// `None` means the observation itself failed — no `harm:*` keys are
    /// written at all. A failed look is not a clean reading: writing zeros
    /// would teach the posterior "no harm" from a measurement that never
    /// looked, and overwrite whatever a sibling attempt already landed.
    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        error_kind: &'static str,
        retryable: bool,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

#[must_use]
pub fn assess_measurement_effect(
    measurement: &ClaimedAutopilotMeasurement,
    observed_value: f64,
    harm: &HarmObservation,
) -> Option<EffectResult> {
    let result = assess_primary(measurement, observed_value)?;
    // A primary metric that did not move over real harm is not neutral:
    // nothing gained, something lost is the definition of worsened. "Did
    // not move" is the primary assessor's own verdict — Improved is the
    // only classification that proves positive movement, whatever shape
    // the kind's observed value takes (delta, rate, or level against a
    // baseline). Harm under genuine growth does not veto the verdict —
    // the send that grew fans while costing some nets out in `harm_fans`,
    // where the subtraction is priced, not in a classification that would
    // hide the growth signal.
    let assessment = if harm.actionable() && result.assessment != EffectAssessment::Improved {
        EffectAssessment::Worsened
    } else {
        result.assessment
    };
    Some(EffectResult {
        assessment,
        delta_basis_points: result.delta_basis_points,
    })
}

/// Whether `observed` sits within counting noise of `expected` — an
/// approximation of a two-sided Poisson test at about 95%. Counts scatter
/// around their expectation by roughly two standard deviations, so a
/// baseline of three observing zero is scatter, not a collapse; the floor
/// keeps that scatter out of the verdict without touching the delta the
/// posterior still reads.
fn within_count_noise(expected: f64, observed: f64) -> bool {
    let spread = (expected.max(observed).max(1.0)).sqrt() * 2.0;
    (observed - expected).abs() < spread.max(1.0)
}

/// The primary-metric classification, before the harm override.
fn assess_primary(
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
        let result = assess_signed_effect(measurement.counterfactual_value(), observed_value, 500)?;
        // A fan-count effect smaller than one whole fan is a fractional
        // counterfactual, not a result. A quiet workspace that gained three
        // fans in the fortnight before a dispatch expects 0.43 in the three
        // days after it; observing zero — the most likely count under that
        // expectation — subtracts to −0.43 and saturates the signed scale to
        // −100%. Production on 2026-09-27 had nineteen such rows in one week,
        // every one the same fractional miss charged to a different action,
        // and all of them fed the two-worsened demotion guard. Under one fan
        // the honest verdict is Neutral; the raw effect still lands in
        // `observed_metrics` for the posterior.
        //
        // The floor also scales with the counterfactual the measurement was
        // scheduled against: a miss of three against an expectation of three
        // is within the same ~2σ Poisson scatter, not a regression — while a
        // whole fan *up* is a real improvement and never floored away.
        let fractional_fan = measurement.kind.counts_attributed_fans()
            && observed_value < 1.0
            && within_count_noise(
                measurement.baseline_value,
                measurement.baseline_value + observed_value,
            );
        return Some(EffectResult {
            assessment: if fractional_fan {
                EffectAssessment::Neutral
            } else {
                result.assessment
            },
            delta_basis_points: result.delta_basis_points,
        });
    }
    let result = assess_effect(
        measurement.baseline_value,
        observed_value,
        measurement.kind.direction(),
        500,
    )?;
    // Level counts carry the same small-count floor the attributed fan
    // effect does: an installs window's observed count scatters around its
    // expectation, and a Worsened inside that scatter is instrumentation
    // noise feeding the demotion guard. The signed fan-growth kinds take
    // their own floor above — and like that floor, a whole unit above the
    // expectation is a real improvement and never floored away.
    let level_count_noise = matches!(
        measurement.kind,
        AutopilotMeasurementKind::SignalInstalls1d
            | AutopilotMeasurementKind::AgentRunSignalInstalls7d
    ) && observed_value < measurement.baseline_value + 1.0
        && within_count_noise(measurement.baseline_value, observed_value);
    Some(EffectResult {
        assessment: if level_count_noise {
            EffectAssessment::Neutral
        } else {
            result.assessment
        },
        delta_basis_points: result.delta_basis_points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claimed(kind: AutopilotMeasurementKind) -> ClaimedAutopilotMeasurement {
        let now = OffsetDateTime::now_utc();
        ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(uuid::Uuid::now_v7()),
            kind,
            subject_id: uuid::Uuid::now_v7(),
            baseline_value: 0.0,
            action_finished_at: now - time::Duration::days(7),
            due_at: now,
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
                assess_measurement_effect(&measurement, observed, &HarmObservation::default())
                    .expect("lift assessment");
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
        let worsened = assess_measurement_effect(&measurement, -40.0, &HarmObservation::default())
            .expect("worsened");
        assert_eq!(worsened.assessment, EffectAssessment::Worsened);
        let improved = assess_measurement_effect(&measurement, 40.0, &HarmObservation::default())
            .expect("improved");
        assert_eq!(improved.assessment, EffectAssessment::Improved);
    }

    /// Zero fans observed against a fractional counterfactual is the most
    /// likely count, not a loss. Under one whole fan the verdict is Neutral;
    /// a real shortfall of a fan or more keeps Worsened.
    #[test]
    fn incremental_fan_growth_reads_fractional_misses_as_neutral() {
        for kind in [
            AutopilotMeasurementKind::IncrementalFanGrowth3d,
            AutopilotMeasurementKind::IncrementalFanGrowth14d,
            AutopilotMeasurementKind::DurableFanGrowth30d,
        ] {
            let measurement = ClaimedAutopilotMeasurement {
                baseline_value: 0.143,
                ..claimed(kind)
            };
            for observed in [-0.429, -0.99, 0.0, 0.5] {
                let result =
                    assess_measurement_effect(&measurement, observed, &HarmObservation::default())
                        .expect("fan growth assessment");
                assert_eq!(
                    result.assessment,
                    EffectAssessment::Neutral,
                    "{kind:?} effect {observed} should classify Neutral"
                );
            }
            let lost = assess_measurement_effect(&measurement, -2.0, &HarmObservation::default())
                .expect("lost");
            assert_eq!(lost.assessment, EffectAssessment::Worsened, "{kind:?}");
            let gained = assess_measurement_effect(&measurement, 2.0, &HarmObservation::default())
                .expect("gained");
            assert_eq!(gained.assessment, EffectAssessment::Improved, "{kind:?}");
        }
    }

    /// A fan measurement scheduled before attribution still carries the
    /// workspace rate it was planned with. That rate is not what a traced
    /// count is compared with: zero traced fans is Neutral, one is Improved —
    /// never "−100% against the 14.8 the workspace averaged".
    #[test]
    fn a_traced_fan_count_ignores_the_rate_it_was_scheduled_with() {
        for kind in [
            AutopilotMeasurementKind::AgentRunFanGrowth14d,
            AutopilotMeasurementKind::AgentRunFanGrowth3d,
            AutopilotMeasurementKind::IncrementalFanGrowth14d,
            AutopilotMeasurementKind::IncrementalFanGrowth3d,
            AutopilotMeasurementKind::DurableFanGrowth30d,
        ] {
            let measurement = ClaimedAutopilotMeasurement {
                baseline_value: 14.8,
                ..claimed(kind)
            };
            assert!(kind.counts_attributed_fans(), "{kind:?}");
            assert!(
                measurement.counterfactual_value().abs() < f64::EPSILON,
                "{kind:?}"
            );
            let none = assess_measurement_effect(&measurement, 0.0, &HarmObservation::default())
                .expect("zero traced fans");
            assert_eq!(none.assessment, EffectAssessment::Neutral, "{kind:?}");
            let one = assess_measurement_effect(&measurement, 1.0, &HarmObservation::default())
                .expect("one traced fan");
            assert_eq!(one.assessment, EffectAssessment::Improved, "{kind:?}");
        }
    }

    /// Install checkpoints compare a window's new installs against the same
    /// width of new installs before the action. Nothing before and nothing
    /// after is Neutral — the case production scored Worsened 43 times in
    /// one week when the baseline was the standing install total.
    #[test]
    fn signal_installs_compare_like_windows() {
        for kind in [
            AutopilotMeasurementKind::SignalInstalls1d,
            AutopilotMeasurementKind::AgentRunSignalInstalls7d,
        ] {
            let quiet = claimed(kind);
            let flat =
                assess_measurement_effect(&quiet, 0.0, &HarmObservation::default()).expect("flat");
            assert_eq!(flat.assessment, EffectAssessment::Neutral, "{kind:?}");
            let one = assess_measurement_effect(&quiet, 1.0, &HarmObservation::default())
                .expect("one install");
            assert_eq!(one.assessment, EffectAssessment::Improved, "{kind:?}");
        }
    }

    /// A send's unsubscribe rate under half a percent is baseline churn, not
    /// harm the send caused — without the floor every send that cost one fan
    /// in a few hundred would read Worsened and feed the demotion guard.
    #[test]
    fn campaign_unsubscribe_reads_baseline_churn_as_neutral() {
        let measurement = claimed(AutopilotMeasurementKind::CampaignUnsubscribe7d);
        for rate in [0.0, 0.001, 0.0049] {
            let result = assess_measurement_effect(&measurement, rate, &HarmObservation::default())
                .expect("unsubscribe assessment");
            assert_eq!(
                result.assessment,
                EffectAssessment::Neutral,
                "rate {rate} should classify Neutral"
            );
        }
        let harmful = assess_measurement_effect(&measurement, 0.05, &HarmObservation::default())
            .expect("harmful");
        assert_eq!(harmful.assessment, EffectAssessment::Worsened);
    }

    /// The harm override: a flat metric over a send that still cost fans
    /// reads Worsened, while the same harm under real growth keeps the
    /// primary verdict — the cost is priced in `harm_fans`, not hidden.
    #[test]
    fn harm_with_no_positive_movement_classifies_worsened() {
        let measurement = claimed(AutopilotMeasurementKind::TicketRevenue72h);
        let harm = HarmObservation {
            unsubscribes: 2.0,
            ..HarmObservation::default()
        };
        let flat = assess_measurement_effect(&measurement, 0.0, &harm).expect("flat assessment");
        assert_eq!(flat.assessment, EffectAssessment::Worsened);
        // A signed kind takes the same verdict below zero — the level kinds
        // refuse a negative reading outright as malformed, so the override
        // only ever sees the zero-or-positive range for them.
        let signed = claimed(AutopilotMeasurementKind::IncrementalFanGrowth3d);
        let negative =
            assess_measurement_effect(&signed, -10.0, &harm).expect("negative assessment");
        assert_eq!(negative.assessment, EffectAssessment::Worsened);
        let grew = assess_measurement_effect(&measurement, 500.0, &harm).expect("grew assessment");
        assert_eq!(grew.assessment, EffectAssessment::Improved);
        let clean = assess_measurement_effect(&measurement, 0.0, &HarmObservation::default())
            .expect("clean assessment");
        assert_eq!(clean.assessment, EffectAssessment::Neutral);
    }

    /// Unsubscribe counts under baseline churn stay neutral — the audience
    /// would have lost them with no send at all, the same floor the
    /// unsubscribe-rate kind applies. Past the floor, the excess is harm
    /// the action earned; the other sources carry no baseline excuse.
    #[test]
    fn unsubscribe_churn_under_the_floor_stays_neutral() {
        let measurement = claimed(AutopilotMeasurementKind::TicketRevenue72h);
        // Two withdrawals across five hundred reached fans is 0.4% — under
        // the 0.5% floor, churn rather than earned harm.
        let churn = HarmObservation {
            unsubscribes: 2.0,
            contacted: 500.0,
            ..HarmObservation::default()
        };
        let flat = assess_measurement_effect(&measurement, 0.0, &churn).expect("flat");
        assert_eq!(flat.assessment, EffectAssessment::Neutral);
        // Three of five hundred crosses the floor — the excess is real.
        let harm = HarmObservation {
            unsubscribes: 3.0,
            ..churn
        };
        let flat = assess_measurement_effect(&measurement, 0.0, &harm).expect("flat");
        assert_eq!(flat.assessment, EffectAssessment::Worsened);
        // A complaint has no baseline excuse: one alone worsens a flat send.
        let complaint = HarmObservation {
            complaints: 1.0,
            ..HarmObservation::default()
        };
        let flat = assess_measurement_effect(&measurement, 0.0, &complaint).expect("flat");
        assert_eq!(flat.assessment, EffectAssessment::Worsened);
    }

    /// Small-count windows scatter around their expectation — a baseline of
    /// three installs observing none is ordinary Poisson noise, while a
    /// baseline of fifteen observing none is a real stop, and a quiet
    /// workspace gaining six is a real start.
    #[test]
    fn installs_counts_read_small_windows_as_noise() {
        for kind in [
            AutopilotMeasurementKind::SignalInstalls1d,
            AutopilotMeasurementKind::AgentRunSignalInstalls7d,
        ] {
            let small = ClaimedAutopilotMeasurement {
                baseline_value: 3.0,
                ..claimed(kind)
            };
            let flat =
                assess_measurement_effect(&small, 0.0, &HarmObservation::default()).expect("flat");
            assert_eq!(flat.assessment, EffectAssessment::Neutral, "{kind:?}");

            let real_stop = ClaimedAutopilotMeasurement {
                baseline_value: 15.0,
                ..claimed(kind)
            };
            let stopped = assess_measurement_effect(&real_stop, 0.0, &HarmObservation::default())
                .expect("stopped");
            assert_eq!(stopped.assessment, EffectAssessment::Worsened, "{kind:?}");

            let quiet = claimed(kind);
            let start =
                assess_measurement_effect(&quiet, 6.0, &HarmObservation::default()).expect("start");
            assert_eq!(start.assessment, EffectAssessment::Improved, "{kind:?}");
        }
    }

    /// The fan-growth noise floor scales with the counterfactual the
    /// measurement was scheduled against: a miss of a few against an
    /// expectation of a few is the scatter the count would have shown anyway;
    /// a miss of fifteen against twenty is real harm.
    #[test]
    fn attributed_fan_growth_floors_misses_at_counterfactual_scale() {
        for (baseline, observed) in [(3.35, -3.35), (0.43, -0.43)] {
            let measurement = ClaimedAutopilotMeasurement {
                baseline_value: baseline,
                ..claimed(AutopilotMeasurementKind::AgentRunFanGrowth3d)
            };
            let result =
                assess_measurement_effect(&measurement, observed, &HarmObservation::default())
                    .expect("fan growth assessment");
            assert_eq!(
                result.assessment,
                EffectAssessment::Neutral,
                "counterfactual {baseline} delta {observed} should classify Neutral"
            );
        }

        let measurement = ClaimedAutopilotMeasurement {
            baseline_value: 20.0,
            ..claimed(AutopilotMeasurementKind::AgentRunFanGrowth3d)
        };
        let result = assess_measurement_effect(&measurement, -15.0, &HarmObservation::default())
            .expect("fan growth assessment");
        assert_eq!(result.assessment, EffectAssessment::Worsened);
    }
}
