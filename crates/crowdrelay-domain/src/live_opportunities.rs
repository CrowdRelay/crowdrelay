//! Festival, showcase, review-contest and support-slot opportunity context.
//!
//! Discovery adapters report facts. The domain scores economic/reputational fit
//! and decides whether an application may be sent automatically. Paid,
//! contractual or exclusive applications are never auto-submitted.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{TeamOpportunityId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveOpportunityKind {
    Festival,
    Showcase,
    ReviewContest,
    SupportSlot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveTravelBand {
    Poland,
    EastGermany,
    CzechiaSlovakia,
    FarShot,
}

impl LiveTravelBand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Poland => "poland",
            Self::EastGermany => "east_germany",
            Self::CzechiaSlovakia => "czechia_slovakia",
            Self::FarShot => "far_shot",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LiveOpportunitySnapshot {
    pub opportunity_id: TeamOpportunityId,
    pub kind: LiveOpportunityKind,
    pub active: bool,
    pub verified_destination: bool,
    pub auto_submission_capable: bool,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub evidence_confidence: Confidence,
    pub expected_fee_minor: i64,
    pub estimated_cost_minor: i64,
    pub application_fee_minor: i64,
    pub requires_contract: bool,
    pub exclusive: bool,
    pub deadline: Option<OffsetDateTime>,
    /// Calendar/travel facts and manager policy are folded into the snapshot by
    /// the infrastructure adapter so the domain owns the actual booking gate.
    pub event_starts_at: Option<OffsetDateTime>,
    pub travel_band: Option<LiveTravelBand>,
    /// True when [`crate::tour_economics`] costed this trip from real inputs
    /// rather than the number falling back to whatever was typed in.
    ///
    /// An uncosted show can still be prepared for a human — that is the whole
    /// point of preparing — but it can never be submitted automatically. The
    /// band does not commit to a 500 km drive on an unverified cost.
    pub costed_from_logistics: bool,
    pub committed_shows_year: u16,
    pub annual_target: u16,
    pub annual_stretch: u16,
    pub stretch_minimum_score_basis_points: u16,
    pub far_shot_minimum_score_basis_points: u16,
    pub prefer_weekend_one_shots: bool,
    pub already_applied: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveOpportunityDiscovery<'a> {
    pub title: &'a str,
    pub summary: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveOpportunityDiscoveryAssessment {
    pub kind: LiveOpportunityKind,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub confidence: Confidence,
}

#[must_use]
pub fn evaluate_live_opportunity_discovery(
    discovery: &LiveOpportunityDiscovery<'_>,
) -> Option<LiveOpportunityDiscoveryAssessment> {
    let text = format!("{} {}", discovery.title, discovery.summary).to_lowercase();
    let kind = if text.contains("festival") {
        LiveOpportunityKind::Festival
    } else if text.contains("showcase") || text.contains("przegląd") {
        LiveOpportunityKind::Showcase
    } else if text.contains("support") {
        LiveOpportunityKind::SupportSlot
    } else if text.contains("review") || text.contains("contest") || text.contains("konkurs") {
        LiveOpportunityKind::ReviewContest
    } else {
        return None;
    };
    Some(LiveOpportunityDiscoveryAssessment {
        kind,
        fit_basis_points: 7_000,
        reputation_basis_points: 5_000,
        confidence: Confidence::saturating_from_basis_points(6_500),
    })
}

/// Operator-owned live calendar guardrails. Google Sheets may be the editing
/// surface, but this validated value is persisted in CrowdRelay before use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingManagerPolicy {
    pub annual_target: u16,
    pub annual_stretch: u16,
    pub stretch_minimum_score_basis_points: u16,
    pub prefer_weekend_one_shots: bool,
    pub priority_markets: Vec<String>,
    pub far_shot_minimum_score_basis_points: u16,
}

impl Default for BookingManagerPolicy {
    fn default() -> Self {
        Self {
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: true,
            priority_markets: vec!["PL".into(), "DE-EAST".into(), "CZ".into(), "SK".into()],
            far_shot_minimum_score_basis_points: 9_000,
        }
    }
}

