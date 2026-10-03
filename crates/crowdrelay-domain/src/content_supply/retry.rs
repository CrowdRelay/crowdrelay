//! Retry bookkeeping for the supply chain's artifact requests — what a
//! failed request means and when it may be asked for again.
//!
//! Split out of `content_supply.rs` for the source-size ratchet; items are
//! re-exported there so paths stay `content_supply::…`.

use serde::Serialize;
use time::{Duration, OffsetDateTime};

use super::ContentArtifactKind;

/// An artifact whose earlier requests failed, for one source version.
///
/// A failed request is neither done nor in flight, so the evaluator asks for
/// the same artifact again next cycle — under the same idempotency key, which
/// dedupes onto the failed action and writes nothing. Before this existed one
/// failure froze the source's whole chain for good: on 2026-09-25 two
/// `live_listing` requests hit Discord's rate limit (HTTP 429, five requests
/// in one second to one webhook), and neither source got another artifact.
/// A retry carries its attempt number in its key, waits out a growing delay,
/// and stops after [`MAX_ARTIFACT_ATTEMPTS`]; the chain skips an artifact in
/// either state rather than waiting on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FailedArtifact {
    pub artifact: ContentArtifactKind,
    pub failures: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_failed_at: OffsetDateTime,
}

/// A whole delivery lane that has stopped answering for one artifact kind.
///
/// `FailedArtifact` remembers failures per source version, so a lane that is
/// down is rediscovered by every source and every new version: on 2026-10-02
/// the live artifact workflow began answering `artifact_surface_unavailable`
/// and the chain minted 10–20 requests an hour into it, each failing the same
/// way, for 16 hours — and every failure was one the brain could not learn
/// from, because nothing about the artifact was wrong. The lane's streak is
/// the fact; this carries it so the chain stops asking until one probe gets
/// through.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactLaneOutage {
    pub artifact: ContentArtifactKind,
    /// Terminal lane-level failures in a row with no success between them.
    pub consecutive_failures: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_failed_at: OffsetDateTime,
}

/// Failures in a row before a lane counts as down. Three is one source's
/// whole attempt budget, so a single unlucky source never trips it.
pub const LANE_OUTAGE_THRESHOLD: u32 = 4;
/// The longest the chain waits between probes of a down lane.
const LANE_PROBE_CAP_MINUTES: i64 = 6 * 60;

impl ArtifactLaneOutage {
    /// When one request may be sent to find out whether the lane is back:
    /// an hour after the failure that tripped it, doubling per further
    /// failure, never more than six hours.
    #[must_use]
    pub fn next_probe_at(&self) -> OffsetDateTime {
        let doublings = self
            .consecutive_failures
            .saturating_sub(LANE_OUTAGE_THRESHOLD)
            .min(4);
        let minutes = (60_i64 << doublings).min(LANE_PROBE_CAP_MINUTES);
        self.last_failed_at + Duration::minutes(minutes)
    }
}

/// Whether an executor `error_kind` says the lane failed rather than this
/// artifact: the destination could not take anything, whatever was sent.
#[must_use]
pub fn is_lane_failure(error_kind: &str) -> bool {
    matches!(
        error_kind,
        "artifact_surface_unavailable" | "artifact_delivery_missing" | "provider_rejected"
    )
}

/// Requests per artifact and source version, the first included.
pub const MAX_ARTIFACT_ATTEMPTS: u32 = 3;

/// How long after its latest failure an artifact may be asked for again: 30
/// minutes after the first failure, an hour after the second.
#[must_use]
pub fn artifact_retry_due(failed: &FailedArtifact) -> OffsetDateTime {
    retry_due_at(failed.failures, failed.last_failed_at)
}

/// A community relay dispatch that failed for one source — the action
/// itself, or the drafting task it queued.
///
/// Same repair the artifact chain and surge lanes got: a failed relay is
/// neither delivered nor in flight, and re-raising it under the same key
/// dedupes onto the dead action. That is worse than it looks — the target
/// loader treats a failed dispatch as a spent turn yet counts it toward no
/// `last_draft_at`, so the community re-enters selection first
/// (`NULLS FIRST`) and burns a per-post relay slot forever while emitting
/// nothing. The retry carries its attempt number in the key and stops after
/// [`MAX_ARTIFACT_ATTEMPTS`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelayLaneFailure {
    /// The synced source whose draft dispatch into the community failed.
    pub source_id: crate::ContentSourceId,
    pub failures: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_failed_at: OffsetDateTime,
}

impl RelayLaneFailure {
    /// The same growing delay as [`artifact_retry_due`].
    #[must_use]
    pub fn retry_due(&self) -> OffsetDateTime {
        retry_due_at(self.failures, self.last_failed_at)
    }
}

fn retry_due_at(failures: u32, last_failed_at: OffsetDateTime) -> OffsetDateTime {
    let doublings = failures.saturating_sub(1).min(4);
    last_failed_at + Duration::minutes(30 * (1_i64 << doublings))
}
