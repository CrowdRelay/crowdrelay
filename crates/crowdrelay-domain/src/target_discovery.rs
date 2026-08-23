//! Where the pitcher's supply comes from, and what may never enter it.
//!
//! A candidate is not a target. Candidates arrive unverified, carrying the
//! source they came from and the raw evidence a submission route was read out
//! of. They become targets only once that route is confirmed. The rules here
//! are the ones that keep the supply clean, and every one of them exists
//! because its absence has a specific cost:
//!
//! - **A contact is read, never inferred.** Guessing `firstname@domain` from a
//!   name and a website is a bounce at best and a burned relationship at worst,
//!   and burned curators do not come back.
//! - **Free is enforced, not assumed.** A submission channel carries its own
//!   cost, and that cost decides the class of every pitch through it. Paid
//!   placement is refused outright at every autonomy level: buying a placement
//!   is fabricated engagement wearing a suit, and it gets the artist flagged.
//! - **A refusal is recorded with its reason**, so the same bad candidate is
//!   not rediscovered, re-screened and re-refused every week.
//!
//! The domain holds no address and fetches nothing. It decides whether what an
//! adapter brought back is admissible.

use serde::{Deserialize, Serialize};

use crate::{action_class::ActionClass, autonomy::Confidence};

/// Where a candidate came from. Recorded on every row so a source that turns
/// out to be bad can be revoked wholesale instead of hunted row by row.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// The submission route a curator published in the playlist description.
    /// The highest-yield source there is, and the one the field is for.
    PlaylistDescription,
    /// A curator-run site or link page reached from the description.
    CuratorSite,
    /// A submission platform, modelled as a channel because its cost decides
    /// the class of the pitch.
    SubmissionChannel,
    /// Anyone who has already written to us. Consent is not in question here.
    Reply,
    /// A list the band already had, imported by the operator.
    OperatorImport,
    /// The owner of a playlist that already carries comparable artists: both a
    /// fit signal and a contact route.
    SceneAdjacentPlaylist,
}

/// How the pitch would actually be delivered. An address and a form are not
/// interchangeable: one is an outbound message, the other is a submission the
/// curator invited.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Email,
    SubmissionForm,
    /// A platform handle. Kept because it is a real published route, even
    /// though nothing pitches through one yet.
    Handle,
}

/// What a submission channel charges, which is what decides whether a pitch
/// through it is ordinary third-party contact or spend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelCost {
    Free,
    /// Sells credits. Small money is still money and still needs the ceiling.
    Credit,
    /// Charges a per-submission fee.
    Fee,
    /// Sells the placement itself. Never usable.
    PaidPlacement,
}

impl ChannelCost {
    /// The class a pitch through this channel would carry, or `None` where the
    /// channel may not be pitched through at all.
    #[must_use]
    pub const fn pitch_class(self) -> Option<ActionClass> {
        match self {
            Self::Free => Some(ActionClass::ThirdParty),
            Self::Credit | Self::Fee => Some(ActionClass::Paid),
            Self::PaidPlacement => None,
        }
    }
}

/// What an adapter brought back about one candidate. Everything here is
/// evidence; nothing here is trusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateSnapshot {
    pub source: CandidateSource,
    pub route: RouteKind,
    /// The channel the route belongs to, where it belongs to one at all.
    pub channel_cost: Option<ChannelCost>,
    /// Whether the route was read verbatim out of published text. False means
    /// somebody worked it out, and worked-out contacts are refused.
    pub route_is_published: bool,
    /// Whether the evidence the route was read from was supplied at all. A
    /// route with no evidence cannot be reviewed by a human later.
    pub has_evidence: bool,
    /// How well the candidate matches what the band actually sounds like, in
    /// basis points. A metal reaction channel is not pitched a folk single.
    pub fit_basis_points: u16,
    pub follower_count: Option<u32>,
    /// Engagement observed against that follower count, where the adapter could
    /// measure it. Absent is not suspicious; implausible is.
    pub engagement_count: Option<u32>,
    /// The description offered placement for money.
    pub sells_placement: bool,
    /// The track list churns indiscriminately: a playlist that swaps its whole
    /// content constantly is not curating, it is farming.
    pub churns_indiscriminately: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TargetDiscoveryPolicy {
    /// Below this, a candidate is not worth a first contact the band only gets
    /// once.
    pub minimum_fit_basis_points: u16,
    /// A playlist smaller than this is not evidence of reach; pitching it costs
    /// the same first contact as one that is.
    pub minimum_follower_count: u32,
    /// Engagement below this share of followers, in basis points, reads as
    /// bought followers rather than an audience.
    pub minimum_engagement_basis_points: u16,
    /// Follower counts above this are only believed with engagement to match,
    /// because that is where the bought-follower market lives.
    pub engagement_scrutiny_follower_count: u32,
}

