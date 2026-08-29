//! Opportunity identity — stable, hashable IDs for growth opportunities.
//!
//! The brain needs to track opportunities across cycles: it sees the same
//! "post to r/MetalMusic about the upcoming show" opportunity every cycle
//! until it's dispatched or expires. Without stable identities, the brain
//! can't deduplicate, track lifecycle, or build an opportunity graph.
//!
//! # Identity structure
//!
//! An opportunity identity is composed of:
//! - **Template**: which worker would address it (reddit-scanner, etc.)
//! - **Target**: the specific community/person/venue being targeted
//! - **Action kind**: what kind of action (scan, post, pitch, invite)
//! - **Context hash**: the dispatch context hash (event proximity, trend, etc.)
//!
//! This produces a stable string ID like:
//! `community-engager:r_MetalMusic:post:event_2:stagnant`

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::causal_model::DispatchContext;
use crate::exploration::context_hash;

/// The kind of action an opportunity represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityAction {
    /// Scan a community for intelligence (reddit-scanner).
    Scan,
    /// Post content to a community (community-engager, social-post).
    Post,
    /// Pitch to press/media (press-pitch).
    Pitch,
    /// Invite fans to install Signal (signal-inviter).
    Invite,
    /// Analyze growth strategy (growth-strategist).
    Analyze,
}

impl OpportunityAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Post => "post",
            Self::Pitch => "pitch",
            Self::Invite => "invite",
            Self::Analyze => "analyze",
        }
    }

    /// Infers the action kind from a worker template ID.
    /// Falls back to `Scan` for unknown templates (the safest
    /// read-only action).
    #[must_use]
    pub fn from_template(template_id: &str) -> Self {
        if template_id.starts_with("reddit-scanner") {
            Self::Scan
        } else if template_id.starts_with("community-engager") {
            Self::Post
        } else if template_id.starts_with("signal-inviter") {
            Self::Invite
        } else if template_id.starts_with("press-pitch") || template_id.starts_with("outreach") {
            Self::Pitch
        } else {
            Self::Scan
        }
    }
}

/// A stable identity for a growth opportunity.
///
/// Two opportunities with the same identity are the same opportunity —
/// the brain should not dispatch both. The identity is deterministic:
/// given the same template, target, action, and context, the identity
/// is always the same.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct OpportunityId {
    /// The worker template that would address this opportunity.
    pub template_id: String,
    /// The specific target (subreddit name, venue name, etc.).
    pub target: String,
    /// The kind of action.
    pub action: OpportunityAction,
    /// The context hash (event proximity, trend, subreddit type, etc.).
    pub context_hash: String,
}

impl OpportunityId {
    /// Creates a new opportunity identity from its components.
    #[must_use]
    pub fn new(
        template_id: &str,
        target: &str,
        action: OpportunityAction,
        context: &DispatchContext,
    ) -> Self {
        Self {
            template_id: template_id.to_owned(),
            target: target.to_owned(),
            action,
            context_hash: context_hash(context),
        }
    }

    /// Creates an opportunity identity from a pre-computed context hash.
    #[must_use]
    pub fn with_context_hash(
        template_id: &str,
        target: &str,
        action: OpportunityAction,
        ctx_hash: &str,
    ) -> Self {
        Self {
            template_id: template_id.to_owned(),
            target: target.to_owned(),
            action,
            context_hash: ctx_hash.to_owned(),
        }
    }
}

impl fmt::Display for OpportunityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.template_id,
            self.target,
            self.action.as_str(),
            self.context_hash
        )
    }
}

/// The lifecycle state of an opportunity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityState {
    /// The opportunity has been identified but not yet dispatched.
    #[default]
    Open,
    /// The brain has dispatched a worker to address this opportunity.
    Dispatched,
    /// The dispatch completed and measurement is in progress.
    Measuring,
    /// The measurement is complete and the outcome has been recorded.
    Resolved,
    /// The opportunity expired before the brain dispatched it.
    Expired,
    /// The brain decided not to pursue this opportunity (low EFE, budget).
    Skipped,
}

