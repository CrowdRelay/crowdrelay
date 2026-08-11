//! Funding and grant-opportunity bounded context.
//!
//! The domain may autonomously prepare a structured application package. Final
//! submission is intentionally always an approval action because grants carry
//! legal declarations and financial commitments.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{TeamOpportunityId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FundingOpportunitySnapshot {
    pub opportunity_id: TeamOpportunityId,
    pub active: bool,
    pub eligible: bool,
    pub evidence_confidence: Confidence,
    pub fit_basis_points: u16,
    pub amount_minor: i64,
    pub own_contribution_minor: i64,
    pub deadline: OffsetDateTime,
    pub package_prepared: bool,
    pub submitted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FundingPolicy {
    pub minimum_fit_basis_points: u16,
    pub minimum_amount_minor: i64,
    pub maximum_own_contribution_basis_points: u16,
    pub preparation_lead_days: u32,
}

impl Default for FundingPolicy {
    fn default() -> Self {
        Self {
            minimum_fit_basis_points: 6_500,
            minimum_amount_minor: 200_000,
            maximum_own_contribution_basis_points: 5_000,
            preparation_lead_days: 45,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FundingDecision {
    Hold,
    PreparePackage { confidence: Confidence },
    SubmitForApproval { confidence: Confidence },
}

#[must_use]
pub fn evaluate_funding(
    snapshot: FundingOpportunitySnapshot,
    policy: FundingPolicy,
    now: OffsetDateTime,
) -> FundingDecision {
    if !valid_policy(policy)
        || !snapshot.active
        || !snapshot.eligible
        || snapshot.submitted
        || snapshot.deadline <= now
        || snapshot.amount_minor <= 0
    {
        return FundingDecision::Hold;
    }
    if snapshot.fit_basis_points < policy.minimum_fit_basis_points
        || snapshot.amount_minor < policy.minimum_amount_minor
    {
        return FundingDecision::Hold;
    }
    if now < snapshot.deadline - Duration::days(i64::from(policy.preparation_lead_days)) {
        return FundingDecision::Hold;
    }

    let contribution_basis_points = contribution_basis_points(snapshot);
    if contribution_basis_points > policy.maximum_own_contribution_basis_points {
        return FundingDecision::Hold;
    }

    let confidence = Confidence::saturating_from_basis_points(
        snapshot.evidence_confidence.basis_points().min(9_500),
    );
    if snapshot.package_prepared {
        FundingDecision::SubmitForApproval { confidence }
    } else {
        FundingDecision::PreparePackage { confidence }
    }
}

#[must_use]
const fn valid_policy(policy: FundingPolicy) -> bool {
    policy.minimum_fit_basis_points <= 10_000
        && policy.maximum_own_contribution_basis_points <= 10_000
        && policy.minimum_amount_minor >= 0
        && policy.preparation_lead_days > 0
}

#[must_use]
fn contribution_basis_points(snapshot: FundingOpportunitySnapshot) -> u16 {
    let contribution = i128::from(snapshot.own_contribution_minor.max(0));
    let amount = i128::from(snapshot.amount_minor.max(1));
    let basis_points = contribution
        .saturating_mul(10_000)
        .checked_div(amount)
        .unwrap_or(10_000)
        .clamp(0, 10_000);
    u16::try_from(basis_points).unwrap_or(10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot() -> FundingOpportunitySnapshot {
        FundingOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            active: true,
            eligible: true,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            fit_basis_points: 8_500,
            amount_minor: 2_000_000,
            own_contribution_minor: 200_000,
            deadline: now() + Duration::days(30),
            package_prepared: false,
            submitted: false,
        }
    }

    #[test]
    fn funding_package_is_prepared_automatically_inside_lead_window() {
        assert!(matches!(
            evaluate_funding(snapshot(), FundingPolicy::default(), now()),
            FundingDecision::PreparePackage { .. }
        ));
    }

    #[test]
    fn final_funding_submission_never_becomes_auto_decision() {
        let mut opportunity = snapshot();
        opportunity.package_prepared = true;
        assert!(matches!(
            evaluate_funding(opportunity, FundingPolicy::default(), now()),
            FundingDecision::SubmitForApproval { .. }
        ));
    }

    #[test]
    fn excessive_own_contribution_is_rejected() {
        let mut opportunity = snapshot();
        opportunity.own_contribution_minor = 1_200_000;
        assert_eq!(
            evaluate_funding(opportunity, FundingPolicy::default(), now()),
            FundingDecision::Hold
        );
    }
}
