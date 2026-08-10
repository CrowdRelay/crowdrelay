//! Merchandising bounded context: stock coverage, replenishment and bounded price yield.
//!
//! The domain never purchases stock and never talks to a payment/provider API.
//! It evaluates typed inventory/economics snapshots and emits deterministic intents.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{MerchProductId, MerchVariantId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MerchInventorySnapshot {
    pub variant_id: MerchVariantId,
    pub available_quantity: u32,
    pub sold_last_30d: u32,
    pub reorder_in_flight: bool,
    pub last_reorder_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MerchReorderPolicy {
    pub target_coverage_days: u32,
    pub safety_stock: u32,
    pub minimum_reorder_quantity: u32,
    pub maximum_reorder_quantity: u32,
    pub reorder_cooldown_days: u32,
}

impl Default for MerchReorderPolicy {
    fn default() -> Self {
        Self {
            target_coverage_days: 45,
            safety_stock: 2,
            minimum_reorder_quantity: 4,
            maximum_reorder_quantity: 30,
            reorder_cooldown_days: 14,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchReorderDecision {
    Hold(MerchReorderHoldReason),
    RequestReorder {
        quantity: u32,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchReorderHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    NoDemandHistory,
    CoverageSufficient,
    ReorderAlreadyInFlight,
    CooldownActive,
}

#[must_use]
pub fn evaluate_reorder(
    snapshot: MerchInventorySnapshot,
    policy: MerchReorderPolicy,
    now: OffsetDateTime,
) -> MerchReorderDecision {
    if policy.target_coverage_days == 0
        || policy.minimum_reorder_quantity == 0
        || policy.minimum_reorder_quantity > policy.maximum_reorder_quantity
    {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::InvalidPolicy);
    }
    if snapshot.last_reorder_at.is_some_and(|at| at > now) {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::InvalidSnapshot);
    }
    if snapshot.reorder_in_flight {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::ReorderAlreadyInFlight);
    }
    if snapshot.last_reorder_at.is_some_and(|last_reorder| {
        now - last_reorder < Duration::days(i64::from(policy.reorder_cooldown_days))
    }) {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::CooldownActive);
    }
    if snapshot.sold_last_30d == 0 {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::NoDemandHistory);
    }

    let target = u64::from(snapshot.sold_last_30d)
        .saturating_mul(u64::from(policy.target_coverage_days))
        .saturating_add(29)
        / 30;
    let target = target.saturating_add(u64::from(policy.safety_stock));
    if u64::from(snapshot.available_quantity) >= target {
        return MerchReorderDecision::Hold(MerchReorderHoldReason::CoverageSufficient);
    }

    let shortage = target.saturating_sub(u64::from(snapshot.available_quantity));
    let requested = shortage
        .max(u64::from(policy.minimum_reorder_quantity))
        .min(u64::from(policy.maximum_reorder_quantity));
    let quantity = u32::try_from(requested).unwrap_or(policy.maximum_reorder_quantity);
    let demand_bonus = snapshot.sold_last_30d.min(10).saturating_mul(250) as u16;
    let confidence =
        Confidence::saturating_from_basis_points(7_500_u16.saturating_add(demand_bonus));

    MerchReorderDecision::RequestReorder {
        quantity,
        confidence,
    }
}

/// Product-level economics and demand facts used by bounded merch repricing.
///
/// `minimum_price_minor` and `maximum_price_minor` are explicit operator-owned
/// guardrails. The domain refuses to infer them from demand or from another SKU.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MerchPriceSnapshot {
    pub product_id: MerchProductId,
    pub current_price_minor: u64,
    pub minimum_price_minor: u64,
    pub maximum_price_minor: u64,
    pub unit_cost_minor: Option<u64>,
    pub economics_version: i64,
    pub available_quantity: u32,
    pub sold_last_7d: u32,
    pub sold_last_30d: u32,
    pub last_price_change_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MerchPricePolicy {
    pub price_step_minor: u64,
    pub minimum_sales_30d: u32,
    /// Recent 7d run-rate must exceed the 30d run-rate by this ratio to raise.
    pub increase_velocity_ratio_basis_points: u32,
    /// Recent 7d run-rate at/below this ratio is considered stagnant.
    pub decrease_velocity_ratio_basis_points: u32,
    pub scarce_coverage_days: u32,
    pub excess_coverage_days: u32,
    pub price_cooldown_days: u32,
    pub minimum_gross_margin_basis_points: u32,
}

