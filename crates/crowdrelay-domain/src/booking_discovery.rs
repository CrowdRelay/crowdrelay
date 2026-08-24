//! Venue and promoter discovery — the booking pipeline's supply.
//!
//! The negotiation machinery is complete and starves: `viryaos_booking_targets`
//! has been operator-upsert-only since 0033, which made zero venues a stable
//! state rather than a problem the agent could notice. This module gives the
//! booking pipeline what Phase 9 gave the pitcher: candidates arrive from an
//! adapter sweep or an operator import, are screened ON WRITE against a closed
//! refusal set, and only an email route can be promoted into a real target.
//!
//! The rule that keeps it clean is Phase 9's, restated for stages:
//! **a candidate is not a target.** A festival application form is a real
//! published route nobody has answered yet. Promotion is a human confirmation,
//! and the promotion never resets a relationship that already exists.

use serde::{Deserialize, Serialize};

/// How the submission route was published.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Email,
    SubmissionForm,
    Handle,
}

impl RouteKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::SubmissionForm => "submission_form",
            Self::Handle => "handle",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "email" => Some(Self::Email),
            "submission_form" => Some(Self::SubmissionForm),
            "handle" => Some(Self::Handle),
            _ => None,
        }
    }
}

/// Why a candidate was refused. A closed set, like the outreach one: the first
/// three are permanent regardless of policy — inferring a contact address,
/// shipping no evidence, or asking the band to pay to play are facts about the
/// candidate, not thresholds somebody could widen. `poor_fit` moves with the
/// policy floor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingRefusalReason {
    RouteInferred,
    EvidenceMissing,
    PaidToApply,
    PoorFit,
}

impl BookingRefusalReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RouteInferred => "route_inferred",
            Self::EvidenceMissing => "evidence_missing",
            Self::PaidToApply => "paid_to_apply",
            Self::PoorFit => "poor_fit",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "route_inferred" => Some(Self::RouteInferred),
            "evidence_missing" => Some(Self::EvidenceMissing),
            "paid_to_apply" => Some(Self::PaidToApply),
            "poor_fit" => Some(Self::PoorFit),
            _ => None,
        }
    }

    /// Refusals that survive any future re-screen. A pay-to-play festival does
    /// not become acceptable when the fit floor drops.
    #[must_use]
    pub const fn is_permanent(self) -> bool {
        !matches!(self, Self::PoorFit)
    }
}

/// Tunable screening knobs. Stored as the booking context's config, editable
/// through the same authority write as every other knob.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BookingDiscoveryPolicy {
    pub minimum_fit_basis_points: u16,
    /// Festivals book a year out and sell out regardless; capacity evidence is
    /// nice, not required. Venues benefit more, but a missing number refuses
    /// nothing by itself — the pitch carries the risk either way.
    pub require_capacity_evidence: bool,
}

impl Default for BookingDiscoveryPolicy {
    fn default() -> Self {
        Self {
            minimum_fit_basis_points: 6_000,
            require_capacity_evidence: false,
        }
    }
}

/// One discovered prospect, exactly as reported. The adapter's claims travel
/// verbatim; this module judges them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingCandidateInput {
    pub kind: crate::booking::BookingTargetKind,
    pub display_name: String,
    /// Required for eventual promotion: booking targets are city-scoped, so a
    /// prospect without a place cannot become one.
    pub city_slug: Option<String>,
    pub route_kind: RouteKind,
    pub route_value: String,
    pub source: String,
    pub source_reference: String,
    /// Verbatim published text the route was read out of. Bounded upstream.
    pub evidence: Option<String>,
    pub fit_basis_points: u16,
    /// The adapter's raw signal, not a verdict — CrowdRelay screens on it.
    pub paid_to_apply: bool,
    pub route_is_published: bool,
    pub capacity: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Screening {
    Admit,
    Refuse(BookingRefusalReason),
}

