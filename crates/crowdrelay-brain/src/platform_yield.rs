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
//! strategy's list by it. Four properties matter:
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
//! - **A rate is noisy on a small audience; a gain never is.** The evidence
//!   floor applies to the rate and only to the rate. Below it the platform is
//!   still ranked on the absolute audience it returned, because that is the
//!   North Star quantity rather than a ratio, and suppressing it let a large
//!   flat platform outrank the one that delivered the tenant's only fan. See
//!   [`PlatformGrowth::rank_key`].
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

    /// What this platform is ranked on: its trustworthy rate first, then the
    /// absolute audience it actually returned.
    ///
    /// The rate alone was not enough, and the gap it left was the worst case for
    /// exactly the tenant this module exists to help. `yield_bps()` is `None`
    /// below the evidence floor, and `None` sorts behind `Some(0)`, so a platform
    /// with five thousand followers that gained *nothing* outranked the platform
    /// that delivered the tenant's only addressable fan. The Signal multiple was
    /// written to prefer Signal and the floor guaranteed Signal could not place
    /// until it already had fifty installs — with one install in production, the
    /// most valuable platform was the one permanently last.
    ///
    /// The absolute weighted gain closes it, and it is safe where a rate is not:
    /// it is the North Star quantity itself, not a ratio, so there is no small
    /// denominator to inflate it. Two followers becoming four is 100% and
    /// meaningless as a rate; as a gain it is two, and two is honestly less than
    /// two hundred. The floor keeps doing its job — it still stops a near-empty
    /// platform's *rate* from winning — it no longer erases the platform's gain.
    ///
    /// `None` only when there is nothing at all: no trustworthy rate and no gain.
    /// A platform that returned nobody is not evidence for trying it again, so it
    /// ranks with the unmeasured templates rather than ahead of them.
    #[must_use]
    pub fn rank_key(&self) -> RankKey {
        let rate = self.yield_bps().unwrap_or(0);
        let absolute = self.weighted_gain();
        if rate == 0 && absolute == 0 {
            return None;
        }
        Some((rate, absolute))
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

/// Growth-metric platforms a template can directly compound.
///
/// Most templates own one platform. `social-post` is different on purpose:
/// one run creates Instagram, Facebook and X variants, so refusing to map it
/// to platform growth made real Meta follower growth invisible to strategy
/// ranking until tracked conversions arrived. The rank uses the strongest
/// measured platform signal; direct retained-fan attribution still lives in
/// the stronger evidence tier below and therefore wins once it matures.
#[must_use]
pub fn template_growth_platforms(template: &str) -> &'static [&'static str] {
    match template {
        "social-post" => &["instagram", "facebook", "x"],
        _ => match template_platform(template) {
            Some("social") => &["social"],
            Some("telegram") => &["telegram"],
            Some("bandcamp") => &["bandcamp"],
            Some("signal") => &["signal"],
            _ => &[],
        },
    }
}

/// What a channel actually returned, measured at the fan rather than the
/// follower: attributed conversions and the distinct visitors whose clicks
/// preceded them, read from `fan_provenance_events`.
///
/// This is the other half of the evidence `PlatformGrowth` never saw. The
/// metric series counts followers going up on a platform — correlational,
/// and blind to whether anyone arrived. The provenance ledger records the
/// arrivals themselves, labelled by channel: a conversion under `instagram`
/// is a fan who exists because that channel's post was clicked.
///
/// The two sources are deliberately different in kind, and the rank keeps
/// them that way rather than averaging them into one number. See
/// [`ChannelYield::rank_key`] for how each is weighted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelYield {
    /// The channel label on the smart link — `reddit`, `telegram`,
    /// `instagram`, `discord`, or an owned-arrival path like `concert_qr`.
    pub channel: String,
    /// Fans attributed to this channel's tracked links in the last 30 days.
    pub conversions_30d: u32,
    /// Distinct visitors who clicked this channel's tracked links in the
    /// last 30 days — the early signal a conversion takes weeks to become.
    pub unique_clickers_30d: u32,
    /// Fans this channel converted in the last 90 days who stayed: still
    /// active, still consented to hear from the band, and meaningfully active
    /// in the last 30 days — the North Star's own definition. A channel that
    /// converts people who leave is a signup farm; this is what separates it
    /// from one whose fans are still here.
    #[serde(default)]
    pub durable_90d: u32,
}

