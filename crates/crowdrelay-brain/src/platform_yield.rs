//! Which platforms actually return audience, and which templates to try first.
//!
//! The strategy's `template_priority()` is a fixed order written once: for
//! `AggressiveDiscovery`, Reddit is always tried before Telegram, which is
//! always tried before Bandcamp. That order encodes a guess about which
//! platform is easiest to grow, made before any tenant had run, and it never
//! changed no matter what the numbers said. A tenant whose Telegram audience
//! doubles monthly while Reddit sits flat kept being sent to Reddit first.
//!
//! Meanwhile the evidence was already being collected — one metric series per
//! platform, sampled daily — and nothing read it for this purpose.
//!
//! This module turns those series into a per-platform yield and reranks the
//! strategy's list by it. Three properties matter:
//!
//! - **The strategy still decides.** Reranking happens inside the list a
//!   strategy chose; it never adds a template the strategy excluded, and never
//!   changes which strategy is selected. `template_priority()` is unchanged and
//!   remains the prior.
//!
//! - **Evidence has to earn the move.** A platform with almost no audience can
//!   post a huge percentage gain from noise. Yield is shrunk toward the prior
//!   by an evidence weight, so a platform reorders only once it has enough
//!   audience behind the number to mean something.
//!
//! - **A Signal install outweighs a follower.** The North Star is fans, and a
//!   Signal install is an addressable fan — someone reachable directly, not a
//!   number on someone else's platform. `SIGNAL_VALUE_MULTIPLE` states that
//!   preference once, in the open, rather than leaving it implicit in a
//!   hand-ordered list.
//!
//! This does not touch the causal model, the context GLM, the EFE calculation
//! or any stored posterior. It reorders a list of candidate templates; the
//! consuming code already treats that order as a rank, not a score.

use serde::{Deserialize, Serialize};

/// How much more a Signal install is worth than one follower elsewhere.
///
/// A Signal install is a fan the tenant can reach on purpose. A follower is
/// reach rented from a platform that decides who sees what. Five is a stated
/// preference, not a measurement — it belongs in the open where it can be
/// argued with, which is the point of naming it.
const SIGNAL_VALUE_MULTIPLE: u32 = 5;

/// Audience below which a platform's growth rate is treated as noise.
///
/// Going from 2 followers to 4 is 100% growth and means nothing. Requiring a
/// floor stops a near-empty platform from winning the ranking on a rate.
const MINIMUM_MEANINGFUL_AUDIENCE: u32 = 50;

/// Audience at which a platform's measured yield is trusted in full.
///
/// Between the floor and here, the yield is blended with the neutral prior in
/// proportion to audience, so confidence grows with evidence instead of
/// switching on.
const FULL_CONFIDENCE_AUDIENCE: u32 = 1_000;

/// One platform's contribution to the North Star, as measured.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformGrowth {
    /// Metric-platform key, e.g. `soundcloud`, `telegram`, `signal`.
    pub platform: String,
    /// Latest known audience size on this platform.
    pub audience: u32,
    /// Audience gained since the start of this month.
    pub gained_this_month: u32,
}

impl PlatformGrowth {
    /// Fans-equivalent gained this month, with Signal weighted up.
    #[must_use]
    pub fn weighted_gain(&self) -> u32 {
        if self.platform == "signal" {
            self.gained_this_month.saturating_mul(SIGNAL_VALUE_MULTIPLE)
        } else {
            self.gained_this_month
        }
    }

    /// Monthly growth in basis points of the existing audience, shrunk toward
    /// zero while the audience is too small to trust.
    ///
    /// Returns `None` below the floor: no answer is better than a loud wrong
    /// one, and the caller keeps the prior order for that platform.
    #[must_use]
    pub fn yield_bps(&self) -> Option<u32> {
        if self.audience < MINIMUM_MEANINGFUL_AUDIENCE {
            return None;
        }
        let raw = u64::from(self.weighted_gain())
            .saturating_mul(10_000)
            .checked_div(u64::from(self.audience))?;
        // Linear shrinkage between the floor and full confidence.
        let confidence = u64::from(self.audience.min(FULL_CONFIDENCE_AUDIENCE));
        let shrunk = raw
            .saturating_mul(confidence)
            .checked_div(u64::from(FULL_CONFIDENCE_AUDIENCE))?;
        u32::try_from(shrunk.min(u64::from(u32::MAX))).ok()
    }
}