impl OpportunityState {
    /// Returns true if the opportunity is still actionable.
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Returns true if the opportunity has been dispatched or resolved.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Expired | Self::Skipped)
    }
}

/// A tracked opportunity with its current lifecycle state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedOpportunity {
    pub id: OpportunityId,
    pub state: OpportunityState,
    /// EFE score at the time of identification (lower = better).
    pub efe_score: f64,
    /// When the opportunity was first identified.
    pub identified_at: time::OffsetDateTime,
    /// When the opportunity was dispatched, if applicable.
    pub dispatched_at: Option<time::OffsetDateTime>,
    /// The action_id if dispatched, for linking to measurements.
    pub action_id: Option<uuid::Uuid>,
}

// ─── Episode / Trajectory Model ─────────────────────────────────────────────

/// The kind of event in a fan's trajectory.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeEventKind {
    /// The brain dispatched a worker (treatment applied).
    Dispatch,
    /// A measurement was taken (fans counted, installs recorded).
    Measurement,
    /// A fan converted (installed Signal, bought a ticket, etc.).
    Conversion,
    /// The episode expired without conversion.
    Expired,
}

/// A single event in a fan's trajectory — one step in the episode.
///
/// Events are ordered by `occurred_at` and record what the brain did and
/// what happened as a result. This is the raw data for temporal credit
/// assignment: which actions contributed to which outcomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeEvent {
    /// What kind of event this is.
    pub kind: EpisodeEventKind,
    /// The worker template that was dispatched (for Dispatch events).
    pub template_id: Option<String>,
    /// The action_id linking to the autopilot action (for Dispatch events).
    pub action_id: Option<uuid::Uuid>,
    /// The measured outcome (for Measurement/Conversion events).
    /// Signed: positive = fans gained, negative = fans lost.
    pub observed_outcome: Option<f64>,
    /// When the event occurred.
    pub occurred_at: time::OffsetDateTime,
}

impl EpisodeEvent {
    /// Creates a dispatch event.
    #[must_use]
    pub fn dispatch(
        template_id: &str,
        action_id: uuid::Uuid,
        occurred_at: time::OffsetDateTime,
    ) -> Self {
        Self {
            kind: EpisodeEventKind::Dispatch,
            template_id: Some(template_id.to_owned()),
            action_id: Some(action_id),
            observed_outcome: None,
            occurred_at,
        }
    }

    /// Creates a measurement event.
    #[must_use]
    pub fn measurement(observed_outcome: f64, occurred_at: time::OffsetDateTime) -> Self {
        Self {
            kind: EpisodeEventKind::Measurement,
            template_id: None,
            action_id: None,
            observed_outcome: Some(observed_outcome),
            occurred_at,
        }
    }

    /// Creates a conversion event.
    #[must_use]
    pub fn conversion(observed_outcome: f64, occurred_at: time::OffsetDateTime) -> Self {
        Self {
            kind: EpisodeEventKind::Conversion,
            template_id: None,
            action_id: None,
            observed_outcome: Some(observed_outcome),
            occurred_at,
        }
    }

    /// Creates an expiry event.
    #[must_use]
    pub fn expired(occurred_at: time::OffsetDateTime) -> Self {
        Self {
            kind: EpisodeEventKind::Expired,
            template_id: None,
            action_id: None,
            observed_outcome: None,
            occurred_at,
        }
    }
}

/// The lifecycle status of an episode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeStatus {
    /// The episode is ongoing — events are still being recorded.
    #[default]
    Active,
    /// The episode ended with a conversion.
    Converted,
    /// The episode expired without conversion.
    Expired,
}

