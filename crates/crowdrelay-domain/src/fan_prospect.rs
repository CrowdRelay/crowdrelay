//! A prospect is a publicly observed person who might become a fan. It is not
//! a fan.
//!
//! `fans` is a first-party relationship: a conscious opt-in, a verified
//! identity, consent, and an acquisition row that says how the person arrived.
//! A commenter under the band's own post is none of those. Writing them into
//! `fans` would make the monthly counter count people who never said yes — the
//! exact failure the counting rule exists to prevent — so the person layer has
//! its own entity and its own vocabulary, and the only road from prospect to
//! fan is the person joining through a tracked, consented path.
//!
//! This module is the closed vocabulary and the rules that need no database:
//! the status machine, what each source class is allowed to hold and for how
//! long, and how a handle is normalized into an identity.
//!
//! What it does not do: it does not decide what to do about anyone. That is
//! the next-best-action evaluator's job, and it reads this vocabulary.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// Where a prospect is in the relationship. Ordered by progress, with the
/// three ways out (`held`, `refused`, `suppressed`) at the end.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProspectStatus {
    Observed,
    Qualified,
    Warming,
    Invited,
    Converted,
    /// Parked with a reason; a later reading can reopen it.
    Held,
    /// The person (or their community) said no. Terminal unless a human
    /// reopens it through research.
    Refused,
    /// A suppression or opt-out in any role. Terminal across every role.
    Suppressed,
}

impl ProspectStatus {
    pub const ALL: [Self; 8] = [
        Self::Observed,
        Self::Qualified,
        Self::Warming,
        Self::Invited,
        Self::Converted,
        Self::Held,
        Self::Refused,
        Self::Suppressed,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Qualified => "qualified",
            Self::Warming => "warming",
            Self::Invited => "invited",
            Self::Converted => "converted",
            Self::Held => "held",
            Self::Refused => "refused",
            Self::Suppressed => "suppressed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }

    /// Nothing more may be written about this person, and nothing more may be
    /// sent to them. `Converted` is terminal for the prospect (the person is a
    /// fan now); `Refused` and `Suppressed` are terminal because someone said no.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Converted | Self::Refused | Self::Suppressed)
    }

    /// A prospect in a state where the system must not touch them and must not
    /// keep collecting evidence about them.
    #[must_use]
    pub const fn forbids_contact(self) -> bool {
        matches!(self, Self::Refused | Self::Suppressed)
    }

    /// Whether a move is legal. The rule is that a "no" is never undone by the
    /// machine: `Refused` and `Suppressed` have no exits, `Converted` has none,
    /// and a prospect is never moved *back* up the ladder by a new reading —
    /// only parked (`Held`) and later reopened.
    #[must_use]
    pub fn may_become(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Refused | Self::Suppressed | Self::Converted => false,
            // Any live state can end in a no, a hold, or a conversion.
            _ if matches!(
                next,
                Self::Refused | Self::Suppressed | Self::Held | Self::Converted
            ) =>
            {
                true
            }
            Self::Held => matches!(next, Self::Observed | Self::Qualified | Self::Warming),
            Self::Observed => matches!(next, Self::Qualified | Self::Warming | Self::Invited),
            Self::Qualified => matches!(next, Self::Warming | Self::Invited),
            Self::Warming => matches!(next, Self::Invited),
            Self::Invited => false,
        }
    }
}

/// What a public signal was. An observation is evidence, never permission to
/// contact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    CommentedSimilarBand,
    CollectsSimilarMusic,
    AskedAboutShow,
    AskedForMusic,
    ActiveUnderOurPost,
    SharedMaterial,
    Replied,
    AppearsRepeatedly,
    SceneParticipant,
    RelatedToProspects,
    ActiveReferrer,
    ContentCreator,
    AttendsLocalShows,
}

