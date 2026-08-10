//! Deterministic paid-promotion budget optimization.
//!
//! The bounded context consumes a provider-agnostic performance snapshot and
//! returns at most one bounded budget adjustment. It never calls an ad network,
//! reads configuration from infrastructure, or relies on an LLM/opaque model.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{PromotionCampaignId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PromotionPerformanceSnapshot {
    pub campaign_id: PromotionCampaignId,
    pub current_daily_budget_minor: i64,
    pub minimum_daily_budget_minor: i64,
    pub maximum_daily_budget_minor: i64,
    pub spend_last_7d_minor: i64,
    pub attributed_revenue_last_7d_minor: i64,
    /// Aggregate active daily budget across the workspace in this currency.
    pub workspace_daily_budget_minor: i64,
    /// Aggregate provider-reported month-to-date spend in this currency.
    pub workspace_spend_month_to_date_minor: i64,
    /// Operator-owned hard cap. Required for autonomous budget increases.
    pub workspace_maximum_daily_budget_minor: Option<i64>,
    /// Operator-owned hard cap. Required for autonomous budget increases.
    pub workspace_maximum_monthly_spend_minor: Option<i64>,
    pub days_to_event: u32,
    pub active: bool,
    pub last_budget_change_at: Option<OffsetDateTime>,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromotionBudgetPolicy {
    /// Minimum seven-day spend before performance is trusted enough to mutate.
    pub minimum_observation_spend_minor: i64,
    /// Increase budget when attributed revenue / spend reaches this threshold.
    pub increase_roas_basis_points: u32,
    /// Decrease budget when ROAS falls to or below this threshold.
    pub decrease_roas_basis_points: u32,
    /// Maximum relative change per decision, expressed in basis points.
    pub maximum_change_basis_points: u16,
    pub cooldown_hours: u32,
    pub minimum_days_to_event: u32,
}

impl Default for PromotionBudgetPolicy {
    fn default() -> Self {
        Self {
            minimum_observation_spend_minor: 2_000,
            increase_roas_basis_points: 20_000,
            decrease_roas_basis_points: 8_000,
            maximum_change_basis_points: 2_000,
            cooldown_hours: 24,
            minimum_days_to_event: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionBudgetDecision {
    Hold(PromotionBudgetHoldReason),
    Adjust {
        from_minor: i64,
        to_minor: i64,
        direction: BudgetDirection,
        roas_basis_points: u32,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetDirection {
    Increase,
    Decrease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionBudgetHoldReason {
    Inactive,
    StaleSnapshot,
    InvalidSnapshot,
    InvalidPolicy,
    TooCloseToEvent,
    InsufficientObservation,
    Cooldown,
    PerformanceInBand,
    MissingWorkspaceGuardrail,
    WorkspaceBudgetCap,
    AtBound,
}

#[must_use]
pub fn evaluate_promotion_budget(
    snapshot: PromotionPerformanceSnapshot,
    policy: PromotionBudgetPolicy,
    now: OffsetDateTime,
) -> PromotionBudgetDecision {
    if !policy_is_valid(policy) {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::InvalidPolicy);
    }
    if !snapshot_is_valid(snapshot, now) {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::InvalidSnapshot);
    }
    if !snapshot.active {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::Inactive);
    }
    if snapshot.expires_at <= now {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::StaleSnapshot);
    }
    if snapshot.days_to_event < policy.minimum_days_to_event {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::TooCloseToEvent);
    }
    if snapshot.spend_last_7d_minor < policy.minimum_observation_spend_minor {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::InsufficientObservation);
    }
    if snapshot.last_budget_change_at.is_some_and(|changed_at| {
        changed_at
            .checked_add(Duration::hours(i64::from(policy.cooldown_hours)))
            .is_some_and(|next_allowed| next_allowed > now)
    }) {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::Cooldown);
    }

    let roas = roas_basis_points(
        snapshot.attributed_revenue_last_7d_minor,
        snapshot.spend_last_7d_minor,
    );
    if roas >= policy.increase_roas_basis_points {
        if snapshot.workspace_maximum_daily_budget_minor.is_none()
            || snapshot.workspace_maximum_monthly_spend_minor.is_none()
        {
            return PromotionBudgetDecision::Hold(
                PromotionBudgetHoldReason::MissingWorkspaceGuardrail,
            );
        }
        return adjustment(snapshot, policy, roas, BudgetDirection::Increase);
    }
    if roas <= policy.decrease_roas_basis_points {
        return adjustment(snapshot, policy, roas, BudgetDirection::Decrease);
    }
    PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::PerformanceInBand)
}

