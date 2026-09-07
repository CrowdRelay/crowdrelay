//! Growth hypothesis lifecycle — testable claims about fan acquisition edge.
//!
//! Ported from Kern's `brain::hypothesis`. Each growth template earns trust
//! through lifecycle stages before getting full dispatch budget. A template
//! that has never produced a fan starts at `Testing` (minimal budget), earns
//! trust through `Paper` → `MicroLive` → `Active`, and gets `Degraded` or
//! `Retired` when it stops working.
//!
//! # Adaptation from Kern
//!
//! Kern's lifecycle is per-hypothesis (a specific testable claim like
//! "NVDA + RSI < 30 → positive 2h return"). CrowdRelay's is per-template-
//! per-workspace (a specific growth template like "community-engager" for
//! a specific tenant). The lifecycle state is scoped to
//! `(workspace_id, template_id)`.
//!
//! # The kill switch
//!
//! Only `Active` and `Degraded` templates can generate dispatches at full
//! or reduced sizing. `Retired` templates generate nothing — the brain
//! has concluded this template does not work for this tenant. This is
//! the kill switch at the template level: a `Retired` template produces
//! no actions, no matter how good it once looked.

use serde::{Deserialize, Serialize};

/// The lifecycle state of a growth hypothesis. Only `Active` and
/// `Degraded` hypotheses can generate dispatches at full or reduced
/// sizing. This is the kill switch.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisState {
    /// Just discovered. Not yet tested. No observations.
    #[default]
    Discovered,
    /// Being tested — minimal dispatch budget. Earning trust.
    Testing,
    /// Paper testing — simulated dispatches with real data. Not yet
    /// trusted with real audience contact at full scale.
    Paper,
    /// Live with tiny dispatch budget. Earning trust with real
    /// audience contact at minimal scale.
    MicroLive,
    /// Fully active. Can generate dispatches at normal budget.
    Active,
    /// Edge is decaying. Watched but not dispatched at full budget.
    Degraded,
    /// Edge is gone (or never existed). No new dispatches. The brain
    /// has concluded this template does not work for this tenant.
    Retired,
}

impl HypothesisState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Testing => "testing",
            Self::Paper => "paper",
            Self::MicroLive => "micro_live",
            Self::Active => "active",
            Self::Degraded => "degraded",
            Self::Retired => "retired",
        }
    }

    /// Returns true if this state permits generating new dispatches.
    /// Testing and Paper states may generate minimal dispatches (for
    /// learning); MicroLive, Active and Degraded may generate live
    /// dispatches (subject to standing). Discovered and Retired do
    /// not generate new dispatches.
    #[must_use]
    pub const fn may_act(self) -> bool {
        matches!(
            self,
            Self::Testing | Self::Paper | Self::MicroLive | Self::Active | Self::Degraded
        )
    }

    /// Returns true if this state permits live (real audience) dispatches.
    #[must_use]
    pub const fn permits_live(self) -> bool {
        matches!(self, Self::MicroLive | Self::Active | Self::Degraded)
    }

    /// Sizing multiplier for this state, in basis points.
    /// Active = 10_000 (full), Degraded = 2_500 (quarter),
    /// MicroLive = 3_000 (small live size, earning trust),
    /// Paper = 1_000 (tenth), Testing = 500 (minimal learning size),
    /// all others = 0.
    ///
    /// This is applied to the dispatch budget: a `Testing` template
    /// gets 5% of the budget an `Active` template would get.
    #[must_use]
    pub const fn sizing_bps(self) -> u16 {
        match self {
            Self::Active => 10_000,
            Self::Degraded => 2_500,
            Self::MicroLive => 3_000,
            Self::Paper => 1_000,
            Self::Testing => 500,
            _ => 0,
        }
    }

    /// Returns the sizing multiplier as a float (0.0–1.0).
    #[must_use]
    pub const fn sizing_multiplier(self) -> f64 {
        self.sizing_bps() as f64 / 10_000.0
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "discovered" => Some(Self::Discovered),
            "testing" => Some(Self::Testing),
            "paper" => Some(Self::Paper),
            "micro_live" => Some(Self::MicroLive),
            "active" => Some(Self::Active),
            "degraded" => Some(Self::Degraded),
            "retired" => Some(Self::Retired),
            _ => None,
        }
    }
}

