//! Sending like somebody who wants replies.
//!
//! Deliverability is not a detail. A burned sending domain does not degrade the
//! outreach channel, it ends it — and it ends the transactional mail sharing
//! that domain along with it. The band cannot buy its reputation back.
//!
//! Two rules, and the second is the one that matters.
//!
//! 1. **Volume ramps.** The operator's weekly third-party budget is a ceiling,
//!    not a target. A workspace that has never sent starts far below it and
//!    earns its way up; a receiving provider reads a standing start as exactly
//!    what it looks like.
//! 2. **A rising bounce or complaint rate stops the wave.** Not reports it
//!    afterwards. By the time a summary is read the reputation is already
//!    spent, so the halt is a precondition of sending rather than a line in a
//!    digest.
//!
//! A hard bounce is also a fact about one address and is handled where
//! suppression already lives: the target is deactivated, not retried.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// What went wrong with one delivery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryFault {
    /// The address does not exist. Permanent, and the target is finished.
    HardBounce,
    /// A mailbox full, a greylist, a temporary refusal. Counts toward the rate
    /// because providers count it, and suppresses nobody.
    SoftBounce,
    /// Somebody pressed "this is spam". The most expensive signal there is: a
    /// provider weighs one of these like dozens of bounces.
    Complaint,
}

impl DeliveryFault {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardBounce => "hard_bounce",
            Self::SoftBounce => "soft_bounce",
            Self::Complaint => "complaint",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hard_bounce" => Some(Self::HardBounce),
            "soft_bounce" => Some(Self::SoftBounce),
            "complaint" => Some(Self::Complaint),
            _ => None,
        }
    }

    /// Whether this address is finished with.
    ///
    /// Only a hard bounce. Retrying a full mailbox is ordinary; retrying an
    /// address that does not exist is how a sender looks like a list buyer.
    #[must_use]
    pub const fn suppresses_target(self) -> bool {
        matches!(self, Self::HardBounce)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct DeliverabilityPolicy {
    /// Where a workspace that has never sent starts, per rolling week.
    pub ramp_start_touches: u32,
    /// How much the ceiling rises each step, and how long a step lasts.
    pub ramp_step_touches: u32,
    pub ramp_step_days: u32,
    /// Bounce rate at which sending stops, in basis points of what was sent.
    pub max_bounce_rate_basis_points: u16,
    /// Complaint rate at which sending stops. An order of magnitude tighter,
    /// because providers treat it that way.
    pub max_complaint_rate_basis_points: u16,
    /// Below this many sends, a rate is noise. Two bounces out of three is not
    /// a sixty-seven per cent bounce rate, it is three sends.
    pub minimum_rate_sample: u32,
}

impl Default for DeliverabilityPolicy {
    fn default() -> Self {
        Self {
            ramp_start_touches: 3,
            ramp_step_touches: 3,
            ramp_step_days: 7,
            // Two per cent. Well under where providers start throttling, which
            // is the point: the ceiling is not the cliff edge.
            max_bounce_rate_basis_points: 200,
            // A tenth of a per cent.
            max_complaint_rate_basis_points: 10,
            minimum_rate_sample: 20,
        }
    }
}

/// What the workspace's sending looks like from outside.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DeliverabilitySnapshot {
    /// Third-party sends in the last thirty days — the denominator.
    pub sent_30d: u32,
    pub bounces_30d: u32,
    pub complaints_30d: u32,
    /// When this workspace first sent anything at all. `None` means it never
    /// has, and the ramp starts at the bottom.
    pub first_sent_at: Option<OffsetDateTime>,
    /// The operator's weekly third-party budget. The ramp never exceeds it.
    pub weekly_third_party_ceiling: u32,
}

/// Whether the agent may send at all right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverabilityVerdict {
    Healthy,
    /// Stop. Named so an operator knows which number moved, because the two
    /// have completely different fixes: a bounce rate is a list-quality
    /// problem and a complaint rate is a message problem.
    HaltBounceRate,
    HaltComplaintRate,
}