fn adjustment(
    snapshot: PromotionPerformanceSnapshot,
    policy: PromotionBudgetPolicy,
    roas: u32,
    direction: BudgetDirection,
) -> PromotionBudgetDecision {
    let change = mul_basis_points_ceil(
        snapshot.current_daily_budget_minor,
        policy.maximum_change_basis_points,
    )
    .max(1);
    let proposed = match direction {
        BudgetDirection::Increase => snapshot
            .current_daily_budget_minor
            .saturating_add(change)
            .min(snapshot.maximum_daily_budget_minor),
        BudgetDirection::Decrease => snapshot
            .current_daily_budget_minor
            .saturating_sub(change)
            .max(snapshot.minimum_daily_budget_minor),
    };
    if proposed == snapshot.current_daily_budget_minor {
        return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::AtBound);
    }

    if matches!(direction, BudgetDirection::Increase) {
        let delta = proposed.saturating_sub(snapshot.current_daily_budget_minor);
        let projected_workspace_daily = snapshot.workspace_daily_budget_minor.saturating_add(delta);
        let daily_cap = snapshot
            .workspace_maximum_daily_budget_minor
            .unwrap_or_default();
        let monthly_cap = snapshot
            .workspace_maximum_monthly_spend_minor
            .unwrap_or_default();
        if projected_workspace_daily > daily_cap
            || snapshot.workspace_spend_month_to_date_minor >= monthly_cap
        {
            return PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::WorkspaceBudgetCap);
        }
    }

    let sample_bonus = (snapshot.spend_last_7d_minor / 1_000).clamp(0, 1_000) as u16;
    let distance_bonus = match direction {
        BudgetDirection::Increase => roas.saturating_sub(policy.increase_roas_basis_points),
        BudgetDirection::Decrease => policy.decrease_roas_basis_points.saturating_sub(roas),
    }
    .min(1_000) as u16;
    let confidence = Confidence::saturating_from_basis_points(
        8_000_u16
            .saturating_add(sample_bonus)
            .saturating_add(distance_bonus),
    );

    PromotionBudgetDecision::Adjust {
        from_minor: snapshot.current_daily_budget_minor,
        to_minor: proposed,
        direction,
        roas_basis_points: roas,
        confidence,
    }
}

fn snapshot_is_valid(snapshot: PromotionPerformanceSnapshot, now: OffsetDateTime) -> bool {
    snapshot.current_daily_budget_minor > 0
        && snapshot.minimum_daily_budget_minor > 0
        && snapshot.minimum_daily_budget_minor <= snapshot.current_daily_budget_minor
        && snapshot.current_daily_budget_minor <= snapshot.maximum_daily_budget_minor
        && snapshot.spend_last_7d_minor >= 0
        && snapshot.attributed_revenue_last_7d_minor >= 0
        && snapshot.workspace_daily_budget_minor >= snapshot.current_daily_budget_minor
        && snapshot.workspace_spend_month_to_date_minor >= 0
        && snapshot
            .workspace_maximum_daily_budget_minor
            .is_none_or(|value| value > 0)
        && snapshot
            .workspace_maximum_monthly_spend_minor
            .is_none_or(|value| value > 0)
        && snapshot.observed_at <= now
        && snapshot.expires_at > snapshot.observed_at
        && snapshot
            .last_budget_change_at
            .is_none_or(|changed_at| changed_at <= now)
}

fn policy_is_valid(policy: PromotionBudgetPolicy) -> bool {
    policy.minimum_observation_spend_minor > 0
        && policy.decrease_roas_basis_points < policy.increase_roas_basis_points
        && (1..=5_000).contains(&policy.maximum_change_basis_points)
        && policy.cooldown_hours > 0
}