impl ObservationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommentedSimilarBand => "commented_similar_band",
            Self::CollectsSimilarMusic => "collects_similar_music",
            Self::AskedAboutShow => "asked_about_show",
            Self::AskedForMusic => "asked_for_music",
            Self::ActiveUnderOurPost => "active_under_our_post",
            Self::SharedMaterial => "shared_material",
            Self::Replied => "replied",
            Self::AppearsRepeatedly => "appears_repeatedly",
            Self::SceneParticipant => "scene_participant",
            Self::RelatedToProspects => "related_to_prospects",
            Self::ActiveReferrer => "active_referrer",
            Self::ContentCreator => "content_creator",
            Self::AttendsLocalShows => "attends_local_shows",
        }
    }
}

/// The lawful basis a prospect is held under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LawfulBasis {
    LegitimateInterest,
    Consent,
}

impl LawfulBasis {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegitimateInterest => "legitimate_interest",
            Self::Consent => "consent",
        }
    }
}

/// A class of surface prospects are read from. Each declares, once, what it
/// may hold and for how long — so retention is a property of where the evidence
/// came from, not a number a caller chooses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProspectSource {
    /// People who commented under a post on a surface the band controls
    /// (its own Instagram, YouTube, Reddit posts). They addressed the band in
    /// public; the band answering in the same thread is the expected reply.
    OwnComments,
}

impl ProspectSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnComments => "own_comments",
        }
    }

    #[must_use]
    pub const fn lawful_basis(self) -> LawfulBasis {
        match self {
            Self::OwnComments => LawfulBasis::LegitimateInterest,
        }
    }

    /// How long a prospect from this source is kept without progress. Each new
    /// reading extends it from that reading, never from first sight.
    #[must_use]
    pub const fn retention(self) -> Duration {
        match self {
            // A commenter who is never engaged has no further claim on us after
            // a season; sixty days covers a release cycle and no more.
            Self::OwnComments => Duration::days(60),
        }
    }

    #[must_use]
    pub fn expires_at(self, observed_at: OffsetDateTime) -> OffsetDateTime {
        observed_at + self.retention()
    }
}

/// Provider/user/channel ids are case-sensitive on some platforms. They are
/// trimmed and bounded, but never lowercased. A stable provider id outranks a
/// handle when both are available: usernames can change, provider ids should not.
pub const MAX_PLATFORM_USER_ID_LEN: usize = 256;

