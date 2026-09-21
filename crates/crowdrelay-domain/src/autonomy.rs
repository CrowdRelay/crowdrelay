//! Shared autonomy primitives used by ViryaOS bounded contexts.
//!
//! This module is deliberately tiny. It contains only stable domain vocabulary
//! shared by bounded contexts; orchestration, persistence and transport stay in
//! the application and infrastructure layers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A bounded confidence value expressed in basis points (`0..=10_000`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Confidence(u16);

impl Confidence {
    pub const MIN: Self = Self(0);
    pub const MAX: Self = Self(10_000);

    /// Creates a validated confidence value.
    pub const fn from_basis_points(value: u16) -> Result<Self, ConfidenceError> {
        if value <= 10_000 {
            Ok(Self(value))
        } else {
            Err(ConfidenceError::OutOfRange)
        }
    }

    /// Creates a confidence value while saturating values above 100%.
    #[must_use]
    pub const fn saturating_from_basis_points(value: u16) -> Self {
        Self(if value > 10_000 { 10_000 } else { value })
    }

    /// Returns the confidence as basis points.
    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

/// Error returned when a confidence value is outside `0..=10_000`.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ConfidenceError {
    #[error("confidence must be between 0 and 10000 basis points")]
    OutOfRange,
}

/// Maximum authority granted to a bounded context.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Measure what would have happened; emit no operator task or side effect.
    Observe,
    /// Surface a recommendation but never enqueue an executable action.
    Recommend,
    /// Prepare an action that must be explicitly approved by an operator.
    RequireApproval,
    /// Execute actions that satisfy deterministic domain and policy limits.
    BoundedAuto,
}

impl AutonomyLevel {
    /// The stored representation — the vocabulary of the `ceiling` and
    /// `autonomy_level` CHECK constraints.
    ///
    /// Here rather than beside each reader because three crates already need
    /// it and each one that restates the list is a place the list can drift
    /// from the constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Recommend => "recommend",
            Self::RequireApproval => "require_approval",
            Self::BoundedAuto => "bounded_auto",
        }
    }

    /// Parses what [`Self::as_str`] wrote.
    ///
    /// `None` for anything this build does not recognise. A caller reading an
    /// authority row must treat that as the safest level, never as an absent
    /// limit — a row a newer deploy wrote and this one cannot read is not a
    /// grant of authority.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "observe" => Some(Self::Observe),
            "recommend" => Some(Self::Recommend),
            "require_approval" => Some(Self::RequireApproval),
            "bounded_auto" => Some(Self::BoundedAuto),
            _ => None,
        }
    }

    /// Returns true when the level is permitted to enqueue an executable action.
    #[must_use]
    pub const fn may_enqueue(self) -> bool {
        matches!(self, Self::RequireApproval | Self::BoundedAuto)
    }

    /// Returns true when the level may execute without human approval.
    #[must_use]
    pub const fn may_auto_execute(self) -> bool {
        matches!(self, Self::BoundedAuto)
    }
}

/// Result of the shared authority gate after a bounded context has made a
/// deterministic business decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDisposition {
    ObserveOnly,
    RecommendOnly,
    RequireApproval,
    AutoExecute,
    Deny,
}

/// Applies authority and confidence gates without knowing anything about the
/// concrete business action. Financial and domain-specific constraints remain
/// owned by the bounded context that produced the decision.
#[must_use]
pub const fn disposition(
    level: AutonomyLevel,
    confidence: Confidence,
    minimum_confidence: Confidence,
) -> PolicyDisposition {
    if confidence.0 < minimum_confidence.0 {
        return PolicyDisposition::Deny;
    }

    match level {
        AutonomyLevel::Observe => PolicyDisposition::ObserveOnly,
        AutonomyLevel::Recommend => PolicyDisposition::RecommendOnly,
        AutonomyLevel::RequireApproval => PolicyDisposition::RequireApproval,
        AutonomyLevel::BoundedAuto => PolicyDisposition::AutoExecute,
    }
}