/// An opportunity episode — the full trajectory of a fan (or audience
/// segment) from first touch to conversion (or expiry).
///
/// The episode model is the foundation for temporal credit assignment. By
/// recording the full sequence of dispatches, measurements, and conversions,
/// the brain can later attribute outcomes to the correct actions — not just
/// the last action before the outcome (recency bias) or the first action
/// (primacy bias).
///
/// # Example trajectory
///
/// ```text
/// t=0:  Dispatch reddit-scanner → r_MetalMusic
/// t=7:  Measurement: +2 fans
/// t=14: Dispatch community-engager → r_MetalMusic
/// t=21: Measurement: +5 fans
/// t=28: Dispatch signal-inviter → r_MetalMusic
/// t=35: Conversion: +1 Signal install
/// ```
///
/// The credit assignment question: how much of the +1 conversion was caused
/// by the scanner vs. the engager vs. the inviter? The episode records the
/// full trajectory so this can be computed in Phase 3.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpportunityEpisode {
    /// A stable identifier for this episode (e.g. the opportunity ID).
    pub id: String,
    /// The workspace this episode belongs to.
    pub workspace_id: uuid::Uuid,
    /// The target audience segment (e.g. "r_MetalMusic").
    pub target: String,
    /// The ordered sequence of events in the trajectory.
    pub events: Vec<EpisodeEvent>,
    /// The current lifecycle status.
    pub status: EpisodeStatus,
    /// When the episode started (first event).
    pub started_at: time::OffsetDateTime,
    /// When the episode reached a terminal state, if ever.
    pub ended_at: Option<time::OffsetDateTime>,
}

impl OpportunityEpisode {
    /// Creates a new episode with the first dispatch event.
    #[must_use]
    pub fn new(
        id: &str,
        workspace_id: uuid::Uuid,
        target: &str,
        first_event: EpisodeEvent,
    ) -> Self {
        let started_at = first_event.occurred_at;
        Self {
            id: id.to_owned(),
            workspace_id,
            target: target.to_owned(),
            events: vec![first_event],
            status: EpisodeStatus::Active,
            started_at,
            ended_at: None,
        }
    }

    /// Appends an event to the episode's trajectory.
    pub fn record(&mut self, event: EpisodeEvent) {
        // Update status based on event kind.
        match event.kind {
            EpisodeEventKind::Conversion => {
                self.status = EpisodeStatus::Converted;
                self.ended_at = Some(event.occurred_at);
            }
            EpisodeEventKind::Expired => {
                self.status = EpisodeStatus::Expired;
                self.ended_at = Some(event.occurred_at);
            }
            _ => {}
        }
        self.events.push(event);
    }

    /// Returns the total observed outcome across all measurement and
    /// conversion events. Signed: positive = net fans gained.
    #[must_use]
    pub fn total_outcome(&self) -> f64 {
        self.events.iter().filter_map(|e| e.observed_outcome).sum()
    }

    /// Returns the number of dispatch events in the episode.
    #[must_use]
    pub fn dispatch_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.kind == EpisodeEventKind::Dispatch)
            .count()
    }

    /// Returns the templates dispatched in this episode, in order.
    #[must_use]
    pub fn dispatched_templates(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter(|e| e.kind == EpisodeEventKind::Dispatch)
            .filter_map(|e| e.template_id.as_deref())
            .collect()
    }

    /// Returns true if the episode has reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status != EpisodeStatus::Active
    }

    /// Returns the duration of the episode in seconds (or None if ongoing).
    #[must_use]
    pub fn duration_seconds(&self) -> Option<i64> {
        self.ended_at
            .map(|end| (end - self.started_at).whole_seconds())
    }
}

// ─── Temporal Credit Assignment ─────────────────────────────────────────────

