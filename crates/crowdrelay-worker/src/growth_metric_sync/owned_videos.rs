//! Owned YouTube video statistics.
//!
//! `video_source_sync` projects the connected channel's uploads into
//! `content_sources` rows keyed `youtube:{video_id}`. Those are not fanbase
//! connections and not release plans, so neither sweep reaches them — yet
//! a fresh upload's view curve is the cleanest signal the channel-lift
//! measurement has. This sweep declares a `content_source` series per
//! counter and reads it on an age-dependent cadence: hourly while the video
//! is young enough for the curve to still be moving, daily after that.

use std::time::Duration;

use super::{
    GrowthMetricSyncError, GrowthMetricSyncWorker, YoutubeVideosResponse, normalize_count,
    record_subject_metric_point,
};
use sqlx::types::Uuid;
use time::OffsetDateTime;

/// Cadence while an upload is young: counters move fast enough that a daily
/// read would flatten the launch curve into one step. `pub(super)` so the
/// run loop can cap its sleep at it while fresh videos exist.
pub(super) const FRESH_VIDEO_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Cadence once the curve has settled.
const SETTLED_VIDEO_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Age at which a video flips from the fresh cadence to the settled one.
const FRESH_VIDEO_AGE: Duration = Duration::from_secs(72 * 60 * 60);
/// Sources older than this stop being measured at all — the launch window
/// is over and the long tail moves too slowly to matter.
const OWNED_VIDEO_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The Data API accepts up to fifty ids per call, so one batch is the cap.
const MAX_OWNED_VIDEOS_PER_CYCLE: i64 = 50;

/// How long between reads for a video of the given age: hourly through the
/// fresh window, daily after. Pure so the SQL's CASE answer can be checked
/// against it without standing up a database.
pub(super) fn owned_video_interval(age: Duration) -> Duration {
    if age < FRESH_VIDEO_AGE {
        FRESH_VIDEO_INTERVAL
    } else {
        SETTLED_VIDEO_INTERVAL
    }
}

impl GrowthMetricSyncWorker {
    /// Sweeps due owned YouTube videos. "Due" is per-source: a fresh upload
    /// needs a point within the last hour, a settled one within the last
    /// day, and either way only if it is still inside the thirty-day window.
    pub(super) async fn sync_owned_video_stats(&self) -> Result<(), GrowthMetricSyncError> {
        let api_key = match self.youtube_api_key.as_ref() {
            Some(key) => key,
            // No key disables the sweep, the same posture the release-video
            // and channel sweeps take toward a missing credential.
            None => return Ok(()),
        };
        // Each row carries its newest point's timestamp; the due predicate
        // belongs to the query — LIMIT ahead of it let the fifty freshest
        // videos permanently starve every stale one. The age rule mirrors
        // `owned_video_interval`; the Rust check below stays as the guard.
        let sources = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                String,
                OffsetDateTime,
                Option<OffsetDateTime>,
            ),
        >(
            r#"
            SELECT cs.id, cs.workspace_id, cs.source_key, cs.title, cs.occurred_at,
                   latest.captured_at
            FROM content_sources cs
            LEFT JOIN LATERAL (
                SELECT max(p.captured_at) AS captured_at
                FROM growth_metric_points p
                JOIN growth_metric_series s ON s.id = p.series_id
                WHERE s.workspace_id = cs.workspace_id
                  AND s.subject_kind = 'content_source'
                  AND s.subject_id = cs.id
            ) latest ON true
            WHERE cs.active
              AND cs.source_kind = 'video'
              AND cs.source_key LIKE 'youtube:%'
              AND cs.occurred_at > now() - ($1::bigint * interval '1 second')
              AND (latest.captured_at IS NULL
                   OR latest.captured_at <= now() - (
                       CASE WHEN cs.occurred_at > now() - ($3::bigint * interval '1 second')
                            THEN $4 ELSE $5 END * interval '1 second'))
            ORDER BY latest.captured_at ASC NULLS FIRST, cs.occurred_at DESC
            LIMIT $2
            "#,
        )
        .bind(OWNED_VIDEO_WINDOW.as_secs() as i64)
        .bind(MAX_OWNED_VIDEOS_PER_CYCLE)
        .bind(FRESH_VIDEO_AGE.as_secs() as i64)
        .bind(FRESH_VIDEO_INTERVAL.as_secs() as i64)
        .bind(SETTLED_VIDEO_INTERVAL.as_secs() as i64)
        .fetch_all(&self.pool)
        .await?;

