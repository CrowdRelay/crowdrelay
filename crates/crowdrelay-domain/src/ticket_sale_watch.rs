//! Ticket-sale pace watch — detect upcoming shows selling far below the
//! workspace's own historical pace at the same lead time.
//!
//! Phase 19 candidate: "A show selling far below its own history at T-14 is
//! visible now; the response is owned-audience and free."
//!
//! The response is a growth-debt observation, not a pricing action: pricing
//! is the `TicketYield` context's job, and that context already has its own
//! hold reasons for insufficient velocity. This detector is about *reach*:
//! a show that is behind pace needs more people to know about it, not a
//! cheaper ticket.
//!
//! The empty-denominator invariant is the foundation: with zero completed
//! shows there is no pace to compare against, and the detector says
//! `InsufficientHistory` rather than inventing one. With one completed show
//! the comparison is thin but honest; with three or more it is robust.

use serde::{Deserialize, Serialize};

use crate::autonomy::Confidence;

/// One upcoming show with the facts the pace rule needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpcomingShowSales {
    /// Days until the show. Negative means the show has passed — the caller
    /// should not pass those, but the rule checks anyway.
    pub days_to_event: i16,
    /// Paid tickets right now.
    pub paid_tickets: u32,
    /// The show's capacity, if known. Used for sell-through context, not as
    /// the primary comparison — pace is about absolute sales against history.
    pub capacity: Option<u32>,
}

/// One historical data point: how many tickets a completed show had sold at
/// a given lead time. The adapter supplies these from past completed events
/// at matching lead times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoricalPacePoint {
    /// Days before the show this measurement was taken.
    pub days_to_event: i16,
    /// Paid tickets at that point.
    pub paid_tickets: u32,
}

/// Tunable thresholds for the pace watch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TicketSaleWatchPolicy {
    /// Minimum number of completed shows with data before any comparison is
    /// made. Below this the detector says `InsufficientHistory`.
    pub minimum_history_points: u8,
    /// A show is "behind pace" when its paid tickets are below this share of
    /// the historical average at the same lead time, in basis points.
    /// 7_000 = 70% of historical pace.
    pub behind_pace_basis_points: u16,
    /// Only check shows inside this many days. A show 6 months out has not
    /// had time to fall behind; a show tomorrow is too late to help.
    pub maximum_days_to_event: i16,
    /// Only check shows at least this many days out. Inside this window the
    /// show is too close to benefit from owned-audience reach.
    pub minimum_days_to_event: i16,
    /// How close in lead time a historical point must be to be used. A point
    /// from ±3 days of the current lead time is close enough.
    pub lead_time_tolerance_days: i16,
}

impl Default for TicketSaleWatchPolicy {
    fn default() -> Self {
        Self {
            minimum_history_points: 2,
            behind_pace_basis_points: 7_000,
            maximum_days_to_event: 21,
            minimum_days_to_event: 3,
            lead_time_tolerance_days: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketSaleWatchDecision {
    /// The show is not in the checkable window, or there is not enough
    /// history to form a pace, or sales are at or above pace.
    Hold(TicketSaleWatchHoldReason),
    /// The show is selling far below the historical pace at this lead time.
    /// The confidence reflects how many historical points corroborate the
    /// comparison and how far below pace the show is.
    Behind {
        current_paid: u32,
        historical_average: u32,
        ratio_basis_points: u32,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketSaleWatchHoldReason {
    /// Outside the checkable window.
    OutsideWindow,
    /// Not enough completed shows to form a baseline.
    InsufficientHistory,
    /// No historical points within the lead-time tolerance.
    NoMatchingHistory,
    /// Sales are at or above the historical pace.
    AtOrAbovePace,
}

/// Pure decision service. Given the current sales for an upcoming show and a
/// set of historical pace points, decide whether the show is behind pace.
///
/// The historical average is computed from points within
/// `lead_time_tolerance_days` of the current `days_to_event`. If no points
/// fall in that window, the detector says `NoMatchingHistory` — it does not
/// widen the window, because a point from a very different lead time is not
/// comparable.
#[must_use]
pub fn evaluate_ticket_sale_pace(
    upcoming: UpcomingShowSales,
    history: &[HistoricalPacePoint],
    policy: TicketSaleWatchPolicy,
) -> TicketSaleWatchDecision {
    // Outside the checkable window.
    if upcoming.days_to_event < policy.minimum_days_to_event
        || upcoming.days_to_event > policy.maximum_days_to_event
    {
        return TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::OutsideWindow);
    }

    // Not enough history to form a baseline.
    if history.len() < usize::from(policy.minimum_history_points) {
        return TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::InsufficientHistory);
    }

    // Find historical points within the lead-time tolerance.
    let matching: Vec<u32> = history
        .iter()
        .filter(|point| {
            (point.days_to_event - upcoming.days_to_event).abs() <= policy.lead_time_tolerance_days
        })
        .map(|point| point.paid_tickets)
        .collect();

    if matching.len() < usize::from(policy.minimum_history_points) {
        return TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::NoMatchingHistory);
    }

    // Compute the historical average. Integer arithmetic: sum then divide.
    let sum: u64 = matching.iter().map(|&v| u64::from(v)).sum();
    let historical_average =
        u32::try_from(sum / u64::try_from(matching.len()).unwrap_or(1)).unwrap_or(u32::MAX);

    if historical_average == 0 {
        return TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::AtOrAbovePace);
    }

    // Ratio of current to historical, in basis points.
    let ratio_basis_points = u32::try_from(
        u64::from(upcoming.paid_tickets).saturating_mul(10_000) / u64::from(historical_average),
    )
    .unwrap_or(u32::MAX);

    if ratio_basis_points >= u32::from(policy.behind_pace_basis_points) {
        return TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::AtOrAbovePace);
    }