impl BookingManagerPolicy {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.annual_target > 0
            && self.annual_target <= 60
            && self.annual_stretch >= self.annual_target
            && self.annual_stretch <= 60
            && self.stretch_minimum_score_basis_points <= 10_000
            && self.far_shot_minimum_score_basis_points <= 10_000
            && !self.priority_markets.is_empty()
            && self.priority_markets.len() <= 12
            && self.priority_markets.iter().all(|market| {
                !market.trim().is_empty()
                    && market.len() <= 24
                    && market.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LiveOpportunityPolicy {
    pub minimum_score: u16,
    pub minimum_auto_score: u16,
    pub max_auto_application_fee_minor: i64,
    pub max_auto_negative_margin_minor: i64,
}

impl Default for LiveOpportunityPolicy {
    fn default() -> Self {
        Self {
            minimum_score: 65,
            minimum_auto_score: 82,
            max_auto_application_fee_minor: 0,
            max_auto_negative_margin_minor: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveOpportunityDecision {
    Hold,
    PrepareForApproval { score: u16, confidence: Confidence },
    SubmitAutomatically { score: u16, confidence: Confidence },
}

#[must_use]
pub fn live_opportunity_score(snapshot: LiveOpportunitySnapshot) -> u16 {
    let fit = u32::from(snapshot.fit_basis_points) * 45 / 10_000;
    let reputation = u32::from(snapshot.reputation_basis_points) * 25 / 10_000;
    let confidence = u32::from(snapshot.evidence_confidence.basis_points()) * 20 / 10_000;
    let economics = economics_score(snapshot);
    u16::try_from((fit + reputation + confidence + economics).min(100)).unwrap_or(100)
}

#[must_use]
pub fn evaluate_live_opportunity(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    now: OffsetDateTime,
) -> LiveOpportunityDecision {
    if !valid_policy(policy)
        || !snapshot.active
        || !snapshot.verified_destination
        || snapshot.already_applied
        || snapshot.expected_fee_minor < 0
        || snapshot.estimated_cost_minor < 0
        || snapshot.application_fee_minor < 0
        || snapshot.deadline.is_some_and(|deadline| deadline <= now)
    {
        return LiveOpportunityDecision::Hold;
    }

    let score = live_opportunity_score(snapshot);
    if score < policy.minimum_score || !passes_show_budget(snapshot, score) {
        return LiveOpportunityDecision::Hold;
    }

    let confidence = Confidence::saturating_from_basis_points(
        7_500_u16
            .saturating_add(
                score
                    .saturating_sub(policy.minimum_score)
                    .saturating_mul(100)
                    .min(2_500),
            )
            .min(10_000),
    );
    if may_auto_submit(snapshot, policy, score) {
        LiveOpportunityDecision::SubmitAutomatically { score, confidence }
    } else {
        LiveOpportunityDecision::PrepareForApproval { score, confidence }
    }
}

#[must_use]
fn economics_score(snapshot: LiveOpportunitySnapshot) -> u32 {
    if !snapshot.costed_from_logistics {
        // No costed trip, no economics points. Scoring an unknown cost as
        // break-even is how an uncosted gig outranks a costed profitable one.
        return 0;
    }
    let margin = net_margin_minor(snapshot);
    if margin >= 100_000 {
        10
    } else if margin >= 0 {
        8
    } else if margin >= -30_000 {
        4
    } else {
        0
    }
}

#[must_use]
fn may_auto_submit(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    score: u16,
) -> bool {
    score >= policy.minimum_auto_score
        && schedule_allows_auto_submit(snapshot, score)
        && snapshot.auto_submission_capable
        && snapshot.costed_from_logistics
        && snapshot.application_fee_minor <= policy.max_auto_application_fee_minor
        && net_margin_minor(snapshot) >= -policy.max_auto_negative_margin_minor
        && !snapshot.requires_contract
        && !snapshot.exclusive
}

#[must_use]
fn passes_show_budget(snapshot: LiveOpportunitySnapshot, score: u16) -> bool {
    if snapshot.annual_target == 0
        || snapshot.annual_stretch < snapshot.annual_target
        || snapshot.committed_shows_year >= snapshot.annual_stretch
    {
        return false;
    }
    let score_basis_points = score.saturating_mul(100);
    if snapshot.committed_shows_year >= snapshot.annual_target
        && score_basis_points < snapshot.stretch_minimum_score_basis_points
    {
        return false;
    }
    if matches!(snapshot.travel_band, Some(LiveTravelBand::FarShot))
        && score_basis_points < snapshot.far_shot_minimum_score_basis_points
    {
        return false;
    }
    true
}

#[must_use]
fn schedule_allows_auto_submit(snapshot: LiveOpportunitySnapshot, score: u16) -> bool {
    let Some(starts_at) = snapshot.event_starts_at else {
        // Unknown calendar date is safe to prepare, but not safe to commit to automatically.
        return false;
    };
    if !snapshot.prefer_weekend_one_shots {
        return true;
    }
    let weekday = starts_at.weekday();
    let weekend = matches!(
        weekday,
        time::Weekday::Friday | time::Weekday::Saturday | time::Weekday::Sunday
    );
    weekend || score.saturating_mul(100) >= snapshot.stretch_minimum_score_basis_points
}

#[must_use]
const fn net_margin_minor(snapshot: LiveOpportunitySnapshot) -> i64 {
    snapshot
        .expected_fee_minor
        .saturating_sub(snapshot.estimated_cost_minor)
        .saturating_sub(snapshot.application_fee_minor)
}

#[must_use]
const fn valid_policy(policy: LiveOpportunityPolicy) -> bool {
    policy.minimum_score <= policy.minimum_auto_score
        && policy.minimum_auto_score <= 100
        && policy.max_auto_application_fee_minor >= 0
        && policy.max_auto_negative_margin_minor >= 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot() -> LiveOpportunitySnapshot {
        LiveOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            kind: LiveOpportunityKind::Festival,
            active: true,
            verified_destination: true,
            auto_submission_capable: true,
            costed_from_logistics: true,
            fit_basis_points: 9_500,
            reputation_basis_points: 9_000,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            expected_fee_minor: 400_000,
            estimated_cost_minor: 100_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: Some(now() + Duration::days(20)),
            event_starts_at: Some(now() + Duration::days(24)),
            travel_band: Some(LiveTravelBand::Poland),
            committed_shows_year: 8,
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            far_shot_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: false,
            already_applied: false,
        }
    }

    #[test]
    fn excellent_free_reversible_opportunity_may_auto_submit() {
        assert!(matches!(
            evaluate_live_opportunity(snapshot(), LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::SubmitAutomatically { .. }
        ));
    }

    #[test]
    fn paid_or_contractual_application_requires_approval() {
        let mut candidate = snapshot();
        candidate.application_fee_minor = 5_000;
        candidate.requires_contract = true;
        assert!(matches!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::PrepareForApproval { .. }
        ));
    }

    #[test]
    fn annual_target_only_allows_exceptional_stretch_shows() {
        let mut candidate = snapshot();
        candidate.committed_shows_year = 15;
        candidate.fit_basis_points = 7_000;
        candidate.reputation_basis_points = 6_000;
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn far_shot_requires_exceptional_score() {
        let mut candidate = snapshot();
        candidate.travel_band = Some(LiveTravelBand::FarShot);
        candidate.fit_basis_points = 7_500;
        candidate.reputation_basis_points = 7_500;
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn discovery_ignores_unrelated_public_text() {
        assert!(
            evaluate_live_opportunity_discovery(&LiveOpportunityDiscovery {
                title: "Newsletter",
                summary: "General music news",
            })
            .is_none()
        );
    }

    #[test]
    fn weak_or_expired_opportunity_is_ignored() {
        let mut candidate = snapshot();
        candidate.fit_basis_points = 1_000;
        candidate.deadline = Some(now() - Duration::hours(1));
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn an_uncosted_show_is_prepared_for_a_human_but_never_submitted_alone() {
        // The band does not commit to a long drive on a cost nobody computed.
        let uncosted = LiveOpportunitySnapshot {
            costed_from_logistics: false,
            ..snapshot()
        };
        assert!(matches!(
            evaluate_live_opportunity(uncosted, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::PrepareForApproval { .. } | LiveOpportunityDecision::Hold
        ));
        assert!(!may_auto_submit(
            uncosted,
            LiveOpportunityPolicy::default(),
            100
        ));
    }

    #[test]
    fn an_uncosted_show_scores_no_economics_points() {
        // Otherwise an unknown cost reads as break-even, and an uncosted gig
        // outranks a costed profitable one.
        let costed = snapshot();
        let uncosted = LiveOpportunitySnapshot {
            costed_from_logistics: false,
            ..costed
        };
        assert!(live_opportunity_score(uncosted) < live_opportunity_score(costed));
    }
}
