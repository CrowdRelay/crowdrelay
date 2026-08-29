//! Resource Cost — the economic abstraction for candidate actions.
//!
//! The brain's portfolio optimizer needs to reason about resource
//! consumption, not just expected fan value. Previously every candidate
//! had `cost: 1`, which meant the brain couldn't distinguish a cheap
//! Reddit scan from an expensive press pitch.
//!
//! # Phase 1: operator-configured
//!
//! The `units` field is set per-template by the operator. These are
//! **operator-configured resource units**, NOT empirically measured
//! costs. The architecture supports `Measured` and `Learned` sources
//! for future phases, but the first version is explicitly configured.
//!
//! Do NOT pretend that `community_engager = 2.0` is empirically correct
//! just because it is configurable. The real win is that the architecture
//! can learn/calibrate those units later.
//!
//! # Phase 2: multi-dimensional (future)
//!
//! The optional dimension fields (`llm_tokens`, `api_calls`, etc.) are
//! NOT summed into `units` blindly. You don't inherently have:
//! `1 API call = 1 audience attention unit`. Blindly doing
//! `cost = tokens + api_calls + attention + risk` creates fake
//! mathematics. Phase 2 will derive `units` from dimensions via
//! learned/defined conversion weights grounded in actual scarcity.
//!
//! # Reputation risk is a constraint, not a fungible cost
//!
//! A candidate with `+10 expected fans` and high reputation risk should
//! not automatically beat `+7 expected fans` with negligible risk.
//! Reputation risk should eventually be a **hard constraint** in
//! `PortfolioConfig`, not an additive cost. For now, it's a future
//! dimension — not in `units`.

use serde::{Deserialize, Serialize};

/// The source of a resource cost value — tracks provenance so the
/// brain never confuses operator guesses with measured reality.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// Operator-configured effective resource units. Not empirically
    /// measured — just a tunable knob. This is the Phase 1 default.
    #[default]
    Configured,
    /// Measured from actual resource consumption (future).
    Measured,
    /// Learned from data (future).
    Learned,
}

/// The resource cost of a candidate action.
///
/// Phase 1: `units` is set directly per-template by the operator.
/// Phase 2: `units` will be derived from the optional dimensions via
/// learned conversion weights.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ResourceCost {
    /// Composite cost used by portfolio optimization.
    /// This is the operator-configured effective cost per template.
    /// Phase 1: set directly. Phase 2: computed from dimensions.
    pub units: f64,
    /// Provenance — how this cost was determined.
    pub source: CostSource,
    /// Optional future dimensions — NOT summed into `units` blindly.
    /// You don't inherently have: 1 API call = 1 audience attention unit.
    pub llm_tokens: Option<f64>,
    pub api_calls: Option<f64>,
    pub audience_attention: Option<f64>,
    pub reputation_risk: Option<f64>,
    pub campaign_slots: Option<f64>,
}

impl ResourceCost {
    /// Creates an operator-configured resource cost with the given
    /// composite units. This is the Phase 1 constructor.
    #[must_use]
    pub const fn configured(units: f64) -> Self {
        Self {
            units,
            source: CostSource::Configured,
            llm_tokens: None,
            api_calls: None,
            audience_attention: None,
            reputation_risk: None,
            campaign_slots: None,
        }
    }

    /// Creates a zero-cost resource cost (for the WAIT candidate).
    /// WAIT consumes no resources — its value comes from information,
    /// fatigue recovery, and option value, not from avoiding resource
    /// spending (that's already captured in the action's value/cost
    /// ratio).
    #[must_use]
    pub const fn zero() -> Self {
        Self::configured(0.0)
    }

    /// Returns true if this cost is zero (the WAIT candidate).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.units == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_cost_has_configured_source() {
        let cost = ResourceCost::configured(2.0);
        assert_eq!(cost.units, 2.0);
        assert_eq!(cost.source, CostSource::Configured);
    }

    #[test]
    fn zero_cost_is_zero() {
        let cost = ResourceCost::zero();
        assert_eq!(cost.units, 0.0);
        assert!(cost.is_zero());
    }

    #[test]
    fn default_cost_is_zero_configured() {
        let cost = ResourceCost::default();
        assert_eq!(cost.units, 0.0);
        assert_eq!(cost.source, CostSource::Configured);
        assert!(cost.is_zero());
    }
}