/// One lifecycle transition event, for audit.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LifecycleTransition {
    pub from: HypothesisState,
    pub to: HypothesisState,
    pub reason: String,
    pub timestamp: time::OffsetDateTime,
}

/// A growth hypothesis — a testable claim about fan acquisition edge
/// for a specific template in a specific workspace.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GrowthHypothesis {
    /// The workspace this hypothesis belongs to.
    pub workspace_id: uuid::Uuid,
    /// The template this hypothesis tracks (e.g., "community-engager").
    pub template_id: String,
    /// The current lifecycle state.
    pub state: HypothesisState,
    /// Lifecycle transition history for audit.
    pub lifecycle_history: Vec<LifecycleTransition>,
    /// Consecutive worsened observations since last non-worsened.
    pub consecutive_worsened: u32,
    /// When this hypothesis was created.
    pub created_at: time::OffsetDateTime,
    /// When the hypothesis state was last updated.
    pub updated_at: Option<time::OffsetDateTime>,
    /// When it was retired, if applicable.
    pub retired_at: Option<time::OffsetDateTime>,
    /// Why it was retired, if applicable.
    pub retire_reason: Option<String>,
    /// How many times this hypothesis has been resurrected from Retired
    /// back to Discovered. After MAX_RESURRECTIONS, the hypothesis is
    /// permanently retired.
    pub resurrection_count: u32,
}

/// Maximum resurrections before permanent retirement.
const MAX_RESURRECTIONS: u32 = 2;

/// Minimum observations before a hypothesis can transition from Testing
/// to Paper.
const MIN_OBSERVATIONS_FOR_PAPER: u32 = 3;

/// Minimum observations before a hypothesis can transition from Paper
/// to MicroLive.
const MIN_OBSERVATIONS_FOR_MICRO_LIVE: u32 = 5;

/// Consecutive worsened observations before a hypothesis is degraded.
const DEGRADED_AFTER_WORSENED: u32 = 3;

/// Consecutive worsened observations before a hypothesis is retired.
const RETIRED_AFTER_WORSENED: u32 = 5;

/// The lifecycle policy — transition rules for growth hypotheses.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HypothesisLifecyclePolicy {
    pub min_observations_for_paper: u32,
    pub min_observations_for_micro_live: u32,
    pub degraded_after_worsened: u32,
    pub retired_after_worsened: u32,
    pub max_resurrections: u32,
}

impl Default for HypothesisLifecyclePolicy {
    fn default() -> Self {
        Self {
            min_observations_for_paper: MIN_OBSERVATIONS_FOR_PAPER,
            min_observations_for_micro_live: MIN_OBSERVATIONS_FOR_MICRO_LIVE,
            degraded_after_worsened: DEGRADED_AFTER_WORSENED,
            retired_after_worsened: RETIRED_AFTER_WORSENED,
            max_resurrections: MAX_RESURRECTIONS,
        }
    }
}

/// Assesses whether a hypothesis should transition to a new state.
///
/// Returns `Some(new_state)` if a transition is warranted, `None` if
/// the current state is correct.
#[must_use]
pub fn assess_hypothesis_transition(
    current: HypothesisState,
    observation_count: u32,
    consecutive_worsened: u32,
    has_positive_y30: bool,
    policy: &HypothesisLifecyclePolicy,
) -> Option<HypothesisState> {
    // Degradation and retirement can happen from any active state.
    if current.may_act() {
        if consecutive_worsened >= policy.retired_after_worsened {
            return Some(HypothesisState::Retired);
        }
        if consecutive_worsened >= policy.degraded_after_worsened
            && current != HypothesisState::Degraded
        {
            return Some(HypothesisState::Degraded);
        }
    }

    // Promotion path: Discovered → Testing → Paper → MicroLive → Active
    match current {
        HypothesisState::Discovered => {
            // Automatically move to Testing when first observed.
            if observation_count > 0 {
                return Some(HypothesisState::Testing);
            }
        }
        HypothesisState::Testing => {
            if observation_count >= policy.min_observations_for_paper {
                return Some(HypothesisState::Paper);
            }
        }
        HypothesisState::Paper => {
            if observation_count >= policy.min_observations_for_micro_live {
                return Some(HypothesisState::MicroLive);
            }
        }
        HypothesisState::MicroLive => {
            // Promote to Active when we have positive Y30 durable fans.
            if has_positive_y30 {
                return Some(HypothesisState::Active);
            }
        }
        HypothesisState::Degraded => {
            // Recovery: if we get positive Y30 again, go back to Active.
            if has_positive_y30 && consecutive_worsened == 0 {
                return Some(HypothesisState::Active);
            }
        }
        HypothesisState::Retired => {
            // Resurrection is handled separately (see resurrect method).
        }
        HypothesisState::Active => {
            // No further promotion.
        }
    }
    None
}

