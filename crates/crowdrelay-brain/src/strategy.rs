//! Hierarchical planning — strategy → pathway → action.
//!
//! The brain doesn't just dispatch individual workers — it plans pathways.
//! The strategy determines which families of actions get priority, with
//! hysteresis to prevent flip-flopping.

use serde::Serialize;

use crate::world_model::{TargetStatus, WorldModel};

/// A growth strategy — the brain's high-level approach to fan acquisition.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthStrategy {
    #[default]
    AggressiveDiscovery,
    EventDriven,
    ContentFirst,
    SignalConversion,
}

impl GrowthStrategy {
    /// Derives the current strategy from the world model.
    #[must_use]
    pub fn from_world_model(world: &WorldModel) -> Self {
        if world.growth_target_progress.status == TargetStatus::Behind
            && world.fan_growth_trend.is_stagnant()
        {
            return Self::AggressiveDiscovery;
        }
        if let Some(days) = world.days_to_next_event
            && days <= 14
        {
            return Self::EventDriven;
        }
        if world.total_fans > 50 && world.signal_conversion_rate_bps < 500 {
            return Self::SignalConversion;
        }
        Self::ContentFirst
    }

    /// Derives the strategy with hysteresis.
    #[must_use]
    pub fn from_world_model_with_hysteresis(world: &WorldModel, current: Option<Self>) -> Self {
        let candidate = Self::from_world_model(world);
        let Some(current) = current else {
            return candidate;
        };
        if candidate == current {
            return current;
        }
        match current {
            Self::EventDriven => {
                if let Some(days) = world.days_to_next_event
                    && days <= 21
                {
                    return Self::EventDriven;
                }
            }
            Self::AggressiveDiscovery => {
                if world.growth_target_progress.status == TargetStatus::Behind
                    && world.fan_growth_trend.is_stagnant()
                {
                    return Self::AggressiveDiscovery;
                }
            }
            Self::SignalConversion => {
                if world.total_fans > 50 && world.signal_conversion_rate_bps < 700 {
                    return Self::SignalConversion;
                }
            }
            Self::ContentFirst => {}
        }
        candidate
    }

    /// Returns the strategy's recommended template priority order.
    #[must_use]
    pub fn template_priority(self) -> &'static [&'static str] {
        match self {
            Self::AggressiveDiscovery => &[
                "reddit-scanner",
                "community-engager",
                "growth-strategist",
                "social-post",
                "signal-inviter",
                "press-pitch",
            ],
            Self::EventDriven => &[
                "press-pitch",
                "social-post",
                "signal-inviter",
                "community-engager",
                "reddit-scanner",
                "growth-strategist",
            ],
            Self::ContentFirst => &[
                "social-post",
                "community-engager",
                "growth-strategist",
                "reddit-scanner",
                "signal-inviter",
                "press-pitch",
            ],
            Self::SignalConversion => &[
                "signal-inviter",
                "social-post",
                "growth-strategist",
                "community-engager",
                "reddit-scanner",
                "press-pitch",
            ],
        }
    }

    /// Returns a human-readable name for the strategy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AggressiveDiscovery => "aggressive_discovery",
            Self::EventDriven => "event_driven",
            Self::ContentFirst => "content_first",
            Self::SignalConversion => "signal_conversion",
        }
    }

    /// Infers the strategy from the most recently dispatched template.
    #[must_use]
    pub fn infer_from_template(template_id: &str) -> Self {
        let strategies = [
            Self::AggressiveDiscovery,
            Self::EventDriven,
            Self::ContentFirst,
            Self::SignalConversion,
        ];
        let mut best = Self::ContentFirst;
        let mut best_rank = usize::MAX;
        for strategy in strategies {
            let rank = strategy
                .template_priority()
                .iter()
                .position(|t| *t == template_id)
                .unwrap_or(usize::MAX);
            if rank < best_rank {
                best_rank = rank;
                best = strategy;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_model::{GrowthTargetProgress, GrowthTrend};

    #[test]
    fn strategy_aggressive_discovery_when_behind_and_stagnant() {
        let world = WorldModel {
            fan_growth_trend: GrowthTrend::Stagnant,
            growth_target_progress: GrowthTargetProgress {
                status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::AggressiveDiscovery
        );
    }

    #[test]
    fn strategy_event_driven_when_event_close() {
        let world = WorldModel {
            days_to_next_event: Some(10),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::EventDriven
        );
    }

    #[test]
    fn strategy_signal_conversion_when_adoption_low() {
        let world = WorldModel {
            total_fans: 100,
            signal_conversion_rate_bps: 200,
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::SignalConversion
        );
    }

    #[test]
    fn strategy_content_first_as_default() {
        let world = WorldModel {
            total_fans: 100,
            signal_conversion_rate_bps: 800,
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn strategy_template_priority_orders_templates() {
        assert_eq!(
            GrowthStrategy::AggressiveDiscovery.template_priority()[0],
            "reddit-scanner"
        );
        assert_eq!(
            GrowthStrategy::EventDriven.template_priority()[0],
            "press-pitch"
        );
        assert_eq!(
            GrowthStrategy::ContentFirst.template_priority()[0],
            "social-post"
        );
        assert_eq!(
            GrowthStrategy::SignalConversion.template_priority()[0],
            "signal-inviter"
        );
    }

    #[test]
    fn strategy_hysteresis_stays_event_driven_past_entry_threshold() {
        let world = WorldModel {
            days_to_next_event: Some(18),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
        assert_eq!(
            GrowthStrategy::from_world_model_with_hysteresis(
                &world,
                Some(GrowthStrategy::EventDriven)
            ),
            GrowthStrategy::EventDriven
        );
    }

    #[test]
    fn strategy_hysteresis_switches_when_far_from_event() {
        let world = WorldModel {
            days_to_next_event: Some(25),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model_with_hysteresis(
                &world,
                Some(GrowthStrategy::EventDriven)
            ),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn strategy_hysteresis_stays_signal_conversion_past_entry_threshold() {
        let world = WorldModel {
            total_fans: 100,
            signal_conversion_rate_bps: 600,
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
        assert_eq!(
            GrowthStrategy::from_world_model_with_hysteresis(
                &world,
                Some(GrowthStrategy::SignalConversion)
            ),
            GrowthStrategy::SignalConversion
        );
    }

    #[test]
    fn strategy_hysteresis_no_previous_strategy_uses_candidate() {
        let world = WorldModel {
            days_to_next_event: Some(10),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model_with_hysteresis(&world, None),
            GrowthStrategy::EventDriven
        );
    }

    #[test]
    fn strategy_infer_from_template() {
        assert_eq!(
            GrowthStrategy::infer_from_template("reddit-scanner"),
            GrowthStrategy::AggressiveDiscovery
        );
        assert_eq!(
            GrowthStrategy::infer_from_template("press-pitch"),
            GrowthStrategy::EventDriven
        );
        assert_eq!(
            GrowthStrategy::infer_from_template("signal-inviter"),
            GrowthStrategy::SignalConversion
        );
    }

    #[test]
    fn strategy_as_str_returns_snake_case() {
        assert_eq!(
            GrowthStrategy::AggressiveDiscovery.as_str(),
            "aggressive_discovery"
        );
        assert_eq!(GrowthStrategy::EventDriven.as_str(), "event_driven");
        assert_eq!(GrowthStrategy::ContentFirst.as_str(), "content_first");
        assert_eq!(
            GrowthStrategy::SignalConversion.as_str(),
            "signal_conversion"
        );
    }
}
