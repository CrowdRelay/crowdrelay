//! The Audience Graph's pure policy: what a place is, which pipeline
//! transitions are legal, and when a place may be contacted again.
//!
//! The graph holds no HTTP and fetches nothing. It decides whether what an
//! adapter or an operator brought back fits the pipeline, the same way
//! `target_discovery` decides it for playlist candidates:
//!
//! - **A stage move is earned, not typed.** Only the transitions in
//!   [`ALLOWED_TRANSITIONS`] exist; everything else is rejected before any
//!   database row moves.
//! - **A place's own rules outvote enthusiasm.** A contact attempt before the
//!   place's cooldown lapses is refused even at maximum confidence, because a
//!   burned community does not un-burn.
//! - **Declines are terminal until new evidence re-opens them.** Re-opening
//!   goes back through research, never straight to contact.

use serde::{Deserialize, Serialize};

/// The kinds of gathering places the graph tracks. Storage maps through
/// [`PlaceKind::from_storage`]; unknown values stay unknown instead of
/// collapsing into a catch-all that would silently misroute policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceKind {
    Subreddit,
    Discord,
    Forum,
    FacebookGroup,
    Instagram,
    Tiktok,
    Youtube,
    Playlist,
    Zine,
    Festival,
    XAccount,
    Other,
}

impl PlaceKind {
    pub const ALL: [PlaceKind; 12] = [
        PlaceKind::Subreddit,
        PlaceKind::Discord,
        PlaceKind::Forum,
        PlaceKind::FacebookGroup,
        PlaceKind::Instagram,
        PlaceKind::Tiktok,
        PlaceKind::Youtube,
        PlaceKind::Playlist,
        PlaceKind::Zine,
        PlaceKind::Festival,
        PlaceKind::XAccount,
        PlaceKind::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            PlaceKind::Subreddit => "subreddit",
            PlaceKind::Discord => "discord",
            PlaceKind::Forum => "forum",
            PlaceKind::FacebookGroup => "facebook_group",
            PlaceKind::Instagram => "instagram",
            PlaceKind::Tiktok => "tiktok",
            PlaceKind::Youtube => "youtube",
            PlaceKind::Playlist => "playlist",
            PlaceKind::Zine => "zine",
            PlaceKind::Festival => "festival",
            PlaceKind::XAccount => "x_account",
            PlaceKind::Other => "other",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

/// Lifecycle of one outreach relationship with one place. Exactly one row per
/// place exists, so the stage is the single source of truth for "what are we
/// doing with this community".
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachStage {
    Discovered,
    Researched,
    Contacted,
    Replied,
    Negotiating,
    Partnered,
    Declined,
    Dormant,
}

impl OutreachStage {
    pub const ALL: [OutreachStage; 8] = [
        OutreachStage::Discovered,
        OutreachStage::Researched,
        OutreachStage::Contacted,
        OutreachStage::Replied,
        OutreachStage::Negotiating,
        OutreachStage::Partnered,
        OutreachStage::Declined,
        OutreachStage::Dormant,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            OutreachStage::Discovered => "discovered",
            OutreachStage::Researched => "researched",
            OutreachStage::Contacted => "contacted",
            OutreachStage::Replied => "replied",
            OutreachStage::Negotiating => "negotiating",
            OutreachStage::Partnered => "partnered",
            OutreachStage::Declined => "declined",
            OutreachStage::Dormant => "dormant",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|stage| stage.as_str() == value)
    }

    /// Stages where reaching out to the place is a meaningful next action.
    /// Partnered means the relationship exists; declined and dormant mean it
    /// must not be disturbed; discovered has not been vetted yet.
    pub const fn contactable(self) -> bool {
        matches!(
            self,
            OutreachStage::Researched
                | OutreachStage::Contacted
                | OutreachStage::Replied
                | OutreachStage::Negotiating
        )
    }

    /// Stages a live relationship decays into when nothing happens for a long
    /// time. Dormancy is bookkeeping, so it applies broadly.
    pub const fn decays_to_dormant(self) -> bool {
        matches!(
            self,
            OutreachStage::Researched
                | OutreachStage::Contacted
                | OutreachStage::Replied
                | OutreachStage::Negotiating
        )
    }
}

/// The only legal stage moves. Anything absent is rejected by callers before
/// persistence, so the database CHECK stays a last-resort net rather than the
/// definition of the pipeline.
pub const ALLOWED_TRANSITIONS: &[(OutreachStage, OutreachStage)] = &[
    (OutreachStage::Discovered, OutreachStage::Researched),
    (OutreachStage::Discovered, OutreachStage::Declined),
    (OutreachStage::Discovered, OutreachStage::Dormant),
    (OutreachStage::Researched, OutreachStage::Contacted),
    (OutreachStage::Researched, OutreachStage::Declined),
    (OutreachStage::Researched, OutreachStage::Dormant),
    (OutreachStage::Contacted, OutreachStage::Replied),
    (OutreachStage::Contacted, OutreachStage::Declined),
    (OutreachStage::Contacted, OutreachStage::Dormant),
    (OutreachStage::Replied, OutreachStage::Negotiating),
    (OutreachStage::Replied, OutreachStage::Partnered),
    (OutreachStage::Replied, OutreachStage::Contacted),
    (OutreachStage::Replied, OutreachStage::Declined),
    (OutreachStage::Replied, OutreachStage::Dormant),
    (OutreachStage::Negotiating, OutreachStage::Partnered),
    (OutreachStage::Negotiating, OutreachStage::Contacted),
    (OutreachStage::Negotiating, OutreachStage::Declined),
    (OutreachStage::Negotiating, OutreachStage::Dormant),
    // A partnership decays like any other neglected relationship.
    (OutreachStage::Partnered, OutreachStage::Dormant),
    // New evidence may reopen a refusal, but only through research.
    (OutreachStage::Declined, OutreachStage::Researched),
    (OutreachStage::Declined, OutreachStage::Dormant),
    // Dormancy restarts through research as well.
    (OutreachStage::Dormant, OutreachStage::Researched),
];

#[must_use]
pub fn can_advance(current: OutreachStage, target: OutreachStage) -> bool {
    ALLOWED_TRANSITIONS
        .iter()
        .any(|(from, to)| *from == current && *to == target)
}

/// Whether a first contact (or a re-contact) may go out right now.
///
/// The stage itself must be contactable and the place-level cooldown must have
/// lapsed. `requires_approval` models places whose rules demand a green light
/// from the operator before anything is sent — money and contracts stay behind
/// approval everywhere else in CrowdRelay, and some communities do too.
#[must_use]
pub fn contact_allowed(
    stage: OutreachStage,
    next_eligible_at: time::OffsetDateTime,
    now: time::OffsetDateTime,
    requires_approval: bool,
    operator_approved: bool,
) -> bool {
    if !stage.contactable() {
        return false;
    }
    if next_eligible_at > now {
        return false;
    }
    !requires_approval || operator_approved
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn at_unix(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).expect("valid timestamp")
    }