impl GrowthHypothesis {
    /// Creates a new growth hypothesis in the Discovered state.
    #[must_use]
    pub fn new(workspace_id: uuid::Uuid, template_id: String, now: time::OffsetDateTime) -> Self {
        Self {
            workspace_id,
            template_id,
            state: HypothesisState::Discovered,
            lifecycle_history: Vec::new(),
            consecutive_worsened: 0,
            created_at: now,
            updated_at: None,
            retired_at: None,
            retire_reason: None,
            resurrection_count: 0,
        }
    }

    /// Records an observation and evaluates lifecycle transitions.
    ///
    /// Call this after each measurement cycle. `worsened` is true if
    /// the latest Y30 outcome was worse than the previous one.
    /// `has_positive_y30` is true if the template has produced at least
    /// one positive Y30 durable fan outcome.
    pub fn observe(
        &mut self,
        worsened: bool,
        has_positive_y30: bool,
        observation_count: u32,
        policy: &HypothesisLifecyclePolicy,
        now: time::OffsetDateTime,
    ) {
        if worsened {
            self.consecutive_worsened += 1;
        } else {
            self.consecutive_worsened = 0;
        }

        // Loop until stable — a single observation can trigger
        // multiple transitions (e.g., Discovered → Testing → Paper
        // when observation_count jumps from 0 to 3).
        while let Some(new_state) = assess_hypothesis_transition(
            self.state,
            observation_count,
            self.consecutive_worsened,
            has_positive_y30,
            policy,
        ) {
            self.transition_to(new_state, "lifecycle policy", now);
        }
    }

    /// Transitions the hypothesis to a new state, recording the transition.
    pub fn transition_to(
        &mut self,
        new_state: HypothesisState,
        reason: &str,
        now: time::OffsetDateTime,
    ) {
        if new_state == self.state {
            return;
        }
        self.lifecycle_history.push(LifecycleTransition {
            from: self.state,
            to: new_state,
            reason: reason.to_string(),
            timestamp: now,
        });
        if new_state == HypothesisState::Retired {
            self.retired_at = Some(now);
            self.retire_reason = Some(reason.to_string());
        }
        if self.state == HypothesisState::Retired && new_state != HypothesisState::Retired {
            self.resurrection_count += 1;
            self.retired_at = None;
            self.retire_reason = None;
        }
        self.state = new_state;
        self.updated_at = Some(now);
    }

    /// Attempts to resurrect a retired hypothesis. Returns false if
    /// the hypothesis has been resurrected too many times.
    #[must_use]
    pub fn can_resurrect(&self, policy: &HypothesisLifecyclePolicy) -> bool {
        self.state == HypothesisState::Retired && self.resurrection_count < policy.max_resurrections
    }

    /// Returns true if this hypothesis is permanently retired (cannot
    /// be resurrected).
    #[must_use]
    pub fn is_permanently_retired(&self, policy: &HypothesisLifecyclePolicy) -> bool {
        self.state == HypothesisState::Retired
            && self.resurrection_count >= policy.max_resurrections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    fn make_hypothesis() -> GrowthHypothesis {
        GrowthHypothesis::new(uuid::Uuid::now_v7(), "community-engager".into(), now())
    }

    #[test]
    fn new_hypothesis_is_discovered() {
        let h = make_hypothesis();
        assert_eq!(h.state, HypothesisState::Discovered);
        assert!(!h.state.may_act());
        assert_eq!(h.state.sizing_bps(), 0);
    }

    #[test]
    fn discovered_promotes_to_testing_on_first_observation() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        h.observe(false, false, 1, &policy, now());
        assert_eq!(h.state, HypothesisState::Testing);
        assert!(h.state.may_act());
        assert_eq!(h.state.sizing_bps(), 500);
    }