impl Default for TargetDiscoveryPolicy {
    fn default() -> Self {
        Self {
            minimum_fit_basis_points: 6_000,
            minimum_follower_count: 250,
            minimum_engagement_basis_points: 100,
            engagement_scrutiny_follower_count: 5_000,
        }
    }
}

/// Why a candidate will never be pitched. Recorded so the next sweep does not
/// rediscover it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The route was worked out rather than read. Never admissible.
    RouteInferred,
    /// No evidence travelled with the route, so no human can check it.
    EvidenceMissing,
    /// The channel sells placement.
    PaidPlacement,
    /// The description offers placement for money.
    SellsPlacement,
    /// Followers and engagement do not belong to the same audience.
    ImplausibleEngagement,
    /// The track list churns indiscriminately.
    IndiscriminateChurn,
    /// Real, but not a fit for this band.
    PoorFit,
    /// Real, but too small for a contact the band only gets one of.
    TooSmall,
}

impl RefusalReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RouteInferred => "route_inferred",
            Self::EvidenceMissing => "evidence_missing",
            Self::PaidPlacement => "paid_placement",
            Self::SellsPlacement => "sells_placement",
            Self::ImplausibleEngagement => "implausible_engagement",
            Self::IndiscriminateChurn => "indiscriminate_churn",
            Self::PoorFit => "poor_fit",
            Self::TooSmall => "too_small",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ScreeningVerdict {
    /// Admissible. The class is what a pitch through this route would cost the
    /// envelope, and confidence is what the evidence supports.
    Admit {
        class: ActionClass,
        confidence: Confidence,
    },
    Refuse(RefusalReason),
}

/// Screens one candidate. Order matters: the refusals that are permanent
/// regardless of policy come first, so tightening a threshold can never turn a
/// scam into a merely-small candidate.
#[must_use]
pub fn screen_candidate(
    snapshot: &CandidateSnapshot,
    policy: TargetDiscoveryPolicy,
) -> ScreeningVerdict {
    if !snapshot.route_is_published {
        return ScreeningVerdict::Refuse(RefusalReason::RouteInferred);
    }
    if !snapshot.has_evidence {
        return ScreeningVerdict::Refuse(RefusalReason::EvidenceMissing);
    }
    if snapshot.sells_placement {
        return ScreeningVerdict::Refuse(RefusalReason::SellsPlacement);
    }
    if snapshot.churns_indiscriminately {
        return ScreeningVerdict::Refuse(RefusalReason::IndiscriminateChurn);
    }

    // A channel that sells placement is refused whatever else is true of it,
    // and a channel that charges makes the pitch spend rather than contact.
    let class = match snapshot.channel_cost {
        Some(cost) => match cost.pitch_class() {
            Some(class) => class,
            None => return ScreeningVerdict::Refuse(RefusalReason::PaidPlacement),
        },
        None => ActionClass::ThirdParty,
    };

    if let (Some(followers), Some(engagement)) =
        (snapshot.follower_count, snapshot.engagement_count)
        && followers >= policy.engagement_scrutiny_follower_count
    {
        let ratio = engagement_basis_points(followers, engagement);
        if ratio < policy.minimum_engagement_basis_points {
            return ScreeningVerdict::Refuse(RefusalReason::ImplausibleEngagement);
        }
    }
    if let Some(followers) = snapshot.follower_count
        && followers < policy.minimum_follower_count
    {
        return ScreeningVerdict::Refuse(RefusalReason::TooSmall);
    }
    if snapshot.fit_basis_points < policy.minimum_fit_basis_points {
        return ScreeningVerdict::Refuse(RefusalReason::PoorFit);
    }

    ScreeningVerdict::Admit {
        class,
        confidence: Confidence::saturating_from_basis_points(snapshot.fit_basis_points),
    }
}

/// Whether an admitted candidate can become a target as it stands.
///
/// A target carries an address, so only an email route promotes today. A form
/// or a handle stays a candidate until the pitcher that can use it exists;
/// dropping it would throw away supply that is already screened and clean.
#[must_use]
pub const fn promotes_to_target(route: RouteKind) -> bool {
    matches!(route, RouteKind::Email)
}