impl DeliverabilityVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::HaltBounceRate => "halt_bounce_rate",
            Self::HaltComplaintRate => "halt_complaint_rate",
        }
    }

    #[must_use]
    pub const fn sending_allowed(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// How many third-party sends this week may hold.
///
/// The operator's ceiling, or less while the workspace is still earning it.
/// Zero is a real answer and means the sending history is not good enough to
/// send at all.
#[must_use]
pub fn ramped_ceiling(
    snapshot: DeliverabilitySnapshot,
    policy: DeliverabilityPolicy,
    now: OffsetDateTime,
) -> u32 {
    if !verdict(snapshot, policy).sending_allowed() {
        return 0;
    }
    let Some(first_sent_at) = snapshot.first_sent_at else {
        return policy
            .ramp_start_touches
            .min(snapshot.weekly_third_party_ceiling);
    };
    let step = i64::from(policy.ramp_step_days).max(1);
    let elapsed_days = (now.unix_timestamp() - first_sent_at.unix_timestamp()) / 86_400;
    let steps = u32::try_from(elapsed_days / step).unwrap_or(u32::MAX);
    policy
        .ramp_start_touches
        .saturating_add(policy.ramp_step_touches.saturating_mul(steps))
        .min(snapshot.weekly_third_party_ceiling)
}

/// Whether the sending record still permits sending.
///
/// A rate below the sample floor is not a rate. Halting a workspace on its
/// third ever send because one address was mistyped would teach an operator to
/// raise the threshold, which is the opposite of what this is for.
#[must_use]
pub fn verdict(
    snapshot: DeliverabilitySnapshot,
    policy: DeliverabilityPolicy,
) -> DeliverabilityVerdict {
    if snapshot.sent_30d < policy.minimum_rate_sample {
        return DeliverabilityVerdict::Healthy;
    }
    let rate = |count: u32| -> u32 {
        u32::try_from(u64::from(count) * 10_000 / u64::from(snapshot.sent_30d.max(1)))
            .unwrap_or(u32::MAX)
    };
    // Complaints first: they are the more expensive signal and the more
    // specific diagnosis, so a workspace failing both should be told about the
    // one that is harder to recover from.
    if rate(snapshot.complaints_30d) >= u32::from(policy.max_complaint_rate_basis_points) {
        return DeliverabilityVerdict::HaltComplaintRate;
    }
    if rate(snapshot.bounces_30d) >= u32::from(policy.max_bounce_rate_basis_points) {
        return DeliverabilityVerdict::HaltBounceRate;
    }
    DeliverabilityVerdict::Healthy
}

/// A day-one workspace's ceiling, for callers that have no snapshot yet.
#[must_use]
pub const fn cold_start_ceiling(policy: DeliverabilityPolicy) -> u32 {
    policy.ramp_start_touches
}

/// Whether the ramp has anything left to give against the operator's ceiling.
#[must_use]
pub fn ramp_complete(
    snapshot: DeliverabilitySnapshot,
    policy: DeliverabilityPolicy,
    now: OffsetDateTime,
) -> bool {
    ramped_ceiling(snapshot, policy, now) >= snapshot.weekly_third_party_ceiling
}

const SECONDS_PER_DAY: i64 = 86_400;

/// How long until the ramp next widens. `None` once it is finished or halted.
#[must_use]
pub fn next_ramp_step_at(
    snapshot: DeliverabilitySnapshot,
    policy: DeliverabilityPolicy,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    if ramp_complete(snapshot, policy, now) || !verdict(snapshot, policy).sending_allowed() {
        return None;
    }
    let first_sent_at = snapshot.first_sent_at?;
    let step = i64::from(policy.ramp_step_days).max(1);
    let elapsed_days = (now.unix_timestamp() - first_sent_at.unix_timestamp()) / SECONDS_PER_DAY;
    let next_step = elapsed_days / step + 1;
    Some(first_sent_at + Duration::days(next_step * step))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn healthy() -> DeliverabilitySnapshot {
        DeliverabilitySnapshot {
            sent_30d: 100,
            bounces_30d: 0,
            complaints_30d: 0,
            first_sent_at: Some(now() - Duration::days(14)),
            weekly_third_party_ceiling: 40,
        }
    }

    #[test]
    fn a_workspace_that_has_never_sent_starts_at_the_bottom() {
        let policy = DeliverabilityPolicy::default();
        let cold = DeliverabilitySnapshot {
            sent_30d: 0,
            first_sent_at: None,
            ..healthy()
        };
        assert_eq!(
            ramped_ceiling(cold, policy, now()),
            policy.ramp_start_touches
        );
        assert_eq!(cold_start_ceiling(policy), policy.ramp_start_touches);
    }

    #[test]
    fn the_ramp_widens_with_time_and_stops_at_the_operators_ceiling() {
        let policy = DeliverabilityPolicy::default();
        // Two weeks in: start plus two steps.
        assert_eq!(
            ramped_ceiling(healthy(), policy, now()),
            policy.ramp_start_touches + policy.ramp_step_touches * 2
        );
        let seasoned = DeliverabilitySnapshot {
            first_sent_at: Some(now() - Duration::days(365)),
            ..healthy()
        };
        assert_eq!(
            ramped_ceiling(seasoned, policy, now()),
            seasoned.weekly_third_party_ceiling,
            "the operator's budget is the ceiling and the ramp never passes it"
        );
        assert!(ramp_complete(seasoned, policy, now()));
        assert!(!ramp_complete(healthy(), policy, now()));
    }

    #[test]
    fn a_complaint_rate_stops_sending_before_a_bounce_rate_does() {
        // Providers weigh one complaint like dozens of bounces, and the two
        // have different fixes: a bounce rate is a list problem, a complaint
        // rate is a message problem.
        let policy = DeliverabilityPolicy::default();
        let complained = DeliverabilitySnapshot {
            complaints_30d: 1,
            bounces_30d: 50,
            ..healthy()
        };
        assert_eq!(
            verdict(complained, policy),
            DeliverabilityVerdict::HaltComplaintRate
        );
        let bounced = DeliverabilitySnapshot {
            bounces_30d: 5,
            ..healthy()
        };
        assert_eq!(
            verdict(bounced, policy),
            DeliverabilityVerdict::HaltBounceRate
        );
        assert!(!verdict(bounced, policy).sending_allowed());
    }

    #[test]
    fn a_halt_closes_the_ceiling_rather_than_being_reported_afterwards() {
        // By the time a digest is read the reputation is already spent.
        let policy = DeliverabilityPolicy::default();
        let bounced = DeliverabilitySnapshot {
            bounces_30d: 20,
            ..healthy()
        };
        assert_eq!(ramped_ceiling(bounced, policy, now()), 0);
        assert_eq!(next_ramp_step_at(bounced, policy, now()), None);
    }

    #[test]
    fn a_rate_below_the_sample_floor_is_not_a_rate() {
        // Two bounces out of three is not a sixty-seven per cent bounce rate,
        // it is three sends. Halting on it teaches an operator to raise the
        // threshold, which is the opposite of the point.
        let policy = DeliverabilityPolicy::default();
        let tiny = DeliverabilitySnapshot {
            sent_30d: 3,
            bounces_30d: 2,
            complaints_30d: 1,
            ..healthy()
        };
        assert_eq!(verdict(tiny, policy), DeliverabilityVerdict::Healthy);
    }

    #[test]
    fn only_a_hard_bounce_finishes_an_address() {
        assert!(DeliveryFault::HardBounce.suppresses_target());
        assert!(!DeliveryFault::SoftBounce.suppresses_target());
        // A complaint is about the message, and suppression there is the
        // operator's call rather than a rule that quietly deletes a
        // relationship.
        assert!(!DeliveryFault::Complaint.suppresses_target());
        for fault in [
            DeliveryFault::HardBounce,
            DeliveryFault::SoftBounce,
            DeliveryFault::Complaint,
        ] {
            assert_eq!(DeliveryFault::parse(fault.as_str()), Some(fault));
        }
    }

    #[test]
    fn the_next_widening_is_a_real_date_while_the_ramp_is_still_climbing() {
        let policy = DeliverabilityPolicy::default();
        let at = next_ramp_step_at(healthy(), policy, now()).expect("still climbing");
        assert!(at > now(), "the next step is ahead, not behind");
        assert_eq!(
            at,
            healthy().first_sent_at.expect("has sent") + Duration::days(21)
        );
    }
}