/// The result of credit assignment for one dispatch in an episode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreditAssignment {
    /// The template that was dispatched.
    pub template_id: String,
    /// The action_id of the dispatch.
    pub action_id: uuid::Uuid,
    /// The credit allocated to this dispatch — how much of the episode's
    /// total outcome is attributed to this action. Signed: can be negative.
    pub credit: f64,
    /// The position of this dispatch in the episode (0-indexed).
    pub position: usize,
    /// The total number of dispatches in the episode.
    pub total_dispatches: usize,
}

/// The default discount factor for temporal credit assignment. Earlier
/// dispatches receive exponentially less credit than later ones, reflecting
/// that later actions are more directly responsible for the outcome.
///
/// With γ=0.9, the last dispatch gets full credit, the second-to-last gets
/// 90%, the third-to-last gets 81%, etc. (normalized so credits sum to the
/// total outcome).
pub const DEFAULT_CREDIT_DISCOUNT: f64 = 0.9;

/// Allocates credit for an episode's outcome across its dispatch events using
/// discounted temporal credit assignment.
///
/// The total outcome is distributed across dispatches with exponentially
/// decaying weight: the last dispatch gets the most credit, earlier ones get
/// less. The weights are normalized so the credits sum to the total outcome.
///
/// # Algorithm
///
/// For N dispatches with discount γ:
/// ```text
/// weight_i = γ^(N-1-i)
/// credit_i = weight_i / Σ weight_j × total_outcome
/// ```
///
/// With γ=1.0, this is equal credit. With γ<1, later dispatches get more.
///
/// # When to use
///
/// Use this when there are no intermediate measurement events between
/// dispatches. When measurements exist, prefer
/// [`credit_allocation_with_measurements`] which uses the incremental
/// outcomes directly.
#[must_use]
pub fn credit_allocation(episode: &OpportunityEpisode, discount: f64) -> Vec<CreditAssignment> {
    let dispatches: Vec<&EpisodeEvent> = episode
        .events
        .iter()
        .filter(|e| e.kind == EpisodeEventKind::Dispatch)
        .collect();
    let n = dispatches.len();
    if n == 0 {
        return Vec::new();
    }
    let total_outcome = episode.total_outcome();
    let gamma = discount.clamp(0.0, 1.0);

    // Compute weights: weight_i = gamma^(N-1-i)
    let weights: Vec<f64> = (0..n).map(|i| gamma.powi((n - 1 - i) as i32)).collect();
    let weight_sum: f64 = weights.iter().sum();

    dispatches
        .iter()
        .enumerate()
        .map(|(i, event)| {
            let credit = if weight_sum > 0.0 {
                weights[i] / weight_sum * total_outcome
            } else {
                total_outcome / n as f64
            };
            CreditAssignment {
                template_id: event.template_id.clone().unwrap_or_default(),
                action_id: event.action_id.unwrap_or(uuid::Uuid::from_u128(0)),
                credit,
                position: i,
                total_dispatches: n,
            }
        })
        .collect()
}

