//! Deterministic ticket-yield bounded context.
//!
//! Pricing decisions are event-wide, explainable and intentionally conservative.
//! The context never performs per-fan price discrimination and never decreases a
//! public ticket price automatically. Weak demand belongs to lifecycle/promotion,
//! not to a hidden discounting rule.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{TicketTypeId, autonomy::Confidence};

/// Current observable state for one public ticket type.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TicketYieldSnapshot {
    pub ticket_type_id: TicketTypeId,
    pub current_price_minor: i64,
    /// Quantity that is still economically paid. Reservations never count.
    pub paid_quantity: u32,
    /// Effective capacity used for price sell-through. For a single uncapped
    /// tier this is the sale capacity; multi-tier allocation requires an
    /// explicit tier capacity and guardrail below.
    pub capacity: u32,
    pub sale_capacity: u32,
    pub paid_last_72h: u32,
    pub days_to_event: u32,
    pub last_price_change_at: Option<OffsetDateTime>,
    pub last_capacity_change_at: Option<OffsetDateTime>,
    pub allocation_guardrail: Option<TicketAllocationGuardrail>,
}

/// Operator-owned allocation bounds for one ticket tier. ViryaOS may only
/// unlock capacity inside these limits; it never invents tier semantics from
/// names, slugs or ordering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TicketAllocationGuardrail {
    pub minimum_capacity: u32,
    pub maximum_capacity: u32,
    pub step_capacity: u32,
    pub version: i64,
}

/// Conservative policy limits for automatic yield decisions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TicketYieldPolicy {
    pub min_price_minor: i64,
    pub max_price_minor: i64,
    pub step_minor: i64,
    pub minimum_paid_quantity: u32,
    pub minimum_sell_through_basis_points: u16,
    pub minimum_paid_last_72h: u32,
    pub minimum_days_to_event: u32,
    pub cooldown_hours: u32,
    pub allocation_minimum_sell_through_basis_points: u16,
    pub allocation_minimum_paid_last_72h: u32,
    pub allocation_cooldown_hours: u32,
}

impl Default for TicketYieldPolicy {
    fn default() -> Self {
        Self {
            min_price_minor: 2_000,
            max_price_minor: 5_000,
            step_minor: 500,
            minimum_paid_quantity: 12,
            minimum_sell_through_basis_points: 7_000,
            minimum_paid_last_72h: 4,
            minimum_days_to_event: 7,
            cooldown_hours: 72,
            allocation_minimum_sell_through_basis_points: 9_000,
            allocation_minimum_paid_last_72h: 4,
            allocation_cooldown_hours: 72,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketYieldDecision {
    Hold(TicketYieldHoldReason),
    Increase {
        from_minor: i64,
        to_minor: i64,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketYieldHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    TooCloseToEvent,
    InsufficientPaidHistory,
    InsufficientSellThrough,
    InsufficientVelocity,
    CooldownActive,
    MaximumPriceReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketAllocationDecision {
    Hold(TicketAllocationHoldReason),
    IncreaseCapacity {
        from_capacity: u32,
        to_capacity: u32,
        guardrail_version: i64,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketAllocationHoldReason {
    NotConfigured,
    InvalidPolicy,
    InvalidSnapshot,
    TooCloseToEvent,
    InsufficientPaidHistory,
    InsufficientSellThrough,
    InsufficientVelocity,
    CooldownActive,
    MaximumAllocationReached,
}

/// Pure ticket-yield decision service.
#[must_use]
pub fn evaluate_ticket_yield(
    snapshot: TicketYieldSnapshot,
    policy: TicketYieldPolicy,
    now: OffsetDateTime,
) -> TicketYieldDecision {
    if !valid_policy(policy) {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::InvalidPolicy);
    }
    if !valid_snapshot(snapshot, policy, now) {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::InvalidSnapshot);
    }
    if snapshot.days_to_event < policy.minimum_days_to_event {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::TooCloseToEvent);
    }
    if snapshot.paid_quantity < policy.minimum_paid_quantity {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::InsufficientPaidHistory);
    }

    let sell_through = sell_through_basis_points(snapshot.paid_quantity, snapshot.capacity);
    if sell_through < u32::from(policy.minimum_sell_through_basis_points) {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::InsufficientSellThrough);
    }
    if snapshot.paid_last_72h < policy.minimum_paid_last_72h {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::InsufficientVelocity);
    }
    if snapshot.last_price_change_at.is_some_and(|changed_at| {
        now - changed_at < Duration::hours(i64::from(policy.cooldown_hours))
    }) {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::CooldownActive);
    }

    let to_minor = snapshot
        .current_price_minor
        .saturating_add(policy.step_minor)
        .min(policy.max_price_minor);
    if to_minor <= snapshot.current_price_minor {
        return TicketYieldDecision::Hold(TicketYieldHoldReason::MaximumPriceReached);
    }

