//! Which angle a post takes, recorded as an identity rather than left in prose.
//!
//! The brain decides whether to post and where. What the post actually says
//! was chosen entirely by the drafting worker, unrecorded, so the variance
//! creative causes in outcomes arrived at the causal model as noise. Two posts
//! to the same community with the same predicted value could be a personal
//! story and a technical breakdown, and nothing anywhere could tell them
//! apart afterwards.
//!
//! A family is not a template for the text. It is the angle the worker is
//! asked to take, named so the outcome can be attributed to it. The worker
//! still writes the post.
//!
//! Selection is learned, not fixed: [`CreativeFamily::choose`] Thompson-samples
//! the per-family treatment-effect posteriors the causal model maintains, with
//! a uniform exploration floor so a family can never starve below
//! measurability. Rotation ([`CreativeFamily::rotate`]) remains the fallback
//! for the community nothing has measured yet, because an even spread is what
//! makes the eventual comparison possible.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The angle a community post takes.
///
/// Scoped to community posts. Press pitches and fan messages have their own
/// angles (release, live, narrative; event, exclusive, reward, reactivation)
/// and will get their own vocabulary when those surfaces start recording
/// outcomes — a shared enum spanning all of them would pool families that are
/// not comparable.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CreativeFamily {
    /// Something that happened to the band, told as a story. The default
    /// because it is the angle that reads least like promotion.
    #[default]
    Story,
    /// The music itself — a riff, a passage, a video of it being played.
    Riff,
    /// How something was made: tuning, gear, production, arrangement.
    Technical,
    /// Belonging to the community's own subject rather than to the band.
    Identity,
    /// A specific upcoming show or release, with the date as the reason to
    /// post now.
    Event,
}

