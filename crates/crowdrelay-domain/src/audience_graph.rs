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
