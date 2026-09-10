//! Agent execution health — is the tool the brain dispatches to actually
//! working right now?
//!
//! `self_assessment` answers "is the North Star moving" — a growth-trend
//! question that takes days to change. This module answers a different,
//! much faster-moving question: over the last few hours, did the worker
//! layer (the TypeScript agent service and the LLM providers behind it)
//! actually produce usable outcomes, or did it claim tasks and produce
//! nothing the brain can act on?
//!
//! The gap this closes: the brain's dispatch budget is scaled by
//! `MetacognitionMonitor::sizing_multiplier()`, which is a function of the
//! North Star series alone. Nothing in that signal reflects "my LLM
//! provider is out of quota" or "my only verifier is dead" — the brain
//! would keep dispatching at full budget while every dispatch produced an
//! outcome that got rejected downstream (`NOT_GROUNDING_CHECKED`) or a
//! task that claimed and completed without a usable result. Both happened
//! in production without the brain's own state model reflecting it.
//!
//! Mirrors Kern's `Regime`-driven `aggression_bps`: a state derived from
//! recent operational signal, with a sizing multiplier that scales
//! dispatch budget down when the tools are unreliable. Unlike `Regime`,
//! this is not multi-axis market physics — it is one ratio (rejected +
//! failed over attempted) with a minimum sample size, because a handful
//! of tasks is not enough to distinguish "broken" from "unlucky".

use serde::{Deserialize, Serialize};

/// Below this many recent attempts, the ratio is too noisy to act on —
/// one failed task out of two looks identical to a real 50% failure rate
/// and to a fluke. Matches the spirit of `self_assessment::MINIMUM_DAYS`:
/// the honest answer below the sample floor is "not enough signal yet",
/// not a guess dressed up as an assessment.
const MINIMUM_SAMPLE: u32 = 5;

/// Failure+rejection ratio (in basis points of `total`) at or above which
/// execution counts as `Failing` rather than merely `Degraded`.
const FAILING_THRESHOLD_BPS: u32 = 7_000;

/// Failure+rejection ratio at or above which execution counts as
/// `Degraded` rather than `Healthy`.
const DEGRADED_THRESHOLD_BPS: u32 = 3_000;

/// The brain's assessment of whether its worker layer is currently
/// producing usable outcomes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionHealth {
    /// Recent tasks are succeeding and their outcomes are getting through
    /// the verification/data-quality gate. Full dispatch budget.
    #[default]
    Healthy,
    /// A material fraction of recent tasks failed outright or produced
    /// outcomes the data-quality gate rejected. Dispatch budget reduced —
    /// still trying, because a single bad provider is not a reason to stop
    /// entirely, but not spending it at the same rate into a gate that is
    /// throwing most of it away.
    Degraded,
    /// Most recent attempts produced nothing usable. Dispatch is throttled
    /// hard: continuing to fire the same broken path at full rate is not
    /// persistence, it is waste — every dispatch still consumes the 24h
    /// action quota and produces a decision row with nothing behind it.
    Failing,
    /// Fewer than `MINIMUM_SAMPLE` attempts in the window. Not enough
    /// signal to say anything — full budget, same as `Healthy`, but kept
    /// distinct so a caller can tell "known good" from "no data yet".
    Unknown,
}

