//! Where each delivery lane stops.
//!
//! Planning without delivery is not growth, and a lane that is quietly stuck
//! looks exactly like a lane that is working until somebody counts. Production
//! on 2026-10-02 had 18 drafts `awaiting_manual_post` (15 of them Reddit, held
//! after moderator removals), five posted in a fortnight, and nothing in any
//! single place that said so. This module turns a lane's post rows into one
//! honest answer to "what happens to what we ask this lane to do": delivered,
//! waiting on a person, rate-limited, failing, queued, or never asked.
//!
//! The vocabulary is the one every post table already shares (`pending`,
//! `posting`, `posted`, `failed`, `rate_limited`, `awaiting_manual_post`, and
//! `cancelled` where a table has it), so a new lane is a new row source, not a
//! new state machine.
//!
//! What it does not do: it never reports a lane as healthy because nothing went
//! wrong — **a lane nobody asked to do anything is `Quiet`, measured zero,
//! which is not the same as delivering.** And it counts *rows the lane wrote*;
//! whether a delivered post produced a visitor, a fan or a retained fan is the
//! organic funnel's question, answered per action, not here.

use serde::Serialize;

/// What became of one request to a lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Waiting its turn: asked, not yet attempted.
    Queued,
    /// Being attempted right now.
    Dispatching,
    /// A real post/receipt exists.
    Delivered,
    /// Drafted and parked for a person (or a halt) to publish.
    HeldForPerson,
    /// The platform or our own ceiling said not yet.
    RateLimited,
    /// Attempted and refused or errored.
    Failed,
    /// Cancelled by a person or superseded: not a failure, not a delivery.
    Withdrawn,
}

impl Outcome {
    /// Maps a post row's `status`. `None` for a value outside the shared
    /// vocabulary, so a new status is noticed rather than silently bucketed.
    #[must_use]
    pub fn from_status(status: &str) -> Option<Self> {
        Some(match status {
            "pending" => Self::Queued,
            "posting" => Self::Dispatching,
            "posted" => Self::Delivered,
            "awaiting_manual_post" => Self::HeldForPerson,
            "rate_limited" => Self::RateLimited,
            "failed" => Self::Failed,
            "cancelled" => Self::Withdrawn,
            _ => return None,
        })
    }
}

/// Counts of one lane's requests by outcome, over one window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LaneCounts {
    pub queued: u32,
    pub dispatching: u32,
    pub delivered: u32,
    pub held_for_person: u32,
    pub rate_limited: u32,
    pub failed: u32,
    pub withdrawn: u32,
}

impl LaneCounts {
    pub fn add(&mut self, outcome: Outcome, count: u32) {
        let slot = match outcome {
            Outcome::Queued => &mut self.queued,
            Outcome::Dispatching => &mut self.dispatching,
            Outcome::Delivered => &mut self.delivered,
            Outcome::HeldForPerson => &mut self.held_for_person,
            Outcome::RateLimited => &mut self.rate_limited,
            Outcome::Failed => &mut self.failed,
            Outcome::Withdrawn => &mut self.withdrawn,
        };
        *slot = slot.saturating_add(count);
    }

    /// Requests the lane was asked to carry. Withdrawn ones were taken back, so
    /// they are neither delivered nor stuck and do not count as asked.
    #[must_use]
    pub const fn asked(&self) -> u32 {
        self.queued
            .saturating_add(self.dispatching)
            .saturating_add(self.delivered)
            .saturating_add(self.held_for_person)
            .saturating_add(self.rate_limited)
            .saturating_add(self.failed)
    }
}

/// The one-line answer for a lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Nothing was asked of this lane in the window. Measured zero, not health.
    Quiet,
    /// Delivering, with nothing stuck behind it.
    Delivering,
    /// Delivering some, with requests also held or failing behind it.
    DeliveringPartly,
    /// Nothing delivered, and what was asked is parked for a person or a halt.
    /// The lane is not broken; it is not producing either.
    HeldForPerson,
    /// Nothing delivered; the platform or a ceiling is saying not yet.
    RateLimited,
    /// Nothing delivered, and what was attempted failed.
    Failing,
    /// Nothing delivered yet; requests are queued or in flight.
    Queued,
}

