//! Operator control plane and provider-neutral state ingress ports.

use async_trait::async_trait;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotDecisionId, AutopilotMeasurementId, BeaconId, BookingAgentId,
    BookingTargetId, CityId, ContentSourceId, ContentSuggestionId, EventId, ExperimentId,
    ExperimentVariantId, GrowthMetricSeriesId, MarketSignalId, MerchProductId,
    OutreachOpportunityId, OutreachTargetId, PlayId, PromotionCampaignId, ReleasePlanId,
    TeamOpportunityId, VenueId, WorkspaceId,
    acquisition_channel::{ChannelAttribution, UnattributedReason},
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconKind, BeaconReplyDisposition},
    booking::{BookingReplyDisposition, BookingTargetKind},
    booking_agent::BookingAgentReplyDisposition,
    content_supply::ContentSourceKind,
    deliverability::DeliveryFault,
    experimentation::{ExperimentAllocationSlot, ExperimentMetric, assign_variant},
    fan_activation::MeaningfulAction,
    growth_metrics::{MetricDirection, MetricPlatform, MetricValueTier},
    live_opportunities::{BookingManagerPolicy, LiveTravelBand},
    market_intelligence::CityMarketSignalKind,
    next_best_action::{AuthorityState, RankFactor},
    objectives::{ObjectiveScope, ObjectiveState},
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
    performance::EffectAssessment,
    play_measurement::PlayClaim,
    plays::PlayKind,
    show_settlement::SettledShowCost,
    target_discovery::{CandidateSource, ChannelCost, RouteKind},
    tour_economics::TourEconomicsPolicy,
};
use serde::{Deserialize, Serialize, Serializer};
use time::OffsetDateTime;

use super::{
    growth_posture::GrowthPosture,
    measurement_ports::AutopilotMeasurementKind,
    model::{
        AutopilotActionPayload, AutopilotContext, PlayAnchorRef, PlayKindStanding,
        RecordPlaylistPlacement,
    },
};
use crate::{IdempotencyKey, RepositoryError, RequestId};

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotPolicySummary {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    pub version: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub guarded_until: Option<OffsetDateTime>,
    pub guardrail_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionBudgetGuardrailSummary {
    pub currency: String,
    pub maximum_total_daily_budget_minor: i64,
    pub maximum_monthly_spend_minor: i64,
    pub version: i64,
}

/// Non-sensitive owner shown consistently across staff surfaces. Contact email
/// stays private in the notification executor payload.
#[derive(Clone, Debug, Serialize)]
pub struct TeamAssigneeSummary {
    pub member_id: uuid::Uuid,
    pub member_key: String,
    pub display_name: String,
}

fn serialize_control_payload<S>(
    payload: &AutopilotActionPayload,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut value = serde_json::to_value(payload).map_err(serde::ser::Error::custom)?;
    let object = value.as_object_mut().ok_or_else(|| {
        serde::ser::Error::custom("autopilot payload must serialize as an object")
    })?;

    // The durable action enum intentionally retains its original Serde shape so
    // historical JSONB rows remain readable. The operator API is a separate
    // boundary: date-times are RFC3339 strings and executor-only recipient PII
    // is never exposed to Signal or another control-plane client.
    match payload {
        AutopilotActionPayload::ExecuteReleaseMilestone { release_at, .. } => {
            let formatted = release_at
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(serde::ser::Error::custom)?;
            object.insert(
                "release_at".to_owned(),
                serde_json::Value::String(formatted),
            );
        }
        AutopilotActionPayload::SendTeamAssignmentEmail { due_at, .. } => {
            object.remove("recipient_email");
            let formatted = (*due_at)
                .map(|value| value.format(&time::format_description::well_known::Rfc3339))
                .transpose()
                .map_err(serde::ser::Error::custom)?;
            object.insert(
                "due_at".to_owned(),
                formatted.map_or(serde_json::Value::Null, serde_json::Value::String),
            );
        }
        _ => {}
    }
    value.serialize(serializer)
}

/// Human-actionable Autopilot job waiting in the approval queue.
#[derive(Clone, Debug, Serialize)]
pub struct PendingAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    #[serde(serialize_with = "serialize_control_payload")]
    pub payload: AutopilotActionPayload,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approval_expires_at: Option<OffsetDateTime>,
    pub assignee: Option<TeamAssigneeSummary>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub assignment_due_at: Option<OffsetDateTime>,
    /// The executor capability this action needs, where it needs one at all.
    /// `None` means CrowdRelay carries it out itself.
    pub required_capability: Option<String>,
    /// Whether a live executor currently advertises that capability.
    ///
    /// False means approving this action queues it and nothing happens: the
    /// worker parks it as `awaiting_executor` until somebody advertises the
    /// capability. An operator who approves it has not passed it on, and a
    /// queue that cannot say so is asking for work it will silently discard.
    pub executor_ready: bool,
    /// Human-readable briefing: what to do, why, steps, and the content
    /// being approved. Generated from the payload so the frontend modal
    /// and the team email share a single source of truth.
    pub briefing: ActionBriefing,
    /// The payload fields an operator may edit on approve, with their
    /// current text — `draft_revision::revisable_fields` is the single
    /// definition, so the modal and the gate can never disagree about
    /// what is editable. Empty when the draft carries no revisable text.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub revisable: std::collections::BTreeMap<String, String>,
}

