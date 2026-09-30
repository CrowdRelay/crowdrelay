//! Which of the band's own posts held attention, measured against the band.
//!
//! The band's strategy is a hook in the first three seconds. Nothing in the
//! Graph API reports the first three seconds, so this reads the two signals
//! that do exist and that a hook moves: average watch time on video, and
//! saves plus shares per reach (a post people keep or pass on). Each is
//! judged against the band's own median over the same window — a band is
//! compared with itself, never with an industry number, because its
//! audience, format and length are what the median already carries.
//!
//! Output is a verdict per post and nothing more. The lessons are drawn by
//! whoever reads the verdicts beside the post itself: the band on the Content
//! page, and the drafters, which are told to open the way the posts that held
//! attention open — never to copy their words.

use serde::Serialize;

/// Posts needed on a signal before its median means anything. Below this,
/// every post on that signal is `Unmeasured` rather than judged against a
/// median of two.
pub const MIN_PEERS: usize = 4;
/// Reach below this is too small for a rate to say anything.
pub const MIN_REACH: i64 = 100;

/// Provider metadata is optional evidence. Invalid, negative or oversized
/// counts remain unknown instead of breaking a read or fabricating zeroes.
#[must_use]
pub fn parse_social_count(value: Option<&str>) -> Option<i64> {
    value?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
}

/// A post, as the scorecard reads it.
#[derive(Clone, Debug, Default)]
pub struct HookPost {
    pub is_video: bool,
    pub reach: Option<i64>,
    pub avg_watch_ms: Option<i64>,
    pub saves: Option<i64>,
    pub shares: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookVerdict {
    /// Watched well past the band's usual, or kept and passed on well above
    /// it.
    HeldAttention,
    /// Watched well short of the band's usual — the opening lost them.
    LostEarly,
    Typical,
    /// Not enough reach or not enough of the band's own posts to judge.
    Unmeasured,
}

/// A post's numbers against the band's medians.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HookScore {
    pub verdict: HookVerdict,
    /// Watch time as a share of the band's median video watch time, in
    /// basis points (10 000 = the median). `None` for stills and unmeasured
    /// video.
    pub watch_index_bps: Option<i64>,
    /// Saves plus shares per 1 000 reached, as a share of the band's median
    /// rate, in basis points.
    pub keep_index_bps: Option<i64>,
}

fn median(mut values: Vec<i64>) -> Option<i64> {
    if values.len() < MIN_PEERS {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(mid).copied()
    } else {
        Some((values.get(mid - 1)?.saturating_add(*values.get(mid)?)) / 2)
    }
}

fn keep_per_mille(post: &HookPost) -> Option<i64> {
    let reach = post.reach.filter(|reach| *reach >= MIN_REACH)?;
    let kept = post
        .saves
        .unwrap_or(0)
        .saturating_add(post.shares.unwrap_or(0));
    if post.saves.is_none() && post.shares.is_none() {
        return None;
    }
    Some(kept.saturating_mul(1_000) / reach)
}

fn watch(post: &HookPost) -> Option<i64> {
    if !post.is_video || post.reach.is_none_or(|reach| reach < MIN_REACH) {
        return None;
    }
    post.avg_watch_ms.filter(|ms| *ms > 0)
}

fn index_bps(value: i64, median: i64) -> Option<i64> {
    (median > 0).then(|| value.saturating_mul(10_000) / median)
}

/// Scores every post against the medians of the set it arrived in.
#[must_use]
pub fn score_hooks(posts: &[HookPost]) -> Vec<HookScore> {
    let watch_median = median(posts.iter().filter_map(watch).collect());
    let keep_median = median(posts.iter().filter_map(keep_per_mille).collect());
    posts
        .iter()
        .map(|post| {
            let watch_index_bps = watch_median.and_then(|median| index_bps(watch(post)?, median));
            let keep_index_bps =
                keep_median.and_then(|median| index_bps(keep_per_mille(post)?, median));
            let verdict = match (watch_index_bps, keep_index_bps) {
                (None, None) => HookVerdict::Unmeasured,
                // A quarter over the median watch, or half again the
                // median keep rate, is a post the band should study.
                (Some(watch), _) if watch >= 12_500 => HookVerdict::HeldAttention,
                (_, Some(keep)) if keep >= 15_000 => HookVerdict::HeldAttention,
                // Only watch time can say the opening lost people: a still
                // with few saves may simply not be a post worth keeping.
                (Some(watch), _) if watch <= 7_500 => HookVerdict::LostEarly,
                _ => HookVerdict::Typical,
            };
            HookScore {
                verdict,
                watch_index_bps,
                keep_index_bps,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_provider_counts_stay_unknown() {
        for value in [
            None,
            Some(""),
            Some("unknown"),
            Some("-1"),
            Some("1.5"),
            Some("9223372036854775808"),
            Some("{\"count\":10}"),
        ] {
            assert_eq!(parse_social_count(value), None, "{value:?}");
        }
        assert_eq!(parse_social_count(Some("0")), Some(0));
        assert_eq!(parse_social_count(Some(" 1000 ")), Some(1000));
        assert_eq!(
            parse_social_count(Some("9223372036854775807")),
            Some(i64::MAX)
        );
    }

    fn video(watch_ms: i64) -> HookPost {
        HookPost {
            is_video: true,
            reach: Some(1_000),
            avg_watch_ms: Some(watch_ms),
            saves: Some(5),
            shares: Some(5),
        }
    }

    #[test]
    fn a_video_watched_well_past_the_bands_median_held_attention() {
        let posts = [
            video(4_000),
            video(4_000),
            video(4_000),
            video(4_000),
            video(6_000),
        ];
        let scores = score_hooks(&posts);
        assert_eq!(scores[4].verdict, HookVerdict::HeldAttention);
        assert_eq!(scores[4].watch_index_bps, Some(15_000));
        assert_eq!(scores[0].verdict, HookVerdict::Typical);
    }

    #[test]
    fn a_video_watched_well_short_lost_them_early() {
        let posts = [
            video(4_000),
            video(4_000),
            video(4_000),
            video(4_000),
            video(2_000),
        ];
        assert_eq!(score_hooks(&posts)[4].verdict, HookVerdict::LostEarly);
    }

    #[test]
    fn a_still_that_people_kept_held_attention() {
        let mut posts: Vec<HookPost> = (0..4).map(|_| video(4_000)).collect();
        posts.push(HookPost {
            is_video: false,
            reach: Some(1_000),
            avg_watch_ms: None,
            saves: Some(20),
            shares: Some(10),
        });
        let scores = score_hooks(&posts);
        assert_eq!(scores[4].watch_index_bps, None);
        assert_eq!(scores[4].verdict, HookVerdict::HeldAttention);
    }

    #[test]
    fn too_few_posts_or_too_little_reach_is_unmeasured_not_judged() {
        let posts = [video(4_000), video(9_000)];
        assert!(
            score_hooks(&posts)
                .iter()
                .all(|score| score.verdict == HookVerdict::Unmeasured)
        );

        let mut tiny = video(9_000);
        tiny.reach = Some(20);
        let posts = [video(4_000), video(4_000), video(4_000), video(4_000), tiny];
        assert_eq!(score_hooks(&posts)[4].verdict, HookVerdict::Unmeasured);
    }
}