impl ChannelYield {
    /// Direct acquisition evidence without pretending unlike stages are one
    /// currency. A conversion is categorically stronger than any number of
    /// clicks; clicks remain useful only when no converted fan exists yet.
    #[must_use]
    pub fn rank_key(&self) -> RankKey {
        if self.conversions_30d > 0 {
            Some((2, self.conversions_30d))
        } else if self.unique_clickers_30d > 0 {
            Some((1, self.unique_clickers_30d))
        } else {
            None
        }
    }
}

/// The provenance channels a template posts or scans on, in the vocabulary
/// `smart_links.channel_source` writes.
///
/// Templates with no outbound channel — a strategist that thinks, a press
/// pitch that targets outlets, a scanner for a platform nobody posts to —
/// return an empty list and keep their position from the strategy's order.
/// Scanners share their channel's poster evidence on purpose: a channel that
/// converts is worth scouting harder, the same rule platform growth already
/// applies.
#[must_use]
pub fn template_channels(template: &str) -> &'static [&'static str] {
    match template {
        "community-engager" | "reddit-scanner" => &["reddit"],
        "telegram-poster" | "telegram-scanner" => &["telegram"],
        "social-post" => &["instagram", "facebook", "x"],
        "discord-poster" => &["discord"],
        _ => &[],
    }
}

/// What a platform is ranked on: trustworthy rate in basis points, then the
/// absolute weighted gain. `None` when the platform is unmeasured.
///
/// Compared in field order, so the rate leads and the gain breaks ties the rate
/// cannot see. See [`PlatformGrowth::rank_key`].
type RankKey = Option<(u32, u32)>;