    #[test]
    fn storage_round_trip_is_total_over_all_variants() {
        for kind in PlaceKind::ALL {
            assert_eq!(PlaceKind::from_storage(kind.as_str()), Some(kind));
        }
        for stage in OutreachStage::ALL {
            assert_eq!(OutreachStage::from_storage(stage.as_str()), Some(stage));
        }
        assert_eq!(PlaceKind::from_storage("webring"), None);
        assert_eq!(OutreachStage::from_storage("blocked"), None);
    }

    #[test]
    fn happy_path_is_walkable_end_to_end() {
        let path = [
            OutreachStage::Discovered,
            OutreachStage::Researched,
            OutreachStage::Contacted,
            OutreachStage::Replied,
            OutreachStage::Negotiating,
            OutreachStage::Partnered,
        ];
        for pair in path.windows(2) {
            assert!(
                can_advance(pair[0], pair[1]),
                "{:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn refusals_reopen_through_research_only() {
        assert!(!can_advance(
            OutreachStage::Declined,
            OutreachStage::Contacted
        ));
        assert!(!can_advance(
            OutreachStage::Declined,
            OutreachStage::Partnered
        ));
        assert!(can_advance(
            OutreachStage::Declined,
            OutreachStage::Researched
        ));
    }

    #[test]
    fn discovery_never_jumps_straight_to_contact() {
        assert!(!can_advance(
            OutreachStage::Discovered,
            OutreachStage::Contacted
        ));
        assert!(!can_advance(
            OutreachStage::Discovered,
            OutreachStage::Negotiating
        ));
    }

    #[test]
    fn partnerships_decay_but_do_not_rewind() {
        assert!(can_advance(
            OutreachStage::Partnered,
            OutreachStage::Dormant
        ));
        assert!(!can_advance(
            OutreachStage::Partnered,
            OutreachStage::Negotiating
        ));
        assert!(can_advance(
            OutreachStage::Dormant,
            OutreachStage::Researched
        ));
    }

    #[test]
    fn contact_requires_stage_cooldown_and_approval() {
        let now = at_unix(1_787_736_000);
        let cooled = at_unix(1_787_692_800);
        let hot = at_unix(1_788_254_400);

        assert!(contact_allowed(
            OutreachStage::Researched,
            cooled,
            now,
            false,
            false
        ));
        // Cooldown still running: no.
        assert!(!contact_allowed(
            OutreachStage::Researched,
            hot,
            now,
            false,
            false
        ));
        // Non-contactable stages refuse regardless of cooldown.
        assert!(!contact_allowed(
            OutreachStage::Discovered,
            cooled,
            now,
            false,
            false
        ));
        assert!(!contact_allowed(
            OutreachStage::Partnered,
            cooled,
            now,
            false,
            false
        ));
        // Approval-gated place without an operator green light: no.
        assert!(!contact_allowed(
            OutreachStage::Researched,
            cooled,
            now,
            true,
            false
        ));
        assert!(contact_allowed(
            OutreachStage::Researched,
            cooled,
            now,
            true,
            true
        ));
    }
}

/// Canonical form of a place URL, so one community is one row.
///
/// `discovery_places` is unique on `(workspace_id, platform, url)`, which makes
/// the URL the identity of a place. Discovery finds the same subreddit written
/// several ways and each spelling became its own row: measured in production,
/// `/r/MetalForTheMasses/` beside `https://www.reddit.com/r/MetalForTheMasses`,
/// and `https://reddit.com/r/Djent` beside `https://www.reddit.com/r/Djent`.
///
/// The cost is not a tidy table. One post is drafted per place, so a duplicated
/// community gets two posts — production held drafts for both `r/MetalMemes` and
/// `r/metalmemes`, and for both `r/listentothis` and `r/ListenToThis`. Publishing
/// both is posting twice to one community under the band's name, which is the
/// definition of the spam this project's North Star rules out. Reddit treats
/// subreddit names case-insensitively, so the two are the same place and only the
/// URL disagreed.
///
/// Deliberately narrow: only Reddit URLs are rewritten, and only into the form
/// Reddit itself canonicalises to. Folding by *name* instead would have been
/// wrong — production also holds `/r/InMetalWeTrust/` beside
/// `https://inmetalwetrust.club`, a subreddit and a website that share a name and
/// are two genuinely different places. Anything this does not recognise is
/// returned trimmed and otherwise untouched, so a new platform is not silently
/// mangled into a shape its own dedupe does not expect.
#[must_use]
pub fn canonical_place_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some(name) = reddit_subreddit_name(trimmed) else {
        return trimmed.to_owned();
    };
    format!("https://www.reddit.com/r/{}", name.to_lowercase())
}

/// The subreddit a URL names, if it names one.
///
/// Accepts the spellings discovery actually produces: absolute with or without
/// `www`, `http` or `https`, `old.` or `new.`, a bare `reddit.com/...`, and the
/// relative `/r/name/` that Reddit's own listing JSON returns.
fn reddit_subreddit_name(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let rest = rest
        .strip_prefix("www.")
        .or_else(|| rest.strip_prefix("old."))
        .or_else(|| rest.strip_prefix("new."))
        .unwrap_or(rest);
    // `/r/name/` with no host at all, as Reddit's listings return it.
    let path = if let Some(path) = rest.strip_prefix("reddit.com") {
        path
    } else if rest.starts_with("/r/") || rest.starts_with("r/") {
        rest
    } else {
        return None;
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let name = path.strip_prefix("r/")?;
    // `/r/name/comments/...` is a post inside the subreddit, not the subreddit.
    // Taking the first segment and discarding the rest would fold a permalink
    // onto the community and make a post look like a place, so a deeper path is
    // refused rather than truncated. A single trailing slash is just a spelling.
    let (name, rest) = match name.split_once(['/', '?', '#']) {
        Some((name, rest)) => (name, rest),
        None => (name, ""),
    };
    if !rest.is_empty() {
        return None;
    }
    let name = name.trim();
    // A name Reddit could not have issued is not a name worth canonicalising on.
    if name.is_empty()
        || name.len() > 21
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod canonical_place_url_tests {
    use super::canonical_place_url;

    /// The spellings production actually held, all folding onto one row.
    #[test]
    fn every_spelling_of_one_subreddit_canonicalises_together() {
        let canonical = "https://www.reddit.com/r/djent";
        for spelling in [
            "https://www.reddit.com/r/Djent",
            "https://reddit.com/r/Djent",
            "http://reddit.com/r/djent",
            "https://old.reddit.com/r/DJENT",
            "https://www.reddit.com/r/Djent/",
            "/r/Djent/",
            "r/djent",
            "  https://www.reddit.com/r/Djent  ",
        ] {
            assert_eq!(
                canonical_place_url(spelling),
                canonical,
                "{spelling} should canonicalise to {canonical}"
            );
        }
    }

    /// Case is the half that caused duplicate *posts*, not just duplicate rows.
    ///
    /// Reddit treats subreddit names case-insensitively, so `r/MetalMemes` and
    /// `r/metalmemes` are one community. Production drafted a post for each.
    #[test]
    fn case_variants_are_one_place() {
        assert_eq!(
            canonical_place_url("https://www.reddit.com/r/MetalMemes"),
            canonical_place_url("https://www.reddit.com/r/metalmemes"),
        );
        assert_eq!(
            canonical_place_url("/r/ListenToThis/"),
            canonical_place_url("https://reddit.com/r/listentothis"),
        );
    }

    /// A website that shares a subreddit's name is a different place.
    ///
    /// Production holds `/r/InMetalWeTrust/` beside `https://inmetalwetrust.club`.
    /// Folding by name would have merged a subreddit into a website.
    #[test]
    fn a_website_sharing_the_name_stays_separate() {
        assert_ne!(
            canonical_place_url("/r/InMetalWeTrust/"),
            canonical_place_url("https://inmetalwetrust.club"),
        );
        assert_eq!(
            canonical_place_url("https://inmetalwetrust.club"),
            "https://inmetalwetrust.club"
        );
    }

    /// A post inside a subreddit is not the subreddit.
    #[test]
    fn a_permalink_is_not_folded_onto_its_subreddit() {
        let post = "https://www.reddit.com/r/Metal/comments/abc123/some_title/";
        assert_eq!(canonical_place_url(post), post);
        assert_ne!(canonical_place_url(post), "https://www.reddit.com/r/metal");
    }

    /// Anything unrecognised passes through, trimmed and otherwise untouched.
    #[test]
    fn other_platforms_are_left_alone() {
        for url in [
            "https://discord.gg/abc123",
            "https://www.facebook.com/groups/12345",
            "https://open.spotify.com/playlist/xyz",
            "",
        ] {
            assert_eq!(canonical_place_url(url), url);
        }
        assert_eq!(
            canonical_place_url("  https://discord.gg/x  "),
            "https://discord.gg/x"
        );
    }

    /// A name Reddit could not have issued is not canonicalised on.
    ///
    /// Subreddit names are at most 21 characters of alphanumerics and
    /// underscores. Rewriting something else into `reddit.com/r/...` would invent
    /// a URL that resolves to nothing and merge unrelated rows onto it.
    #[test]
    fn an_impossible_subreddit_name_is_left_alone() {
        for url in [
            "https://www.reddit.com/r/",
            "https://www.reddit.com/r/way_too_long_to_be_a_real_subreddit_name",
            "https://www.reddit.com/r/has-a-hyphen",
            "https://www.reddit.com/user/someone",
        ] {
            assert_eq!(canonical_place_url(url), url, "{url} should pass through");
        }
    }
}
