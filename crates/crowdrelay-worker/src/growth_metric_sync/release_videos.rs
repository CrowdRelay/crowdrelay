//! Release-bound video statistics.
//!
//! Release plans are not fanbase connections — they have no lease row and no
//! synced-platform answer — but a plan whose `listen_url` points at a YouTube
//! video has a measurable timeline the channel-lift measurement reads. This
//! sweep declares a `release_plan` series per counter the Data API reports
//! and records a point per plan per sync interval, due on the series'
//! staleness the same way a connection's sync is due on its own.

use super::{
    GrowthMetricSyncError, GrowthMetricSyncWorker, SYNC_INTERVAL, normalize_count,
    record_subject_metric_point,
};
use serde::Deserialize;
use sqlx::types::Uuid;
use time::OffsetDateTime;

impl GrowthMetricSyncWorker {
    /// Sweeps active release plans whose listen URL is a YouTube video. The
    /// API accepts up to fifty ids per call, so plans batch into one request
    /// rather than one request each.
    pub(super) async fn sync_release_video_stats(&self) -> Result<(), GrowthMetricSyncError> {
        let api_key = match self.youtube_api_key.as_ref() {
            Some(key) => key,
            // No key means the whole sweep is disabled, not failed — the same
            // posture sync_youtube takes toward a missing credential.
            None => return Ok(()),
        };
        let plans = sqlx::query_as::<_, (Uuid, Uuid, String, Option<String>)>(
            r#"
            SELECT plan.id, plan.workspace_id, plan.title, plan.listen_url
            FROM release_plans AS plan
            WHERE plan.active AND plan.listen_url IS NOT NULL
              -- Only YouTube destinations carry a video the Data API can
              -- read; a Spotify or storefront URL would stay due forever
              -- and be re-selected every cycle.
              AND (plan.listen_url ILIKE '%youtube%' OR plan.listen_url ILIKE '%youtu.be%')
              AND plan.release_at BETWEEN now() - INTERVAL '90 days'
                                      AND now() + INTERVAL '180 days'
              AND NOT EXISTS (
                  SELECT 1
                  FROM growth_metric_points p
                  JOIN growth_metric_series s ON s.id = p.series_id
                  WHERE s.workspace_id = plan.workspace_id
                    AND s.subject_kind = 'release_plan'
                    AND s.subject_id = plan.id
                    AND p.captured_at > now() - ($1::bigint * interval '1 second')
              )
            ORDER BY plan.release_at DESC
            LIMIT 20
            "#,
        )
        .bind(SYNC_INTERVAL.as_secs() as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut jobs: Vec<(Uuid, Uuid, String, String)> = Vec::new(); // (workspace, release, title, video_id)
        for (release_id, workspace_id, title, listen_url) in plans {
            let Some(url) = listen_url else { continue };
            let Some(video_id) = extract_youtube_video_id(&url) else {
                continue;
            };
            jobs.push((workspace_id, release_id, title, video_id));
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
        // the same way sync_youtube does.
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
            // Two plans can point at the same video — each gets its own
            // series, so the point lands under every matching plan rather
            // than only the first.
            for (workspace_id, release_id, title, _) in jobs.iter().filter(|job| job.3 == item.id) {
                for (metric_key, value) in [
                    ("views", item.statistics.view_count.as_ref()),
                    ("likes", item.statistics.like_count.as_ref()),
                    ("comments", item.statistics.comment_count.as_ref()),
                ] {
                    let Some(value) = value.and_then(normalize_count) else {
                        continue;
                    };
                    record_subject_metric_point(
                        &self.pool,
                        *workspace_id,
                        "youtube",
                        metric_key,
                        "release_plan",
                        *release_id,
                        &format!("YouTube {metric_key} — {title}"),
                        value,
                        observed_at,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct YoutubeVideosResponse {
    items: Vec<YoutubeVideoItem>,
}

#[derive(Debug, Deserialize)]
struct YoutubeVideoItem {
    id: String,
    statistics: YoutubeVideoStatistics,
}

#[derive(Debug, Deserialize)]
struct YoutubeVideoStatistics {
    #[serde(rename = "viewCount")]
    view_count: Option<serde_json::Value>,
    #[serde(rename = "likeCount")]
    like_count: Option<serde_json::Value>,
    #[serde(rename = "commentCount")]
    comment_count: Option<serde_json::Value>,
}

/// The eleven-character video id out of the YouTube link shapes a release
/// plan's `listen_url` can realistically carry — watch URLs, short links,
/// shorts, embeds and lives. Anything else answers None rather than guessing:
/// a Spotify or own-site listen URL is not this sweep's subject.
fn extract_youtube_video_id(url: &str) -> Option<String> {
    let url = url.trim();
    for marker in ["watch?v=", "youtu.be/", "/shorts/", "/embed/", "/live/"] {
        if let Some(pos) = url.find(marker) {
            let Some(rest) = url.get(pos + marker.len()..) else {
                continue;
            };
            let id: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every link shape a release plan can realistically carry yields the
    /// same eleven-character id — watch, share, shorts, embed, live — and a
    /// non-YouTube listen URL yields nothing rather than a guess.
    #[test]
    fn youtube_video_id_parses_every_link_shape() {
        for url in [
            "https://www.youtube.com/watch?v=abc12345-XY",
            "https://youtu.be/abc12345-XY?t=42",
            "https://www.youtube.com/shorts/abc12345-XY",
            "https://www.youtube.com/embed/abc12345-XY",
            "https://www.youtube.com/live/abc12345-XY?si=zz",
        ] {
            assert_eq!(
                extract_youtube_video_id(url).as_deref(),
                Some("abc12345-XY"),
                "{url}"
            );
        }
        for url in [
            "https://open.spotify.com/track/abc12345-XY",
            "https://band.example/listen",
            "https://www.youtube.com/watch?v=short",
        ] {
            assert_eq!(extract_youtube_video_id(url), None, "{url}");
        }
    }
}