/// Screens one candidate. Order matters and is deliberate: permanent refusals
/// fire before policy-dependent ones, so dropping the fit floor cannot
/// resurrect a pay-to-play festival or launder an inferred address.
#[must_use]
pub fn screen_candidate(
    input: &BookingCandidateInput,
    policy: BookingDiscoveryPolicy,
) -> Screening {
    if !input.route_is_published {
        return Screening::Refuse(BookingRefusalReason::RouteInferred);
    }
    if input.paid_to_apply {
        return Screening::Refuse(BookingRefusalReason::PaidToApply);
    }
    if input
        .evidence
        .as_deref()
        .is_none_or(|evidence| evidence.trim().is_empty())
    {
        return Screening::Refuse(BookingRefusalReason::EvidenceMissing);
    }
    if input.fit_basis_points < policy.minimum_fit_basis_points {
        return Screening::Refuse(BookingRefusalReason::PoorFit);
    }
    if policy.require_capacity_evidence && input.capacity.is_none() {
        return Screening::Refuse(BookingRefusalReason::PoorFit);
    }
    Screening::Admit
}

/// Whether this admitted route can be promoted into a bookable target at all.
/// Only an email route promotes: a form or a handle is a real published route
/// with nowhere for CrowdRelay's outreach to go yet, exactly like Phase 9's
/// rule for playlists.
#[must_use]
pub const fn promotes_to_target(route_kind: RouteKind) -> bool {
    matches!(route_kind, RouteKind::Email)
}

/// What the supply rule needs: how many bookable targets exist, and when the
/// agent last asked for more.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BookingSupplySnapshot {
    /// Active targets the outreach machinery could actually contact today.
    pub active_eligible_targets: u32,
    /// Hours since this workspace last produced a supply request. `None` when
    /// it never has.
    pub hours_since_last_request: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BookingSupplyPolicy {
    /// Below this many contactable targets the pipeline is starving.
    pub supply_floor: u32,
    /// One request per cooldown, whatever the count says — the adapter needs
    /// time to sweep, and re-asking hourly would spend noise on the same gap.
    pub cooldown_hours: u32,
    pub requested_count: u16,
}