impl Default for MerchPricePolicy {
    fn default() -> Self {
        Self {
            price_step_minor: 500,
            minimum_sales_30d: 4,
            increase_velocity_ratio_basis_points: 14_000,
            decrease_velocity_ratio_basis_points: 5_000,
            scarce_coverage_days: 21,
            excess_coverage_days: 60,
            price_cooldown_days: 7,
            minimum_gross_margin_basis_points: 4_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchPriceDirection {
    Increase,
    Decrease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchPriceDecision {
    Hold(MerchPriceHoldReason),
    ChangePrice {
        direction: MerchPriceDirection,
        to_minor: u64,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchPriceHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    InsufficientDemandHistory,
    CooldownActive,
    DemandBalanced,
    PriceBoundary,
    MarginGuard,
}

/// Evaluates a deliberately conservative merch price step.
///
/// This is not an elasticity model. Until ViryaOS has enough price/outcome
/// history, the service reacts only to strong velocity + stock-coverage signals
/// and never crosses explicit price/margin guardrails.
#[must_use]
pub fn evaluate_merch_price(
    snapshot: MerchPriceSnapshot,
    policy: MerchPricePolicy,
    now: OffsetDateTime,
) -> MerchPriceDecision {
    if !valid_price_policy(policy) {
        return MerchPriceDecision::Hold(MerchPriceHoldReason::InvalidPolicy);
    }
    if !valid_price_snapshot(snapshot, now) {
        return MerchPriceDecision::Hold(MerchPriceHoldReason::InvalidSnapshot);
    }
    if snapshot.last_price_change_at.is_some_and(|last_change| {
        now - last_change < Duration::days(i64::from(policy.price_cooldown_days))
    }) {
        return MerchPriceDecision::Hold(MerchPriceHoldReason::CooldownActive);
    }
    if snapshot.sold_last_30d < policy.minimum_sales_30d {
        return MerchPriceDecision::Hold(MerchPriceHoldReason::InsufficientDemandHistory);
    }

    let coverage_days = stock_coverage_days(snapshot.available_quantity, snapshot.sold_last_30d);
    let recent_velocity_ratio =
        recent_velocity_ratio_basis_points(snapshot.sold_last_7d, snapshot.sold_last_30d);

    if recent_velocity_ratio >= u64::from(policy.increase_velocity_ratio_basis_points)
        && coverage_days <= u64::from(policy.scarce_coverage_days)
    {
        let target = snapshot
            .current_price_minor
            .saturating_add(policy.price_step_minor);
        if target > snapshot.maximum_price_minor {
            return MerchPriceDecision::Hold(MerchPriceHoldReason::PriceBoundary);
        }
        let sample_bonus = snapshot.sold_last_30d.min(20).saturating_mul(60) as u16;
        let confidence =
            Confidence::saturating_from_basis_points(8_200_u16.saturating_add(sample_bonus));
        return MerchPriceDecision::ChangePrice {
            direction: MerchPriceDirection::Increase,
            to_minor: target,
            confidence,
        };
    }

    if recent_velocity_ratio <= u64::from(policy.decrease_velocity_ratio_basis_points)
        && coverage_days >= u64::from(policy.excess_coverage_days)
    {
        let safe_floor = minimum_safe_price(snapshot, policy);
        if safe_floor > snapshot.current_price_minor {
            return MerchPriceDecision::Hold(MerchPriceHoldReason::MarginGuard);
        }
        let target = snapshot
            .current_price_minor
            .saturating_sub(policy.price_step_minor);
        if target < safe_floor || target == snapshot.current_price_minor {
            return MerchPriceDecision::Hold(MerchPriceHoldReason::PriceBoundary);
        }
        let sample_bonus = snapshot.sold_last_30d.min(20).saturating_mul(40) as u16;
        let confidence =
            Confidence::saturating_from_basis_points(8_000_u16.saturating_add(sample_bonus));
        return MerchPriceDecision::ChangePrice {
            direction: MerchPriceDirection::Decrease,
            to_minor: target,
            confidence,
        };
    }

    MerchPriceDecision::Hold(MerchPriceHoldReason::DemandBalanced)
}

fn valid_price_policy(policy: MerchPricePolicy) -> bool {
    policy.price_step_minor > 0
        && policy.minimum_sales_30d > 0
        && policy.increase_velocity_ratio_basis_points > 10_000
        && policy.decrease_velocity_ratio_basis_points < 10_000
        && policy.scarce_coverage_days > 0
        && policy.excess_coverage_days > policy.scarce_coverage_days
        && policy.price_cooldown_days > 0
        && policy.minimum_gross_margin_basis_points < 10_000
}

fn valid_price_snapshot(snapshot: MerchPriceSnapshot, now: OffsetDateTime) -> bool {
    snapshot.minimum_price_minor <= snapshot.current_price_minor
        && snapshot.current_price_minor <= snapshot.maximum_price_minor
        && snapshot.minimum_price_minor <= snapshot.maximum_price_minor
        && snapshot.sold_last_7d <= snapshot.sold_last_30d
        && snapshot.economics_version > 0
        && snapshot.last_price_change_at.is_none_or(|at| at <= now)
        && snapshot
            .unit_cost_minor
            .is_none_or(|cost| cost <= snapshot.maximum_price_minor)
}

fn stock_coverage_days(available_quantity: u32, sold_last_30d: u32) -> u64 {
    if sold_last_30d == 0 {
        return u64::MAX;
    }
    u64::from(available_quantity)
        .saturating_mul(30)
        .saturating_add(u64::from(sold_last_30d) - 1)
        / u64::from(sold_last_30d)
}

fn recent_velocity_ratio_basis_points(sold_last_7d: u32, sold_last_30d: u32) -> u64 {
    if sold_last_30d == 0 {
        return 0;
    }
    // (sold_7d / 7) / (sold_30d / 30), expressed as basis points.
    u64::from(sold_last_7d)
        .saturating_mul(30)
        .saturating_mul(10_000)
        / u64::from(sold_last_30d).saturating_mul(7)
}

fn minimum_safe_price(snapshot: MerchPriceSnapshot, policy: MerchPricePolicy) -> u64 {
    let Some(unit_cost) = snapshot.unit_cost_minor else {
        return snapshot.minimum_price_minor;
    };
    let retained_basis_points =
        10_000_u64.saturating_sub(u64::from(policy.minimum_gross_margin_basis_points));
    if retained_basis_points == 0 {
        return snapshot.maximum_price_minor.saturating_add(1);
    }
    let cost_floor = unit_cost
        .saturating_mul(10_000)
        .saturating_add(retained_basis_points - 1)
        / retained_basis_points;
    snapshot.minimum_price_minor.max(cost_floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    #[test]
    fn reorder_targets_coverage_plus_safety_stock() {
        let decision = evaluate_reorder(
            MerchInventorySnapshot {
                variant_id: MerchVariantId::new(),
                available_quantity: 3,
                sold_last_30d: 12,
                reorder_in_flight: false,
                last_reorder_at: None,
            },
            MerchReorderPolicy::default(),
            now(),
        );
        assert!(matches!(
            decision,
            MerchReorderDecision::RequestReorder { quantity: 17, .. }
        ));
    }

    #[test]
    fn in_flight_reorder_suppresses_duplicate_request() {
        let decision = evaluate_reorder(
            MerchInventorySnapshot {
                variant_id: MerchVariantId::new(),
                available_quantity: 0,
                sold_last_30d: 30,
                reorder_in_flight: true,
                last_reorder_at: None,
            },
            MerchReorderPolicy::default(),
            now(),
        );
        assert_eq!(
            decision,
            MerchReorderDecision::Hold(MerchReorderHoldReason::ReorderAlreadyInFlight)
        );
    }

    #[test]
    fn recent_reorder_enforces_domain_cooldown() {
        let decision = evaluate_reorder(
            MerchInventorySnapshot {
                variant_id: MerchVariantId::new(),
                available_quantity: 0,
                sold_last_30d: 30,
                reorder_in_flight: false,
                last_reorder_at: Some(now() - Duration::days(3)),
            },
            MerchReorderPolicy::default(),
            now(),
        );
        assert_eq!(
            decision,
            MerchReorderDecision::Hold(MerchReorderHoldReason::CooldownActive)
        );
    }

    #[test]
    fn zero_sales_never_invents_reorder_demand() {
        let decision = evaluate_reorder(
            MerchInventorySnapshot {
                variant_id: MerchVariantId::new(),
                available_quantity: 0,
                sold_last_30d: 0,
                reorder_in_flight: false,
                last_reorder_at: None,
            },
            MerchReorderPolicy::default(),
            now(),
        );
        assert_eq!(
            decision,
            MerchReorderDecision::Hold(MerchReorderHoldReason::NoDemandHistory)
        );
    }

    fn price_snapshot() -> MerchPriceSnapshot {
        MerchPriceSnapshot {
            product_id: MerchProductId::new(),
            current_price_minor: 6_900,
            minimum_price_minor: 5_900,
            maximum_price_minor: 8_900,
            unit_cost_minor: Some(3_000),
            economics_version: 1,
            available_quantity: 4,
            sold_last_7d: 5,
            sold_last_30d: 10,
            last_price_change_at: None,
        }
    }

    #[test]
    fn accelerated_demand_and_scarce_stock_raise_one_bounded_step() {
        let decision = evaluate_merch_price(price_snapshot(), MerchPricePolicy::default(), now());
        assert!(matches!(
            decision,
            MerchPriceDecision::ChangePrice {
                direction: MerchPriceDirection::Increase,
                to_minor: 7_400,
                ..
            }
        ));
    }

    #[test]
    fn stagnant_demand_and_excess_stock_can_lower_one_bounded_step() {
        let mut snapshot = price_snapshot();
        snapshot.available_quantity = 40;
        snapshot.sold_last_7d = 0;
        let decision = evaluate_merch_price(snapshot, MerchPricePolicy::default(), now());
        assert!(matches!(
            decision,
            MerchPriceDecision::ChangePrice {
                direction: MerchPriceDirection::Decrease,
                to_minor: 6_400,
                ..
            }
        ));
    }

    #[test]
    fn margin_floor_prevents_discount_below_safe_economics() {
        let mut snapshot = price_snapshot();
        snapshot.current_price_minor = 5_900;
        snapshot.minimum_price_minor = 4_000;
        snapshot.unit_cost_minor = Some(3_500);
        snapshot.available_quantity = 40;
        snapshot.sold_last_7d = 0;
        let decision = evaluate_merch_price(snapshot, MerchPricePolicy::default(), now());
        assert_eq!(
            decision,
            MerchPriceDecision::Hold(MerchPriceHoldReason::PriceBoundary)
        );
    }

    #[test]
    fn recent_price_change_suppresses_churn() {
        let mut snapshot = price_snapshot();
        snapshot.last_price_change_at = Some(now() - Duration::days(2));
        let decision = evaluate_merch_price(snapshot, MerchPricePolicy::default(), now());
        assert_eq!(
            decision,
            MerchPriceDecision::Hold(MerchPriceHoldReason::CooldownActive)
        );
    }

    #[test]
    fn insufficient_sales_never_invents_price_signal() {
        let mut snapshot = price_snapshot();
        snapshot.sold_last_30d = 2;
        snapshot.sold_last_7d = 2;
        let decision = evaluate_merch_price(snapshot, MerchPricePolicy::default(), now());
        assert_eq!(
            decision,
            MerchPriceDecision::Hold(MerchPriceHoldReason::InsufficientDemandHistory)
        );
    }
}