        let now = OffsetDateTime::now_utc();
        let mut jobs: Vec<(Uuid, Uuid, String, String)> = Vec::new(); // (workspace, source, title, video_id)
        for (source_id, workspace_id, source_key, title, occurred_at, latest) in sources {
            let age = (now - occurred_at).unsigned_abs();
            let due = match latest {
                Some(captured_at) => {
                    (now - captured_at).unsigned_abs() >= owned_video_interval(age)
                }
                // No point yet — first sight is always due.
                None => true,
            };
            if !due {
                continue;
            }
            let Some(video_id) = source_key.strip_prefix("youtube:") else {
                continue;
            };
            if !is_video_id(video_id) {
                continue;
            }
            jobs.push((workspace_id, source_id, title, video_id.to_string()));
        }
        if jobs.is_empty() {
            return Ok(());
        }

        let ids: Vec<&str> = jobs.iter().map(|job| job.3.as_str()).collect();
        let url = format!(
            "https://www.googleapis.com/youtube/v3/videos?part=statistics&id={}&key={api_key}",
            ids.join(",")
        );
        // The API key is a query parameter — strip the URL off every error
        // the same way the other YouTube sweeps do.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "YouTube videos API returned HTTP {}",
                response.status()
            )));
        }
        let body: YoutubeVideosResponse = response.json().await?;
        let observed_at = OffsetDateTime::now_utc();
        for item in &body.items {
            for (workspace_id, source_id, title, _) in jobs.iter().filter(|job| job.3 == item.id) {
                for (metric_key, value) in [
                    ("views", item.statistics.view_count.as_ref()),
                    ("likes", item.statistics.like_count.as_ref()),
                    ("comments", item.statistics.comment_count.as_ref()),
                ] {
                    let Some(value) = value.and_then(normalize_count) else {
                        continue;
                    };
                    // The display name is also the series' human label — one
                    // per counter, so views, likes and comments stay
                    // distinguishable, capped at 120 characters without
                    // splitting a multi-byte one.
                    let display_name: String = format!("YouTube {metric_key} — {title}")
                        .chars()
                        .take(120)
                        .collect();
                    record_subject_metric_point(
                        &self.pool,
                        *workspace_id,
                        "youtube",
                        metric_key,
                        "content_source",
                        *source_id,
                        &display_name,
                        value,
                        observed_at,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Whether any active owned YouTube video is still inside the fresh
    /// window. The run loop caps its sleep at one hour while that is true so
    /// the hourly cadence survives a quiet connection schedule. Returns the
    /// workspace it found — the answer only needs existence, but selecting a
    /// workspace_id column keeps the row's tenant scope explicit.
    pub(super) async fn has_fresh_owned_videos(&self) -> bool {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT cs.workspace_id
            FROM content_sources cs
            WHERE cs.active
              AND cs.source_kind = 'video'
              AND cs.source_key LIKE 'youtube:%'
              AND cs.occurred_at > now() - ($1::bigint * interval '1 second')
            LIMIT 1
            "#,
        )
        .bind(FRESH_VIDEO_AGE.as_secs() as i64)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .is_some()
    }
}

/// The eleven-character id a `youtube:` source key carries. Video ids come
/// from the channel feed, so this is a belt over the suspenders: anything
/// malformed answers false rather than reaching the request URL.
/// `pub(super)` for the traffic sweep, which validates the same key shape.
pub(super) fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cadence flips at exactly 72 hours: anything younger reads
    /// hourly, anything older reads daily.
    #[test]
    fn owned_video_interval_flips_at_seventy_two_hours() {
        let hour = Duration::from_secs(60 * 60);
        let day = Duration::from_secs(24 * 60 * 60);
        assert_eq!(owned_video_interval(Duration::ZERO), hour);
        assert_eq!(owned_video_interval(hour * 71), hour);
        assert_eq!(owned_video_interval(hour * 72), day);
        assert_eq!(owned_video_interval(day * 29), day);
    }

    /// A real `youtube:` source key yields its eleven-character video id;
    /// anything malformed is refused rather than interpolated into the
    /// request URL.
    #[test]
    fn video_id_gate_accepts_real_keys_only() {
        assert!(is_video_id("abc12345-XY"));
        assert!(!is_video_id("../watch?v=x"));
        assert!(!is_video_id("short"));
        assert!(!is_video_id("abc12345-XY?f"));
        assert!(!is_video_id(""));
    }
}