/// The disposition for internal, first-party-reversible work — an agent
/// drafting run, a deterministic artifact render — whose every external
/// effect re-gates downstream on its own approval.
///
/// # Why `require_approval` upgrades instead of gating
///
/// A `require_approval` policy means "a person decides before anything
/// reaches the outside world". An internal dispatch reaches nothing: the
/// drafted post, pitch or artifact arrives back as its own action and waits
/// there. Parking the dispatch behind the same approval does not protect an
/// audience — it produces an "approve this prompt" card the operator cannot
/// meaningfully review, a second card later for the thing they actually can,
/// and an assignment email for each. Measured in production: forty-eight
/// agent-run approvals in fourteen days, every one of them a prompt.
///
/// Upgrading `RequireApproval` to `AutoExecute` removes the meaningless gate
/// while preserving every real one: `Deny` still denies (a context below its
/// confidence floor should not even draft), and `ObserveOnly`/`RecommendOnly`
/// still produce no action at all.
#[must_use]
pub const fn internal_work_disposition(mapped: PolicyDisposition) -> PolicyDisposition {
    match mapped {
        PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute => {
            PolicyDisposition::AutoExecute
        }
        other => other,
    }
}

/// How many observations stand behind the confidence a context reported.
///
/// Separate from [`Confidence`] because they answer different questions and
/// only one of them is currently asked. Confidence is what an estimator says
/// about itself; this is how much evidence it saw. A Beta posterior updated
/// four times reports a confidence, and that confidence is not small — it is
/// simply unearned, and nothing downstream can tell the difference by looking
/// at the number.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EvidenceCount(pub i64);

impl Default for EvidenceCount {
    /// Nothing measured. The state every estimator starts in, and the only
    /// safe reading of a count nobody supplied.
    fn default() -> Self {
        Self::NONE
    }
}

impl EvidenceCount {
    /// No observations at all — the state every estimator starts in.
    pub const NONE: Self = Self(0);

    /// Whether this many observations clears `floor`.
    #[must_use]
    pub const fn clears(self, floor: i64) -> bool {
        self.0 >= floor
    }
}

/// How much unattended action a context may still take while it is below the
/// evidence floor.
///
/// # Why the floor needs one
///
/// [`disposition_with_evidence`] downgrades unattended execution to approval
/// until a context has cleared its floor, and its own doc comment names the
/// trap: *"acting is how the observations that clear the floor get made, so a
/// gate that blocks action below the floor guarantees the floor is never
/// reached."* It avoids that by routing below-floor work through approval
/// rather than denial — which is correct in principle and, in production, the
/// same thing. Approvals expire at 72 hours, one person empties the queue, and
/// the floor of twenty was never approached from below: measured, zero
/// resolved outcomes against a floor of twenty.
///
/// So the gate was self-sealing in practice. An allowance is the standard way
/// out: a bounded number of unattended actions, spent per context per week,
/// whose whole purpose is to produce the observations the floor is waiting
/// for. It is deliberately small — this is the warm-up, not the posture.
///
/// # Why it is not a way around the ladder
///
/// The allowance is read only where the floor is what is holding an action
/// back. Everything else still applies and applies first: a context below its
/// confidence bar is still denied, `observe` and `recommend` still produce
/// nothing, the class ceiling still clamps, and the envelope still bounds
/// volume. An allowance cannot promote an action the operator's dial never
/// permitted; it can only decline to punish a context for not yet having the
/// evidence that acting is how you get.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootstrapAllowance {
    /// Unattended actions this context has already taken inside the window,
    /// counted from the durable action rows rather than a second ledger that
    /// could disagree with them.
    pub spent: i64,
    /// The operator's cap. Zero means no warm-up at all, which is the honest
    /// reading of an operator who set it to zero and the safe reading of a
    /// workspace whose envelope row is missing.
    pub cap: i64,
}

