//! What a band may publish about itself when it is looking for representation.
//!
//! A band's career moves on a different axis from its fanbase: an agent, a
//! label, a better bill. The system holds the evidence those decisions are made
//! on, so publishing a listing is a short step — and it is the step where this
//! stops being software one tenant runs and becomes something strangers read.
//!
//! # The rule that keeps tenant boundaries intact
//!
//! A label reading another tenant's calendar and fanbase is one tenant reading
//! another's commercial position, which is what `workspace_id` exists to
//! prevent. A label reading a **listing the band published** is the band
//! advertising. The whole difference is who acted, so it has to be impossible
//! for a field to arrive in a listing without the band putting it there:
//!
//! **Nothing is inferred from a workspace into a listing.** Every claim is
//! composed by the band, and `redact` is what a reader outside the tenant gets.
//!
//! # An unsupported number is absent, not zero
//!
//! An agent's decision is made on draw, and a listing carrying reachable fans
//! by city and repeat attendance is worth more than a press kit. That value
//! depends entirely on the numbers being true, so a claim the band cannot
//! support is left out rather than rounded down — the same discipline as a
//! missing read-model number being `null` and never `0`.
//!
//! A listing may also say *insufficient evidence* out loud. A band honest about
//! what it cannot yet show reads better than one quietly missing a field, and
//! it stops a reader inferring the worst.
//!
//! # Why the tier matters
//!
//! `MetricValueTier` already ranks how close a metric sits to something the
//! business banks: followers are `Vanity`, trackers are `Intermediate`, tickets
//! and retained fans are `Downstream`. A listing built from follower counts is
//! the press kit this is supposed to replace, so the tier is carried on every
//! claim and a listing that leads with vanity is refused rather than published.

use serde::{Deserialize, Serialize};

use crate::value_tier::MetricValueTier;

/// One published claim: a number the band chose to show, and what backs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedClaim {
    /// What is being claimed, in the band's words — "reachable fans in Wrocław".
    pub label: String,
    /// The value, or `None` for a claim the band wants to make and cannot yet
    /// support. A listing may say "we do not know this yet"; it may not guess.
    pub value: Option<i64>,
    pub tier: MetricValueTier,
    /// Where the number came from, in words a reader can check against the
    /// band — "our own check-ins", "ticket orders". A claim with no basis is
    /// refused: an unbacked number on a listing is worse than no listing.
    pub basis: String,
}

impl ListedClaim {
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.value.is_some()
    }
}

/// How visible a band wants to be. Default is invisible, because being listed
/// is a decision and not a side effect of signing up.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingVisibility {
    /// Not published. The band exists; nobody outside can see a listing.
    #[default]
    Unlisted,
    /// Readable by parties the operator has admitted — agents and labels who
    /// accepted the terms, not the open web.
    AdmittedReaders,
}

/// A band's published profile. Composed by the band, revocable by the band.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BandListing {
    pub act_name: String,
    /// Free-form tags, the same shape as a venue's genre and a format's
    /// `genre_fit`: a bias for matching, never a filter that hides a band.
    pub genre_tags: Vec<String>,
    /// Cities the band says it can draw in. Its own claim, not a read of where
    /// its fans happen to be.
    pub cities: Vec<String>,
    pub claims: Vec<ListedClaim>,
    /// Dates the band chose to show. A listing is not a calendar feed.
    pub published_dates: Vec<String>,
    pub seeking: Vec<String>,
    pub visibility: ListingVisibility,
}

/// Why a listing may not be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListingRefusal {
    /// A listing with no act name is not a listing.
    MissingActName,
    /// Nothing to read. Publishing an empty profile wastes the one look a busy
    /// agent gives a band.
    NothingClaimed,
    /// A number with nothing behind it. The listing's whole value is that its
    /// numbers are true.
    ClaimWithoutBasis { label: String },
    /// Every supported claim is a follower count. That is the press kit this
    /// replaces, and an agent reading it learns nothing about draw.
    OnlyVanityClaims,
}