fn roas_basis_points(revenue_minor: i64, spend_minor: i64) -> u32 {
    if spend_minor <= 0 {
        return 0;
    }
    let numerator = i128::from(revenue_minor).saturating_mul(10_000);
    let value = numerator / i128::from(spend_minor);
    u32::try_from(value.clamp(0, i128::from(u32::MAX))).unwrap_or(u32::MAX)
}

fn mul_basis_points_ceil(value: i64, basis_points: u16) -> i64 {
    let product = i128::from(value).saturating_mul(i128::from(basis_points));
    let rounded = product.saturating_add(9_999) / 10_000;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> PromotionPerformanceSnapshot {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        PromotionPerformanceSnapshot {
            campaign_id: PromotionCampaignId::new(),
            current_daily_budget_minor: 10_000,
            minimum_daily_budget_minor: 5_000,
            maximum_daily_budget_minor: 20_000,
            spend_last_7d_minor: 10_000,
            attributed_revenue_last_7d_minor: 25_000,
            workspace_daily_budget_minor: 10_000,
            workspace_spend_month_to_date_minor: 30_000,
            workspace_maximum_daily_budget_minor: Some(50_000),
            workspace_maximum_monthly_spend_minor: Some(200_000),
            days_to_event: 14,
            active: true,
            last_budget_change_at: None,
            observed_at: now - Duration::minutes(5),
            expires_at: now + Duration::hours(2),
        }
    }

    #[test]
    fn strong_roas_increases_budget_by_bounded_step() {
        let data = snapshot();
        let now = data.observed_at + Duration::minutes(5);
        assert!(matches!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Adjust {
                from_minor: 10_000,
                to_minor: 12_000,
                direction: BudgetDirection::Increase,
                ..
            }
        ));
    }

    #[test]
    fn weak_roas_decreases_but_never_below_floor() {
        let mut data = snapshot();
        data.current_daily_budget_minor = 5_500;
        data.attributed_revenue_last_7d_minor = 5_000;
        let now = data.observed_at + Duration::minutes(5);
        assert!(matches!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Adjust {
                to_minor: 5_000,
                direction: BudgetDirection::Decrease,
                ..
            }
        ));
    }

    #[test]
    fn stale_or_tiny_samples_never_change_money() {
        let mut data = snapshot();
        data.spend_last_7d_minor = 100;
        let now = data.observed_at + Duration::minutes(5);
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::InsufficientObservation)
        );
        data.spend_last_7d_minor = 10_000;
        data.expires_at = now;
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::StaleSnapshot)
        );
    }

    #[test]
    fn cooldown_prevents_budget_thrashing() {
        let mut data = snapshot();
        let now = data.observed_at + Duration::minutes(5);
        data.last_budget_change_at = Some(now - Duration::hours(2));
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::Cooldown)
        );
    }

    #[test]
    fn budget_increase_requires_workspace_financial_guardrails() {
        let mut data = snapshot();
        data.workspace_maximum_daily_budget_minor = None;
        let now = data.observed_at + Duration::minutes(5);
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::MissingWorkspaceGuardrail)
        );
    }

    #[test]
    fn aggregate_daily_and_monthly_caps_fail_closed() {
        let mut data = snapshot();
        let now = data.observed_at + Duration::minutes(5);
        data.workspace_daily_budget_minor = 49_000;
        data.workspace_maximum_daily_budget_minor = Some(50_000);
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::WorkspaceBudgetCap)
        );

        data.workspace_daily_budget_minor = 10_000;
        data.workspace_spend_month_to_date_minor = 200_000;
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::WorkspaceBudgetCap)
        );
    }

    #[test]
    fn malformed_bounds_fail_closed() {
        let mut data = snapshot();
        data.minimum_daily_budget_minor = 15_000;
        let now = data.observed_at + Duration::minutes(5);
        assert_eq!(
            evaluate_promotion_budget(data, PromotionBudgetPolicy::default(), now),
            PromotionBudgetDecision::Hold(PromotionBudgetHoldReason::InvalidSnapshot)
        );
    }
}