    // Crossing every hard gate starts at 80%. Stronger sell-through and recent
    // velocity add bounded evidence; no opaque model influences the result.
    let sell_bonus = sell_through
        .saturating_sub(u32::from(policy.minimum_sell_through_basis_points))
        .min(1_000);
    let velocity_bonus = snapshot
        .paid_last_72h
        .saturating_sub(policy.minimum_paid_last_72h)
        .saturating_mul(250)
        .min(1_000);
    let basis_points = 8_000_u32
        .saturating_add(sell_bonus)
        .saturating_add(velocity_bonus)
        .min(10_000);
    let confidence = Confidence::saturating_from_basis_points(basis_points as u16);

    TicketYieldDecision::Increase {
        from_minor: snapshot.current_price_minor,
        to_minor,
        confidence,
    }
}

/// Pure tier-allocation decision service. Capacity is only ever increased and
/// only when an operator has explicitly configured versioned bounds for the
/// tier. The global sale capacity remains an independent hard ceiling.
#[must_use]
pub fn evaluate_ticket_allocation(
    snapshot: TicketYieldSnapshot,
    policy: TicketYieldPolicy,
    now: OffsetDateTime,
) -> TicketAllocationDecision {
    let Some(guardrail) = snapshot.allocation_guardrail else {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::NotConfigured);
    };
    if !valid_policy(policy) {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::InvalidPolicy);
    }
    if !valid_snapshot(snapshot, policy, now)
        || guardrail.version <= 0
        || guardrail.minimum_capacity == 0
        || guardrail.minimum_capacity > snapshot.capacity
        || guardrail.maximum_capacity < snapshot.capacity
        || guardrail.maximum_capacity > snapshot.sale_capacity
        || guardrail.step_capacity == 0
    {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::InvalidSnapshot);
    }
    if snapshot.days_to_event < policy.minimum_days_to_event {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::TooCloseToEvent);
    }
    if snapshot.paid_quantity < policy.minimum_paid_quantity {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::InsufficientPaidHistory);
    }
    let sell_through = sell_through_basis_points(snapshot.paid_quantity, snapshot.capacity);
    if sell_through < u32::from(policy.allocation_minimum_sell_through_basis_points) {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::InsufficientSellThrough);
    }
    if snapshot.paid_last_72h < policy.allocation_minimum_paid_last_72h {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::InsufficientVelocity);
    }
    if snapshot.last_capacity_change_at.is_some_and(|changed_at| {
        now - changed_at < Duration::hours(i64::from(policy.allocation_cooldown_hours))
    }) {
        return TicketAllocationDecision::Hold(TicketAllocationHoldReason::CooldownActive);
    }
    let to_capacity = snapshot
        .capacity
        .saturating_add(guardrail.step_capacity)
        .min(guardrail.maximum_capacity)
        .min(snapshot.sale_capacity);
    if to_capacity <= snapshot.capacity {
        return TicketAllocationDecision::Hold(
            TicketAllocationHoldReason::MaximumAllocationReached,
        );
    }
    let sell_bonus = sell_through
        .saturating_sub(u32::from(
            policy.allocation_minimum_sell_through_basis_points,
        ))
        .min(1_000);
    let velocity_bonus = snapshot
        .paid_last_72h
        .saturating_sub(policy.allocation_minimum_paid_last_72h)
        .saturating_mul(250)
        .min(500);
    let basis_points = 8_500_u32
        .saturating_add(sell_bonus)
        .saturating_add(velocity_bonus)
        .min(10_000);
    TicketAllocationDecision::IncreaseCapacity {
        from_capacity: snapshot.capacity,
        to_capacity,
        guardrail_version: guardrail.version,
        confidence: Confidence::saturating_from_basis_points(basis_points as u16),
    }
}

#[must_use]
const fn valid_policy(policy: TicketYieldPolicy) -> bool {
    policy.min_price_minor > 0
        && policy.max_price_minor >= policy.min_price_minor
        && policy.step_minor > 0
        && policy.minimum_sell_through_basis_points <= 10_000
        && policy.allocation_minimum_sell_through_basis_points <= 10_000
}

#[must_use]
fn valid_snapshot(
    snapshot: TicketYieldSnapshot,
    policy: TicketYieldPolicy,
    now: OffsetDateTime,
) -> bool {
    snapshot.capacity > 0
        && snapshot.paid_quantity <= snapshot.capacity
        && snapshot.paid_last_72h <= snapshot.paid_quantity
        && snapshot.current_price_minor >= policy.min_price_minor
        && snapshot.current_price_minor <= policy.max_price_minor
        && snapshot.sale_capacity >= snapshot.capacity
        && snapshot
            .last_price_change_at
            .is_none_or(|changed_at| changed_at <= now)
        && snapshot
            .last_capacity_change_at
            .is_none_or(|changed_at| changed_at <= now)
}

