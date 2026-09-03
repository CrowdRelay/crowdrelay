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
use time::{Duration, OffsetDateTime};

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

/// What is known about a public community an agent proposed engaging.
///
/// A community is not a personal contact, so the route rules do not apply to
/// it: there is no address to read or infer, and posting in a public space
/// costs nobody a first contact they only get once. Everything else the
/// screening policy checks still applies, because the failure it prevents is
/// the same one — posting into a space that is dead, bought, hostile to
/// promotion, or not this band's audience.
///
/// Every measurement is optional because the agent that proposed the target
/// may have found it before the audience graph did. Absent is not suspicious;
/// implausible is. A community with no measured size is admitted and left to
/// the causal model, not refused on a number nobody has.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CommunityCandidateSnapshot {
    /// Whether the agent supplied any evidence for the proposal. Without it
    /// no human can check why the community was ever engaged.
    pub has_evidence: bool,
    /// Members, where the audience graph has measured them.
    pub member_count: Option<u32>,
    /// Observed activity against that member count, in basis points.
    pub activity_basis_points: Option<u16>,
    /// The share of the community's content that may be self-promotion, as
    /// the community's own rules state it. Zero means promotion is banned:
    /// posting there is a removal and a moderator who remembers the band.
    pub self_promo_ratio_percent: Option<u8>,
    /// The community sells placement or promotion slots.
    pub sells_placement: bool,
    /// Our own recorded judgement: the community rejected us, or an operator
    /// marked it as not a fit.
    pub refused_by_us_or_them: bool,
}

/// Screens one public community. The refusal order matches
/// [`screen_candidate`]: what is permanently wrong comes before what a policy
/// threshold makes wrong, so tightening a threshold can never reclassify a
/// community that sells placement as a merely small one.
///
/// Admission carries [`ActionClass::ThirdParty`] — a post in a public space
/// is third-party contact, never spend. Confidence is what the evidence
/// supports: measured size and activity raise it, an unmeasured community
/// sits at the floor rather than borrowing confidence it has not earned.
#[must_use]
pub fn screen_community_candidate(
    snapshot: &CommunityCandidateSnapshot,
    policy: TargetDiscoveryPolicy,
) -> ScreeningVerdict {
    if !snapshot.has_evidence {
        return ScreeningVerdict::Refuse(RefusalReason::EvidenceMissing);
    }
    if snapshot.sells_placement {
        return ScreeningVerdict::Refuse(RefusalReason::SellsPlacement);
    }
    if snapshot.refused_by_us_or_them {
        return ScreeningVerdict::Refuse(RefusalReason::PoorFit);
    }
    // A community that bans self-promotion outright is not a growth target at
    // any size. Posting there costs a removal and a moderator who remembers.
    if snapshot.self_promo_ratio_percent == Some(0) {
        return ScreeningVerdict::Refuse(RefusalReason::PoorFit);
    }
    // Large membership is only believed with activity to match — that is
    // where inflated subscriber counts live.
    if let (Some(members), Some(activity)) = (snapshot.member_count, snapshot.activity_basis_points)
        && members >= policy.engagement_scrutiny_follower_count
        && u32::from(activity) < u32::from(policy.minimum_engagement_basis_points)
    {
        return ScreeningVerdict::Refuse(RefusalReason::ImplausibleEngagement);
    }
    if let Some(members) = snapshot.member_count
        && members < policy.minimum_follower_count
    {
        return ScreeningVerdict::Refuse(RefusalReason::TooSmall);
    }
    ScreeningVerdict::Admit {
        class: ActionClass::ThirdParty,
        confidence: community_confidence(snapshot),
    }
}