/// Compact immutable decision trail shown in the operator cockpit.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotDecision {
    pub id: crowdrelay_domain::AutopilotDecisionId,
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub confidence: Confidence,
    pub disposition: PolicyDisposition,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
}

/// A bounded human-only step returned by an executor when a free distribution
/// surface cannot be automated safely (for example CAPTCHA/login verification).
/// Only public destination data is exposed to staff surfaces; arbitrary executor
/// metadata remains private to the execution ledger.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotManualStep {
    pub destination: String,
    pub url: String,
    pub what_to_do: String,
    pub why_it_matters: String,
}

/// A human-readable briefing for a pending autopilot action.
///
/// Generated from the payload by the brain — tells the operator what to do,
/// why it matters, and what content they're approving. Single source of truth:
/// the frontend modal and the team assignment email both consume this struct.
#[derive(Clone, Debug, Serialize)]
pub struct ActionBriefing {
    /// One-line summary: "Zatwierdź draft posta na Reddit r/PolskaMuzyka"
    pub summary: String,
    /// 2-3 sentences explaining why this action matters and what happens
    /// if approved/rejected.
    pub why_it_matters: String,
    /// Ordered list of concrete steps the operator should take.
    pub steps: Vec<BriefingStep>,
    /// The actual content being approved (draft text, push notification,
    /// budget change details, etc.) — structured as key-value pairs so
    /// the frontend can render it without knowing every action kind.
    pub content: Vec<BriefingField>,
    /// When this needs to happen, as a human-readable note.
    /// E.g. "Termin: 5 wrz 2026, 20:00" or "Brak twardego terminu".
    pub deadline_note: String,
}

/// One concrete step in an action briefing.
#[derive(Clone, Debug, Serialize)]
pub struct BriefingStep {
    pub what_to_do: String,
    pub why_it_matters: String,
}

/// One labeled field in the briefing content section.
#[derive(Clone, Debug, Serialize)]
pub struct BriefingField {
    pub label: String,
    pub value: String,
}

/// Recent action execution evidence. This is deliberately compact and contains
/// no recipient PII; the cockpit is an exception surface, not a data browser.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub status: String,
    pub attempt_count: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub last_error_kind: Option<String>,
    pub executor_status: Option<String>,
    pub executor_id: Option<String>,
    pub provider_reference: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub executor_reported_at: Option<OffsetDateTime>,
    pub manual_steps: Vec<AutopilotManualStep>,
}