impl ListingRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingActName => "a listing needs the act's name".to_owned(),
            Self::NothingClaimed => {
                "a listing with no claims wastes the one look an agent gives it".to_owned()
            }
            Self::ClaimWithoutBasis { label } => format!(
                "`{label}` states a number with nothing behind it — say where it came from, \
                 or leave it out"
            ),
            Self::OnlyVanityClaims => {
                "every supported claim here is a reach number; an agent decides on draw, \
                 so show something banked — tickets, attendance, fans who stayed"
                    .to_owned()
            }
        }
    }
}

/// Checks a listing before it may be published.
///
/// # Errors
///
/// Refuses a listing with no name, no claims, an unbacked number, or nothing
/// above `Vanity` among the claims it can actually support.
pub fn review_listing(listing: &BandListing) -> Result<(), ListingRefusal> {
    if listing.act_name.trim().is_empty() {
        return Err(ListingRefusal::MissingActName);
    }
    if listing.claims.is_empty() {
        return Err(ListingRefusal::NothingClaimed);
    }

    for claim in &listing.claims {
        if claim.basis.trim().is_empty() {
            return Err(ListingRefusal::ClaimWithoutBasis {
                label: claim.label.clone(),
            });
        }
    }

    // An unsupported claim is allowed — saying "we do not know this yet" is
    // honest. But a listing whose *supported* claims are all vanity tells an
    // agent nothing, and a listing that supports nothing at all is the same
    // problem in a stronger form.
    let leads_with_substance = listing
        .claims
        .iter()
        .any(|claim| claim.is_supported() && claim.tier > MetricValueTier::Vanity);
    if !leads_with_substance {
        return Err(ListingRefusal::OnlyVanityClaims);
    }

    Ok(())
}

