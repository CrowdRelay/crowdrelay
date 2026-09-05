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
            // Discovery is the wrong lever when reach already exists but is
            // not converting — see `reach_outruns_conversion`.
            && !Self::reach_outruns_conversion(world)
            // ...and it is not a lever at all when the feeds it works through
            // have stopped answering — see `discovery_channels_are_silent`.
            && !Self::discovery_channels_are_silent(world)
        {
            return Self::AggressiveDiscovery;
        }
        if let Some(days) = world.days_to_next_event
            && days <= 14
        {
            return Self::EventDriven;
        }
        // Audience already gathered, but almost none of it has become a fan we
        // can address directly. Finding more communities does not fix that;
        // publishing to the audience that already exists does. This is the
        // aggregate-then-convert split from the North Star, and without it a
        // tenant with 200k followers and 40 fans would be sent to look for
        // more followers.
        if Self::reach_outruns_conversion(world) {
            return Self::ContentFirst;
        }
        if Self::needs_conversion_push(world) {
            return Self::SignalConversion;
        }
        // A tenant behind on an off-platform north star needs more reach, not
        // more Signal invites. Widening the top of the funnel is the lever that
        // moves subscriber and follower counts — through feeds that are
        // answering.
        if world.growth_target_progress.north_star_status == TargetStatus::Behind
            && !Self::discovery_channels_are_silent(world)
        {
            return Self::AggressiveDiscovery;
        }
        Self::ContentFirst
    }

    /// Whether every feed this tenant has connected has stopped reporting.
    ///
    /// `fresh_platforms` counts platforms whose newest audience observation is
    /// under a week old. The world model has carried it, and
    /// `connected_platforms` beside it, since they were added — and nothing
    /// read either one. The brain therefore had the evidence that its channels
    /// were dead and planned as though they were live: production sat on
    /// `AggressiveDiscovery`, whose entire lead is scanners, while every Reddit
    /// connection was failing on an invalid credential. Every cycle dispatched
    /// scanners at feeds that answered nothing, and the operator watched the
    /// agent produce outreach proposals built on no data at all.
    ///
    /// Deliberately not `fresh_platforms == 0` alone. A tenant who has
    /// connected nothing yet has no feeds to have gone quiet, and discovery is
    /// exactly right for them — it is how they get a first channel. The
    /// condition is connected-and-silent: the channels exist and have stopped
    /// answering, which is the state in which looking harder through them is
    /// not a plan.
    ///
    /// The fallthrough is `ContentFirst`, which leads with publishing to the
    /// audience already held rather than with scanners. That is the same
    /// reasoning `reach_outruns_conversion` already applies, for the same
    /// reason: when the top of the funnel cannot be widened, work the part of
    /// it that exists.
    #[must_use]
    pub fn discovery_channels_are_silent(world: &WorldModel) -> bool {
        world.connected_platforms > 0 && world.fresh_platforms == 0
    }

    /// Whether the tenant's off-platform reach dwarfs the fanbase it converted
    /// out of that reach.
    ///
    /// The floor keeps a tenant with 5 followers and 0 fans out of this branch:
    /// a 10x ratio is meaningless at that scale, and their real problem is
    /// discovery. Above the floor the ratio is real evidence that the funnel,
    /// not the top of it, is the constraint.
    #[must_use]
    fn reach_outruns_conversion(world: &WorldModel) -> bool {
        const MIN_REACH: u32 = 1_000;
        const RATIO: u32 = 10;
        world.off_platform_audience >= MIN_REACH
            && world.off_platform_audience >= world.total_fans.saturating_mul(RATIO)
    }

    /// Whether the brain should prioritize Signal conversion workers.
    ///
    /// Only when Signal installs *are* the north star. `SignalConversion`
    /// leads with `signal-inviter`, and the evaluator refuses to dispatch that
    /// template unless the north star is `SignalInstalls` — so selecting this
    /// strategy for a YouTube or Spotify tenant would pick a plan whose first
    /// action can never fire.
    #[must_use]
    fn needs_conversion_push(world: &WorldModel) -> bool {
        use crowdrelay_domain::growth_metrics::NorthStarMetric;
        world.north_star == NorthStarMetric::SignalInstalls
            && world.total_fans > 50
            && world.signal_conversion_rate_bps < 500
    }

    /// Hysteresis version of the conversion push check: a more lenient
    /// threshold so the brain doesn't flip-flop in and out of SignalConversion.
    #[must_use]
    fn needs_conversion_push_with_hysteresis(world: &WorldModel) -> bool {
        use crowdrelay_domain::growth_metrics::NorthStarMetric;
        world.north_star == NorthStarMetric::SignalInstalls
            && world.total_fans > 50
            && world.signal_conversion_rate_bps < 700
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
                // Hysteresis keeps the brain from flip-flopping; it must not
                // keep it on a plan whose channels have gone quiet. Silence is
                // not a borderline reading.
                if world.growth_target_progress.status == TargetStatus::Behind
                    && world.fan_growth_trend.is_stagnant()
                    && !Self::discovery_channels_are_silent(world)
                {
                    return Self::AggressiveDiscovery;
                }
            }
            Self::SignalConversion => {
                if Self::needs_conversion_push_with_hysteresis(world) {
                    return Self::SignalConversion;
                }
            }
            Self::ContentFirst => {}
        }
        candidate
    }

    /// Returns the strategy's recommended template priority order.
    #[must_use]
    /// The strategy's order, reranked by what each platform actually returns.
    ///
    /// `template_priority` stays the prior and is still the only thing that
    /// decides *which* templates are candidates. This reorders that list using
    /// measured per-platform yield, so a tenant whose Telegram audience is
    /// compounding stops being sent to Reddit first merely because the list was
    /// written that way. With no evidence it returns the prior unchanged.
    pub fn template_priority_for(self, world: &WorldModel) -> Vec<&'static str> {
        crate::platform_yield::rank_templates(self.template_priority(), &world.platform_growth)
    }

    pub fn template_priority(self) -> &'static [&'static str] {
        match self {
            Self::AggressiveDiscovery => &[
                "reddit-scanner",
                "telegram-scanner",
                "metal-archives-scanner",
                "bandcamp-scanner",
                "community-engager",
                "growth-strategist",
                "social-post",
                "telegram-poster",
                "signal-inviter",
                "press-pitch",
            ],
            Self::EventDriven => &[
                "press-pitch",
                "social-post",
                "telegram-poster",
                "signal-inviter",
                "community-engager",
                "reddit-scanner",
                "telegram-scanner",
                "metal-archives-scanner",
                "bandcamp-scanner",
                "growth-strategist",
            ],
            Self::ContentFirst => &[
                "social-post",
                "telegram-poster",
                "community-engager",
                "growth-strategist",
                "reddit-scanner",
                "telegram-scanner",
                "metal-archives-scanner",
                "bandcamp-scanner",
                "signal-inviter",
                "press-pitch",
            ],
            Self::SignalConversion => &[
                "signal-inviter",
                "social-post",
                "telegram-poster",
                "growth-strategist",
                "community-engager",
                "reddit-scanner",
                "telegram-scanner",
                "metal-archives-scanner",
                "bandcamp-scanner",
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

    /// Returns all strategy variants, in a stable order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [
            Self::AggressiveDiscovery,
            Self::EventDriven,
            Self::ContentFirst,
            Self::SignalConversion,
        ]
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
    fn large_reach_with_a_tiny_fanbase_publishes_instead_of_discovering() {
        // 200k reachable, 40 fans: the funnel is the constraint, not the top
        // of it. Before this rule the brain sent them looking for more
        // communities they already had reach into.
        let world = WorldModel {
            total_fans: 40,
            off_platform_audience: 200_000,
            fan_growth_trend: GrowthTrend::Stagnant,
            growth_target_progress: GrowthTargetProgress {
                status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn silent_feeds_stop_the_brain_planning_more_discovery() {
        // Production's state for weeks: behind, stagnant, six Reddit feeds
        // connected and every one of them failing on an invalid credential, so
        // nothing had reported in over a week. The brain read `Behind` and
        // `Stagnant`, chose `AggressiveDiscovery` — whose entire lead is
        // scanners — and dispatched them at feeds that answered nothing, every
        // five minutes. `fresh_platforms` said so the whole time and nothing
        // read it.
        let world = WorldModel {
            total_fans: 10,
            connected_platforms: 6,
            fresh_platforms: 0,
            fan_growth_trend: GrowthTrend::Stagnant,
            growth_target_progress: GrowthTargetProgress {
                status: TargetStatus::Behind,
                north_star_status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst,
            "with every connected feed silent, publishing to the audience \
             already held is a plan and scanning dead feeds is not"
        );
    }

    #[test]
    fn a_tenant_with_nothing_connected_still_discovers() {
        // The narrow half of the rule. A tenant who has connected no feed has
        // none that could have gone quiet, and discovery is how they get a
        // first channel. Blocking them would be the opposite of the fix.
        let world = WorldModel {
            total_fans: 0,
            connected_platforms: 0,
            fresh_platforms: 0,
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
    fn one_live_feed_is_enough_to_keep_discovering() {
        let world = WorldModel {
            total_fans: 10,
            connected_platforms: 6,
            fresh_platforms: 1,
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
    fn hysteresis_does_not_hold_the_brain_on_silent_feeds() {
        // Hysteresis exists so a borderline reading does not flip the plan
        // every cycle. Every feed having stopped answering is not borderline,
        // and holding the previous strategy through it is how a brain stays
        // fixated on a dead channel.
        let world = WorldModel {
            total_fans: 10,
            connected_platforms: 6,
            fresh_platforms: 0,
            fan_growth_trend: GrowthTrend::Stagnant,
            growth_target_progress: GrowthTargetProgress {
                status: TargetStatus::Behind,
                north_star_status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model_with_hysteresis(
                &world,
                Some(GrowthStrategy::AggressiveDiscovery)
            ),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn an_off_platform_north_star_behind_still_needs_a_live_feed() {
        // The second discovery branch: behind on an off-platform north star is
        // a reach problem, and reach is widened through feeds. Silent ones
        // widen nothing.
        let world = WorldModel {
            total_fans: 10,
            connected_platforms: 3,
            fresh_platforms: 0,
            fan_growth_trend: GrowthTrend::Steady,
            growth_target_progress: GrowthTargetProgress {
                north_star_status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn small_reach_still_discovers_even_at_a_high_ratio() {
        // 200 reachable, 0 fans is a 200x ratio but says nothing — below the
        // floor the tenant genuinely needs discovery.
        let world = WorldModel {
            total_fans: 0,
            off_platform_audience: 200,
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
    fn an_imminent_event_still_outranks_the_conversion_gap() {
        let world = WorldModel {
            total_fans: 40,
            off_platform_audience: 200_000,
            days_to_next_event: Some(3),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::EventDriven
        );
    }

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