/// A delayed, measured effect of one successfully executed Autopilot action.
/// This is evidence for calibration, not proof of causality; the metric name is
/// intentionally explicit about proxies where exact attribution is unavailable.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotEffect {
    pub measurement_id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub context: AutopilotContext,
    pub measurement_kind: AutopilotMeasurementKind,
    pub assessment: crowdrelay_domain::performance::EffectAssessment,
    pub delta_basis_points: i32,
    pub baseline_value: f64,
    pub observed_value: f64,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

/// Exception-first operator view. The UI should emphasize `needs_you`; recent
/// decisions/actions and measured effects exist as evidence, not as another dashboard
/// operators must watch.
#[derive(Clone, Debug, Serialize)]
pub struct RumMetricSummary {
    pub surface: String,
    pub metric_key: String,
    pub samples_24h: i64,
    pub p75: f64,
    pub p95: f64,
}

/// The content page's pipeline read: which pieces of real material are live,
/// which drafts wait on a person, and what each draft was built from.
///
/// Deliberately separate from [`AutopilotControlOverview`]: the overview fans
/// out across eleven reads to staff the whole cockpit, while the content page
/// only needs its own queue slice — asking for the overview here made the
/// page's first paint pay for reads it never renders.
#[derive(Clone, Debug, Serialize)]
pub struct ContentPipeline {
    /// Content sources still usable as material (active and unexpired).
    pub live_sources: i64,
    /// Content actions waiting for approval, oldest first.
    pub pending: Vec<PendingAutopilotAction>,
    /// `content_source_id` → title for every source a pending payload cites,
    /// so the page can say what a draft is built from without a second fetch.
    pub source_titles: std::collections::BTreeMap<String, String>,
    /// The §4d-3.2 voice signal: how far operator edits on approve moved the
    /// machine's words. `None` until the first revised approval — a queue
    /// with no edits has no signal, and a 0 would lie that the machine
    /// writes perfectly.
    pub revision_trend: Option<RevisionTrend>,
}

/// How the band's edits to drafts are trending — the only honest measure of
/// whether voice-matching is working. Distance should fall over time; when it
/// does not, the machine's words keep needing the same human fix.
#[derive(Clone, Debug, Serialize)]
pub struct RevisionTrend {
    /// Edited fields in the last 30 days — the n any reading of the trend
    /// must respect before it is a trend at all.
    pub revised_fields_30d: i64,
    /// Actions approved with an edit in the same window.
    pub revised_actions_30d: i64,
    /// Mean changed characters per edited field over the window.
    pub avg_distance_chars_30d: i64,
    /// Per-week buckets, oldest first — direction matters more than any
    /// single number, and the panel draws this without another query.
    pub weekly: Vec<RevisionTrendWeek>,
}

