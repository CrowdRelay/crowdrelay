//! What happened after a playlist pitch, and whether anybody can prove it.
//!
//! A pitcher that counts sends is a spam cannon with a dashboard. The number
//! that matters is placements, and placements are exactly the number somebody
//! has a motive to lie about: the whole playlist-promotion economy runs on
//! screenshots of adds that were removed the following week.
//!
//! So nothing here takes a curator's word for anything.
//!
//! * **A claim is not a placement.** A claimed add that no public read confirms
//!   settles as `ghosted`. Not as a failure of the curator's honesty — it may
//!   be a typo, a private playlist, a track that never propagated — but as a
//!   thing that cannot be counted.
//! * **Verification repeats.** Adding a track for a screenshot and removing it
//!   days later is a known pattern, so the same placement is re-read after a
//!   week and after a month. A placement that goes away inside that window is
//!   `withdrawn`, which is the single strongest scam signal in the system.
//! * **A read that failed is not a read that found nothing.** An unreadable
//!   check settles nothing and consumes no checkpoint. Treating a dead
//!   credential as an absent track would mark honest curators as scammers.
//! * **Withdrawal belongs to the operator, not the playlist.** One person often
//!   runs dozens of playlists, so a withdrawal suppresses the identity behind
//!   it, and their other playlists go back for screening.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::OutreachOpportunityId;

/// Where a claimed placement has got to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementState {
    /// A curator says the track is in. Nothing has confirmed it yet, and it
    /// counts toward nothing until something does.
    Claimed,
    /// Seen in the playlist by a public read. Still provisional: the re-checks
    /// have not all run.
    Verified,
    /// Claimed and never confirmed. Not an accusation, and not a placement.
    Ghosted,
    /// Confirmed and then gone inside the verification window. The strongest
    /// scam signal there is.
    Withdrawn,
}

impl PlacementState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Verified => "verified",
            Self::Ghosted => "ghosted",
            Self::Withdrawn => "withdrawn",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claimed" => Some(Self::Claimed),
            "verified" => Some(Self::Verified),
            "ghosted" => Some(Self::Ghosted),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }

    /// True once nothing further will change it.
    ///
    /// `Verified` is deliberately not terminal: it is what a placement looks
    /// like between checkpoints, and the last checkpoint is what makes it real.
    #[must_use]
    pub const fn settled(self) -> bool {
        matches!(self, Self::Ghosted | Self::Withdrawn)
    }

    /// Whether this may be counted as a placement in any report.
    ///
    /// A placement that cannot be verified never counts toward a result, or the
    /// learning layer is trained on somebody else's marketing.
    #[must_use]
    pub const fn countable(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// What one public read of the playlist found.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementObservation {
    /// The track is in the playlist.
    Present,
    /// The playlist was read and the track is not in it.
    Absent,
    /// The read did not happen — a dead credential, a rate limit, a playlist
    /// that has gone private. Evidence of nothing.
    Unreadable,
}

impl PlacementObservation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Unreadable => "unreadable",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "present" => Some(Self::Present),
            "absent" => Some(Self::Absent),
            "unreadable" => Some(Self::Unreadable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PlacementPolicy {
    /// How long a claim has to be confirmed before it is written off. Generous,
    /// because a track can take a day to propagate and a curator can add it on
    /// the Monday after they said so.
    pub confirm_within_hours: u32,
    /// The re-check schedule after the first confirmation, in hours from the
    /// claim. Adding for a screenshot and removing days later is what these
    /// catch.
    pub first_recheck_hours: u32,
    pub final_recheck_hours: u32,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self {
            confirm_within_hours: 72,
            first_recheck_hours: 7 * 24,
            final_recheck_hours: 30 * 24,
        }
    }
}

/// One claimed placement as the database holds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlacementSnapshot {
    pub opportunity_id: OutreachOpportunityId,
    pub state: PlacementState,
    pub claimed_at: OffsetDateTime,
    /// The last read that actually happened. `Unreadable` reads are not
    /// recorded here, because they are not reads.
    pub last_observation: Option<PlacementObservation>,
    pub last_checked_at: Option<OffsetDateTime>,
    /// Checkpoints already satisfied by a real read: confirmation, first
    /// re-check, final re-check.
    pub checks_completed: u8,
}

/// What to do about a claimed placement now.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlacementDecision {
    /// Read the playlist again. The checkpoint is named so a late worker cannot
    /// satisfy two of them with one read.
    Verify { checkpoint: u8 },
    /// Nothing further will change this.
    Settle { state: PlacementState },
    /// Between checkpoints, or already settled.
    Hold,
}

