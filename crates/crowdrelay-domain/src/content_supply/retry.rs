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

/// Requests per artifact and source version, the first included.
pub const MAX_ARTIFACT_ATTEMPTS: u32 = 3;

/// How long after its latest failure an artifact may be asked for again: 30
/// minutes after the first failure, an hour after the second.
#[must_use]
pub fn artifact_retry_due(failed: &FailedArtifact) -> OffsetDateTime {
    let doublings = failed.failures.saturating_sub(1).min(4);
    failed.last_failed_at + Duration::minutes(30 * (1_i64 << doublings))
}