/// What a reader outside the tenant receives.
///
/// This is the boundary. A reader gets the listing and nothing else: no
/// workspace, no fan records, no unpublished dates, and no claim the band left
/// unsupported — an absent number must not become a zero in somebody else's
/// view, and a half-finished claim is not an advertisement.
///
/// Returns `None` for an unlisted band, so a caller that forgets to check
/// visibility cannot leak one by default.
#[must_use]
pub fn redact(listing: &BandListing) -> Option<BandListing> {
    if listing.visibility == ListingVisibility::Unlisted {
        return None;
    }
    Some(BandListing {
        act_name: listing.act_name.clone(),
        genre_tags: listing.genre_tags.clone(),
        cities: listing.cities.clone(),
        claims: listing
            .claims
            .iter()
            .filter(|claim| claim.is_supported())
            .cloned()
            .collect(),
        published_dates: listing.published_dates.clone(),
        seeking: listing.seeking.clone(),
        visibility: listing.visibility,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(label: &str, value: Option<i64>, tier: MetricValueTier) -> ListedClaim {
        ListedClaim {
            label: label.to_owned(),
            value,
            tier,
            basis: "our own check-ins".to_owned(),
        }
    }

    fn listing() -> BandListing {
        BandListing {
            act_name: "Virya".to_owned(),
            genre_tags: vec!["metal".to_owned(), "alternative".to_owned()],
            cities: vec!["Wrocław".to_owned()],
            claims: vec![
                claim(
                    "Reachable fans in Wrocław",
                    Some(300),
                    MetricValueTier::Intermediate,
                ),
                claim(
                    "Tickets sold, last show",
                    Some(120),
                    MetricValueTier::Downstream,
                ),
            ],
            published_dates: vec!["2026-11-14 Klub X, Wrocław".to_owned()],
            seeking: vec!["booking agent".to_owned()],
            visibility: ListingVisibility::AdmittedReaders,
        }
    }

    #[test]
    fn a_listing_with_banked_numbers_publishes() {
        assert_eq!(review_listing(&listing()), Ok(()));
    }

    /// The press kit this is meant to replace: all reach, no draw.
    #[test]
    fn a_listing_of_only_follower_counts_is_refused() {
        let mut only_reach = listing();
        only_reach.claims = vec![
            claim("Instagram followers", Some(4_000), MetricValueTier::Vanity),
            claim("Spotify followers", Some(2_100), MetricValueTier::Vanity),
        ];
        assert_eq!(
            review_listing(&only_reach),
            Err(ListingRefusal::OnlyVanityClaims)
        );
    }

    /// Saying "we do not know this yet" is honest and allowed.
    #[test]
    fn an_unsupported_claim_may_sit_beside_a_supported_one() {
        let mut mixed = listing();
        mixed.claims.push(claim(
            "Repeat attendance",
            None,
            MetricValueTier::Downstream,
        ));
        assert_eq!(review_listing(&mixed), Ok(()));
    }

    /// But a band that can support nothing has not got a listing yet.
    #[test]
    fn a_listing_supporting_nothing_is_refused() {
        let mut unsupported = listing();
        unsupported.claims = vec![
            claim("Tickets sold", None, MetricValueTier::Downstream),
            claim("Repeat attendance", None, MetricValueTier::Downstream),
        ];
        assert_eq!(
            review_listing(&unsupported),
            Err(ListingRefusal::OnlyVanityClaims)
        );
    }

    #[test]
    fn a_number_with_nothing_behind_it_is_refused() {
        let mut unbacked = listing();
        unbacked.claims[0].basis = "  ".to_owned();
        assert_eq!(
            review_listing(&unbacked),
            Err(ListingRefusal::ClaimWithoutBasis {
                label: "Reachable fans in Wrocław".to_owned()
            })
        );
    }

    #[test]
    fn an_empty_listing_is_refused() {
        let mut empty = listing();
        empty.claims.clear();
        assert_eq!(review_listing(&empty), Err(ListingRefusal::NothingClaimed));

        let mut nameless = listing();
        nameless.act_name = "  ".to_owned();
        assert_eq!(
            review_listing(&nameless),
            Err(ListingRefusal::MissingActName)
        );
    }

    /// The boundary. Default visibility is `Unlisted`, and a caller that
    /// forgets to check it gets nothing rather than a leak.
    #[test]
    fn an_unlisted_band_redacts_to_nothing() {
        let mut private = listing();
        private.visibility = ListingVisibility::Unlisted;
        assert_eq!(redact(&private), None);
        assert_eq!(ListingVisibility::default(), ListingVisibility::Unlisted);
    }

    /// An absent number must not reach a reader at all — outside this tenant it
    /// would be indistinguishable from a zero.
    #[test]
    fn a_reader_never_sees_an_unsupported_claim() {
        let mut mixed = listing();
        mixed.claims.push(claim(
            "Repeat attendance",
            None,
            MetricValueTier::Downstream,
        ));
        let seen = redact(&mixed).expect("listing is visible");
        assert_eq!(seen.claims.len(), 2);
        assert!(seen.claims.iter().all(ListedClaim::is_supported));
    }

    #[test]
    fn a_reader_sees_only_what_the_band_published() {
        let seen = redact(&listing()).expect("listing is visible");
        assert_eq!(seen.act_name, "Virya");
        assert_eq!(seen.published_dates, vec!["2026-11-14 Klub X, Wrocław"]);
        assert_eq!(seen.cities, vec!["Wrocław"]);
    }

    #[test]
    fn every_refusal_tells_the_band_what_to_do() {
        for refusal in [
            ListingRefusal::MissingActName,
            ListingRefusal::NothingClaimed,
            ListingRefusal::ClaimWithoutBasis {
                label: "Tickets sold".to_owned(),
            },
            ListingRefusal::OnlyVanityClaims,
        ] {
            let message = refusal.message();
            assert!(message.len() > 20, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the band: {message}"
            );
        }
    }
}
