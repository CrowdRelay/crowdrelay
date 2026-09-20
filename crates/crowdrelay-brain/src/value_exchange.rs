//! The fan-equivalent price of a minor unit of revenue.
//!
//! `DecisionValue` adds in one currency only — expected Y30 fans — so a
//! revenue-bearing action needs an auditable conversion before its revenue
//! prediction can join `total()`. This type is that conversion: the tenant's
//! own realized ratio of revenue to new fans, accumulated from first-party
//! sales (ticket orders net of refunds, merch goods gross) divided by the
//! fans gained over the same days.
//!
//! The rate is *learned per tenant*, never a compiled constant — one band's
//! €20 ticket and another's free-entry merch table are different economies
//! and the exchange must not average them. Until the tenant has enough
//! history the conversion refuses to answer: an action with no measured
//! exchange earns zero economic term, not an invented one.
//!
//! Deliberately cumulative rather than a rolling window: the state lives in
//! the causal-model checkpoint and survives cycles, and a lifetime ratio
//! drifts with the tenant instead of forgetting. `observe` dedupes by day —
//! the loader replays the same trailing range every cycle, and a day that
//! has already been folded must not be learned twice.

use serde::{Deserialize, Serialize};

/// Minimum distinct days of history before the ratio is believed. A single
/// big day (one release, one show) is a story, not a rate.
const MIN_EXCHANGE_DAYS: u32 = 7;
/// Minimum new fans over the observed period. Below this the denominator is
/// noise and a few purchases could name any price for a fan.
const MIN_EXCHANGE_FANS: f64 = 20.0;

/// The tenant's learned revenue-per-fan exchange rate.
///
/// `revenue_minor`/`new_fans` are running sums over the days folded so far;
/// `last_observed_day` (Julian day) is the replay cursor that keeps a
/// repeated loader scan idempotent.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ValueExchange {
    /// Cumulative revenue in minor units across folded days (tickets net of
    /// refunds plus merch goods gross).
    pub revenue_minor: f64,
    /// Cumulative new fans across the same days.
    pub new_fans: f64,
    /// Number of distinct day-buckets folded.
    pub days_observed: u32,
    /// Julian day number of the newest folded bucket — the dedupe cursor.
    /// Zero means nothing has been observed.
    pub last_observed_day: i64,
}

impl ValueExchange {
    /// Folds one complete day-bucket into the rate. Days already observed
    /// are skipped — the loader re-scans the trailing window every cycle and
    /// only genuinely new days may move the sums.
    pub fn observe(&mut self, day_julian: i64, revenue_minor: f64, new_fans: f64) {
        if day_julian <= self.last_observed_day {
            return;
        }
        self.last_observed_day = day_julian;
        self.revenue_minor += revenue_minor;
        self.new_fans += new_fans;
        self.days_observed += 1;
    }

    /// Minor units of revenue per new fan — the tenant's realized rate.
    /// `None` until the history is broad enough to be a rate rather than an
    /// anecdote, or when the tenant has sold nothing at all (a rate of zero
    /// would report every revenue prediction as zero fans forever).
    #[must_use]
    pub fn minor_per_fan(&self) -> Option<f64> {
        if self.days_observed < MIN_EXCHANGE_DAYS
            || self.new_fans < MIN_EXCHANGE_FANS
            || self.revenue_minor <= 0.0
        {
            return None;
        }
        Some(self.revenue_minor / self.new_fans)
    }

    /// Converts a revenue prediction into fan-equivalent value. `None` when
    /// the exchange is unconfident — the caller then leaves the economic
    /// term unset rather than pricing revenue with a made-up rate.
    #[must_use]
    pub fn revenue_to_fans(&self, revenue_minor: f64) -> Option<f64> {
        let rate = self.minor_per_fan()?;
        if revenue_minor <= 0.0 {
            return None;
        }
        Some(revenue_minor / rate)
    }
}

/// The metric keys that denominate in currency minor units and so can pass
/// through the exchange into fan-equivalent value. Ratios and counts —
/// `promotion_roas_bps`, replies, clicks — deliberately do not appear here:
/// a return-on-ad-spread is not revenue and a reply is not a fan; those stay
/// provenance and constraints, not additive utility.
pub const REVENUE_MINOR_METRICS: &[&str] = &[
    "ticket_revenue_minor",
    "merch_gross_minor",
    "audience_ticket_revenue_minor",
    "show_ticket_revenue_minor",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exchange_refuses_to_answer_without_history() {
        let exchange = ValueExchange::default();
        assert_eq!(exchange.minor_per_fan(), None);
        assert_eq!(exchange.revenue_to_fans(10_000.0), None);
    }

    #[test]
    fn the_rate_is_the_tenants_realized_ratio() {
        let mut exchange = ValueExchange::default();
        for day in 1..=10 {
            // €5,000 a day, 10 new fans a day → 50_000 minor per fan.
            exchange.observe(day, 500_000.0, 10.0);
        }
        assert_eq!(exchange.minor_per_fan(), Some(50_000.0));
        assert_eq!(exchange.revenue_to_fans(100_000.0), Some(2.0));
    }

    #[test]
    fn a_day_is_never_learned_twice() {
        let mut exchange = ValueExchange::default();
        for day in 1..=10 {
            exchange.observe(day, 500_000.0, 10.0);
        }
        let rate = exchange.minor_per_fan();
        // The loader re-scans the window every cycle — the same days must
        // not move the sums a second time.
        for day in 1..=10 {
            exchange.observe(day, 9_999_999.0, 999.0);
        }
        assert_eq!(exchange.minor_per_fan(), rate);
        assert_eq!(exchange.days_observed, 10);
    }

    #[test]
    fn one_loud_day_is_not_a_rate() {
        let mut exchange = ValueExchange::default();
        exchange.observe(1, 10_000_000.0, 500.0);
        assert_eq!(exchange.minor_per_fan(), None);
    }
}