/// One week of revision signal.
#[derive(Clone, Debug, Serialize)]
pub struct RevisionTrendWeek {
    /// Monday of the week, `YYYY-MM-DD`.
    pub week_start: String,
    /// Fields edited that week.
    pub revised_fields: i64,
    /// Mean changed characters per field that week.
    pub avg_distance_chars: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlOverview {
    pub policies: Vec<AutopilotPolicySummary>,
    pub promotion_budget_guardrails: Vec<PromotionBudgetGuardrailSummary>,
    pub needs_you: Vec<PendingAutopilotAction>,
    /// Active human owners eligible for an explicit operator re-assignment.
    /// No contact PII is exposed on staff read models.
    pub available_assignees: Vec<TeamAssigneeSummary>,
    pub recent_decisions: Vec<RecentAutopilotDecision>,
    pub recent_actions: Vec<RecentAutopilotAction>,
    pub recent_effects: Vec<RecentAutopilotEffect>,
    pub queued_actions: i64,
    pub processing_actions: i64,
    pub succeeded_24h: i64,
    pub failed_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub awaiting_executor: i64,
    pub release_ledger: ReleaseLedgerOverview,
    pub rum_metrics_24h: Vec<RumMetricSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffOpportunity {
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub confidence: Confidence,
    pub reason: String,
    pub needs_approval: bool,
}

/// How one acquisition channel actually performed.
///
/// Signups and activated fans are reported side by side and never merged,
/// because a channel that produced two hundred signups and four active people
/// is a bad channel wearing a good number.
#[derive(Clone, Debug, Serialize)]
pub struct ChannelPerformance {
    /// Where these people came from, or an honest statement that we cannot say.
    pub attribution: ChannelAttribution,
    /// People who arrived through this channel and are still here.
    pub signups: u32,
    /// Signed up, consented, and did something meaningful in the last 30 days.
    pub activated_30d: u32,
    /// Activated out of signed up, in basis points. `None` when there are no
    /// signups to divide by — a rate from an empty denominator is not a zero.
    ///
    /// Read it beside `departed`. The denominator is the people who stayed,
    /// so a channel that lost nine of every ten arrivals can still show a
    /// perfect rate over the one who remained.
    pub activation_basis_points: Option<u32>,
    /// People who arrived through this channel and have since gone —
    /// unsubscribed, or suppressed (bounced, complained, or deleted their
    /// account). Counted, never named: a deleted account is a number here
    /// and nothing else.
    ///
    /// The counts above net churn out by construction, which is right for
    /// "how many fans did this channel produce" and blind to "how many did
    /// it burn to produce them". Without this, a channel that churns hard
    /// is indistinguishable from one that brought fewer people who all
    /// stayed (§4e-7 #4).
    pub departed: u32,
    /// The strongest thing anybody from this channel actually did, so a channel
    /// that produces ticket buyers is distinguishable from one that produces
    /// people who clicked once.
    pub best_action: Option<MeaningfulAction>,
}

/// The whole picture, with the unattributable part kept in view rather than
/// quietly dropped.
#[derive(Clone, Debug, Serialize)]
pub struct AcquisitionChannels {
    pub channels: Vec<ChannelPerformance>,
    pub total_signups: u32,
    pub total_activated_30d: u32,
    /// Did something meaningful in the last 30 days, however they arrived —
    /// retention, not acquisition. The campaign brief's headline number,
    /// derived from the facts on every read rather than stored and left to go
    /// stale.
    pub active_30d: u32,
    /// Everyone contactable right now, whatever their signup date. The
    /// denominator every owned-audience decision should be checked against.
    pub reachable_consented: u32,
    /// Fans active in both the current and previous 30-day windows. The
    /// campaign plan's retention KPI — not just "how many are active now"
    /// but "how many stayed active".
    pub retained_30d: u32,
    /// People whose channel could not be established, by reason. Reported
    /// prominently: a report that hides its unknowns is how a 40% attribution
    /// gap goes unnoticed for a month.
    pub unattributed: Vec<UnattributedGroup>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct UnattributedGroup {
    pub reason: UnattributedReason,
    /// What to do about it. Each reason is a different fix.
    pub remedy: &'static str,
    pub signups: u32,
    pub activated_30d: u32,
    /// Arrived, then left — the same count `ChannelPerformance::departed`
    /// carries, for the people we cannot attribute to a channel.
    pub departed: u32,
}

/// The band's vehicles and rates, as an operator reads and edits them.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TourEconomicsSummary {
    pub policy: TourEconomicsPolicy,
    /// Optimistic-concurrency guard. Two people editing the van in the same
    /// afternoon should not silently overwrite each other's fuel price.
    pub version: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct SetTourEconomics {
    pub policy: TourEconomicsPolicy,
    pub expected_version: i64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TourEconomicsMutation {
    pub operation_id: uuid::Uuid,
    pub version: i64,
    pub replayed: bool,
}

/// One entry of the cross-context Next Best Action queue.
///
/// A view, not a stored row: every field is read from a decision, its action
/// payload or the subject's own date. Nothing here is denormalized into a table,
/// so the queue can never disagree with the evidence it came from.
#[derive(Clone, Debug, Serialize)]
pub struct NextBestAction {
    pub position: u8,
    /// The finding itself, so an operator can say "we did this ourselves"
    /// about exactly this row rather than about a subject and a guess.
    pub decision_id: uuid::Uuid,
    /// The newest action this finding produced, where one exists — what a
    /// "do it" click approves through the existing approval path.
    pub action_id: Option<uuid::Uuid>,
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub authority: AuthorityState,
    pub confidence: Confidence,
    pub reason: String,
    pub recommended_action: String,
    /// The factor that decided this entry's position against its neighbour.
    pub ranked_by: RankFactor,
    /// What happens if this entry is ignored — a statement about the system's
    /// own behaviour, never a predicted business outcome.
    pub consequence: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub due_at: Option<OffsetDateTime>,
    pub value_tier: Option<MetricValueTier>,
    /// Measured deviation or overdue ratio in basis points. Deliberately not a
    /// currency amount: the system does not know what a stalled channel is
    /// worth, and a plausible figure would be the most convincing lie here.
    pub deviation_basis_points: Option<u32>,
    /// Human-readable briefing generated from the action payload: summary,
    /// why it matters, concrete steps, and the content being approved.
    /// `None` when the payload could not be parsed or carried no briefing.
    pub briefing: Option<ActionBriefing>,
}

/// One community relay batch — a piece of content's whole community spread,
/// read as a single campaign rather than a card per community.
///
/// The card this feeds answers the only question the batch asks: "does this
/// post go to these communities, one an hour?" — the content, the image, the
/// target list and the cadence in one view, and after approval the same row
/// reports what the spread actually did.
#[derive(Clone, Debug, Serialize)]
pub struct CommunityRelayBatchView {
    /// The batch's identity: the content source every delivery carries.
    pub source_id: uuid::Uuid,
    /// `awaiting_approval`, `approved`, `revoked` or `done`.
    pub status: String,
    /// Seconds the executor leaves between two posts from this batch — the
    /// cadence the operator approved.
    pub interval_seconds: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approved_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Approval + 7 days: the window the progress counts report against.
    #[serde(with = "time::serde::rfc3339::option")]
    pub observe_until: Option<OffsetDateTime>,
    /// The source's own title and link — what the posts are about.
    pub source_title: Option<String>,
    pub source_url: Option<String>,
    /// The image the posts carry, from the source row — never model-supplied.
    pub image_url: Option<String>,
    /// One drafted delivery as a sample of what the posts look like. Per-target
    /// copy differs; this is the first draft's title and body.
    pub sample_title: Option<String>,
    pub sample_body: Option<String>,
    /// Every community the batch is aimed at, with where its delivery stands.
    pub targets: Vec<CommunityRelayDelivery>,
    /// Progress counts across the batch's `community_posts` rows.
    pub posts_posted: i64,
    pub posts_pending: i64,
    pub posts_failed: i64,
    /// Clicks the batch's tracked links earned — the measurement half of the
    /// card, live until `observe_until` closes the window.
    pub clicks: i64,
}

/// One community inside a relay batch: where its delivery stands.
///
/// Named `Delivery`, not `Target`, because `content_supply::CommunityRelayTarget`
/// already names the drafting input — target id plus language. This is the
/// read side: the delivery's own state, not the target record.
#[derive(Clone, Debug, Serialize)]
pub struct CommunityRelayDelivery {
    /// The delivery's action — the key a batch-approval revision names when
    /// the operator edits this target's words on the card.
    pub action_id: uuid::Uuid,
    pub subreddit: String,
    /// The post's language tag, when the target row carries one.
    pub language: Option<String>,
    /// `awaiting_approval`, `queued`, `posted`, `failed`, `cancelled` —
    /// the delivery's own state, not the batch's.
    pub status: String,
    /// The title and body this delivery will post — the card's editable
    /// surface, per target, because each community gets its own wording
    /// and language. Edits go back through `approve` as revisions keyed
    /// by `action_id`.
    pub draft_title: Option<String>,
    pub draft_body: Option<String>,
    /// The Reddit permalink once the post landed.
    #[serde(with = "time::serde::rfc3339::option")]
    pub posted_at: Option<OffsetDateTime>,
    pub post_url: Option<String>,
}

/// One row of the scout shortlist — every tracked opportunity, whether or not
/// it is currently actionable.
///
/// A view, not a stored row: every field is read from
/// `team_opportunities` plus the newest autopilot decision naming the
/// opportunity as its subject. Ineligible and stale rows stay on the list with
/// their reason — a shortlist that silently drops what it rejected is a
/// shortlist the operator has to double-check by hand.
#[derive(Clone, Debug, Serialize)]
pub struct OpportunityShortlistEntry {
    pub opportunity_id: uuid::Uuid,
    pub kind: String,
    pub source: String,
    pub external_key: String,
    pub title: String,
    pub organization: String,
    /// The link the finding stands on. `None` means no usable destination was
    /// ever supplied — such rows are always `stale_reason = "no_destination"`.
    pub destination_url: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub source_observed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub deadline: Option<OffsetDateTime>,
    pub status: String,
    /// Why a terminal row closed. `null` for live rows.
    pub status_reason: Option<String>,
    pub eligible: bool,
    /// Money fields stay `null` when unknown — never `0`, which would read as
    /// "free" or "unpaid" to somebody scanning the list.
    pub expected_fee_minor: Option<i64>,
    pub estimated_cost_minor: Option<i64>,
    pub application_fee_minor: Option<i64>,
    /// The row's own ISO currency for those amounts.
    pub currency: String,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub confidence_basis_points: u16,
    /// Why the row cannot be worked right now, when it cannot:
    /// `no_destination`, `stale_observation`, `deadline_passed`,
    /// `ineligible`, `closed`.
    pub stale_reason: Option<String>,
    /// Newest autopilot decision naming this opportunity, if any. The
    /// disposition is what a decision records — `require_approval` and its
    /// siblings — not a lifecycle status.
    pub latest_decision_id: Option<uuid::Uuid>,
    pub latest_decision_kind: Option<String>,
    pub latest_decision_disposition: Option<String>,
    /// Whether the cost figure came from the tour-economics engine rather
    /// than from whatever was typed in.
    pub costed_from_logistics: bool,
}

/// The scout shortlist: what was found, what it would cost, and what
/// happened to it. `degraded` names the sections that could not be read —
/// the answer is still 200, and the absent sections stay `null`-shaped.
#[derive(Clone, Debug, Serialize)]
pub struct OpportunityShortlist {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub entries: Vec<OpportunityShortlistEntry>,
    /// Rows whose `stale_reason` is set.
    pub stale_count: i64,
    /// Rows whose status is terminal (`lost`, `dismissed`, `won`).
    pub closed_count: i64,
    /// Rows not eligible for any pipeline path.
    pub ineligible_count: i64,
    /// Sections that could not be read this time.
    pub degraded: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffShowTask {
    pub event_id: EventId,
    pub event_title: String,
    pub task_key: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
}

/// Time-sensitive fact surfaced by the Chief-of-Staff read model. This does
/// not create another task system: approvals and team-opportunity deadlines
/// remain owned by their existing bounded contexts.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffAttentionItem {
    pub kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub title: String,
    pub detail: String,
    #[serde(with = "time::serde::rfc3339")]
    pub due_at: OffsetDateTime,
    pub urgency: String,
}

/// One line of "the agent did this", "it is about to do this" or "this is
/// waiting for you", grouped by what the work actually is.
///
/// Counts by action kind rather than a list of rows. An operator reading a
/// morning brief needs to know that eleven fan messages went out unattended,
/// not eleven identifiers.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffActivity {
    pub action_kind: String,
    pub action_class: String,
    pub count: i64,
}

/// Something the agent did not do, and why.
///
/// The section with no equivalent anywhere else in the system. Every other read
/// model reports what happened; an autonomous agent that only reports its
/// successes is one whose gaps are invisible until somebody goes looking.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffStopped {
    /// `play_step_skipped`, `action_failed`, `play_retired` or
    /// `outcome_insufficient`.
    pub kind: String,
    /// The stored reason, verbatim. Never a summary: `window_closed` and
    /// `no_eligible_recipients` are different problems with different fixes.
    pub reason: String,
    pub count: i64,
    pub detail: String,
}

/// A declared target the operator should hear about without asking.
///
/// Only the ones that warrant it: a target being met is good news that can wait
/// for somebody to look, and one nobody can measure belongs with the gaps.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffObjective {
    pub platform: String,
    pub metric_key: String,
    pub scope_kind: String,
    /// `behind` or `missed`.
    pub state: String,
    pub progress_basis_points: u32,
    pub shortfall: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub deadline: OffsetDateTime,
}

/// Something that moved, and what the number is allowed to prove.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffMovement {
    pub subject: String,
    /// `attributed` or `correlational`, carried so the strength of the claim
    /// travels with the number into the brief.
    pub claim: String,
    pub assessment: String,
    /// Absent when the baseline was too flat to carry a percentage.
    pub delta_basis_points: Option<i32>,
}