/// The three checkpoints, in hours from the claim.
#[must_use]
pub const fn checkpoint_hours(checkpoint: u8, policy: PlacementPolicy) -> u32 {
    match checkpoint {
        0 => 0,
        1 => policy.first_recheck_hours,
        _ => policy.final_recheck_hours,
    }
}

/// Decides what a claimed placement needs next.
///
/// Order is the rule. A settled placement is never revisited. A confirmation
/// deadline that passes with nothing confirmed writes the claim off, whatever
/// is scheduled after it. And an absence only means withdrawal *after* a
/// confirmation: before one, the track was never seen there in the first place.
#[must_use]
pub fn evaluate_placement(
    snapshot: PlacementSnapshot,
    policy: PlacementPolicy,
    now: OffsetDateTime,
) -> PlacementDecision {
    if snapshot.state.settled() {
        return PlacementDecision::Hold;
    }
    // An absence after a confirmation is the pattern the whole re-check
    // schedule exists to catch, and it settles the moment it is seen rather
    // than waiting for the last checkpoint.
    if matches!(snapshot.state, PlacementState::Verified)
        && matches!(
            snapshot.last_observation,
            Some(PlacementObservation::Absent)
        )
    {
        return PlacementDecision::Settle {
            state: PlacementState::Withdrawn,
        };
    }
    if matches!(snapshot.state, PlacementState::Claimed) {
        let deadline =
            snapshot.claimed_at + Duration::hours(i64::from(policy.confirm_within_hours));
        if now.unix_timestamp() >= deadline.unix_timestamp() {
            // Not an accusation. A claim nothing confirmed is a claim that
            // cannot be counted, which is a different and smaller statement.
            return PlacementDecision::Settle {
                state: PlacementState::Ghosted,
            };
        }
    }
    // Three real reads and the placement has survived the window it needed to.
    if snapshot.checks_completed >= 3 {
        return PlacementDecision::Hold;
    }
    let checkpoint = snapshot.checks_completed;
    let due =
        snapshot.claimed_at + Duration::hours(i64::from(checkpoint_hours(checkpoint, policy)));
    if now.unix_timestamp() < due.unix_timestamp() {
        return PlacementDecision::Hold;
    }
    PlacementDecision::Verify { checkpoint }
}

/// Folds one completed read into the placement.
///
/// Returns the new state and whether the checkpoint counts. An unreadable check
/// advances nothing: a dead credential is not evidence that a track is gone,
/// and treating it as one would mark honest curators as scammers.
#[must_use]
pub fn apply_observation(
    snapshot: PlacementSnapshot,
    observation: PlacementObservation,
) -> (PlacementState, bool) {
    match observation {
        PlacementObservation::Unreadable => (snapshot.state, false),
        PlacementObservation::Present => (PlacementState::Verified, true),
        PlacementObservation::Absent => match snapshot.state {
            // Seen, then gone. The strongest scam signal there is.
            PlacementState::Verified => (PlacementState::Withdrawn, true),
            // Never seen at all. That is a claim nobody could confirm, not a
            // withdrawal, and the two must not be counted together because they
            // predict differently next release.
            PlacementState::Claimed => (PlacementState::Ghosted, true),
            settled => (settled, false),
        },
    }
}