impl CreativeFamily {
    /// Every family, in rotation order.
    pub const ALL: [Self; 5] = [
        Self::Story,
        Self::Riff,
        Self::Technical,
        Self::Identity,
        Self::Event,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Story => "story",
            Self::Riff => "riff",
            Self::Technical => "technical",
            Self::Identity => "identity",
            Self::Event => "event",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "story" => Some(Self::Story),
            "riff" => Some(Self::Riff),
            "technical" => Some(Self::Technical),
            "identity" => Some(Self::Identity),
            "event" => Some(Self::Event),
            _ => None,
        }
    }

    /// The angle instruction handed to the drafting worker.
    #[must_use]
    pub const fn brief(self) -> &'static str {
        match self {
            Self::Story => {
                "Angle: tell something that actually happened — a rehearsal, a drive, a bad show, a small win. No announcement, no call to action beyond the story itself."
            }
            Self::Riff => {
                "Angle: lead with the music. A riff, a passage, a section worth hearing on its own. Say what makes it worth a listen rather than that it exists."
            }
            Self::Technical => {
                "Angle: how it was made — tuning, gear, arrangement, production choices. Write for people who will argue with the details."
            }
            Self::Identity => {
                "Angle: the community's own subject, not the band. Contribute to what they already talk about; the band is context, not the point."
            }
            Self::Event => {
                "Angle: a specific date — an upcoming show or release. The date is the reason this is worth posting now; say what is actually happening."
            }
        }
    }

    /// Picks the family for the next post to a community, by rotation.
    ///
    /// `posts_so_far` is how many posts this community has already had, so
    /// each community walks its own cycle and every family gets comparable
    /// exposure per community rather than per workspace. Rotation, not
    /// sampling: it is the pre-evidence default — with no outcomes there is
    /// nothing to weight by, and an even spread is what makes the eventual
    /// comparison possible. Once evidence exists, [`Self::choose`] weights
    /// the pick instead.
    #[must_use]
    pub fn rotate(posts_so_far: u32) -> Self {
        let index = (posts_so_far as usize) % Self::ALL.len();
        // The modulus guarantees the index is in range; `get` states that to
        // the compiler rather than to a reader, and the fallback is the
        // default family rather than a panic in the dispatch path.
        Self::ALL.get(index).copied().unwrap_or(Self::Story)
    }

    /// The event family only makes sense with an event to name. Rotation
    /// skips to the next family when there is no date, rather than asking a
    /// worker to write an announcement about nothing.
    #[must_use]
    pub fn rotate_with_event(posts_so_far: u32, has_upcoming_event: bool) -> Self {
        let chosen = Self::rotate(posts_so_far);
        if chosen == Self::Event && !has_upcoming_event {
            return Self::rotate(posts_so_far.wrapping_add(1));
        }
        chosen
    }

    /// Chooses a family by Thompson sampling over learned per-family
    /// treatment effects.
    ///
    /// `stats` maps each family to the `(mean, std)` of its effect posterior
    /// for this community — the caller fills prior entries for families with
    /// no observations, so an unmeasured family still gets its fair draw
    /// instead of starving on behalf of families that merely measured first.
    /// `seed` makes the pick deterministic per community and post index, so
    /// re-evaluating the same candidate within a cycle names the same family.
    ///
    /// Two rules keep the learning honest. Event is only eligible with a
    /// date to name — the same rule rotation obeys. And a uniform floor
    /// spends a share of picks at random, because a bandit that stops
    /// sampling an arm can never learn that the arm got better: the
    /// posteriors only update from posts that carried the label.
    ///
    /// The caller decides when to call this at all — with no measured
    /// evidence anywhere, rotation gives the first posts the even coverage
    /// the comparison needs.
    #[must_use]
    pub fn choose(stats: &BTreeMap<Self, (f64, f64)>, has_upcoming_event: bool, seed: u64) -> Self {
        let eligible: Vec<Self> = stats
            .keys()
            .copied()
            .filter(|family| *family != Self::Event || has_upcoming_event)
            .collect();
        let Some(&first) = eligible.first() else {
            return Self::Story;
        };
        if eligible.len() == 1 {
            return first;
        }
        // splitmix64 — deterministic, and domain carries no RNG dependency.
        let mut state = seed;
        let mut next_u64 = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut uniform = || (next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        // The exploration floor: one pick in ten ignores the posteriors and
        // draws uniformly across the eligible families.
        const EXPLORATION_FRACTION: f64 = 0.10;
        if uniform() < EXPLORATION_FRACTION {
            let index = (next_u64() as usize) % eligible.len();
            return eligible.get(index).copied().unwrap_or(first);
        }
        // Thompson draw: sample each family's posterior, take the argmax.
        // Families at their prior draw N(0, 4) — a wide, skeptical draw that
        // keeps them competitive until evidence separates them.
        let mut best = first;
        let mut best_draw = f64::NEG_INFINITY;
        for family in &eligible {
            let (mean, std) = stats.get(family).copied().unwrap_or((0.0, 2.0));
            // Box–Muller: two uniforms → one standard normal.
            let u1 = uniform().max(f64::MIN_POSITIVE);
            let u2 = uniform();
            let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
            let draw = std.mul_add(z, mean);
            if draw > best_draw {
                best_draw = draw;
                best = *family;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_round_trip_through_strings() {
        for family in CreativeFamily::ALL {
            assert_eq!(CreativeFamily::parse(family.as_str()), Some(family));
        }
        assert_eq!(CreativeFamily::parse("unknown"), None);
    }

    #[test]
    fn rotation_covers_every_family_before_repeating() {
        let seen: Vec<CreativeFamily> = (0..CreativeFamily::ALL.len() as u32)
            .map(CreativeFamily::rotate)
            .collect();
        for family in CreativeFamily::ALL {
            assert!(seen.contains(&family), "{family:?} never rotated in");
        }
        assert_eq!(CreativeFamily::rotate(0), CreativeFamily::rotate(5));
    }

    #[test]
    fn event_family_is_skipped_without_an_event() {
        // Index 4 is Event.
        assert_eq!(CreativeFamily::rotate(4), CreativeFamily::Event);
        assert_eq!(
            CreativeFamily::rotate_with_event(4, false),
            CreativeFamily::Story
        );
        assert_eq!(
            CreativeFamily::rotate_with_event(4, true),
            CreativeFamily::Event
        );
    }

    #[test]
    fn every_family_briefs_the_worker() {
        for family in CreativeFamily::ALL {
            assert!(!family.brief().is_empty());
        }
    }

    #[test]
    fn choose_prefers_the_family_with_the_better_effect() {
        // Story has a clearly positive effect; everything else sits at the
        // skeptical prior. Over many seeds the Thompson draw should pick
        // Story almost always — the floor is the only way it loses.
        let mut stats = BTreeMap::new();
        for family in CreativeFamily::ALL {
            stats.insert(family, (0.0, 2.0));
        }
        stats.insert(CreativeFamily::Story, (5.0, 0.5));
        let mut story = 0u32;
        let trials = 500u64;
        for seed in 0..trials {
            if CreativeFamily::choose(&stats, true, seed) == CreativeFamily::Story {
                story += 1;
            }
        }
        assert!(
            u64::from(story) > trials * 8 / 10,
            "a family with a strong measured effect should win nearly every draw; got {story}/{trials}"
        );
    }

    #[test]
    fn choose_keeps_every_family_measurable() {
        // Even with a dominant family, the exploration floor must keep the
        // others appearing — a starved arm can never earn evidence again.
        let mut stats = BTreeMap::new();
        for family in CreativeFamily::ALL {
            stats.insert(family, (0.0, 2.0));
        }
        stats.insert(CreativeFamily::Riff, (8.0, 0.1));
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..2000u64 {
            seen.insert(CreativeFamily::choose(&stats, true, seed));
        }
        assert_eq!(seen.len(), CreativeFamily::ALL.len());
    }

    #[test]
    fn choose_never_names_event_without_a_date() {
        // Event with the strongest effect still cannot be picked when there
        // is nothing upcoming — an announcement about nothing is not a post.
        let mut stats = BTreeMap::new();
        for family in CreativeFamily::ALL {
            stats.insert(family, (0.0, 2.0));
        }
        stats.insert(CreativeFamily::Event, (50.0, 0.1));
        for seed in 0..500u64 {
            assert_ne!(
                CreativeFamily::choose(&stats, false, seed),
                CreativeFamily::Event
            );
        }
    }

    #[test]
    fn choose_is_deterministic_per_seed() {
        let mut stats = BTreeMap::new();
        for family in CreativeFamily::ALL {
            stats.insert(family, (0.0, 2.0));
        }
        for seed in 0..50u64 {
            assert_eq!(
                CreativeFamily::choose(&stats, true, seed),
                CreativeFamily::choose(&stats, true, seed)
            );
        }
    }
}