#[must_use]
pub fn normalize_platform_user_id(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty()
        && value.chars().count() <= MAX_PLATFORM_USER_ID_LEN
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

/// The longest handle the system will treat as an identity. Longer is not a
/// platform handle; it is text.
pub const MAX_HANDLE_LEN: usize = 100;

/// The handle as the platform shows it, minus the `@` / `u/` the source may
/// have prefixed — case kept, for display and for addressing a reply. Matching
/// always goes through [`normalize_handle`].
#[must_use]
pub fn display_handle(raw: &str) -> Option<String> {
    normalize_handle(raw)?;
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("u/")
        .or_else(|| trimmed.strip_prefix('@'))
        .unwrap_or(trimmed);
    Some(trimmed.trim_start_matches('@').to_owned())
}

/// The normalized form a handle is matched on: trimmed, no leading `@` or
/// `u/`, lowercase. `None` for anything that is not a handle — empty, over
/// [`MAX_HANDLE_LEN`], or containing whitespace — so a comment's body or a
/// display name with a space in it can never become an identity.
#[must_use]
pub fn normalize_handle(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("u/")
        .or_else(|| trimmed.strip_prefix('@'))
        .unwrap_or(trimmed);
    let normalized = trimmed.trim_start_matches('@').to_lowercase();
    if normalized.is_empty()
        || normalized.chars().count() > MAX_HANDLE_LEN
        || normalized.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(normalized)
}

/// Platform names are matched lowercase and trimmed.
#[must_use]
pub fn normalize_platform(raw: &str) -> Option<String> {
    let platform = raw.trim().to_lowercase();
    (!platform.is_empty()
        && platform
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_no_is_never_undone_by_the_machine() {
        for terminal in [
            ProspectStatus::Refused,
            ProspectStatus::Suppressed,
            ProspectStatus::Converted,
        ] {
            for next in ProspectStatus::ALL {
                assert_eq!(
                    terminal.may_become(next),
                    terminal == next,
                    "{terminal:?} -> {next:?}"
                );
            }
        }
        assert!(ProspectStatus::Refused.forbids_contact());
        assert!(ProspectStatus::Suppressed.forbids_contact());
        assert!(!ProspectStatus::Converted.forbids_contact());
    }

    #[test]
    fn a_new_reading_parks_a_prospect_but_never_walks_it_back_up_the_ladder() {
        use ProspectStatus::*;
        assert!(Observed.may_become(Qualified));
        assert!(Qualified.may_become(Warming));
        assert!(Warming.may_become(Invited));
        assert!(!Warming.may_become(Observed));
        assert!(!Invited.may_become(Qualified));
        assert!(Invited.may_become(Converted));
        assert!(Invited.may_become(Held));
        assert!(Held.may_become(Observed));
        assert!(
            !Held.may_become(Invited),
            "a hold is reopened, not skipped past"
        );
    }

    #[test]
    fn every_status_round_trips_through_its_wire_name() {
        for status in ProspectStatus::ALL {
            assert_eq!(ProspectStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ProspectStatus::parse("fan"), None);
    }

    #[test]
    fn stable_platform_ids_preserve_case() {
        assert_eq!(
            normalize_platform_user_id(" UCaBcD123 "),
            Some("UCaBcD123".to_owned())
        );
        assert_eq!(normalize_platform_user_id(""), None);
        assert_eq!(
            normalize_platform_user_id(&"x".repeat(MAX_PLATFORM_USER_ID_LEN + 1)),
            None
        );
    }

    #[test]
    fn a_handle_is_an_identity_only_when_it_is_one() {
        assert_eq!(
            normalize_handle("  @Kuba_Metal "),
            Some("kuba_metal".into())
        );
        assert_eq!(normalize_handle("u/SomeUser"), Some("someuser".into()));
        assert_eq!(normalize_handle("@@double"), Some("double".into()));
        for not_a_handle in ["", "   ", "@", "two words", &"a".repeat(MAX_HANDLE_LEN + 1)] {
            assert_eq!(normalize_handle(not_a_handle), None, "{not_a_handle:?}");
        }
    }

    #[test]
    fn the_display_handle_keeps_case_and_drops_the_prefix() {
        assert_eq!(display_handle(" @Kuba_Metal "), Some("Kuba_Metal".into()));
        assert_eq!(display_handle("u/SomeUser"), Some("SomeUser".into()));
        assert_eq!(display_handle("two words"), None);
    }

    #[test]
    fn a_platform_is_a_single_lowercase_word() {
        assert_eq!(normalize_platform(" Instagram "), Some("instagram".into()));
        assert_eq!(
            normalize_platform("youtube_music"),
            Some("youtube_music".into())
        );
        assert_eq!(normalize_platform("face book"), None);
        assert_eq!(normalize_platform(""), None);
    }

    #[test]
    fn retention_runs_from_the_reading_not_from_first_sight() {
        let first = datetime!(2026-10-01 12:00 UTC);
        let later = datetime!(2026-11-15 12:00 UTC);
        assert_eq!(
            ProspectSource::OwnComments.expires_at(first),
            datetime!(2026-11-30 12:00 UTC)
        );
        assert!(
            ProspectSource::OwnComments.expires_at(later)
                > ProspectSource::OwnComments.expires_at(first)
        );
        assert_eq!(
            ProspectSource::OwnComments.lawful_basis().as_str(),
            "legitimate_interest"
        );
    }
}