/// Allocates credit using intermediate measurement events when available.
///
/// When the episode has measurement events between dispatches, the outcome
/// of each dispatch is the difference between the measurement after it and
/// the measurement before it. This is more accurate than discounted credit
/// because it uses actual observed incremental outcomes.
///
/// If no intermediate measurements exist, falls back to
/// [`credit_allocation`] with the given discount.
#[must_use]
pub fn credit_allocation_with_measurements(
    episode: &OpportunityEpisode,
    discount: f64,
) -> Vec<CreditAssignment> {
    // Build a timeline of (event_index, kind, outcome) to find measurements
    // between dispatches.
    let dispatch_indices: Vec<usize> = episode
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EpisodeEventKind::Dispatch)
        .map(|(i, _)| i)
        .collect();

    if dispatch_indices.is_empty() {
        return Vec::new();
    }

    // Check if there are measurement events between dispatches.
    let has_intermediate_measurements = dispatch_indices.windows(2).any(|w| {
        episode.events[w[0]..w[1]]
            .iter()
            .any(|e| e.kind == EpisodeEventKind::Measurement)
    });

    if !has_intermediate_measurements {
        return credit_allocation(episode, discount);
    }

    // Compute incremental outcomes per dispatch.
    let mut prev_outcome = 0.0;
    let mut assignments = Vec::with_capacity(dispatch_indices.len());
    for (pos, &dispatch_idx) in dispatch_indices.iter().enumerate() {
        // Find the next measurement or conversion event after this dispatch.
        let next_outcome = episode.events[dispatch_idx..]
            .iter()
            .find_map(|e| {
                if e.kind == EpisodeEventKind::Measurement || e.kind == EpisodeEventKind::Conversion
                {
                    e.observed_outcome
                } else {
                    None
                }
            })
            .unwrap_or(prev_outcome);
        let incremental = next_outcome - prev_outcome;
        prev_outcome = next_outcome;
        let event = &episode.events[dispatch_idx];
        assignments.push(CreditAssignment {
            template_id: event.template_id.clone().unwrap_or_default(),
            action_id: event.action_id.unwrap_or(uuid::Uuid::from_u128(0)),
            credit: incremental,
            position: pos,
            total_dispatches: dispatch_indices.len(),
        });
    }
    assignments
}

/// The episode tracker — manages all active and completed episodes.
///
/// The brain records events as they happen and uses the tracker to query
/// completed episodes for credit assignment and model updates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpisodeTracker {
    /// All episodes (active + completed), keyed by episode ID.
    pub episodes: std::collections::HashMap<String, OpportunityEpisode>,
}

impl EpisodeTracker {
    /// Creates a new, empty episode tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new episode with the first event.
    pub fn start_episode(&mut self, episode: OpportunityEpisode) {
        self.episodes.insert(episode.id.clone(), episode);
    }

    /// Records an event in an existing episode. Does nothing if the episode
    /// ID is unknown or the episode is already terminal.
    pub fn record_event(&mut self, episode_id: &str, event: EpisodeEvent) {
        if let Some(episode) = self.episodes.get_mut(episode_id)
            && !episode.is_terminal()
        {
            episode.record(event);
        }
    }

    /// Returns all completed (terminal) episodes.
    #[must_use]
    pub fn completed_episodes(&self) -> Vec<&OpportunityEpisode> {
        self.episodes.values().filter(|e| e.is_terminal()).collect()
    }