/// Whether this outcome permanently suppresses the curator behind it.
///
/// Scoped to the identity rather than the playlist: one person often runs
/// dozens, and a withdrawal is a fact about how they operate.
#[must_use]
pub const fn suppresses_identity(state: PlacementState) -> bool {
    matches!(state, PlacementState::Withdrawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn claimed() -> PlacementSnapshot {
        PlacementSnapshot {
            opportunity_id: OutreachOpportunityId::new(),
            state: PlacementState::Claimed,
            claimed_at: now(),
            last_observation: None,
            last_checked_at: None,
            checks_completed: 0,
        }
    }

    #[test]
    fn a_claim_is_read_immediately_and_counts_for_nothing_until_it_is() {
        let snapshot = claimed();
        assert!(!snapshot.state.countable());
        assert_eq!(
            evaluate_placement(snapshot, PlacementPolicy::default(), now()),
            PlacementDecision::Verify { checkpoint: 0 }
        );
    }

    #[test]
    fn a_claim_nobody_confirms_is_ghosted_rather_than_counted() {
        let policy = PlacementPolicy::default();
        let late = now() + Duration::hours(i64::from(policy.confirm_within_hours) + 1);
        assert_eq!(
            evaluate_placement(claimed(), policy, late),
            PlacementDecision::Settle {
                state: PlacementState::Ghosted
            }
        );
        assert!(!PlacementState::Ghosted.countable());
    }

    #[test]
    fn a_present_read_verifies_and_an_absent_one_after_it_withdraws() {
        let (verified, counted) = apply_observation(claimed(), PlacementObservation::Present);
        assert_eq!(verified, PlacementState::Verified);
        assert!(counted);
        assert!(verified.countable());
        let seen = PlacementSnapshot {
            state: verified,
            last_observation: Some(PlacementObservation::Present),
            checks_completed: 1,
            ..claimed()
        };
        let (withdrawn, counted) = apply_observation(seen, PlacementObservation::Absent);
        assert_eq!(withdrawn, PlacementState::Withdrawn);
        assert!(counted);
        assert!(!withdrawn.countable());
        assert!(suppresses_identity(withdrawn));
        assert!(!suppresses_identity(PlacementState::Ghosted));
    }

    #[test]
    fn an_absence_before_any_confirmation_is_ghosted_not_withdrawn() {
        // The two predict differently next release, so counting them together
        // would train the ranker on a distinction it can no longer see.
        let (state, counted) = apply_observation(claimed(), PlacementObservation::Absent);
        assert_eq!(state, PlacementState::Ghosted);
        assert!(counted);
    }

    #[test]
    fn an_unreadable_check_settles_nothing_and_consumes_no_checkpoint() {
        // A dead credential is not evidence that a track is gone. Treating it
        // as one marks honest curators as scammers.
        let seen = PlacementSnapshot {
            state: PlacementState::Verified,
            last_observation: Some(PlacementObservation::Present),
            checks_completed: 1,
            ..claimed()
        };
        let (state, counted) = apply_observation(seen, PlacementObservation::Unreadable);
        assert_eq!(state, PlacementState::Verified);
        assert!(!counted);
    }

    #[test]
    fn the_rechecks_are_a_week_and_a_month_out_and_run_in_order() {
        let policy = PlacementPolicy::default();
        let mut snapshot = PlacementSnapshot {
            state: PlacementState::Verified,
            last_observation: Some(PlacementObservation::Present),
            checks_completed: 1,
            ..claimed()
        };
        // Not yet: a re-check on the same day proves nothing the first read did
        // not already.
        assert_eq!(
            evaluate_placement(snapshot, policy, now() + Duration::hours(1)),
            PlacementDecision::Hold
        );
        let week = now() + Duration::hours(i64::from(policy.first_recheck_hours) + 1);
        assert_eq!(
            evaluate_placement(snapshot, policy, week),
            PlacementDecision::Verify { checkpoint: 1 }
        );
        snapshot.checks_completed = 2;
        assert_eq!(
            evaluate_placement(snapshot, policy, week),
            PlacementDecision::Hold,
            "the month check is not due a week in"
        );
        let month = now() + Duration::hours(i64::from(policy.final_recheck_hours) + 1);
        assert_eq!(
            evaluate_placement(snapshot, policy, month),
            PlacementDecision::Verify { checkpoint: 2 }
        );
        snapshot.checks_completed = 3;
        assert_eq!(
            evaluate_placement(snapshot, policy, month),
            PlacementDecision::Hold,
            "three real reads and the placement has survived its window"
        );
    }

    #[test]
    fn a_settled_placement_is_never_revisited() {
        for state in [PlacementState::Ghosted, PlacementState::Withdrawn] {
            let snapshot = PlacementSnapshot { state, ..claimed() };
            assert!(state.settled());
            assert_eq!(
                evaluate_placement(
                    snapshot,
                    PlacementPolicy::default(),
                    now() + Duration::days(90)
                ),
                PlacementDecision::Hold
            );
            assert_eq!(PlacementState::parse(state.as_str()), Some(state));
        }
        for observation in [
            PlacementObservation::Present,
            PlacementObservation::Absent,
            PlacementObservation::Unreadable,
        ] {
            assert_eq!(
                PlacementObservation::parse(observation.as_str()),
                Some(observation)
            );
        }
    }

    #[test]
    fn a_withdrawal_settles_the_moment_it_is_seen() {
        // Waiting for the last checkpoint to record a withdrawal leaves a
        // curator we already know about being pitched in the meantime.
        let seen = PlacementSnapshot {
            state: PlacementState::Verified,
            last_observation: Some(PlacementObservation::Absent),
            checks_completed: 2,
            ..claimed()
        };
        assert_eq!(
            evaluate_placement(seen, PlacementPolicy::default(), now() + Duration::days(8)),
            PlacementDecision::Settle {
                state: PlacementState::Withdrawn
            }
        );
    }
}