/// Deterministic daily operating brief. `estimated_minutes_saved_24h` is a
/// coarse action-kind workload estimate, not fabricated AI precision.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotChiefOfStaff {
    pub executed_24h: i64,
    pub failed_24h: i64,
    pub needs_you: i64,
    /// An estimate, not a measurement: a flat per-action-kind guess at how
    /// long the work would have taken a person. `estimated_minutes_basis`
    /// carries the assumption so no surface can present it as measured.
    pub estimated_minutes_saved_24h: i64,
    /// The assumption behind `estimated_minutes_saved_24h`, stated where the
    /// number travels — per-kind minutes, not observed work.
    pub estimated_minutes_basis: &'static str,
    /// Measured turnaround: median minutes from `assigned_at` to
    /// `completed_at` for assignments completed in the last 7 days. `None`
    /// when nothing completed — a turnaround nobody finished has no median,
    /// and 0 would read as instant work.
    pub median_assignment_turnaround_minutes_7d: Option<i64>,
    /// How many completions the median covers — the n to check before
    /// reading a trend into it.
    pub assignments_completed_7d: i64,
    /// Actions whose outcome was measured in the last 7 days, one verdict
    /// per action (its latest outcome-kind measurement). Process checks are
    /// excluded — they are counted in `process_checks_*` below.
    pub measured_improved_7d: i64,
    pub measured_neutral_7d: i64,
    pub measured_worsened_7d: i64,
    /// Measurements in the last 7 days that graded the worker's own output
    /// (`AutopilotMeasurementKind::PROCESS_CHECKS`), and how many of those
    /// the worker passed. Not outcomes: no fan did anything.
    pub process_checks_7d: i64,
    pub process_checks_improved_7d: i64,
    pub emitted_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub attention_items: Vec<ChiefOfStaffAttentionItem>,
    pub top_opportunities: Vec<ChiefOfStaffOpportunity>,
    pub show_tasks: Vec<ChiefOfStaffShowTask>,
    /// What the agent did with nobody watching, in the last 24 hours.
    pub acted_alone_24h: Vec<ChiefOfStaffActivity>,
    /// What it will do next unless somebody stops it.
    pub about_to_act: Vec<ChiefOfStaffActivity>,
    /// What it will not do until somebody says yes.
    pub parked_for_approval: Vec<ChiefOfStaffActivity>,
    /// What it stopped, and why. Read before the rest: this is the agent
    /// reporting its own gaps, and it is the only section nothing else covers.
    pub stopped: Vec<ChiefOfStaffStopped>,
    /// What moved, with the strength of the claim attached to every number.
    pub moved: Vec<ChiefOfStaffMovement>,
    /// Declared targets that are behind or already missed. A target being met
    /// is good news that can wait; one nobody can measure is a gap, and is
    /// reported as one.
    pub objectives_at_risk: Vec<ChiefOfStaffObjective>,
}