#[must_use]
const fn sell_through_basis_points(paid_quantity: u32, capacity: u32) -> u32 {
    if capacity == 0 {
        return 0;
    }
    paid_quantity.saturating_mul(10_000) / capacity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot() -> TicketYieldSnapshot {
        let now = now();
        TicketYieldSnapshot {
            ticket_type_id: TicketTypeId::new(),
            current_price_minor: 3_000,
            paid_quantity: 75,
            capacity: 100,
            sale_capacity: 120,
            paid_last_72h: 8,
            days_to_event: 21,
            last_price_change_at: Some(now - Duration::hours(96)),
            last_capacity_change_at: Some(now - Duration::hours(96)),
            allocation_guardrail: Some(TicketAllocationGuardrail {
                minimum_capacity: 80,
                maximum_capacity: 120,
                step_capacity: 10,
                version: 1,
            }),
        }
    }

    #[test]
    fn strong_paid_demand_increases_exactly_one_bounded_step() {
        assert!(matches!(
            evaluate_ticket_yield(snapshot(), TicketYieldPolicy::default(), now()),
            TicketYieldDecision::Increase {
                from_minor: 3_000,
                to_minor: 3_500,
                ..
            }
        ));
    }

    #[test]
    fn reservations_cannot_inflate_price_because_only_paid_quantity_is_modeled() {
        let mut state = snapshot();
        state.paid_quantity = 8;
        state.paid_last_72h = 4;

        assert_eq!(
            evaluate_ticket_yield(state, TicketYieldPolicy::default(), now()),
            TicketYieldDecision::Hold(TicketYieldHoldReason::InsufficientPaidHistory)
        );
    }

    #[test]
    fn impossible_velocity_snapshot_fails_closed() {
        let mut state = snapshot();
        state.paid_last_72h = state.paid_quantity + 1;

        assert_eq!(
            evaluate_ticket_yield(state, TicketYieldPolicy::default(), now()),
            TicketYieldDecision::Hold(TicketYieldHoldReason::InvalidSnapshot)
        );
    }

    #[test]
    fn cooldown_prevents_repeated_price_changes() {
        let mut state = snapshot();
        state.last_price_change_at = Some(now() - Duration::hours(24));

        assert_eq!(
            evaluate_ticket_yield(state, TicketYieldPolicy::default(), now()),
            TicketYieldDecision::Hold(TicketYieldHoldReason::CooldownActive)
        );
    }

    #[test]
    fn price_never_exceeds_policy_cap() {
        let mut state = snapshot();
        state.current_price_minor = 4_800;

        assert!(matches!(
            evaluate_ticket_yield(state, TicketYieldPolicy::default(), now()),
            TicketYieldDecision::Increase {
                to_minor: 5_000,
                ..
            }
        ));
    }

    #[test]
    fn near_sold_out_tier_unlocks_one_operator_bounded_capacity_step() {
        let mut state = snapshot();
        state.paid_quantity = 92;
        assert!(matches!(
            evaluate_ticket_allocation(state, TicketYieldPolicy::default(), now()),
            TicketAllocationDecision::IncreaseCapacity {
                from_capacity: 100,
                to_capacity: 110,
                guardrail_version: 1,
                ..
            }
        ));
    }

    #[test]
    fn tier_allocation_never_runs_without_explicit_guardrails() {
        let mut state = snapshot();
        state.paid_quantity = 95;
        state.allocation_guardrail = None;
        assert_eq!(
            evaluate_ticket_allocation(state, TicketYieldPolicy::default(), now()),
            TicketAllocationDecision::Hold(TicketAllocationHoldReason::NotConfigured)
        );
    }

    #[test]
    fn tier_allocation_cannot_exceed_global_sale_capacity() {
        let mut state = snapshot();
        state.paid_quantity = 95;
        state.sale_capacity = 105;
        state.allocation_guardrail = Some(TicketAllocationGuardrail {
            minimum_capacity: 80,
            maximum_capacity: 105,
            step_capacity: 20,
            version: 2,
        });
        assert!(matches!(
            evaluate_ticket_allocation(state, TicketYieldPolicy::default(), now()),
            TicketAllocationDecision::IncreaseCapacity {
                to_capacity: 105,
                ..
            }
        ));
    }

    #[test]
    fn malformed_policy_fails_closed() {
        let policy = TicketYieldPolicy {
            minimum_sell_through_basis_points: 10_001,
            ..TicketYieldPolicy::default()
        };
        assert_eq!(
            evaluate_ticket_yield(snapshot(), policy, now()),
            TicketYieldDecision::Hold(TicketYieldHoldReason::InvalidPolicy)
        );
    }
}
