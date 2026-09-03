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
//! This is deliberately identity and rotation only — no posterior level, no
//! bandit. There is not yet a single measured community post to learn from,
//! and building the estimator before the data exists is how you end up with
//! machinery that has never been evaluated. What matters now is that the
//! first post already carries its label, because a label cannot be added to
//! an outcome after the fact.

use serde::{Deserialize, Serialize};

/// The angle a community post takes.
///
/// Scoped to community posts. Press pitches and fan messages have their own
/// angles (release, live, narrative; event, exclusive, reward, reactivation)
/// and will get their own vocabulary when those surfaces start recording
/// outcomes — a shared enum spanning all of them would pool families that are
/// not comparable.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
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
    /// sampling: with no outcomes yet there is nothing to weight by, and an
    /// even spread is what makes the eventual comparison possible.
    ///
    /// When a family does eventually earn a posterior, this is the one
    /// function that has to change.
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
}