/// Authority-only policy mutation. Domain-specific thresholds remain typed
/// config owned by code/migrations until a dedicated validated editor exists.
#[derive(Clone, Debug)]
pub struct SetAutopilotAuthority {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    /// Optimistic-concurrency guard from the overview read model.
    pub expected_version: i64,
    /// Optional domain-policy knobs (already validated by the caller against
    /// [`AutopilotPolicyConfig::parse_for`]). `None` leaves the stored knobs
    /// alone; an empty object resets them to defaults.
    pub config: Option<serde_json::Value>,
}

/// Operator command for the envelope, including the kill switch.
///
/// The envelope had no mutation path at all until the agent was already live:
/// it was written by its migration and changeable only by hand in psql. The one
/// control an operator reaches for in a hurry was the one with no button, which
/// is the wrong shape for a safety mechanism.
///
/// Every field is required rather than optional. A partial update of a limit
/// set is a way to widen one ceiling while believing you tightened another, and
/// `expected_version` makes two operators editing at once a refusal instead of
/// a silent last-writer-wins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetGrowthEnvelope {
    pub agent_enabled: bool,
    pub dry_run: bool,
    pub weekly_owned_audience_touches: u32,
    pub weekly_third_party_touches: u32,
    /// Third-party approaches allowed inside a rolling day — the volume wall
    /// behind the per-subject cooldown (2.4).
    pub daily_third_party_touches: u32,
    pub subject_cooldown_hours: u32,
    pub max_recipients_per_step: u32,
    /// Decisions the agent may put in front of a person per rolling week.
    /// When spent, findings surface as recommendations rather than approval
    /// requests. Zero means never ask.
    pub weekly_approval_requests: u32,
    /// Unattended actions one context may take per rolling week while below
    /// its evidence floor, so the floor can be reached. Zero disables the
    /// warm-up and restores the behaviour that never reached it.
    pub weekly_bootstrap_actions: u32,
    pub expected_version: i64,
    /// Tenant park flag: when true, the autopilot cycle skips entirely.
    /// Set by the Control Plane on park, cleared on resume.
    pub parked: bool,
}