impl Verdict {
    /// Whether the operator needs to do (or look at) something for this lane to
    /// deliver. `Quiet` and `Queued` are not problems yet; `Delivering` is not.
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::HeldForPerson | Self::RateLimited | Self::Failing | Self::DeliveringPartly
        )
    }
}

/// Priority is the order of what an operator most needs to hear: a lane that
/// delivers nothing and is parked outranks one that fails (the failing one is at
/// least trying), and anything stuck behind a delivering lane is *partly*.
#[must_use]
pub fn verdict(counts: &LaneCounts) -> Verdict {
    if counts.asked() == 0 {
        return Verdict::Quiet;
    }
    let stuck = counts.held_for_person + counts.rate_limited + counts.failed;
    if counts.delivered > 0 {
        return if stuck == 0 {
            Verdict::Delivering
        } else {
            Verdict::DeliveringPartly
        };
    }
    if counts.held_for_person > 0 {
        Verdict::HeldForPerson
    } else if counts.rate_limited > 0 {
        Verdict::RateLimited
    } else if counts.failed > 0 {
        Verdict::Failing
    } else {
        Verdict::Queued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(parts: &[(Outcome, u32)]) -> LaneCounts {
        let mut counts = LaneCounts::default();
        for (outcome, n) in parts {
            counts.add(*outcome, *n);
        }
        counts
    }

    #[test]
    fn every_status_a_post_table_can_hold_has_one_outcome() {
        for (status, outcome) in [
            ("pending", Outcome::Queued),
            ("posting", Outcome::Dispatching),
            ("posted", Outcome::Delivered),
            ("awaiting_manual_post", Outcome::HeldForPerson),
            ("rate_limited", Outcome::RateLimited),
            ("failed", Outcome::Failed),
            ("cancelled", Outcome::Withdrawn),
        ] {
            assert_eq!(Outcome::from_status(status), Some(outcome), "{status}");
        }
        assert_eq!(
            Outcome::from_status("posted_twice"),
            None,
            "a new status is noticed"
        );
    }

    #[test]
    fn a_lane_nobody_asked_is_quiet_never_healthy() {
        assert_eq!(verdict(&LaneCounts::default()), Verdict::Quiet);
        // Cancelled requests were taken back: still nothing was asked of it.
        assert_eq!(verdict(&counts(&[(Outcome::Withdrawn, 9)])), Verdict::Quiet);
        assert!(!Verdict::Quiet.needs_attention());
    }

    #[test]
    fn reddit_on_2026_10_02_is_held_for_a_person_not_failing() {
        // 15 parked after the moderator removals, 5 posted earlier in the month
        // would read as delivering partly; the *recent* window had only the held.
        let reddit = counts(&[(Outcome::HeldForPerson, 15), (Outcome::Failed, 4)]);
        assert_eq!(verdict(&reddit), Verdict::HeldForPerson);
        assert!(verdict(&reddit).needs_attention());
    }

    #[test]
    fn a_delivering_lane_with_a_backlog_behind_it_is_partly_not_fine() {
        assert_eq!(
            verdict(&counts(&[
                (Outcome::Delivered, 5),
                (Outcome::HeldForPerson, 15)
            ])),
            Verdict::DeliveringPartly
        );
        assert_eq!(
            verdict(&counts(&[(Outcome::Delivered, 5)])),
            Verdict::Delivering
        );
        assert_eq!(
            verdict(&counts(&[(Outcome::Delivered, 1), (Outcome::Queued, 40)])),
            Verdict::Delivering,
            "a queue is not stuck"
        );
    }

    #[test]
    fn nothing_delivered_is_ranked_by_what_the_operator_must_hear_first() {
        assert_eq!(
            verdict(&counts(&[
                (Outcome::Failed, 3),
                (Outcome::HeldForPerson, 1)
            ])),
            Verdict::HeldForPerson
        );
        assert_eq!(
            verdict(&counts(&[(Outcome::Failed, 3), (Outcome::RateLimited, 1)])),
            Verdict::RateLimited
        );
        assert_eq!(verdict(&counts(&[(Outcome::Failed, 3)])), Verdict::Failing);
        assert_eq!(
            verdict(&counts(&[(Outcome::Queued, 2), (Outcome::Dispatching, 1)])),
            Verdict::Queued
        );
        assert!(!Verdict::Queued.needs_attention());
    }
}