impl Default for BookingSupplyPolicy {
    fn default() -> Self {
        Self {
            // Five venues/promoters is the smallest tour leg worth planning;
            // below it the negotiation machinery idles behind an empty queue.
            supply_floor: 5,
            cooldown_hours: 168,
            requested_count: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingSupplyDecision {
    Hold(BookingSupplyHoldReason),
    Request { requested_count: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingSupplyHoldReason {
    EnoughTargets,
    OnCooldown,
}

/// Decides whether the agent should ask for more venue/promoter supply.
///
/// Discovery reads published data, contacts nobody and costs nothing, which
/// is why the requesting action is `first_party_reversible`: every judgement
/// about who may be approached stays in screening and in the third-party
/// ceiling where it belongs.
#[must_use]
pub fn evaluate_booking_supply(
    snapshot: BookingSupplySnapshot,
    policy: BookingSupplyPolicy,
) -> BookingSupplyDecision {
    if snapshot.active_eligible_targets >= policy.supply_floor {
        return BookingSupplyDecision::Hold(BookingSupplyHoldReason::EnoughTargets);
    }
    if snapshot
        .hours_since_last_request
        .is_some_and(|hours| hours < policy.cooldown_hours)
    {
        return BookingSupplyDecision::Hold(BookingSupplyHoldReason::OnCooldown);
    }
    BookingSupplyDecision::Request {
        requested_count: policy.requested_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::booking::BookingTargetKind;

    fn input() -> BookingCandidateInput {
        BookingCandidateInput {
            kind: BookingTargetKind::Venue,
            display_name: "Klub Transfuzja".into(),
            city_slug: Some("wroclaw".into()),
            route_kind: RouteKind::Email,
            route_value: "booking@transfuzja.example".into(),
            source: "venue_site".into(),
            source_reference: "https://transfuzja.example/contact".into(),
            evidence: Some("Zgloszenia koncertow: booking@transfuzja.example".into()),
            fit_basis_points: 8_000,
            paid_to_apply: false,
            route_is_published: true,
            capacity: Some(350),
        }
    }

    #[test]
    fn a_published_email_route_with_evidence_is_admitted() {
        assert_eq!(
            screen_candidate(&input(), BookingDiscoveryPolicy::default()),
            Screening::Admit
        );
    }

    #[test]
    fn permanent_refusals_fire_before_the_policy_floor_could_save_them() {
        let mut paid = input();
        paid.paid_to_apply = true;
        assert_eq!(
            screen_candidate(&paid, BookingDiscoveryPolicy::default()),
            Screening::Refuse(BookingRefusalReason::PaidToApply)
        );

        let mut inferred = input();
        inferred.route_is_published = false;
        assert_eq!(
            screen_candidate(&inferred, BookingDiscoveryPolicy::default()),
            Screening::Refuse(BookingRefusalReason::RouteInferred)
        );

        // And even a below-floor fit stays refused while the above-floor ones
        // pass, proving the floor still bites.
        let mut weak = input();
        weak.fit_basis_points = 1_000;
        assert_eq!(
            screen_candidate(&weak, BookingDiscoveryPolicy::default()),
            Screening::Refuse(BookingRefusalReason::PoorFit)
        );
    }

    #[test]
    fn evidence_missing_is_refused_even_when_everything_else_shines() {
        let mut bare = input();
        bare.evidence = None;
        assert_eq!(
            screen_candidate(&bare, BookingDiscoveryPolicy::default()),
            Screening::Refuse(BookingRefusalReason::EvidenceMissing)
        );
    }

    #[test]
    fn only_an_email_route_promotes_to_a_bookable_target() {
        assert!(promotes_to_target(RouteKind::Email));
        assert!(!promotes_to_target(RouteKind::SubmissionForm));
        assert!(!promotes_to_target(RouteKind::Handle));
    }

    #[test]
    fn supply_requests_only_when_starving_and_past_cooldown() {
        let policy = BookingSupplyPolicy::default();
        let fed = BookingSupplySnapshot {
            active_eligible_targets: policy.supply_floor,
            hours_since_last_request: None,
        };
        assert_eq!(
            evaluate_booking_supply(fed, policy),
            BookingSupplyDecision::Hold(BookingSupplyHoldReason::EnoughTargets)
        );
        let starving = BookingSupplySnapshot {
            active_eligible_targets: policy.supply_floor - 1,
            hours_since_last_request: Some(policy.cooldown_hours - 1),
        };
        assert_eq!(
            evaluate_booking_supply(starving, policy),
            BookingSupplyDecision::Hold(BookingSupplyHoldReason::OnCooldown)
        );
        let due = BookingSupplySnapshot {
            active_eligible_targets: policy.supply_floor - 1,
            hours_since_last_request: Some(policy.cooldown_hours),
        };
        assert_eq!(
            evaluate_booking_supply(due, policy),
            BookingSupplyDecision::Request {
                requested_count: policy.requested_count
            }
        );
    }

    #[test]
    fn refusal_reasons_round_trip_and_permanence_holds() {
        for reason in [
            BookingRefusalReason::RouteInferred,
            BookingRefusalReason::EvidenceMissing,
            BookingRefusalReason::PaidToApply,
            BookingRefusalReason::PoorFit,
        ] {
            assert_eq!(BookingRefusalReason::parse(reason.as_str()), Some(reason));
        }
        assert!(BookingRefusalReason::PaidToApply.is_permanent());
        assert!(!BookingRefusalReason::PoorFit.is_permanent());
        for kind in [
            RouteKind::Email,
            RouteKind::SubmissionForm,
            RouteKind::Handle,
        ] {
            assert_eq!(RouteKind::parse(kind.as_str()), Some(kind));
        }
    }
}