/// Applies one of the three named postures atomically.
///
/// One write sets every context level, all four class ceilings and the
/// envelope switches from the posture template — the alternative is
/// twenty-six endpoint calls and a missed switch. `expected_version` guards
/// the posture row itself; individual knobs stay editable afterwards and are
/// only overwritten the next time somebody applies a posture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetGrowthPosture {
    pub posture: GrowthPosture,
    pub expected_version: i64,
}

/// What the operator sees when they ask which posture is live.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthPostureView {
    /// `None` until somebody has applied a posture for the first time; every
    /// authority surface still holds its provisioned defaults meanwhile, so
    /// "never set" reads as exactly what it is rather than as a guess.
    #[serde(rename = "posture")]
    pub posture: Option<GrowthPosture>,
    pub expected_version: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub set_at: Option<OffsetDateTime>,
}

/// Result of an audited operator mutation.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub status: String,
    pub replayed: bool,
}

include!("control/state_ports.rs");
include!("control/runtime_ports.rs");
include!("control/growth_ports.rs");
include!("control/growth_metric_ports.rs");
include!("control/target_discovery_ports.rs");
include!("control/play_ports.rs");
include!("control/show_cost_ports.rs");
include!("control/objective_ports.rs");
include!("control/scorecard_views.rs");