/// The metric platform a template acts on, if it acts on exactly one.
///
/// Templates that work across platforms or none — a strategist that only
/// thinks, a press pitch that targets outlets rather than a feed — return
/// `None` and keep their position from the strategy's order.
#[must_use]
pub fn template_platform(template: &str) -> Option<&'static str> {
    match template {
        // Reddit records its metrics under the `social` coverage bucket
        // because `MetricPlatform` has no Reddit variant.
        "reddit-scanner" | "community-engager" => Some("social"),
        "telegram-scanner" | "telegram-poster" => Some("telegram"),
        "bandcamp-scanner" => Some("bandcamp"),
        "signal-inviter" => Some("signal"),
        _ => None,
    }
}

/// Reorders a strategy's template list by measured platform yield.
///
/// Stable: templates whose platform has no trustworthy measurement keep their
/// relative position, and ties are broken by the prior order. The returned list
/// always contains exactly the input templates — this reranks, it never adds or
/// drops one.
#[must_use]
pub fn rank_templates(prior: &[&'static str], growth: &[PlatformGrowth]) -> Vec<&'static str> {
    let score_for = |template: &str| -> Option<u32> {
        let platform = template_platform(template)?;
        growth
            .iter()
            .find(|entry| entry.platform == platform)
            .and_then(PlatformGrowth::yield_bps)
    };

    let mut ranked: Vec<(usize, &'static str, Option<u32>)> = prior
        .iter()
        .enumerate()
        .map(|(index, template)| (index, *template, score_for(template)))
        .collect();

    ranked.sort_by(|left, right| {
        // Measured platforms sort ahead of unmeasured ones, best first. Two
        // unmeasured templates — or two with equal yield — fall back to the
        // strategy's own order, so the prior survives wherever evidence does
        // not contradict it.
        right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0))
    });

    ranked
        .into_iter()
        .map(|(_, template, _)| template)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn growth(platform: &str, audience: u32, gained: u32) -> PlatformGrowth {
        PlatformGrowth {
            platform: platform.to_owned(),
            audience,
            gained_this_month: gained,
        }
    }

    const PRIOR: &[&str] = &[
        "reddit-scanner",
        "telegram-scanner",
        "bandcamp-scanner",
        "growth-strategist",
        "signal-inviter",
    ];

    #[test]
    fn no_evidence_leaves_the_strategy_order_untouched() {
        assert_eq!(rank_templates(PRIOR, &[]), PRIOR.to_vec());
    }

    #[test]
    fn a_platform_that_actually_grows_is_tried_first() {
        // Telegram grows 10% of a real audience; Reddit is flat.
        let measured = [growth("social", 5_000, 0), growth("telegram", 2_000, 200)];
        let ranked = rank_templates(PRIOR, &measured);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "the platform returning audience should be tried before the flat one"
        );
    }

    #[test]
    fn a_tiny_audience_cannot_win_on_a_percentage() {
        // 2 followers becoming 4 is 100% growth and is meaningless.
        let measured = [growth("telegram", 4, 2), growth("social", 5_000, 250)];
        let ranked = rank_templates(PRIOR, &measured);
        assert_eq!(
            ranked.first(),
            Some(&"reddit-scanner"),
            "a platform below the evidence floor must not outrank a measured one"
        );
    }

    #[test]
    fn a_signal_install_outweighs_a_follower() {
        // Equal audience, equal raw gain. Signal should still win, because an
        // addressable fan is worth more than a follower.
        let measured = [growth("telegram", 1_000, 50), growth("signal", 1_000, 50)];
        let ranked = rank_templates(PRIOR, &measured);
        assert_eq!(
            ranked.first(),
            Some(&"signal-inviter"),
            "Signal installs are the North Star's most valuable unit"
        );
    }

    #[test]
    fn reranking_never_adds_or_drops_a_template() {
        let measured = [growth("signal", 900, 90), growth("telegram", 4, 4)];
        let mut ranked = rank_templates(PRIOR, &measured);
        ranked.sort_unstable();
        let mut expected = PRIOR.to_vec();
        expected.sort_unstable();
        assert_eq!(ranked, expected, "the rerank must be a permutation");
    }

    #[test]
    fn shrinkage_makes_confidence_grow_with_audience() {
        // Same 10% growth rate, different amounts of evidence behind it.
        let small = growth("telegram", 100, 10)
            .yield_bps()
            .expect("above floor");
        let large = growth("telegram", 1_000, 100)
            .yield_bps()
            .expect("above floor");
        assert!(
            small < large,
            "the same rate on a larger audience must count for more: {small} vs {large}"
        );
    }

    #[test]
    fn unmeasured_templates_keep_their_relative_order() {
        let measured = [growth("signal", 1_000, 100)];
        let ranked = rank_templates(PRIOR, &measured);
        let strategist = ranked.iter().position(|t| *t == "growth-strategist");
        let bandcamp = ranked.iter().position(|t| *t == "bandcamp-scanner");
        assert!(
            bandcamp < strategist,
            "templates with no measurement should keep the strategy's ordering"
        );
    }
}