impl BootstrapAllowance {
    /// No warm-up: every below-floor action waits for a person. The default,
    /// and what an absent envelope row reads as.
    pub const NONE: Self = Self { spent: 0, cap: 0 };

    /// Whether one more unattended action fits inside the cap.
    #[must_use]
    pub const fn has_room(self) -> bool {
        self.spent < self.cap
    }
}

/// What a context has learned, and what it may still spend learning.
///
/// One value rather than two parameters because the two are only ever read
/// together, and a caller that passed the right count with the wrong
/// allowance would widen authority silently.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextEvidence {
    pub observations: EvidenceCount,
    pub bootstrap: BootstrapAllowance,
}

impl ContextEvidence {
    /// A context with nothing measured and no warm-up left. The strictest
    /// reading, and the right one for a caller that has not loaded either.
    pub const UNPROVEN: Self = Self {
        observations: EvidenceCount::NONE,
        bootstrap: BootstrapAllowance::NONE,
    };

    /// Measured observations with no warm-up allowance, for a caller that has
    /// a count but no envelope to read a cap from.
    #[must_use]
    pub const fn measured(observations: EvidenceCount) -> Self {
        Self {
            observations,
            bootstrap: BootstrapAllowance::NONE,
        }
    }
}

/// [`disposition`] with the evidence floor applied.
///
/// # Why a second gate
///
/// `disposition` asks how confident a context is and never asks what that
/// confidence was computed from. Those come apart exactly where it costs the
/// most: an estimator with four observations can report high confidence, clear
/// `minimum_confidence`, and be handed `AutoExecute` over an action the tenant
/// cannot take back. `action_class` already records that a venue, curator or
/// press contact gets one first approach — spending it on an unevaluated
/// posterior is not a small error, and it is not recoverable by learning later.
///
/// # Why this caps rather than denies
///
/// Below the floor the result is [`PolicyDisposition::RequireApproval`], never
/// `ObserveOnly` and never `Deny`. Denying would be self-defeating: acting is
/// how the observations that clear the floor get made, so a gate that blocks
/// action below the floor guarantees the floor is never reached. What must not
/// happen is *unattended* action on an untested estimator, so the action still
/// goes out — with a person on it.
///
/// `Deny` from the confidence gate is preserved. A context that failed its own
/// confidence bar is refused, and having little evidence does not soften that.
///
/// The floor is the caller's, because the caller knows what it is counting.
/// `measurement::RATE_FLOOR` is the floor for rates and the right default for
/// anything counted per-observation.
/// # The warm-up
///
/// Below the floor the downgrade is skipped while the context still has room
/// in its [`BootstrapAllowance`]. That is not a hole in the gate — it is what
/// keeps the gate from sealing itself shut. See the allowance's own comment
/// for why, and for what it still cannot do.
#[must_use]
pub const fn disposition_with_evidence(
    level: AutonomyLevel,
    confidence: Confidence,
    minimum_confidence: Confidence,
    evidence: ContextEvidence,
    floor: i64,
) -> PolicyDisposition {
    let granted = disposition(level, confidence, minimum_confidence);
    if evidence.observations.clears(floor) {
        return granted;
    }
    match granted {
        // The one downgrade: unattended execution becomes attended — unless
        // the context is still inside the warm-up that exists to produce the
        // observations this floor is waiting for.
        PolicyDisposition::AutoExecute => {
            if evidence.bootstrap.has_room() {
                PolicyDisposition::AutoExecute
            } else {
                PolicyDisposition::RequireApproval
            }
        }
        // Already at or below approval, or refused outright. A thin posterior
        // is no reason to widen authority, and no reason to narrow one that is
        // already narrow. The allowance is not read here: it answers "may this
        // run unattended", and nothing at or below approval is asking that.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_levels_round_trip() {
        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
            AutonomyLevel::BoundedAuto,
        ] {
            assert_eq!(AutonomyLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(AutonomyLevel::parse("bounded-auto"), None);
        assert_eq!(AutonomyLevel::parse(""), None);
    }

    #[test]
    fn confidence_rejects_values_above_one_hundred_percent() {
        assert_eq!(
            Confidence::from_basis_points(10_001),
            Err(ConfidenceError::OutOfRange)
        );
    }

    #[test]
    fn confidence_saturation_is_explicit_and_bounded() {
        assert_eq!(
            Confidence::saturating_from_basis_points(u16::MAX),
            Confidence::MAX
        );
    }

    #[test]
    fn authority_never_escalates_below_confidence_threshold() {
        let confidence = Confidence::saturating_from_basis_points(7_999);
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, confidence, minimum),
            PolicyDisposition::Deny
        );
    }

    #[test]
    fn bounded_auto_is_the_only_level_that_can_auto_execute() {
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, Confidence::MAX, minimum),
            PolicyDisposition::AutoExecute
        );
        assert_eq!(
            disposition(AutonomyLevel::RequireApproval, Confidence::MAX, minimum),
            PolicyDisposition::RequireApproval
        );
        assert!(!AutonomyLevel::RequireApproval.may_auto_execute());
        assert!(AutonomyLevel::RequireApproval.may_enqueue());
    }

    /// The case the confidence gate alone cannot see: a maximally confident
    /// estimator that has been tested four times.
    #[test]
    fn a_confident_estimator_with_thin_evidence_cannot_execute_unattended() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        let floor = crate::measurement::RATE_FLOOR;

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, Confidence::MAX, minimum),
            PolicyDisposition::AutoExecute,
            "the confidence gate alone grants unattended execution"
        );
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                Confidence::MAX,
                minimum,
                ContextEvidence::measured(EvidenceCount(4)),
                floor,
            ),
            PolicyDisposition::RequireApproval,
            "four observations must not license an unattended irreversible action"
        );
    }

    #[test]
    fn clearing_the_floor_changes_nothing() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        let floor = crate::measurement::RATE_FLOOR;

        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
            AutonomyLevel::BoundedAuto,
        ] {
            assert_eq!(
                disposition_with_evidence(
                    level,
                    Confidence::MAX,
                    minimum,
                    ContextEvidence::measured(EvidenceCount(floor)),
                    floor,
                ),
                disposition(level, Confidence::MAX, minimum),
                "at the floor the evidence gate is transparent"
            );
        }
    }

    /// Below the floor is unproven, not refused. Blocking action below the
    /// floor would stop the observations that clear it from ever being made.
    #[test]
    fn thin_evidence_never_narrows_authority_past_approval() {
        let minimum = Confidence::saturating_from_basis_points(8_000);

        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
        ] {
            let granted = disposition(level, Confidence::MAX, minimum);
            assert_eq!(
                disposition_with_evidence(
                    level,
                    Confidence::MAX,
                    minimum,
                    ContextEvidence::UNPROVEN,
                    crate::measurement::RATE_FLOOR,
                ),
                granted,
                "the evidence floor only ever downgrades AutoExecute"
            );
        }
    }

    /// Internal work runs under require_approval — the approval that matters
    /// is the downstream one on the external action — but every refusal still
    /// refuses: a denied or observed context must not even draft.
    #[test]
    fn internal_work_upgrades_approval_but_never_refusal() {
        assert_eq!(
            internal_work_disposition(PolicyDisposition::RequireApproval),
            PolicyDisposition::AutoExecute
        );
        assert_eq!(
            internal_work_disposition(PolicyDisposition::AutoExecute),
            PolicyDisposition::AutoExecute
        );
        for refusal in [
            PolicyDisposition::Deny,
            PolicyDisposition::ObserveOnly,
            PolicyDisposition::RecommendOnly,
        ] {
            assert_eq!(internal_work_disposition(refusal), refusal);
        }
    }

    fn warm_up(spent: i64, cap: i64) -> ContextEvidence {
        ContextEvidence {
            observations: EvidenceCount::NONE,
            bootstrap: BootstrapAllowance { spent, cap },
        }
    }

    /// The trap the floor's own comment names, made real: with no warm-up,
    /// nothing below the floor executes, so nothing produces the observations
    /// the floor is waiting for and the gate never opens by itself.
    #[test]
    fn without_a_warm_up_the_floor_seals_itself() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                Confidence::MAX,
                minimum,
                warm_up(0, 0),
                crate::measurement::RATE_FLOOR,
            ),
            PolicyDisposition::RequireApproval
        );
    }

    #[test]
    fn a_warm_up_with_room_keeps_unattended_execution() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                Confidence::MAX,
                minimum,
                warm_up(4, 5),
                crate::measurement::RATE_FLOOR,
            ),
            PolicyDisposition::AutoExecute
        );
    }

    /// The warm-up is bounded, which is the whole difference between it and
    /// removing the floor.
    #[test]
    fn a_spent_warm_up_goes_back_to_asking() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        for spent in [5, 6, 50] {
            assert_eq!(
                disposition_with_evidence(
                    AutonomyLevel::BoundedAuto,
                    Confidence::MAX,
                    minimum,
                    warm_up(spent, 5),
                    crate::measurement::RATE_FLOOR,
                ),
                PolicyDisposition::RequireApproval,
                "spent {spent} of 5 must be exhausted"
            );
        }
    }

    /// The allowance answers "may this run unattended". Nothing at or below
    /// approval is asking that, so a warm-up must not promote one.
    #[test]
    fn a_warm_up_never_promotes_a_narrower_level() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
        ] {
            assert_eq!(
                disposition_with_evidence(
                    AutonomyLevel::BoundedAuto,
                    Confidence::MAX,
                    minimum,
                    warm_up(0, 100),
                    crate::measurement::RATE_FLOOR,
                ),
                PolicyDisposition::AutoExecute,
                "the warm-up applies to bounded_auto"
            );
            assert_eq!(
                disposition_with_evidence(
                    level,
                    Confidence::MAX,
                    minimum,
                    warm_up(0, 100),
                    crate::measurement::RATE_FLOOR,
                ),
                disposition(level, Confidence::MAX, minimum),
                "{level:?} must be untouched by a warm-up"
            );
        }
    }

    /// A denial is not a permissions question, and a warm-up is not an answer
    /// to one. A context below its own confidence bar stays refused however
    /// much allowance it has.
    #[test]
    fn a_warm_up_does_not_soften_a_denial() {
        let confidence = Confidence::saturating_from_basis_points(7_999);
        let minimum = Confidence::saturating_from_basis_points(8_000);
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                confidence,
                minimum,
                warm_up(0, 100),
                crate::measurement::RATE_FLOOR,
            ),
            PolicyDisposition::Deny
        );
    }

    /// Above the floor the allowance is irrelevant: a context that has earned
    /// its evidence is not spending a warm-up any more.
    #[test]
    fn a_cleared_floor_ignores_the_warm_up() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        let floor = crate::measurement::RATE_FLOOR;
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                Confidence::MAX,
                minimum,
                ContextEvidence {
                    observations: EvidenceCount(floor),
                    bootstrap: BootstrapAllowance { spent: 999, cap: 0 },
                },
                floor,
            ),
            PolicyDisposition::AutoExecute
        );
    }

    /// A context that failed its own confidence bar stays refused. Thin
    /// evidence must not turn a denial into an approval queue item.
    #[test]
    fn thin_evidence_does_not_soften_a_denial() {
        let confidence = Confidence::saturating_from_basis_points(7_999);
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                confidence,
                minimum,
                ContextEvidence::UNPROVEN,
                crate::measurement::RATE_FLOOR,
            ),
            PolicyDisposition::Deny
        );
    }
}