/// One template's position in the prior, its name, how many durable fans its
/// attributed channel retained, and the weaker platform/channel evidence.
///
/// Durable fans are a direct observation of the default North Star. They sort
/// before proxy evidence; the existing RankKey decides only when neither side
/// has earned that stronger signal.
type RankedTemplate = (usize, &'static str, u32, u32, RankKey, u32);

/// Reorders a strategy's template list by measured platform yield and by the
/// attributed fan yield of the channel each template posts on.
///
/// Evidence sources keep their semantics instead of being collapsed into an
/// arbitrary fan-equivalent sum. The ordering follows how directly each stage
/// answers the North Star:
///
/// 1. **durable attributed fans** — people the channel produced who reached
///    the meaningful-retention boundary;
/// 2. **fresh attributed conversions** — real people who joined, but whose
///    retention window has not matured yet;
/// 3. **trusted platform growth** — useful rented-audience proxy evidence;
/// 4. **tracked unique clickers** — an early leading signal only;
/// 5. **the strategy prior** — preserved where measurement is absent or tied.
///
/// This means no amount of click-only traffic can outrank one actual attributed
/// fan, and follower growth cannot impersonate a first-party conversion.
///
/// Stable: templates with no evidence on either axis keep their relative
/// position, and ties are broken by the prior order. The returned list
/// always contains exactly the input templates — this reranks, it never
/// adds or drops one.
#[must_use]
pub fn rank_templates(
    prior: &[&'static str],
    growth: &[PlatformGrowth],
    channel_yield: &[ChannelYield],
) -> Vec<&'static str> {
    let score_for = |template: &str| -> (u32, u32, RankKey, u32) {
        let growth_key = template_growth_platforms(template)
            .iter()
            .filter_map(|platform| {
                growth
                    .iter()
                    .find(|entry| entry.platform == *platform)
                    .and_then(PlatformGrowth::rank_key)
            })
            .max();

        let channels = template_channels(template);
        let durable = channels
            .iter()
            .filter_map(|channel| {
                channel_yield
                    .iter()
                    .find(|entry| entry.channel == *channel)
                    .map(|entry| entry.durable_90d)
            })
            .max()
            .unwrap_or(0);
        let conversions = channels
            .iter()
            .filter_map(|channel| {
                channel_yield
                    .iter()
                    .find(|entry| entry.channel == *channel)
                    .map(|entry| entry.conversions_30d)
            })
            .max()
            .unwrap_or(0);
        let clickers = channels
            .iter()
            .filter_map(|channel| {
                channel_yield
                    .iter()
                    .find(|entry| entry.channel == *channel)
                    .map(|entry| entry.unique_clickers_30d)
            })
            .max()
            .unwrap_or(0);

        // Evidence is intentionally not collapsed into fan-equivalent points.
        // A retained fan is the North Star itself; a fresh attributed fan is
        // stronger than rented-platform growth; platform growth is stronger
        // than a click-only leading signal.
        (durable, conversions, growth_key, clickers)
    };

    let mut ranked: Vec<RankedTemplate> = prior
        .iter()
        .enumerate()
        .map(|(index, template)| {
            let (durable, conversions, growth, clickers) = score_for(template);
            (index, *template, durable, conversions, growth, clickers)
        })
        .collect();

    ranked.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| right.4.cmp(&left.4))
            .then_with(|| right.5.cmp(&left.5))
            .then_with(|| left.0.cmp(&right.0))
    });

    ranked
        .into_iter()
        .map(|(_, template, _, _, _, _)| template)
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
        assert_eq!(rank_templates(PRIOR, &[], &[]), PRIOR.to_vec());
    }

    #[test]
    fn social_post_uses_the_best_meta_platform_growth_signal() {
        let prior = ["telegram-poster", "social-post", "growth-strategist"];
        let measured = [
            growth("telegram", 2_000, 0),
            growth("facebook", 5_000, 0),
            growth("instagram", 2_000, 200),
        ];

        let ranked = rank_templates(&prior, &measured, &[]);

        assert_eq!(
            ranked.first(),
            Some(&"social-post"),
            "a multi-platform social template must benefit from the Meta surface that is actually compounding"
        );
    }

    #[test]
    fn durable_social_fan_yield_still_outranks_meta_follower_growth() {
        let prior = ["social-post", "telegram-poster"];
        let measured = [growth("instagram", 10_000, 1_000)];
        let mut telegram = yielded("telegram", 1, 0);
        telegram.durable_90d = 1;

        let ranked = rank_templates(&prior, &measured, &[telegram]);

        assert_eq!(
            ranked.first(),
            Some(&"telegram-poster"),
            "a retained attributed fan is stronger North-Star evidence than even healthy follower growth"
        );
    }

    #[test]
    fn a_platform_that_actually_grows_is_tried_first() {
        // Telegram grows 10% of a real audience; Reddit is flat.
        let measured = [growth("social", 5_000, 0), growth("telegram", 2_000, 200)];
        let ranked = rank_templates(PRIOR, &measured, &[]);
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
        let ranked = rank_templates(PRIOR, &measured, &[]);
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
        let ranked = rank_templates(PRIOR, &measured, &[]);
        assert_eq!(
            ranked.first(),
            Some(&"signal-inviter"),
            "Signal installs are the North Star's most valuable unit"
        );
    }

    #[test]
    fn reranking_never_adds_or_drops_a_template() {
        let measured = [growth("signal", 900, 90), growth("telegram", 4, 4)];
        let mut ranked = rank_templates(PRIOR, &measured, &[]);
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
        let ranked = rank_templates(PRIOR, &measured, &[]);
        let strategist = ranked.iter().position(|t| *t == "growth-strategist");
        let bandcamp = ranked.iter().position(|t| *t == "bandcamp-scanner");
        assert!(
            bandcamp < strategist,
            "templates with no measurement should keep the strategy's ordering"
        );
    }

    /// The case production was actually in.
    ///
    /// One Signal install against a large, flat Reddit audience. The rate floor
    /// hides the install, and a platform that gained nobody used to rank ahead of
    /// the one that delivered the tenant's only addressable fan.
    #[test]
    fn the_only_platform_that_gained_anyone_is_tried_first() {
        let measured = [growth("social", 5_000, 0), growth("signal", 1, 1)];
        let ranked = rank_templates(PRIOR, &measured, &[]);
        assert_eq!(
            ranked.first(),
            Some(&"signal-inviter"),
            "a platform that gained nobody must not outrank one that gained a fan"
        );
    }

    /// A gain below the floor still counts, because a gain is not a rate.
    #[test]
    fn a_below_floor_gain_outranks_a_flat_platform() {
        let measured = [growth("telegram", 2_000, 0), growth("bandcamp", 10, 3)];
        let ranked = rank_templates(PRIOR, &measured, &[]);
        assert_eq!(
            ranked.first(),
            Some(&"bandcamp-scanner"),
            "three followers gained beats zero, whatever the denominators are"
        );
    }

    /// The floor still holds where it was pointed: at rates.
    ///
    /// Both platforms gained somebody, so both have a key and the comparison is
    /// the rate's to make. The tiny platform's 100% must not win it.
    #[test]
    fn the_absolute_gain_does_not_let_a_rate_jump_the_floor() {
        let measured = [growth("telegram", 2, 2), growth("social", 5_000, 250)];
        let ranked = rank_templates(PRIOR, &measured, &[]);
        assert_eq!(
            ranked.first(),
            Some(&"reddit-scanner"),
            "the gain breaks ties the rate cannot see; it does not overrule one"
        );
    }

    /// The rate leads and the gain only breaks ties — pinned where they disagree.
    ///
    /// Telegram returns 30% of a thousand; Reddit returns 5% of ten thousand. The
    /// bigger *number* of followers is Reddit's, the better *return per follower
    /// already held* is Telegram's, and the rate is what predicts the next
    /// dispatch. Both are above the floor, so this is the rate's call to make.
    ///
    /// Without this case the ordering inside the key is untested: the earlier
    /// tests all have the same platform winning on both halves, so reversing the
    /// two fields passes them all.
    #[test]
    fn the_rate_decides_when_it_disagrees_with_the_raw_gain() {
        let measured = [
            growth("telegram", 1_000, 300),
            growth("social", 10_000, 500),
        ];
        let ranked = rank_templates(PRIOR, &measured, &[]);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "the better return per follower held should be tried first, even \
             though the other platform added more followers in total"
        );
    }

    /// A platform that returned nobody is not evidence for trying it.
    #[test]
    fn a_platform_that_gained_nobody_ranks_with_the_unmeasured() {
        assert_eq!(
            growth("telegram", 5_000, 0).rank_key(),
            None,
            "no rate and no gain is no evidence, however large the audience"
        );
    }

    fn yielded(channel: &str, conversions: u32, clickers: u32) -> ChannelYield {
        ChannelYield {
            channel: channel.to_owned(),
            conversions_30d: conversions,
            unique_clickers_30d: clickers,
            durable_90d: 0,
        }
    }

    #[test]
    fn a_channel_whose_fans_stayed_outranks_one_whose_fans_left() {
        let mut stayed = yielded("telegram", 2, 0);
        stayed.durable_90d = 2;
        let ranked = rank_templates(PRIOR, &[], &[yielded("reddit", 3, 0), stayed]);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "two fans who stayed beat three who left"
        );
    }

    #[test]
    fn a_channel_that_converted_fans_leads_without_follower_data() {
        // No follower series anywhere; telegram produced two fans.
        let ranked = rank_templates(PRIOR, &[], &[yielded("telegram", 2, 0)]);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "the channel a real fan arrived through should be tried first"
        );
    }

    #[test]
    fn click_evidence_alone_ranks_a_channel_being_tried() {
        // Telegram produced clicks but no fans yet — the early signal.
        let ranked = rank_templates(PRIOR, &[], &[yielded("telegram", 0, 12)]);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "a channel with measured clicks outranks channels nobody measured"
        );
    }

    #[test]
    fn a_conversion_always_outranks_click_only_volume() {
        let ranked = rank_templates(
            PRIOR,
            &[],
            &[yielded("reddit", 1, 0), yielded("telegram", 0, 10_000)],
        );
        assert_eq!(
            ranked.first(),
            Some(&"reddit-scanner"),
            "click volume is exploration evidence and can never impersonate a converted fan"
        );
    }

    #[test]
    fn an_attributed_conversion_outranks_follower_growth_proxy() {
        let measured = [growth("social", 5_000, 250)];
        let ranked = rank_templates(PRIOR, &measured, &[yielded("telegram", 1, 0)]);
        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "a first-party converted fan is stronger evidence than rented-platform follower growth"
        );
    }

    #[test]
    fn durable_fans_outrank_follower_growth_proxy() {
        // Reddit has healthy follower growth, but Telegram has already proved
        // that one of its attributed arrivals survived the full retention
        // window. The latter is direct evidence for activated/durable fans,
        // which is the default North Star.
        let measured = [growth("social", 5_000, 250)];
        let mut telegram = yielded("telegram", 1, 0);
        telegram.durable_90d = 1;

        let ranked = rank_templates(PRIOR, &measured, &[telegram]);

        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "a retained attributed fan must outrank follower-growth proxy evidence"
        );
    }

    #[test]
    fn durable_evidence_still_outranks_more_fresh_conversions() {
        let mut telegram = yielded("telegram", 1, 0);
        telegram.durable_90d = 1;
        let ranked = rank_templates(
            PRIOR,
            &[],
            &[yielded("reddit", 100, 0), telegram],
        );

        assert_eq!(
            ranked.first(),
            Some(&"telegram-scanner"),
            "meaningful retention is stronger evidence than any number of still-immature signups"
        );
    }

    #[test]
    fn a_channel_that_produced_nothing_ranks_with_the_unmeasured() {
        assert_eq!(
            yielded("telegram", 0, 0).rank_key(),
            None,
            "a channel with no fans and no clickers is unmeasured, not zero"
        );
    }
}
