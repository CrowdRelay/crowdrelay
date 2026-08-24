//! Free-reach pitching, run as waves rather than as one-offs.
//!
//! Everything the outreach context already does — relevance, cadence,
//! follow-ups, decline cooldowns — stays exactly where it is. What is missing
//! is the shape an operator can actually work with.
//!
//! Forty individual approvals is how a human stops approving. The queue fills,
//! the operator clicks through the first six, and the rest sit there being
//! evidence that the agent is working while nothing goes out. So a batch of
//! pitches for one release, of one kind, is drafted together, sealed together
//! and approved in one move — and the batch is sized to the operator's own
//! weekly third-party budget rather than to however many targets happen to
//! exist.
//!
//! Three rules make that safe.
//!
//! 1. **A wave never widens the envelope.** It is a way of presenting work the
//!    budget already allowed, not a way of asking for more. A wave sized above
//!    the remaining weekly budget is a wave that would have to be throttled
//!    halfway through, which is worse than a smaller wave.
//! 2. **Sealing is what makes it reviewable.** A wave still being drafted must
//!    not be approvable: approving a batch that grows afterwards is approving
//!    something nobody read.
//! 3. **The anchor decides when it stops mattering.** A release-week pitch sent
//!    a month late is a different, worse message, so an unsealed or unapproved
//!    wave past its anchor expires and says so.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{EventId, ReleasePlanId, outreach::OutreachTargetKind};

/// What a wave is pitched around.
///
/// A tour leg has no row of its own, so the leg is represented by the show it
/// hangs off. That is honest as long as it is written down: a wave anchored on
/// an event is a wave about the run that show belongs to, and the day the
/// schema grows real legs this becomes a third variant rather than a
/// reinterpretation of this one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaveAnchor {
    Release { release_id: ReleasePlanId },
    Event { event_id: EventId },
}

impl WaveAnchor {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release { .. } => "release",
            Self::Event { .. } => "event",
        }
    }

    #[must_use]
    pub fn id(self) -> uuid::Uuid {
        match self {
            Self::Release { release_id } => release_id.into_uuid(),
            Self::Event { event_id } => event_id.into_uuid(),
        }
    }
}

/// Where a wave has got to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveState {
    /// The agent is still choosing who is in it. Not approvable: a batch that
    /// grows after somebody reads it is a batch nobody read.
    Drafting,
    /// Closed for changes and in front of a human.
    Sealed,
    /// The operator said yes, once, to all of it.
    Approved,
    /// The moment it was written for has gone.
    Expired,
}

impl WaveState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drafting => "drafting",
            Self::Sealed => "sealed",
            Self::Approved => "approved",
            Self::Expired => "expired",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "drafting" => Some(Self::Drafting),
            "sealed" => Some(Self::Sealed),
            "approved" => Some(Self::Approved),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    #[must_use]
    pub const fn settled(self) -> bool {
        matches!(self, Self::Approved | Self::Expired)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FreeReachPolicy {
    /// Most pitches one wave may hold, before the weekly budget is considered.
    /// A ceiling on how much an operator is ever asked to read in one go.
    pub max_pitches_per_wave: u16,
    /// Fewest pitches worth sealing. One pitch is not a wave, and presenting it
    /// as one trains an operator to click approve without reading.
    pub min_pitches_per_wave: u16,
    /// How long before the anchor a wave opens. Far enough out that a reply is
    /// still useful, near enough that the pitch is about something real.
    pub open_lead_hours: u32,
    /// How long the agent keeps adding to an open wave before sealing it.
    /// Bounded, because a wave that waits for one more good target never seals.
    pub drafting_hours: u32,
}

impl Default for FreeReachPolicy {
    fn default() -> Self {
        Self {
            // Deliberately small. The first thing an operator does with a wave
            // that works is raise this, and that is a row update.
            max_pitches_per_wave: 12,
            min_pitches_per_wave: 3,
            open_lead_hours: 42 * 24,
            drafting_hours: 7 * 24,
        }
    }
}

/// One wave as the database holds it, with the outside facts its next decision
/// needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct WaveSnapshot {
    pub anchor: WaveAnchor,
    pub target_kind: OutreachTargetKind,
    pub state: WaveState,
    pub opened_at: OffsetDateTime,
    /// When the thing being pitched happens. Past it, an unapproved wave is a
    /// worse message than none.
    pub anchor_at: OffsetDateTime,
    /// Pitches already drafted into this wave.
    pub pitches: u16,
    /// Targets that would pass the outreach rules right now and are not already
    /// in the wave.
    pub eligible_targets: u32,
    /// What is left of the operator's rolling weekly third-party budget. A wave
    /// never widens it: this is presentation, not permission.
    pub third_party_budget_remaining: u32,
    /// True when the anchor is still a reason to pitch — a released record, a
    /// show still on.
    pub anchor_active: bool,
}

