//! The value ordering every ranked queue shares.
//!
//! Extracted from `growth_metrics` so engine-core ranking (`next_best_action`,
//! `growth_debt`) can depend on the *ordering* without importing a bounded
//! context. One tier list decides what outranks what across every detector —
//! vanity never outranks a downstream number, whatever business those numbers
//! describe, which is exactly why it lives in engine core.

use serde::{Deserialize, Serialize};

/// How close a metric sits to value the business banks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricValueTier {
    /// Reach and follower counts: real, but far from an outcome.
    Vanity,
    /// Intent: saves, trackers, listing interest, session depth.
    Intermediate,
    /// Banked outcomes: tickets, attendance, merch, retained fans.
    Downstream,
}

impl MetricValueTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vanity => "vanity",
            Self::Intermediate => "intermediate",
            Self::Downstream => "downstream",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "vanity" => Some(Self::Vanity),
            "intermediate" => Some(Self::Intermediate),
            "downstream" => Some(Self::Downstream),
            _ => None,
        }
    }

    pub(crate) const fn weight(self) -> u16 {
        match self {
            Self::Vanity => 20,
            Self::Intermediate => 55,
            Self::Downstream => 100,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_outranks_vanity_by_construction() {
        assert!(MetricValueTier::Downstream > MetricValueTier::Intermediate);
        assert!(MetricValueTier::Intermediate > MetricValueTier::Vanity);
        for tier in [
            MetricValueTier::Vanity,
            MetricValueTier::Intermediate,
            MetricValueTier::Downstream,
        ] {
            assert_eq!(MetricValueTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(MetricValueTier::parse("impressions"), None);
    }
}