    #[test]
    fn testing_promotes_to_paper_after_min_observations() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        // 3 observations should promote to Paper
        h.observe(false, false, 3, &policy, now());
        assert_eq!(h.state, HypothesisState::Paper);
        assert_eq!(h.state.sizing_bps(), 1000);
    }

    #[test]
    fn paper_promotes_to_micro_live_after_more_observations() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        h.observe(false, false, 5, &policy, now());
        // Should jump to Paper (min 3), then MicroLive (min 5)
        assert_eq!(h.state, HypothesisState::MicroLive);
        assert_eq!(h.state.sizing_bps(), 3000);
    }

    #[test]
    fn micro_live_promotes_to_active_with_positive_y30() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        h.observe(false, false, 5, &policy, now());
        assert_eq!(h.state, HypothesisState::MicroLive);
        // Now with positive Y30, should promote to Active
        h.observe(false, true, 6, &policy, now());
        assert_eq!(h.state, HypothesisState::Active);
        assert_eq!(h.state.sizing_bps(), 10000);
    }

    #[test]
    fn consecutive_worsened_degrades_then_retires() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        // Get to Active first
        h.observe(false, true, 10, &policy, now());
        assert_eq!(h.state, HypothesisState::Active);
        // 3 worsened observations → Degraded
        for _ in 0..3 {
            h.observe(true, true, 11, &policy, now());
        }
        assert_eq!(h.state, HypothesisState::Degraded);
        assert_eq!(h.state.sizing_bps(), 2500);
        // 2 more worsened (5 total) → Retired
        for _ in 0..2 {
            h.observe(true, true, 12, &policy, now());
        }
        assert_eq!(h.state, HypothesisState::Retired);
        assert!(!h.state.may_act());
        assert_eq!(h.state.sizing_bps(), 0);
    }

    #[test]
    fn degraded_recovers_to_active_with_positive_y30() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        h.observe(false, true, 10, &policy, now());
        // Degrade
        for _ in 0..3 {
            h.observe(true, true, 11, &policy, now());
        }
        assert_eq!(h.state, HypothesisState::Degraded);
        // Non-worsened + positive Y30 → Active
        h.observe(false, true, 12, &policy, now());
        assert_eq!(h.state, HypothesisState::Active);
    }

    #[test]
    fn resurrection_increments_count() {
        let mut h = make_hypothesis();
        // Retire
        h.transition_to(HypothesisState::Retired, "test", now());
        assert_eq!(h.resurrection_count, 0);
        // Resurrect
        h.transition_to(HypothesisState::Discovered, "resurrect", now());
        assert_eq!(h.resurrection_count, 1);
        assert_eq!(h.state, HypothesisState::Discovered);
    }

    #[test]
    fn permanently_retired_cannot_resurrect() {
        let mut h = make_hypothesis();
        let policy = HypothesisLifecyclePolicy::default();
        // Retire and resurrect twice (max resurrections = 2)
        for _ in 0..2 {
            h.transition_to(HypothesisState::Retired, "test", now());
            h.transition_to(HypothesisState::Discovered, "resurrect", now());
        }
        // Now retire again
        h.transition_to(HypothesisState::Retired, "final", now());
        assert_eq!(h.resurrection_count, 2);
        assert!(h.is_permanently_retired(&policy));
        assert!(!h.can_resurrect(&policy));
    }

    #[test]
    fn sizing_multiplier_is_fraction() {
        assert_eq!(HypothesisState::Active.sizing_multiplier(), 1.0);
        assert_eq!(HypothesisState::Testing.sizing_multiplier(), 0.05);
        assert_eq!(HypothesisState::Retired.sizing_multiplier(), 0.0);
    }

    #[test]
    fn permits_live_only_for_live_states() {
        assert!(!HypothesisState::Testing.permits_live());
        assert!(!HypothesisState::Paper.permits_live());
        assert!(HypothesisState::MicroLive.permits_live());
        assert!(HypothesisState::Active.permits_live());
        assert!(HypothesisState::Degraded.permits_live());
        assert!(!HypothesisState::Retired.permits_live());
    }

    #[test]
    fn roundtrip_parse() {
        for state in [
            HypothesisState::Discovered,
            HypothesisState::Testing,
            HypothesisState::Paper,
            HypothesisState::MicroLive,
            HypothesisState::Active,
            HypothesisState::Degraded,
            HypothesisState::Retired,
        ] {
            assert_eq!(HypothesisState::parse(state.as_str()), Some(state));
        }
    }
}