fn engagement_basis_points(followers: u32, engagement: u32) -> u16 {
    if followers == 0 {
        return 0;
    }
    let ratio = u64::from(engagement) * 10_000 / u64::from(followers);
    u16::try_from(ratio.min(10_000)).unwrap_or(10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> CandidateSnapshot {
        CandidateSnapshot {
            source: CandidateSource::PlaylistDescription,
            route: RouteKind::Email,
            channel_cost: None,
            route_is_published: true,
            has_evidence: true,
            fit_basis_points: 8_000,
            follower_count: Some(4_000),
            engagement_count: Some(300),
            sells_placement: false,
            churns_indiscriminately: false,
        }
    }

    fn refusal(verdict: ScreeningVerdict) -> RefusalReason {
        match verdict {
            ScreeningVerdict::Refuse(reason) => reason,
            ScreeningVerdict::Admit { .. } => panic!("expected a refusal"),
        }
    }

    #[test]
    fn a_published_route_with_evidence_and_fit_is_admitted_as_third_party() {
        let verdict = screen_candidate(&clean(), TargetDiscoveryPolicy::default());
        assert!(matches!(
            verdict,
            ScreeningVerdict::Admit {
                class: ActionClass::ThirdParty,
                ..
            }
        ));
    }

    #[test]
    fn an_inferred_route_is_refused_before_anything_else_is_considered() {
        let snapshot = CandidateSnapshot {
            route_is_published: false,
            // Everything else about this candidate is excellent, which is
            // exactly the case where a guessed address is tempting.
            fit_basis_points: 10_000,
            follower_count: Some(50_000),
            engagement_count: Some(20_000),
            ..clean()
        };
        assert_eq!(
            refusal(screen_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            RefusalReason::RouteInferred
        );
    }

    #[test]
    fn a_route_without_evidence_cannot_be_reviewed_and_is_refused() {
        let snapshot = CandidateSnapshot {
            has_evidence: false,
            ..clean()
        };
        assert_eq!(
            refusal(screen_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            RefusalReason::EvidenceMissing
        );
    }

    #[test]
    fn paid_placement_is_refused_at_every_threshold() {
        let snapshot = CandidateSnapshot {
            channel_cost: Some(ChannelCost::PaidPlacement),
            ..clean()
        };
        for policy in [
            TargetDiscoveryPolicy::default(),
            TargetDiscoveryPolicy {
                minimum_fit_basis_points: 0,
                minimum_follower_count: 0,
                minimum_engagement_basis_points: 0,
                engagement_scrutiny_follower_count: u32::MAX,
            },
        ] {
            assert_eq!(
                refusal(screen_candidate(&snapshot, policy)),
                RefusalReason::PaidPlacement
            );
        }
        assert_eq!(ChannelCost::PaidPlacement.pitch_class(), None);
    }

    #[test]
    fn a_channel_that_charges_makes_the_pitch_spend_rather_than_contact() {
        for cost in [ChannelCost::Credit, ChannelCost::Fee] {
            let snapshot = CandidateSnapshot {
                channel_cost: Some(cost),
                ..clean()
            };
            assert!(matches!(
                screen_candidate(&snapshot, TargetDiscoveryPolicy::default()),
                ScreeningVerdict::Admit {
                    class: ActionClass::Paid,
                    ..
                }
            ));
        }
    }

    #[test]
    fn bought_followers_are_refused_where_the_market_for_them_is() {
        let snapshot = CandidateSnapshot {
            follower_count: Some(80_000),
            engagement_count: Some(4),
            ..clean()
        };
        assert_eq!(
            refusal(screen_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            RefusalReason::ImplausibleEngagement
        );
    }

    #[test]
    fn a_small_playlist_is_not_scrutinised_for_engagement_it_cannot_have() {
        // Below the scrutiny threshold a low ratio is ordinary, not suspicious;
        // the candidate is judged on size and fit instead.
        let snapshot = CandidateSnapshot {
            follower_count: Some(900),
            engagement_count: Some(2),
            ..clean()
        };
        assert!(matches!(
            screen_candidate(&snapshot, TargetDiscoveryPolicy::default()),
            ScreeningVerdict::Admit { .. }
        ));
    }

    #[test]
    fn poor_fit_and_too_small_are_distinguished_from_each_other() {
        let small = CandidateSnapshot {
            follower_count: Some(10),
            ..clean()
        };
        assert_eq!(
            refusal(screen_candidate(&small, TargetDiscoveryPolicy::default())),
            RefusalReason::TooSmall
        );
        let unfit = CandidateSnapshot {
            fit_basis_points: 1_000,
            ..clean()
        };
        assert_eq!(
            refusal(screen_candidate(&unfit, TargetDiscoveryPolicy::default())),
            RefusalReason::PoorFit
        );
    }

    #[test]
    fn only_an_address_promotes_to_a_target_today() {
        assert!(promotes_to_target(RouteKind::Email));
        assert!(!promotes_to_target(RouteKind::SubmissionForm));
        assert!(!promotes_to_target(RouteKind::Handle));
    }
}