    // Confidence: more historical points and a worse ratio both raise it.
    // Starts at 4_000 (we have a real comparison) and earns up to 4_000 more
    // from the depth of the deficit and 2_000 from corroboration.
    let deficit_bp = u32::from(policy.behind_pace_basis_points)
        .saturating_sub(ratio_basis_points)
        .min(3_000);
    let deficit_contribution = deficit_bp / 750; // 0..=4
    let corroboration = match matching.len() {
        2 => 1_000,
        3..=5 => 1_500,
        _ => 2_000,
    };
    let confidence = Confidence::saturating_from_basis_points(
        u16::try_from(
            4_000_u32
                .saturating_add(deficit_contribution * 1_000)
                .saturating_add(corroboration)
                .min(10_000),
        )
        .unwrap_or(u16::MAX),
    );

    TicketSaleWatchDecision::Behind {
        current_paid: upcoming.paid_tickets,
        historical_average,
        ratio_basis_points,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TicketSaleWatchPolicy {
        TicketSaleWatchPolicy::default()
    }

    #[test]
    fn no_history_means_insufficient_evidence() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 10,
                capacity: Some(200),
            },
            &[],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::InsufficientHistory)
        );
    }

    #[test]
    fn one_point_is_not_enough() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 10,
                capacity: Some(200),
            },
            &[HistoricalPacePoint {
                days_to_event: 14,
                paid_tickets: 100,
            }],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::InsufficientHistory)
        );
    }

    #[test]
    fn at_pace_does_not_raise() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 100,
                capacity: Some(200),
            },
            &[
                HistoricalPacePoint {
                    days_to_event: 14,
                    paid_tickets: 100,
                },
                HistoricalPacePoint {
                    days_to_event: 15,
                    paid_tickets: 110,
                },
            ],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::AtOrAbovePace)
        );
    }

    #[test]
    fn far_below_pace_raises() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 30,
                capacity: Some(200),
            },
            &[
                HistoricalPacePoint {
                    days_to_event: 14,
                    paid_tickets: 100,
                },
                HistoricalPacePoint {
                    days_to_event: 15,
                    paid_tickets: 110,
                },
            ],
            policy(),
        );
        match decision {
            TicketSaleWatchDecision::Behind {
                current_paid,
                historical_average,
                ratio_basis_points,
                ..
            } => {
                assert_eq!(current_paid, 30);
                assert_eq!(historical_average, 105);
                // 30/105 = ~2857 bps
                assert!(ratio_basis_points < 7_000);
            }
            _ => panic!("expected Behind, got {decision:?}"),
        }
    }

    #[test]
    fn outside_window_does_not_check() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 1,
                paid_tickets: 0,
                capacity: Some(200),
            },
            &[
                HistoricalPacePoint {
                    days_to_event: 1,
                    paid_tickets: 100,
                },
                HistoricalPacePoint {
                    days_to_event: 2,
                    paid_tickets: 90,
                },
            ],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::OutsideWindow)
        );
    }

    #[test]
    fn no_matching_history_within_tolerance_holds() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 10,
                capacity: Some(200),
            },
            &[
                HistoricalPacePoint {
                    days_to_event: 30,
                    paid_tickets: 100,
                },
                HistoricalPacePoint {
                    days_to_event: 28,
                    paid_tickets: 110,
                },
            ],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::NoMatchingHistory)
        );
    }

    #[test]
    fn zero_historical_average_holds() {
        let decision = evaluate_ticket_sale_pace(
            UpcomingShowSales {
                days_to_event: 14,
                paid_tickets: 10,
                capacity: Some(200),
            },
            &[
                HistoricalPacePoint {
                    days_to_event: 14,
                    paid_tickets: 0,
                },
                HistoricalPacePoint {
                    days_to_event: 15,
                    paid_tickets: 0,
                },
            ],
            policy(),
        );
        assert_eq!(
            decision,
            TicketSaleWatchDecision::Hold(TicketSaleWatchHoldReason::AtOrAbovePace)
        );
    }
}
