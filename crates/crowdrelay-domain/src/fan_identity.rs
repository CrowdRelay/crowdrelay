//! The identity spine (§4e-5): a fan is a person reachable through several
//! verified identifiers, not a single email string. Duplicates are surfaced
//! as merge *candidates* for a human decision; merges are explicit and
//! reversible; an unmerged record stays unmerged rather than being guessed.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// A verified identifier kind attached to a fan. The set is closed and
/// mirrors the `fan_identifiers.kind` CHECK — extend both together.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierKind {
    /// A normalized email address.
    Email,
    /// A Signal installation id.
    SignalInstall,
}

impl IdentifierKind {
    /// The canonical string stored in `fan_identifiers.kind`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::SignalInstall => "signal_install",
        }
    }

    /// Parses a stored kind back into the enum; unknown values are not kinds.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "email" => Some(Self::Email),
            "signal_install" => Some(Self::SignalInstall),
            _ => None,
        }
    }
}

/// How the system came to suspect two fan records are one person. Recorded on
/// the candidate so the human deciding sees *why*, not just *that*.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateEvidenceKind {
    /// A fan checked in while a ticket order for the same event carries
    /// another fan's buyer email. The buyer may not be the attendee, so this
    /// is deliberately weaker — still a human's call either way.
    OrderEmailVsCheckin,
    /// One Signal installation was linked to two different fans — the same
    /// device identified itself as two people. Strong, but shared devices
    /// exist, so still a human's call.
    SharedSignalInstall,
}

impl CandidateEvidenceKind {
    /// The canonical string stored inside the candidate's `evidence` payload.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrderEmailVsCheckin => "order_email_vs_checkin",
            Self::SharedSignalInstall => "shared_signal_install",
        }
    }
}

/// Lifecycle of a merge candidate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Waiting on a human decision.
    Pending,
    /// Resolved by a merge.
    Merged,
    /// A human decided these are two people.
    Dismissed,
}

impl CandidateStatus {
    /// The canonical string stored in `fan_merge_candidates.status`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Merged => "merged",
            Self::Dismissed => "dismissed",
        }
    }
}

/// Orders a fan pair canonically (smaller id first) so a candidate for the
/// same two fans can't be recorded in both directions.
pub fn canonical_pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b { (a, b) } else { (b, a) }
}

/// Why a merge request was refused. Each variant maps to one HTTP problem;
/// they are refused at the domain boundary so a bad merge is impossible to
/// express, not just unlikely.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MergeError {
    /// A fan cannot merge into itself.
    #[error("a fan cannot be merged into itself")]
    SameFan,
    /// One of the fans does not exist in this workspace.
    #[error("fan not found")]
    FanNotFound,
    /// The fan being merged away is already merged — a tombstone has nothing
    /// left to give and re-merging would orphan its audit trail.
    #[error("the merged fan is already merged into another fan")]
    AlreadyMerged,
    /// The survivor is itself merged — a tombstone cannot absorb another
    /// identity. Merge into the fan it points at instead.
    #[error("the surviving fan is itself merged")]
    SurvivorMerged,
    /// The fan being merged away is the survivor of an open merge. Chain
    /// merges would make an out-of-order unmerge silently lose rows, so
    /// the new identity merges into the root survivor instead.
    #[error("the merged fan is the survivor of an open merge")]
    MergedFanIsSurvivor,
}

/// Validates a merge request against the two fans' states.
pub fn validate_merge(
    survivor_id: Uuid,
    survivor_status: &str,
    merged_id: Uuid,
    merged_status: &str,
) -> Result<(), MergeError> {
    if survivor_id == merged_id {
        return Err(MergeError::SameFan);
    }
    if survivor_status == "merged" {
        return Err(MergeError::SurvivorMerged);
    }
    if merged_status == "merged" {
        return Err(MergeError::AlreadyMerged);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_pair_orders_either_direction() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(canonical_pair(a, b), (a, b));
        assert_eq!(canonical_pair(b, a), (a, b));
    }

    #[test]
    fn merge_validation_refuses_the_four_bad_shapes() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(
            validate_merge(a, "active", a, "active"),
            Err(MergeError::SameFan)
        );
        assert_eq!(
            validate_merge(a, "merged", b, "active"),
            Err(MergeError::SurvivorMerged)
        );
        assert_eq!(
            validate_merge(a, "active", b, "merged"),
            Err(MergeError::AlreadyMerged)
        );
        assert!(validate_merge(a, "active", b, "pending").is_ok());
        assert!(validate_merge(a, "suppressed", b, "unsubscribed").is_ok());
    }

    #[test]
    fn identifier_kinds_round_trip() {
        for kind in [IdentifierKind::Email, IdentifierKind::SignalInstall] {
            assert_eq!(IdentifierKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(IdentifierKind::parse("phone"), None);
    }
}