impl AgentExecutionHealth {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failing => "failing",
            Self::Unknown => "unknown",
        }
    }

    /// Multiplier for dispatch budget (0.0–1.0). Combines with
    /// `MetacognitionMonitor::sizing_multiplier()` by multiplication — the
    /// two signals are independent (growth trend vs tool reliability) and
    /// either one being bad should reduce budget regardless of the other.
    #[must_use]
    pub const fn sizing_multiplier(self) -> f64 {
        match self {
            Self::Healthy | Self::Unknown => 1.0,
            Self::Degraded => 0.5,
            Self::Failing => 0.15,
        }
    }

    /// Whether a person should look at this now. `Unknown` is not urgent —
    /// it is the honest answer for a workspace with too little recent
    /// activity to assess, not a fault.
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::Degraded | Self::Failing)
    }

    /// Classifies execution health from a window's outcome counts.
    ///
    /// `failed_or_rejected` counts both task-level failures (the agent
    /// service marked the task `failed`) and outcome-level rejections
    /// (the data-quality gate marked an `agent_outcomes` row `rejected` —
    /// most commonly `NOT_GROUNDING_CHECKED`, a real task that completed
    /// but produced nothing the brain can act on). `total` is every
    /// attempt in the window, successful or not.
    ///
    /// `total < failed_or_rejected` is a caller error (can't reject more
    /// than attempted); it is clamped rather than panicking, because a
    /// window straddling two different counting queries racing against
    /// live writes is a real possibility and "assume healthy" is the
    /// wrong failure direction for a caller bug here — clamping to 100%
    /// failure keeps the assessment honest instead of hiding the
    /// inconsistency behind a good score.
    #[must_use]
    pub fn assess(total: u32, failed_or_rejected: u32) -> Self {
        if total < MINIMUM_SAMPLE {
            return Self::Unknown;
        }
        let failed_or_rejected = failed_or_rejected.min(total);
        let ratio_bps = (u64::from(failed_or_rejected) * 10_000 / u64::from(total)) as u32;
        if ratio_bps >= FAILING_THRESHOLD_BPS {
            Self::Failing
        } else if ratio_bps >= DEGRADED_THRESHOLD_BPS {
            Self::Degraded
        } else {
            Self::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_minimum_sample_is_unknown_not_healthy() {
        assert_eq!(
            AgentExecutionHealth::assess(4, 4),
            AgentExecutionHealth::Unknown
        );
        assert_eq!(
            AgentExecutionHealth::assess(0, 0),
            AgentExecutionHealth::Unknown
        );
    }

    #[test]
    fn all_succeeding_is_healthy() {
        assert_eq!(
            AgentExecutionHealth::assess(20, 0),
            AgentExecutionHealth::Healthy
        );
    }

    #[test]
    fn below_degraded_threshold_is_healthy() {
        // 29% failure — just under the 30% degraded threshold.
        assert_eq!(
            AgentExecutionHealth::assess(100, 29),
            AgentExecutionHealth::Healthy
        );
    }

    #[test]
    fn at_degraded_threshold_is_degraded() {
        assert_eq!(
            AgentExecutionHealth::assess(100, 30),
            AgentExecutionHealth::Degraded
        );
    }

    #[test]
    fn below_failing_threshold_is_degraded() {
        assert_eq!(
            AgentExecutionHealth::assess(100, 69),
            AgentExecutionHealth::Degraded
        );
    }

    #[test]
    fn at_failing_threshold_is_failing() {
        assert_eq!(
            AgentExecutionHealth::assess(100, 70),
            AgentExecutionHealth::Failing
        );
    }

    #[test]
    fn total_failure_is_failing() {
        assert_eq!(
            AgentExecutionHealth::assess(10, 10),
            AgentExecutionHealth::Failing
        );
    }

    #[test]
    fn rejected_count_above_total_is_clamped_not_panicking() {
        // A caller bug (two counts racing against live writes) must not
        // panic and must not read as healthy — clamp to 100% failure.
        assert_eq!(
            AgentExecutionHealth::assess(10, 15),
            AgentExecutionHealth::Failing
        );
    }

    #[test]
    fn sizing_multiplier_is_full_when_healthy_or_unknown() {
        assert_eq!(AgentExecutionHealth::Healthy.sizing_multiplier(), 1.0);
        assert_eq!(AgentExecutionHealth::Unknown.sizing_multiplier(), 1.0);
    }

    #[test]
    fn sizing_multiplier_drops_with_severity() {
        assert!(
            AgentExecutionHealth::Failing.sizing_multiplier()
                < AgentExecutionHealth::Degraded.sizing_multiplier()
        );
        assert!(AgentExecutionHealth::Degraded.sizing_multiplier() < 1.0);
    }

    #[test]
    fn needs_attention_is_false_for_healthy_and_unknown() {
        assert!(!AgentExecutionHealth::Healthy.needs_attention());
        assert!(!AgentExecutionHealth::Unknown.needs_attention());
        assert!(AgentExecutionHealth::Degraded.needs_attention());
        assert!(AgentExecutionHealth::Failing.needs_attention());
    }

    #[test]
    fn as_str_round_trips_through_serde() {
        for state in [
            AgentExecutionHealth::Healthy,
            AgentExecutionHealth::Degraded,
            AgentExecutionHealth::Failing,
            AgentExecutionHealth::Unknown,
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            let back: AgentExecutionHealth = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(state, back);
            assert!(json.contains(state.as_str()));
        }
    }
}