/// Confidence in a community proposal, from what has actually been measured.
/// An unmeasured community earns the floor; a measured one earns more the
/// larger and more active it is. This is deliberately coarse — it ranks
/// proposals, it does not pretend to be a fit model.
fn community_confidence(snapshot: &CommunityCandidateSnapshot) -> Confidence {
    let mut basis_points: u16 = 3_000;
    if let Some(members) = snapshot.member_count {
        basis_points = basis_points.saturating_add(match members {
            0..=999 => 0,
            1_000..=9_999 => 1_000,
            10_000..=99_999 => 2_000,
            _ => 3_000,
        });
    }
    if let Some(activity) = snapshot.activity_basis_points {
        basis_points = basis_points.saturating_add(activity / 5);
    }
    Confidence::saturating_from_basis_points(basis_points.min(10_000))
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

/// What the pitcher has to work with, and what it is waiting on.
///
/// Screening is cheap and durable, so the expensive thing is not judging a
/// candidate — it is deciding to go looking for one at all. This snapshot
/// exists so that decision is made from evidence rather than on a timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutreachSupplySnapshot {
    /// Targets that could be pitched today: active, still accepting outreach,
    /// and not marked do-not-contact.
    pub pitchable_targets: u32,
    /// Candidates that passed screening and are waiting for a human to confirm
    /// the route. Supply that exists but cannot be used yet.
    pub admitted_candidates: u32,
    pub last_sweep_requested_at: Option<OffsetDateTime>,
    /// Candidates ingested since the last sweep was asked for. Zero means the
    /// adapter never came back, which is a different problem from a dry source.
    pub candidates_since_last_sweep: u32,
    /// How many sweeps in a row returned candidates but nothing admissible.
    /// A source that keeps producing refusals is exhausted, not unlucky.
    pub consecutive_barren_sweeps: u16,
    /// Items the most recent sweep reported reading, before any screening.
    ///
    /// `None` means the adapter reported no such count, which is how every
    /// sweep predating the report looks; the rule then falls back to its
    /// previous reading rather than inventing a fault.
    ///
    /// `Some(0)` is the one that matters: the adapter answered and read
    /// nothing, so there was never anything to screen. That is a broken read
    /// path, not a dry source, and the two need different owners.
    pub items_seen_in_last_sweep: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct OutreachSupplyPolicy {
    /// Below this many pitchable targets the pitcher is starving and a sweep is
    /// worth asking for.
    pub minimum_pitchable_targets: u32,
    /// A sweep reads public data and contacts nobody, but it still calls
    /// somebody else's API. Once a day is plenty for a supply that moves this
    /// slowly.
    pub sweep_cooldown_hours: u16,
    /// How many candidates one sweep is asked for. Bounded so an adapter cannot
    /// turn a request into an unbounded crawl.
    pub requested_candidates: u16,
    /// After this many barren sweeps in a row the agent stops asking. Widening
    /// the source is an operator decision, and hammering a dry one is the
    /// autonomous equivalent of refreshing an empty inbox.
    pub barren_sweep_limit: u16,
}

impl Default for OutreachSupplyPolicy {
    fn default() -> Self {
        Self {
            minimum_pitchable_targets: 25,
            sweep_cooldown_hours: 24,
            requested_candidates: 40,
            barren_sweep_limit: 3,
        }
    }
}

/// Why no sweep was asked for. Each variant is a different owner: two of these
/// are the agent waiting on itself, and two are the agent waiting on a human.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplyHold {
    /// Enough pitchable targets already.
    SupplyIsAdequate,
    /// A sweep was asked for recently.
    CooldownActive,
    /// Screened candidates are piling up unconfirmed. Fetching more would grow
    /// a queue nobody is working, and the route confirmation is the operator's.
    AwaitingRouteConfirmation,
    /// Repeated sweeps found nothing admissible. The source needs widening,
    /// which is not something the agent can decide.
    SourceExhausted,
    /// Repeated sweeps answered having read nothing at all. Nothing was ever
    /// screened, so the source cannot be called dry: the read path is broken,
    /// and the fix is a credential or an adapter rather than a wider query.
    AdapterReadNothing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum OutreachSupplyDecision {
    Request {
        requested_candidates: u16,
        confidence: Confidence,
    },
    Hold(SupplyHold),
}

/// Decides whether to ask an adapter for more candidates.
///
/// Order matters. The two holds that belong to a human come before the ones
/// that belong to the clock, so a starving pitcher never looks like a healthy
/// one just because a sweep happens to be on cooldown.
#[must_use]
pub fn evaluate_outreach_supply(
    snapshot: &OutreachSupplySnapshot,
    policy: OutreachSupplyPolicy,
    now: OffsetDateTime,
) -> OutreachSupplyDecision {
    if snapshot.consecutive_barren_sweeps >= policy.barren_sweep_limit {
        // The back-off is the same either way — the agent stops asking, because
        // neither a dry source nor a broken adapter is fixed by asking again.
        // Only the reported cause differs, and it has to, because it decides
        // whether an operator widens a query or renews a credential.
        return OutreachSupplyDecision::Hold(match snapshot.items_seen_in_last_sweep {
            Some(0) => SupplyHold::AdapterReadNothing,
            _ => SupplyHold::SourceExhausted,
        });
    }
    if snapshot.pitchable_targets >= policy.minimum_pitchable_targets {
        return OutreachSupplyDecision::Hold(SupplyHold::SupplyIsAdequate);
    }
    // Admitted candidates are supply the agent already found. Asking for more
    // while that queue is full would hide an operator bottleneck behind
    // apparent activity.
    if snapshot.admitted_candidates >= policy.minimum_pitchable_targets {
        return OutreachSupplyDecision::Hold(SupplyHold::AwaitingRouteConfirmation);
    }
    if let Some(last) = snapshot.last_sweep_requested_at {
        let cooldown = Duration::hours(i64::from(policy.sweep_cooldown_hours));
        if now - last < cooldown {
            return OutreachSupplyDecision::Hold(SupplyHold::CooldownActive);
        }
    }
    OutreachSupplyDecision::Request {
        requested_candidates: policy.requested_candidates,
        confidence: deficit_confidence(
            snapshot.pitchable_targets + snapshot.admitted_candidates,
            policy.minimum_pitchable_targets,
        ),
    }
}

/// How short the supply is, as confidence. An empty pipeline is a certainty,
/// not a guess, so zero supply reads as full confidence.
fn deficit_confidence(available: u32, minimum: u32) -> Confidence {
    if minimum == 0 {
        return Confidence::MIN;
    }
    let deficit = minimum.saturating_sub(available);
    let ratio = u64::from(deficit) * 10_000 / u64::from(minimum);
    Confidence::saturating_from_basis_points(u16::try_from(ratio).unwrap_or(10_000))
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

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn starving() -> OutreachSupplySnapshot {
        OutreachSupplySnapshot {
            pitchable_targets: 0,
            admitted_candidates: 0,
            last_sweep_requested_at: None,
            candidates_since_last_sweep: 0,
            consecutive_barren_sweeps: 0,
            items_seen_in_last_sweep: None,
        }
    }

    fn hold(decision: OutreachSupplyDecision) -> SupplyHold {
        match decision {
            OutreachSupplyDecision::Hold(reason) => reason,
            OutreachSupplyDecision::Request { .. } => panic!("expected a hold"),
        }
    }

    #[test]
    fn an_empty_pipeline_asks_for_a_sweep_with_full_confidence() {
        let decision =
            evaluate_outreach_supply(&starving(), OutreachSupplyPolicy::default(), now());
        assert_eq!(
            decision,
            OutreachSupplyDecision::Request {
                requested_candidates: 40,
                confidence: Confidence::MAX,
            }
        );
    }

    #[test]
    fn a_stocked_pipeline_asks_for_nothing() {
        let snapshot = OutreachSupplySnapshot {
            pitchable_targets: 25,
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::SupplyIsAdequate
        );
    }

    #[test]
    fn candidates_waiting_on_a_human_are_not_answered_with_more_candidates() {
        // The pitcher is still starving, but the bottleneck is route
        // confirmation. Sweeping again would grow a queue nobody is working.
        let snapshot = OutreachSupplySnapshot {
            pitchable_targets: 0,
            admitted_candidates: 30,
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::AwaitingRouteConfirmation
        );
    }

    #[test]
    fn a_sweep_is_not_repeated_inside_its_cooldown() {
        let policy = OutreachSupplyPolicy::default();
        let snapshot = OutreachSupplySnapshot {
            last_sweep_requested_at: Some(now() - Duration::hours(3)),
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(&snapshot, policy, now())),
            SupplyHold::CooldownActive
        );
        let expired = OutreachSupplySnapshot {
            last_sweep_requested_at: Some(now() - Duration::hours(25)),
            ..starving()
        };
        assert!(matches!(
            evaluate_outreach_supply(&expired, policy, now()),
            OutreachSupplyDecision::Request { .. }
        ));
    }

    #[test]
    fn a_source_that_keeps_returning_nothing_admissible_stops_being_asked() {
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 3,
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::SourceExhausted
        );
    }

    #[test]
    fn exhaustion_outranks_every_other_reason_to_keep_sweeping() {
        // Starving, off cooldown and nothing awaiting a human: every signal
        // says sweep. The dry source still wins, because the next sweep would
        // cost an API call to learn what the last three already proved.
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 5,
            last_sweep_requested_at: Some(now() - Duration::days(30)),
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::SourceExhausted
        );
    }

    #[test]
    fn a_sweep_that_read_nothing_is_a_broken_adapter_not_a_dry_source() {
        // The production failure this exists for: the sweep answered, reported
        // zero candidates and was counted barren three times, and the agent was
        // about to tell an operator to widen a source that had never returned a
        // single item to screen.
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 3,
            items_seen_in_last_sweep: Some(0),
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::AdapterReadNothing
        );
    }

    #[test]
    fn a_sweep_that_read_plenty_and_admitted_none_is_still_a_dry_source() {
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 3,
            items_seen_in_last_sweep: Some(200),
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::SourceExhausted
        );
    }

    #[test]
    fn an_unreported_read_count_does_not_invent_an_adapter_fault() {
        // Every sweep from before the report existed looks like this. Silence
        // about what was read is not evidence that nothing was.
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 3,
            items_seen_in_last_sweep: None,
            ..starving()
        };
        assert_eq!(
            hold(evaluate_outreach_supply(
                &snapshot,
                OutreachSupplyPolicy::default(),
                now()
            )),
            SupplyHold::SourceExhausted
        );
    }

    #[test]
    fn a_read_count_of_zero_never_stops_a_sweep_that_was_not_yet_barren() {
        // The count only ever renames an existing hold. Below the barren limit
        // there is no hold to rename, and a starving pitcher still sweeps.
        let snapshot = OutreachSupplySnapshot {
            consecutive_barren_sweeps: 1,
            items_seen_in_last_sweep: Some(0),
            ..starving()
        };
        assert!(matches!(
            evaluate_outreach_supply(&snapshot, OutreachSupplyPolicy::default(), now()),
            OutreachSupplyDecision::Request { .. }
        ));
    }

    #[test]
    fn partial_supply_lowers_confidence_rather_than_silencing_the_request() {
        let snapshot = OutreachSupplySnapshot {
            pitchable_targets: 20,
            ..starving()
        };
        let OutreachSupplyDecision::Request { confidence, .. } =
            evaluate_outreach_supply(&snapshot, OutreachSupplyPolicy::default(), now())
        else {
            panic!("expected a request");
        };
        // 5 short of 25 is a fifth of the target missing.
        assert_eq!(confidence.basis_points(), 2_000);
    }

    // ── Community screening ──

    fn community() -> CommunityCandidateSnapshot {
        CommunityCandidateSnapshot {
            has_evidence: true,
            member_count: Some(40_000),
            activity_basis_points: Some(5_000),
            self_promo_ratio_percent: Some(10),
            sells_placement: false,
            refused_by_us_or_them: false,
        }
    }

    fn community_refusal(verdict: ScreeningVerdict) -> Option<RefusalReason> {
        match verdict {
            ScreeningVerdict::Refuse(reason) => Some(reason),
            ScreeningVerdict::Admit { .. } => None,
        }
    }

    #[test]
    fn healthy_community_is_admitted_as_third_party_contact() {
        let verdict = screen_community_candidate(&community(), TargetDiscoveryPolicy::default());
        match verdict {
            ScreeningVerdict::Admit { class, confidence } => {
                assert_eq!(class, ActionClass::ThirdParty);
                assert!(confidence.basis_points() > 3_000);
            }
            ScreeningVerdict::Refuse(reason) => panic!("unexpected refusal: {reason:?}"),
        }
    }

    #[test]
    fn community_without_evidence_is_refused() {
        let snapshot = CommunityCandidateSnapshot {
            has_evidence: false,
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::EvidenceMissing)
        );
    }

    #[test]
    fn community_that_bans_self_promotion_is_refused_at_any_size() {
        let snapshot = CommunityCandidateSnapshot {
            self_promo_ratio_percent: Some(0),
            member_count: Some(500_000),
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::PoorFit)
        );
    }

    #[test]
    fn large_community_with_no_activity_is_refused_as_implausible() {
        let snapshot = CommunityCandidateSnapshot {
            member_count: Some(200_000),
            activity_basis_points: Some(10),
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::ImplausibleEngagement)
        );
    }

    #[test]
    fn community_below_the_size_floor_is_refused() {
        let snapshot = CommunityCandidateSnapshot {
            member_count: Some(40),
            activity_basis_points: Some(2_000),
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::TooSmall)
        );
    }

    #[test]
    fn community_we_or_they_refused_is_refused() {
        let snapshot = CommunityCandidateSnapshot {
            refused_by_us_or_them: true,
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::PoorFit)
        );
    }

    #[test]
    fn unmeasured_community_is_admitted_at_the_confidence_floor() {
        // Absent is not suspicious. A community discovery has not measured
        // is a real candidate the causal model has to judge, not a refusal —
        // but it earns no confidence it has not evidenced.
        let snapshot = CommunityCandidateSnapshot {
            has_evidence: true,
            ..CommunityCandidateSnapshot::default()
        };
        match screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()) {
            ScreeningVerdict::Admit { confidence, .. } => {
                assert_eq!(confidence.basis_points(), 3_000);
            }
            ScreeningVerdict::Refuse(reason) => panic!("unexpected refusal: {reason:?}"),
        }
    }

    #[test]
    fn selling_placement_outranks_a_size_problem() {
        // Order matters: a paid-placement community that is also tiny must
        // refuse for the permanent reason, not the policy-threshold one.
        let snapshot = CommunityCandidateSnapshot {
            sells_placement: true,
            member_count: Some(10),
            ..community()
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::SellsPlacement)
        );
    }
}
