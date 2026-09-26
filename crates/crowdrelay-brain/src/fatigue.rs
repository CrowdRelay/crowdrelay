//! What an audience loses when it hears from the band again too soon —
//! measured from the band's own posting history, never assumed.
//!
//! WAIT's `fatigue_recovery_value` was an unfilled seam: always 0.0, which
//! biased the brain toward acting by however much a rested audience is
//! worth. The honest fill is a measurement, and the band's own community
//! history carries one: posts that followed another post to the same
//! community within [`REST_DAYS`] against posts that came after a rest,
//! each scored against its own community's median so a big subreddit and a
//! small one compare. When the quick follow-ups do measurably worse, that
//! shortfall is what waiting recovers.
//!
//! The measure refuses to answer on thin evidence ([`MIN_PER_ARM`] posts in
//! each arm) and never reports a negative discount: a history in which quick
//! follow-ups did better says nothing about rest being harmful, only that
//! this band's audiences were not tired yet.

use serde::{Deserialize, Serialize};

/// Days an audience needs between two posts to count as rested. The median
/// cooldown the communities' own rules ask for.
pub const REST_DAYS: f64 = 7.0;
/// Posts each arm needs before the measure answers.
pub const MIN_PER_ARM: usize = 5;
/// Posts a community needs before its median normalises anything.
const MIN_COMMUNITY_POSTS: usize = 3;

/// One published post, as the fatigue measure reads it.
#[derive(Clone, Debug)]
pub struct SpacedPost {
    pub audience: String,
    /// Days since some fixed origin; only differences are read.
    pub posted_day: f64,
    /// The post's engagement (Reddit score), comparable within an audience.
    pub score: f64,
}

/// The measured cost of posting to a tired audience.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FatigueMeasure {
    /// Share of a rested post's engagement a quick follow-up loses, 0.0–1.0.
    pub discount: f64,
    pub quick_posts: u32,
    pub rested_posts: u32,
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(mid).copied()
    } else {
        Some((values.get(mid - 1)? + values.get(mid)?) / 2.0)
    }
}

/// Measures fatigue from a posting history. `None` when either arm is thin
/// or the rested arm has no engagement to compare against.
#[must_use]
pub fn measure_fatigue(posts: &[SpacedPost]) -> Option<FatigueMeasure> {
    let mut by_audience: std::collections::BTreeMap<&str, Vec<&SpacedPost>> =
        std::collections::BTreeMap::new();
    for post in posts {
        by_audience
            .entry(post.audience.as_str())
            .or_default()
            .push(post);
    }
    let (mut quick, mut rested) = (Vec::new(), Vec::new());
    for community in by_audience.values_mut() {
        if community.len() < MIN_COMMUNITY_POSTS {
            continue;
        }
        let mut scores: Vec<f64> = community.iter().map(|post| post.score).collect();
        let Some(base) = median(&mut scores).filter(|base| *base > 0.0) else {
            continue;
        };
        community.sort_by(|a, b| a.posted_day.total_cmp(&b.posted_day));
        for pair in community.windows(2) {
            let (Some(previous), Some(post)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let normalised = post.score / base;
            if post.posted_day - previous.posted_day < REST_DAYS {
                quick.push(normalised);
            } else {
                rested.push(normalised);
            }
        }
    }
    if quick.len() < MIN_PER_ARM || rested.len() < MIN_PER_ARM {
        return None;
    }
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let rested_mean = mean(&rested);
    if rested_mean <= 0.0 {
        return None;
    }
    Some(FatigueMeasure {
        discount: (1.0 - mean(&quick) / rested_mean).clamp(0.0, 1.0),
        quick_posts: u32::try_from(quick.len()).unwrap_or(u32::MAX),
        rested_posts: u32::try_from(rested.len()).unwrap_or(u32::MAX),
    })
}

/// What waiting recovers for the best candidate: its expected value times
/// the measured discount, when its audience heard from the band within
/// [`REST_DAYS`]. Zero with no measure, no recent touch, or no value.
#[must_use]
pub fn recovery_value(
    measure: Option<&FatigueMeasure>,
    best_candidate_y30: f64,
    days_since_audience_touched: Option<u32>,
) -> f64 {
    match (measure, days_since_audience_touched) {
        (Some(measure), Some(days)) if f64::from(days) < REST_DAYS && best_candidate_y30 > 0.0 => {
            measure.discount * best_candidate_y30
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(audience: &str, day: f64, score: f64) -> SpacedPost {
        SpacedPost {
            audience: audience.to_owned(),
            posted_day: day,
            score,
        }
    }

    /// Two communities, each alternating rested posts (score 10) with quick
    /// follow-ups two days later (score 5).
    fn tired_history() -> Vec<SpacedPost> {
        let mut posts = Vec::new();
        for audience in ["doom", "sludge"] {
            for round in 0..4 {
                let day = f64::from(round) * 20.0;
                posts.push(post(audience, day, 10.0));
                posts.push(post(audience, day + 2.0, 5.0));
            }
        }
        posts
    }

    #[test]
    fn quick_follow_ups_that_do_worse_measure_as_fatigue() {
        let measure = measure_fatigue(&tired_history()).expect("both arms are thick enough");
        assert!(
            measure.discount > 0.3 && measure.discount < 0.6,
            "{measure:?}"
        );
        assert_eq!(measure.quick_posts, 8);
    }

    #[test]
    fn thin_history_does_not_answer() {
        assert_eq!(measure_fatigue(&tired_history()[..4]), None);
    }

    #[test]
    fn quick_posts_doing_better_is_no_fatigue_not_negative_fatigue() {
        let mut posts = tired_history();
        for post in &mut posts {
            post.score = if post.score < 7.0 { 20.0 } else { 10.0 };
        }
        let measure = measure_fatigue(&posts).expect("measured");
        assert_eq!(measure.discount, 0.0);
    }

    #[test]
    fn waiting_recovers_value_only_for_a_recently_touched_audience() {
        let measure = FatigueMeasure {
            discount: 0.4,
            quick_posts: 8,
            rested_posts: 8,
        };
        assert!((recovery_value(Some(&measure), 5.0, Some(2)) - 2.0).abs() < 1e-9);
        assert_eq!(recovery_value(Some(&measure), 5.0, Some(10)), 0.0);
        assert_eq!(recovery_value(Some(&measure), 5.0, None), 0.0);
        assert_eq!(recovery_value(None, 5.0, Some(2)), 0.0);
    }
}
