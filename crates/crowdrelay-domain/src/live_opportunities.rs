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
    if score < policy.minimum_score {
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
        && snapshot.auto_submission_capable
        && snapshot.application_fee_minor <= policy.max_auto_application_fee_minor
        && net_margin_minor(snapshot) >= -policy.max_auto_negative_margin_minor
        && !snapshot.requires_contract
        && !snapshot.exclusive
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
            fit_basis_points: 9_500,
            reputation_basis_points: 9_000,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            expected_fee_minor: 400_000,
            estimated_cost_minor: 100_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: Some(now() + Duration::days(20)),
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
}