/// What the wave wants to do next.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum WaveDecision {
    /// Draft one more pitch into it.
    AddPitch,
    /// Close it for changes and put it in front of a human.
    Seal,
    /// The moment has gone, or it never had enough in it to be worth reading.
    Expire { reason: WaveExpiry },
    /// Nothing to do this cycle.
    Hold(WaveHold),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveExpiry {
    /// The release came out, or the show happened, with the wave unapproved.
    AnchorPassed,
    /// The anchor stopped being a reason to pitch at all.
    AnchorWithdrawn,
    /// The drafting window closed with too few pitches to be worth a human's
    /// attention.
    TooFewPitches,
}

impl WaveExpiry {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnchorPassed => "anchor_passed",
            Self::AnchorWithdrawn => "anchor_withdrawn",
            Self::TooFewPitches => "too_few_pitches",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveHold {
    /// Waiting on a human, or already settled.
    NotOurs,
    /// Nobody left to pitch, and the drafting window has not closed. A target
    /// screened in tomorrow still belongs in this wave.
    NoEligibleTargets,
    /// The wave is as big as the operator's remaining weekly budget allows.
    /// Sealing early would be right if the window were closing, and it is not.
    BudgetReached,
}

/// How big this wave may get.
///
/// The lower of the operator's per-wave ceiling and what is left of their
/// rolling weekly third-party budget. A wave never widens the envelope: sized
/// above the remaining budget it would be throttled halfway through, which
/// leaves an operator having approved forty pitches and watched six go out.
#[must_use]
pub fn wave_capacity(snapshot: WaveSnapshot, policy: FreeReachPolicy) -> u16 {
    let budget = u16::try_from(snapshot.third_party_budget_remaining).unwrap_or(u16::MAX);
    policy.max_pitches_per_wave.min(budget)
}

/// Decides the wave's next move.
///
/// Order is the rule. Withdrawal and a passed anchor beat everything, because a
/// release-week pitch sent a month late is a different and worse message.
/// Sealing beats adding, so a wave at capacity stops growing rather than
/// drifting past the budget it was sized against. And an empty target pool
/// holds rather than sealing, because a target screened in tomorrow still
/// belongs in this wave.
#[must_use]
pub fn evaluate_wave(
    snapshot: WaveSnapshot,
    policy: FreeReachPolicy,
    now: OffsetDateTime,
) -> WaveDecision {
    if snapshot.state.settled() {
        return WaveDecision::Hold(WaveHold::NotOurs);
    }
    if !snapshot.anchor_active {
        return WaveDecision::Expire {
            reason: WaveExpiry::AnchorWithdrawn,
        };
    }
    if now.unix_timestamp() >= snapshot.anchor_at.unix_timestamp() {
        return WaveDecision::Expire {
            reason: WaveExpiry::AnchorPassed,
        };
    }
    // A sealed wave is a human's. The agent neither adds to it nor withdraws
    // it while somebody is reading.
    if matches!(snapshot.state, WaveState::Sealed) {
        return WaveDecision::Hold(WaveHold::NotOurs);
    }

    let capacity = wave_capacity(snapshot, policy);
    let window_closed = now.unix_timestamp()
        >= (snapshot.opened_at + Duration::hours(i64::from(policy.drafting_hours)))
            .unix_timestamp();

    if snapshot.pitches >= capacity {
        return if snapshot.pitches >= policy.min_pitches_per_wave {
            WaveDecision::Seal
        } else {
            // The budget itself is smaller than the smallest wave worth
            // reading. Nothing here can fix that, and pretending otherwise
            // would put a two-pitch batch in front of somebody.
            WaveDecision::Expire {
                reason: WaveExpiry::TooFewPitches,
            }
        };
    }
    if window_closed {
        return if snapshot.pitches >= policy.min_pitches_per_wave {
            WaveDecision::Seal
        } else {
            WaveDecision::Expire {
                reason: WaveExpiry::TooFewPitches,
            }
        };
    }
    if snapshot.eligible_targets == 0 {
        return WaveDecision::Hold(WaveHold::NoEligibleTargets);
    }
    WaveDecision::AddPitch
}

/// Whether an anchor is worth opening a wave for.
#[must_use]
pub fn wave_is_worth_opening(
    anchor_active: bool,
    hours_until_anchor: i64,
    eligible_targets: u32,
    third_party_budget_remaining: u32,
    policy: FreeReachPolicy,
) -> bool {
    anchor_active
        // Not yet: opening a wave six months out means drafting pitches about
        // a record nobody has heard, and sealing them long before anyone could
        // usefully reply.
        && hours_until_anchor <= i64::from(policy.open_lead_hours)
        // And not too late: a wave needs its whole drafting window inside the
        // run-up, or it seals on the day and expires the next.
        && hours_until_anchor > i64::from(policy.drafting_hours)
        && u32::from(policy.min_pitches_per_wave) <= eligible_targets
        && u32::from(policy.min_pitches_per_wave) <= third_party_budget_remaining
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn drafting() -> WaveSnapshot {
        WaveSnapshot {
            anchor: WaveAnchor::Release {
                release_id: ReleasePlanId::new(),
            },
            target_kind: OutreachTargetKind::Press,
            state: WaveState::Drafting,
            opened_at: now(),
            anchor_at: now() + Duration::days(30),
            pitches: 0,
            eligible_targets: 40,
            third_party_budget_remaining: 10,
            anchor_active: true,
        }
    }

    #[test]
    fn a_wave_is_never_bigger_than_what_is_left_of_the_weekly_budget() {
        // Sized above it, the wave is throttled halfway through and the
        // operator has approved forty pitches to watch six go out.
        let policy = FreeReachPolicy::default();
        let mut snapshot = drafting();
        snapshot.third_party_budget_remaining = 4;
        assert_eq!(wave_capacity(snapshot, policy), 4);
        snapshot.third_party_budget_remaining = 1_000;
        assert_eq!(wave_capacity(snapshot, policy), policy.max_pitches_per_wave);
    }

    #[test]
    fn an_open_wave_with_targets_and_budget_drafts_one_more() {
        assert_eq!(
            evaluate_wave(drafting(), FreeReachPolicy::default(), now()),
            WaveDecision::AddPitch
        );
    }

    #[test]
    fn a_wave_at_capacity_seals_rather_than_drifting_past_its_budget() {
        let policy = FreeReachPolicy::default();
        let mut snapshot = drafting();
        snapshot.pitches = wave_capacity(snapshot, policy);
        assert_eq!(
            evaluate_wave(snapshot, policy, now()),
            WaveDecision::Seal,
            "a wave stops growing at the number it was sized against"
        );
    }

    #[test]
    fn a_sealed_wave_is_a_humans_and_the_agent_leaves_it_alone() {
        // Adding to a batch somebody is reading is approving something nobody
        // read.
        let mut snapshot = drafting();
        snapshot.state = WaveState::Sealed;
        assert_eq!(
            evaluate_wave(snapshot, FreeReachPolicy::default(), now()),
            WaveDecision::Hold(WaveHold::NotOurs)
        );
    }

    #[test]
    fn an_empty_target_pool_waits_instead_of_sealing_early() {
        // A curator screened in tomorrow still belongs in this wave.
        let mut snapshot = drafting();
        snapshot.eligible_targets = 0;
        snapshot.pitches = 4;
        assert_eq!(
            evaluate_wave(snapshot, FreeReachPolicy::default(), now()),
            WaveDecision::Hold(WaveHold::NoEligibleTargets)
        );
    }

    #[test]
    fn the_drafting_window_closes_and_a_big_enough_wave_seals() {
        let policy = FreeReachPolicy::default();
        let mut snapshot = drafting();
        snapshot.pitches = policy.min_pitches_per_wave;
        snapshot.eligible_targets = 0;
        let late = now() + Duration::hours(i64::from(policy.drafting_hours) + 1);
        assert_eq!(evaluate_wave(snapshot, policy, late), WaveDecision::Seal);
    }

    #[test]
    fn a_wave_too_small_to_be_worth_reading_expires_rather_than_seals() {
        // One pitch presented as a wave trains an operator to click approve
        // without reading, which is the thing waves exist to prevent.
        let policy = FreeReachPolicy::default();
        let mut snapshot = drafting();
        snapshot.pitches = policy.min_pitches_per_wave - 1;
        snapshot.eligible_targets = 0;
        let late = now() + Duration::hours(i64::from(policy.drafting_hours) + 1);
        assert_eq!(
            evaluate_wave(snapshot, policy, late),
            WaveDecision::Expire {
                reason: WaveExpiry::TooFewPitches
            }
        );
        // And the same when the budget itself is the constraint.
        let mut starved = drafting();
        starved.third_party_budget_remaining = 1;
        starved.pitches = 1;
        assert_eq!(
            evaluate_wave(starved, policy, now()),
            WaveDecision::Expire {
                reason: WaveExpiry::TooFewPitches
            }
        );
    }

    #[test]
    fn a_passed_or_withdrawn_anchor_beats_everything() {
        let policy = FreeReachPolicy::default();
        let mut full = drafting();
        full.pitches = policy.max_pitches_per_wave;
        let after = full.anchor_at + Duration::hours(1);
        assert_eq!(
            evaluate_wave(full, policy, after),
            WaveDecision::Expire {
                reason: WaveExpiry::AnchorPassed
            },
            "a release-week pitch sent a month late is a worse message than none"
        );
        let mut withdrawn = drafting();
        withdrawn.anchor_active = false;
        assert_eq!(
            evaluate_wave(withdrawn, policy, now()),
            WaveDecision::Expire {
                reason: WaveExpiry::AnchorWithdrawn
            }
        );
    }

    #[test]
    fn an_anchor_is_only_worth_a_wave_inside_its_run_up() {
        let policy = FreeReachPolicy::default();
        // Six months out: pitches about a record nobody has heard, sealed long
        // before anyone could usefully reply.
        assert!(!wave_is_worth_opening(true, 180 * 24, 40, 10, policy));
        // Three days out: the drafting window alone outlasts the run-up.
        assert!(!wave_is_worth_opening(true, 3 * 24, 40, 10, policy));
        assert!(wave_is_worth_opening(true, 30 * 24, 40, 10, policy));
        // Never on a withdrawn anchor, an empty pool, or a spent budget.
        assert!(!wave_is_worth_opening(false, 30 * 24, 40, 10, policy));
        assert!(!wave_is_worth_opening(true, 30 * 24, 1, 10, policy));
        assert!(!wave_is_worth_opening(true, 30 * 24, 40, 1, policy));
    }

    #[test]
    fn every_state_and_reason_survives_a_round_trip() {
        for state in [
            WaveState::Drafting,
            WaveState::Sealed,
            WaveState::Approved,
            WaveState::Expired,
        ] {
            assert_eq!(WaveState::parse(state.as_str()), Some(state));
        }
        for reason in [
            WaveExpiry::AnchorPassed,
            WaveExpiry::AnchorWithdrawn,
            WaveExpiry::TooFewPitches,
        ] {
            assert!(!reason.as_str().is_empty());
        }
    }
}