    /// Returns the number of active episodes.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.episodes.values().filter(|e| !e.is_terminal()).count()
    }

    /// Returns the number of completed episodes.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.episodes.values().filter(|e| e.is_terminal()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_model::GrowthTrend;

    #[test]
    fn opportunity_id_is_deterministic() {
        let ctx = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            subreddit_type: Some("metal".to_owned()),
            ..Default::default()
        };
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_targets() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_ProgMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_actions() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Scan,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_contexts() {
        let ctx1 = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx2 = DispatchContext {
            days_to_event: Some(30),
            ..Default::default()
        };
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx1,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx2,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_display_is_colon_separated() {
        let ctx = DispatchContext::default();
        let id = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let display = id.to_string();
        assert!(display.starts_with("community-engager:r_MetalMusic:post:"));
    }

    #[test]
    fn opportunity_id_is_hashable() {
        use std::collections::HashSet;
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new("a", "b", OpportunityAction::Scan, &ctx);
        let id2 = OpportunityId::new("a", "b", OpportunityAction::Scan, &ctx);
        let id3 = OpportunityId::new("a", "c", OpportunityAction::Scan, &ctx);
        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2);
        set.insert(id3);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn opportunity_state_open_is_actionable() {
        assert!(OpportunityState::Open.is_actionable());
        assert!(!OpportunityState::Dispatched.is_actionable());
        assert!(!OpportunityState::Resolved.is_actionable());
    }

    #[test]
    fn opportunity_state_terminal_states() {
        assert!(OpportunityState::Resolved.is_terminal());
        assert!(OpportunityState::Expired.is_terminal());
        assert!(OpportunityState::Skipped.is_terminal());
        assert!(!OpportunityState::Open.is_terminal());
        assert!(!OpportunityState::Dispatched.is_terminal());
        assert!(!OpportunityState::Measuring.is_terminal());
    }

    #[test]
    fn opportunity_action_as_str() {
        assert_eq!(OpportunityAction::Scan.as_str(), "scan");
        assert_eq!(OpportunityAction::Post.as_str(), "post");
        assert_eq!(OpportunityAction::Pitch.as_str(), "pitch");
        assert_eq!(OpportunityAction::Invite.as_str(), "invite");
        assert_eq!(OpportunityAction::Analyze.as_str(), "analyze");
    }

    #[test]
    fn with_context_hash_creates_id_without_context() {
        let id = OpportunityId::with_context_hash(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Scan,
            "1:Std:metal:text_report",
        );
        assert_eq!(id.context_hash, "1:Std:metal:text_report");
        assert_eq!(id.template_id, "reddit-scanner");
    }

    // ─── Episode model tests ──────────────────────────────────────────────

    fn test_episode() -> OpportunityEpisode {
        let now = time::OffsetDateTime::now_utc();
        OpportunityEpisode::new(
            "ep1",
            uuid::Uuid::from_u128(0),
            "r_MetalMusic",
            EpisodeEvent::dispatch("reddit-scanner", uuid::Uuid::from_u128(1), now),
        )
    }

    #[test]
    fn episode_starts_active_with_one_event() {
        let ep = test_episode();
        assert_eq!(ep.status, EpisodeStatus::Active);
        assert_eq!(ep.events.len(), 1);
        assert!(!ep.is_terminal());
        assert_eq!(ep.dispatch_count(), 1);
    }

    #[test]
    fn episode_records_measurement_and_conversion() {
        let mut ep = test_episode();
        let now = time::OffsetDateTime::now_utc();
        ep.record(EpisodeEvent::measurement(5.0, now));
        ep.record(EpisodeEvent::conversion(1.0, now));
        assert_eq!(ep.status, EpisodeStatus::Converted);
        assert!(ep.is_terminal());
        assert_eq!(ep.total_outcome(), 6.0);
        assert!(ep.ended_at.is_some());
    }

    #[test]
    fn episode_expires_without_conversion() {
        let mut ep = test_episode();
        let now = time::OffsetDateTime::now_utc();
        ep.record(EpisodeEvent::measurement(2.0, now));
        ep.record(EpisodeEvent::expired(now));
        assert_eq!(ep.status, EpisodeStatus::Expired);
        assert!(ep.is_terminal());
        assert_eq!(ep.total_outcome(), 2.0);
    }

    #[test]
    fn episode_dispatched_templates_in_order() {
        let mut ep = test_episode();
        let now = time::OffsetDateTime::now_utc();
        ep.record(EpisodeEvent::dispatch(
            "community-engager",
            uuid::Uuid::from_u128(2),
            now,
        ));
        ep.record(EpisodeEvent::dispatch(
            "signal-inviter",
            uuid::Uuid::from_u128(3),
            now,
        ));
        let templates = ep.dispatched_templates();
        assert_eq!(
            templates,
            vec!["reddit-scanner", "community-engager", "signal-inviter"]
        );
    }

    #[test]
    fn episode_tracker_starts_and_records() {
        let mut tracker = EpisodeTracker::new();
        let ep = test_episode();
        tracker.start_episode(ep);
        assert_eq!(tracker.active_count(), 1);
        assert_eq!(tracker.completed_count(), 0);

        let now = time::OffsetDateTime::now_utc();
        tracker.record_event("ep1", EpisodeEvent::conversion(1.0, now));
        assert_eq!(tracker.active_count(), 0);
        assert_eq!(tracker.completed_count(), 1);
    }

    #[test]
    fn episode_tracker_ignores_events_for_unknown_episodes() {
        let mut tracker = EpisodeTracker::new();
        let now = time::OffsetDateTime::now_utc();
        tracker.record_event("unknown", EpisodeEvent::conversion(1.0, now));
        assert_eq!(tracker.completed_count(), 0);
    }

    #[test]
    fn episode_tracker_ignores_events_for_terminal_episodes() {
        let mut tracker = EpisodeTracker::new();
        let now = time::OffsetDateTime::now_utc();
        let mut ep = test_episode();
        ep.record(EpisodeEvent::conversion(1.0, now));
        tracker.start_episode(ep);
        // Try to record another event on a terminal episode.
        tracker.record_event("ep1", EpisodeEvent::measurement(5.0, now));
        let completed = tracker.completed_episodes();
        assert_eq!(completed.len(), 1);
        // The terminal episode should still have only 2 events (dispatch + conversion).
        assert_eq!(completed[0].events.len(), 2);
    }

    #[test]
    fn episode_duration_is_none_when_active() {
        let ep = test_episode();
        assert!(ep.duration_seconds().is_none());
    }

    #[test]
    fn episode_duration_is_some_when_terminal() {
        let mut ep = test_episode();
        let now = time::OffsetDateTime::now_utc();
        ep.record(EpisodeEvent::conversion(1.0, now));
        assert!(ep.duration_seconds().is_some());
    }

    #[test]
    fn episode_total_outcome_sums_all_measurements_and_conversions() {
        let mut ep = test_episode();
        let now = time::OffsetDateTime::now_utc();
        ep.record(EpisodeEvent::measurement(3.0, now));
        ep.record(EpisodeEvent::measurement(-1.0, now));
        ep.record(EpisodeEvent::conversion(2.0, now));
        assert_eq!(ep.total_outcome(), 4.0);
    }

    // ─── Credit assignment tests ──────────────────────────────────────────

    fn multi_dispatch_episode() -> OpportunityEpisode {
        let now = time::OffsetDateTime::now_utc();
        let mut ep = OpportunityEpisode::new(
            "ep_multi",
            uuid::Uuid::from_u128(10),
            "r_MetalMusic",
            EpisodeEvent::dispatch("reddit-scanner", uuid::Uuid::from_u128(1), now),
        );
        ep.record(EpisodeEvent::dispatch(
            "community-engager",
            uuid::Uuid::from_u128(2),
            now,
        ));
        ep.record(EpisodeEvent::dispatch(
            "signal-inviter",
            uuid::Uuid::from_u128(3),
            now,
        ));
        ep.record(EpisodeEvent::conversion(9.0, now));
        ep
    }

    #[test]
    fn credit_allocation_sums_to_total_outcome() {
        let ep = multi_dispatch_episode();
        let credits = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        let total_credit: f64 = credits.iter().map(|c| c.credit).sum();
        assert!(
            (total_credit - ep.total_outcome()).abs() < 0.001,
            "credits should sum to total outcome, got {total_credit}, expected {}",
            ep.total_outcome()
        );
    }

    #[test]
    fn credit_allocation_equal_with_gamma_one() {
        let ep = multi_dispatch_episode();
        let credits = credit_allocation(&ep, 1.0);
        // With γ=1.0, all dispatches get equal credit.
        let expected = ep.total_outcome() / 3.0;
        for c in &credits {
            assert!(
                (c.credit - expected).abs() < 0.001,
                "equal credit expected, got {}",
                c.credit
            );
        }
    }

    #[test]
    fn credit_allocation_discounted_favors_later_dispatches() {
        let ep = multi_dispatch_episode();
        let credits = credit_allocation(&ep, 0.5);
        assert_eq!(credits.len(), 3);
        // Last dispatch should get the most credit.
        assert!(credits[2].credit > credits[1].credit);
        assert!(credits[1].credit > credits[0].credit);
    }

    #[test]
    fn credit_allocation_single_dispatch_gets_full_credit() {
        let now = time::OffsetDateTime::now_utc();
        let mut ep = OpportunityEpisode::new(
            "ep_single",
            uuid::Uuid::from_u128(20),
            "r_MetalMusic",
            EpisodeEvent::dispatch("reddit-scanner", uuid::Uuid::from_u128(1), now),
        );
        ep.record(EpisodeEvent::conversion(5.0, now));
        let credits = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert_eq!(credits.len(), 1);
        assert!((credits[0].credit - 5.0).abs() < 0.001);
    }

    #[test]
    fn credit_allocation_no_dispatches_returns_empty() {
        let now = time::OffsetDateTime::now_utc();
        let mut ep = OpportunityEpisode::new(
            "ep_empty",
            uuid::Uuid::from_u128(30),
            "r_MetalMusic",
            EpisodeEvent::measurement(5.0, now),
        );
        ep.record(EpisodeEvent::conversion(5.0, now));
        let credits = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert!(credits.is_empty());
    }

    #[test]
    fn credit_allocation_with_measurements_uses_incremental_outcomes() {
        let now = time::OffsetDateTime::now_utc();
        let mut ep = OpportunityEpisode::new(
            "ep_meas",
            uuid::Uuid::from_u128(40),
            "r_MetalMusic",
            EpisodeEvent::dispatch("reddit-scanner", uuid::Uuid::from_u128(1), now),
        );
        // Measurement after first dispatch: +3 fans
        ep.record(EpisodeEvent::measurement(3.0, now));
        // Second dispatch
        ep.record(EpisodeEvent::dispatch(
            "community-engager",
            uuid::Uuid::from_u128(2),
            now,
        ));
        // Measurement after second dispatch: +6 total (so +3 incremental)
        ep.record(EpisodeEvent::measurement(6.0, now));
        // Third dispatch
        ep.record(EpisodeEvent::dispatch(
            "signal-inviter",
            uuid::Uuid::from_u128(3),
            now,
        ));
        // Conversion: +9 total (so +3 incremental)
        ep.record(EpisodeEvent::conversion(9.0, now));

        let credits = credit_allocation_with_measurements(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert_eq!(credits.len(), 3);
        // Each dispatch should get credit for its incremental outcome.
        assert!(
            (credits[0].credit - 3.0).abs() < 0.001,
            "first dispatch credit"
        );
        assert!(
            (credits[1].credit - 3.0).abs() < 0.001,
            "second dispatch credit"
        );
        assert!(
            (credits[2].credit - 3.0).abs() < 0.001,
            "third dispatch credit"
        );
    }

    #[test]
    fn credit_allocation_with_measurements_falls_back_without_intermediate() {
        let ep = multi_dispatch_episode();
        // No intermediate measurements → should fall back to discounted.
        let credits = credit_allocation_with_measurements(&ep, DEFAULT_CREDIT_DISCOUNT);
        let discounted = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert_eq!(credits.len(), discounted.len());
        for (a, b) in credits.iter().zip(discounted.iter()) {
            assert!((a.credit - b.credit).abs() < 0.001, "should match fallback");
        }
    }

    #[test]
    fn credit_assignment_preserves_template_ids() {
        let ep = multi_dispatch_episode();
        let credits = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert_eq!(credits[0].template_id, "reddit-scanner");
        assert_eq!(credits[1].template_id, "community-engager");
        assert_eq!(credits[2].template_id, "signal-inviter");
    }

    #[test]
    fn credit_assignment_preserves_positions() {
        let ep = multi_dispatch_episode();
        let credits = credit_allocation(&ep, DEFAULT_CREDIT_DISCOUNT);
        assert_eq!(credits[0].position, 0);
        assert_eq!(credits[1].position, 1);
        assert_eq!(credits[2].position, 2);
        assert_eq!(credits[0].total_dispatches, 3);
    }
}
