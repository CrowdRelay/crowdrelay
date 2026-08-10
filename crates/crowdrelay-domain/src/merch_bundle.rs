//! Merch-bundle bounded capability.
//!
//! The domain uses first-party co-purchase evidence plus explicit product
//! economics. It may propose a bounded bundle price, but never invents costs.

use serde::{Deserialize, Serialize};

use crate::{MerchProductId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MerchBundleSnapshot {
    pub product_a: MerchProductId,
    pub product_b: MerchProductId,
    pub price_a_minor: i64,
    pub price_b_minor: i64,
    pub unit_cost_a_minor: Option<i64>,
    pub unit_cost_b_minor: Option<i64>,
    pub orders_a: u32,
    pub orders_b: u32,
    pub joint_orders: u32,
    pub in_flight: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MerchBundlePolicy {
    pub minimum_joint_orders: u32,
    pub minimum_affinity_basis_points: u16,
    pub discount_basis_points: u16,
    pub minimum_margin_basis_points: u16,
}

impl Default for MerchBundlePolicy {
    fn default() -> Self {
        Self {
            minimum_joint_orders: 4,
            minimum_affinity_basis_points: 2_500,
            discount_basis_points: 1_000,
            minimum_margin_basis_points: 2_500,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchBundleDecision {
    Hold(MerchBundleHoldReason),
    Recommend {
        bundle_price_minor: i64,
        affinity_basis_points: u16,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerchBundleHoldReason {
    InvalidSnapshot,
    MissingEconomics,
    InsufficientEvidence,
    InFlight,
    MarginFloor,
}

#[must_use]
pub fn evaluate_merch_bundle(
    snapshot: MerchBundleSnapshot,
    policy: MerchBundlePolicy,
) -> MerchBundleDecision {
    if snapshot.product_a == snapshot.product_b
        || snapshot.price_a_minor <= 0
        || snapshot.price_b_minor <= 0
        || snapshot.joint_orders > snapshot.orders_a
        || snapshot.joint_orders > snapshot.orders_b
        || policy.minimum_affinity_basis_points > 10_000
        || policy.discount_basis_points > 2_500
        || policy.minimum_margin_basis_points > 10_000
    {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::InvalidSnapshot);
    }
    if snapshot.in_flight {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::InFlight);
    }

    let (Some(cost_a), Some(cost_b)) = (snapshot.unit_cost_a_minor, snapshot.unit_cost_b_minor)
    else {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::MissingEconomics);
    };
    if cost_a < 0 || cost_b < 0 {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::InvalidSnapshot);
    }
    if snapshot.joint_orders < policy.minimum_joint_orders {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::InsufficientEvidence);
    }

    let denominator = snapshot.orders_a.min(snapshot.orders_b).max(1);
    let affinity = snapshot.joint_orders.saturating_mul(10_000) / denominator;
    let affinity_basis_points = u16::try_from(affinity.min(10_000)).unwrap_or(10_000);
    if affinity_basis_points < policy.minimum_affinity_basis_points {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::InsufficientEvidence);
    }

    let list_price = snapshot
        .price_a_minor
        .saturating_add(snapshot.price_b_minor);
    let discount =
        i128::from(list_price).saturating_mul(i128::from(policy.discount_basis_points)) / 10_000;
    let discount = i64::try_from(discount).unwrap_or(i64::MAX);
    let bundle_price = list_price.saturating_sub(discount);
    let total_cost = cost_a.saturating_add(cost_b);
    if bundle_price <= 0 || bundle_price <= total_cost {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::MarginFloor);
    }

    let margin_basis_points =
        (i128::from(bundle_price - total_cost) * 10_000) / i128::from(bundle_price);
    let margin_basis_points = u16::try_from(margin_basis_points.clamp(0, 10_000)).unwrap_or(10_000);
    if margin_basis_points < policy.minimum_margin_basis_points {
        return MerchBundleDecision::Hold(MerchBundleHoldReason::MarginFloor);
    }

    let confidence = 7_500_u16
        .saturating_add(
            affinity_basis_points.saturating_sub(policy.minimum_affinity_basis_points) / 2,
        )
        .min(9_800);
    MerchBundleDecision::Recommend {
        bundle_price_minor: bundle_price,
        affinity_basis_points,
        confidence: Confidence::saturating_from_basis_points(confidence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> MerchBundleSnapshot {
        MerchBundleSnapshot {
            product_a: MerchProductId::new(),
            product_b: MerchProductId::new(),
            price_a_minor: 5_000,
            price_b_minor: 3_000,
            unit_cost_a_minor: Some(2_000),
            unit_cost_b_minor: Some(1_000),
            orders_a: 20,
            orders_b: 20,
            joint_orders: 10,
            in_flight: false,
        }
    }

    #[test]
    fn economics_are_required() {
        let mut snapshot = snapshot();
        snapshot.unit_cost_a_minor = None;
        assert_eq!(
            evaluate_merch_bundle(snapshot, MerchBundlePolicy::default()),
            MerchBundleDecision::Hold(MerchBundleHoldReason::MissingEconomics),
        );
    }

    #[test]
    fn strong_affinity_produces_a_margin_safe_bundle() {
        assert!(matches!(
            evaluate_merch_bundle(snapshot(), MerchBundlePolicy::default()),
            MerchBundleDecision::Recommend {
                bundle_price_minor: 7_200,
                ..
            }
        ));
    }
}
