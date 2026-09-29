//! Owned YouTube video traffic sources, from the Analytics API.
//!
//! The public view counter — what `owned_videos` records — cannot separate
//! what CrowdRelay sent from what a Google Ads campaign bought, so a total
//! says nothing about the machine's own reach. The Analytics API can:
//! `insightTrafficSourceType` splits a video's views by class (EXT_URL for
//! external referrers, ADVERTISING for paid), and
//! `insightTrafficSourceDetail` names the external domains. This sweep
//! records both as `content_source` series — `traffic:{TYPE}` per class and
//! `ext:{domain}` per external referrer — so the video scorecard can report
//! views from surfaces CrowdRelay actually touched instead of a number that
//! lies. Analytics numbers are cumulative from the publish date and lag
//! 24–72 hours; what the API returns is stored and missing days are never
//! fabricated.
//!
//! The read needs the `youtube_account` OAuth grant carrying
//! `yt-analytics.readonly`. With no grant — or a token Google refuses —
//! the sweep warns once per cycle and skips: a missing credential is a
//! posture to report, not an error to loop on.

use std::collections::HashMap;
use std::time::Duration;

use crowdrelay_infra::gdrive::PostgresGDriveRepository;
use serde::Deserialize;
use sqlx::types::Uuid;
use time::OffsetDateTime;

use super::{GrowthMetricSyncError, GrowthMetricSyncWorker, record_subject_metric_point};
use crate::google_oauth::resolve_google_access_token;

/// One Analytics read per video per day: the report is cumulative from
/// publish, so a faster cadence only re-records the same totals.
const TRAFFIC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Sources older than the launch window stop being read — the scorecard
/// closes at fourteen days and the long tail moves too slowly to pay for.
const TRAFFIC_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Per-cycle bound so a channel with a large recent catalog spreads its
/// reads over several days rather than bursting.
const MAX_VIDEOS_PER_CYCLE: i64 = 50;
/// `growth_metric_series.metric_key` tops out at 64 characters; a referral
/// detail whose domain would overflow it is tracking-parameter noise, not
/// a referrer worth a series.
const MAX_METRIC_KEY: usize = 64;

const ANALYTICS_ENDPOINT: &str = "https://youtubeanalytics.googleapis.com/v2/reports";

/// A YouTube Analytics report: `columnHeaders` names the columns and each
/// `rows` entry is `[dimension…, views]` in the same order. Every query
/// here asks for one dimension then `views`, so a row is exactly
/// `[dimension, views]`.
#[derive(Debug, Deserialize)]
struct AnalyticsReport {
    #[serde(default)]
    rows: Vec<Vec<serde_json::Value>>,
}

/// `[traffic source type, views]` pairs from the type report. A row that
/// is short, wrongly shaped, or uncountable is dropped rather than guessed.
fn traffic_type_rows(report: &AnalyticsReport) -> Vec<(String, i64)> {
    report
        .rows
        .iter()
        .filter_map(|row| {
            let kind = row.first()?.as_str()?;
            let views = count(row.get(1)?)?;
            Some((kind.to_string(), views))
        })
        .collect()
}

/// `[external url, views]` pairs from the EXT_URL detail report — the same
/// `[dimension, views]` shape as the type report.
fn traffic_detail_rows(report: &AnalyticsReport) -> Vec<(String, i64)> {
    traffic_type_rows(report)
}

/// Analytics counts arrive as JSON numbers; integer or float both read.
fn count(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_f64().map(|v| v as i64))
}

/// The referring domain of an EXT_URL detail value: scheme, credentials,
/// port, path and a leading `www.` stripped, lowercased. A detail with no
/// usable host — the odd `"(other)"` bucket Analytics emits — answers None
/// instead of minting a junk series.
fn ext_domain(detail: &str) -> Option<String> {
    let mut rest = detail.trim();
    if let Some((_scheme, after)) = rest.split_once("://") {
        rest = after;
    } else if let Some(stripped) = rest.strip_prefix("//") {
        rest = stripped;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port
        .split(':')
        .next()?
        .trim()
        .trim_end_matches('.')
        .to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(host.as_str());
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    Some(host.to_string())
}

/// The series label: metric key plus the video's title, capped where every
/// display name is capped.
fn display_name(metric_key: &str, title: &str) -> String {
    format!("YouTube {metric_key} — {title}")
        .chars()
        .take(120)
        .collect()
}

impl GrowthMetricSyncWorker {
    /// Sweeps due owned YouTube videos for their traffic-source split.
    /// "Due" is per-source: no `traffic:` point in the last day. Runs even
    /// when no connection is due, on the same own-schedule posture as the
    /// release and stats sweeps.
    pub(super) async fn sync_owned_video_traffic(&self) -> Result<(), GrowthMetricSyncError> {
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
                  AND s.metric_key LIKE 'traffic:%'
            ) latest ON true
            WHERE cs.active
              AND cs.source_kind = 'video'
              AND cs.source_key LIKE 'youtube:%'
              AND cs.occurred_at > now() - ($1::bigint * interval '1 second')
            ORDER BY cs.occurred_at DESC
            LIMIT $2
            "#,
        )
        .bind(TRAFFIC_WINDOW.as_secs() as i64)
        .bind(MAX_VIDEOS_PER_CYCLE)
        .fetch_all(&self.pool)
        .await?;

        let now = OffsetDateTime::now_utc();
        // Tokens are resolved per workspace and cached for the cycle, so a
        // workspace without a grant warns once rather than once per video.
        let mut tokens: HashMap<Uuid, Option<String>> = HashMap::new();
        // A refused token ends the cycle: every remaining video would meet
        // the same 401/403, and the warn below already said so once.
        let mut auth_blocked = false;
        for (source_id, workspace_id, source_key, title, occurred_at, latest) in sources {
            if auth_blocked {
                break;
            }
            let due = match latest {
                Some(captured_at) => (now - captured_at).unsigned_abs() >= TRAFFIC_INTERVAL,
                // No traffic point yet — first sight is always due.
                None => true,
            };
            if !due {
                continue;
            }
            let Some(video_id) = source_key
                .strip_prefix("youtube:")
                .filter(|id| super::owned_videos::is_video_id(id))
            else {
                continue;
            };

            let token = match tokens.get(&workspace_id) {
                Some(cached) => cached.clone(),
                None => {
                    let resolved = self.analytics_token(workspace_id).await;
                    tokens.insert(workspace_id, resolved.clone());
                    resolved
                }
            };
            let Some(token) = token else {
                continue;
            };

            match self
                .record_video_traffic(
                    workspace_id,
                    source_id,
                    &title,
                    occurred_at,
                    video_id,
                    &token,
                )
                .await
            {
                Ok(()) => {}
                Err(TrafficError::Forbidden) => {
                    auth_blocked = true;
                }
                Err(TrafficError::Request(error)) => return Err(error),
            }
        }
        Ok(())
    }

    /// The channel owner's grant for Analytics reads, refreshed if due.
    /// None with no grant — and warns once, since that is the whole reason
    /// this cycle has nothing to do for this workspace.
    async fn analytics_token(&self, workspace_id: Uuid) -> Option<String> {
        let grant: Option<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT id, external_account_ref FROM fanbase_connections
            WHERE workspace_id = $1 AND platform = 'youtube_account' AND status = 'connected'
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "youtube analytics: grant lookup failed");
            None
        });
        let Some((connection_id, account_ref)) = grant else {
            tracing::warn!(
                "youtube analytics: no youtube_account grant; traffic sources are unmeasured"
            );
            return None;
        };
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let repo = PostgresGDriveRepository::new(self.pool.clone());
        match resolve_google_access_token(
            &repo,
            &self.http_client,
            &self.response_encryption_key,
            workspace_id,
            connection_id,
            &account_ref,
            "youtube_account",
            env("CROWDRELAY_GOOGLE_ADS_CLIENT_ID").as_deref(),
            env("CROWDRELAY_GOOGLE_ADS_CLIENT_SECRET").as_deref(),
        )
        .await
        {
            Ok(token) => Some(token),
            Err(error) => {
                tracing::warn!(%error, "youtube analytics: grant could not produce an access token");
                None
            }
        }
    }

    /// Fetches both reports for one video and records every row. Values are
    /// cumulative from the publish date, so one call per day per video is
    /// the whole cadence.
    async fn record_video_traffic(
        &self,
        workspace_id: Uuid,
        source_id: Uuid,
        title: &str,
        occurred_at: OffsetDateTime,
        video_id: &str,
        token: &str,
    ) -> Result<(), TrafficError> {
        let start = occurred_at.date().to_string();
        let end = OffsetDateTime::now_utc().date().to_string();
        let types_report = self
            .analytics_report(
                &format!(
                    "{ANALYTICS_ENDPOINT}?ids=channel==MINE&startDate={start}&endDate={end}\
                     &metrics=views&dimensions=insightTrafficSourceType&filters=video=={video_id}"
                ),
                token,
            )
            .await?;
        let detail_report = self
            .analytics_report(
                &format!(
                    "{ANALYTICS_ENDPOINT}?ids=channel==MINE&startDate={start}&endDate={end}\
                     &metrics=views&dimensions=insightTrafficSourceDetail\
                     &filters=video=={video_id};insightTrafficSourceType==EXT_URL\
                     &sort=-views&maxResults=25"
                ),
                token,
            )
            .await?;

        let observed_at = OffsetDateTime::now_utc();
        for (kind, views) in traffic_type_rows(&types_report) {
            let key = format!("traffic:{kind}");
            if key.chars().count() > MAX_METRIC_KEY {
                continue;
            }
            record_subject_metric_point(
                &self.pool,
                workspace_id,
                "youtube",
                &key,
                "content_source",
                source_id,
                &display_name(&key, title),
                views,
                observed_at,
            )
            .await?;
        }
        for (detail, views) in traffic_detail_rows(&detail_report) {
            let Some(domain) = ext_domain(&detail) else {
                continue;
            };
            let key = format!("ext:{domain}");
            if key.chars().count() > MAX_METRIC_KEY {
                continue;
            }
            record_subject_metric_point(
                &self.pool,
                workspace_id,
                "youtube",
                &key,
                "content_source",
                source_id,
                &display_name(&key, title),
                views,
                observed_at,
            )
            .await?;
        }
        Ok(())
    }

    /// One Analytics GET. A 401/403 is the grant's problem — the token is
    /// expired or the consent predates the analytics scope — so it returns
    /// Forbidden and the caller stops the cycle rather than retrying every
    /// video into the same refusal.
    async fn analytics_report(
        &self,
        url: &str,
        token: &str,
    ) -> Result<AnalyticsReport, TrafficError> {
        let response = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            tracing::warn!(
                %status,
                "youtube analytics: token refused; reconnect the youtube_account grant \
                 with the yt-analytics scope"
            );
            return Err(TrafficError::Forbidden);
        }
        if !status.is_success() {
            return Err(TrafficError::Request(GrowthMetricSyncError::ProviderApi(
                format!("YouTube Analytics API returned HTTP {status}"),
            )));
        }
        response.json::<AnalyticsReport>().await.map_err(|error| {
            TrafficError::Request(GrowthMetricSyncError::Http(error.without_url()))
        })
    }
}

/// Forbidden is a lane posture, not an error: it stops the cycle quietly.
/// Request failures propagate like every other sweep's.
enum TrafficError {
    Forbidden,
    Request(GrowthMetricSyncError),
}

impl From<GrowthMetricSyncError> for TrafficError {
    fn from(error: GrowthMetricSyncError) -> Self {
        TrafficError::Request(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the type report actually returns: one row per traffic
    /// source class with its cumulative views.
    #[test]
    fn traffic_type_report_reads_every_class() {
        let report: AnalyticsReport = serde_json::from_str(
            r#"{
                "columnHeaders": [
                    {"name": "insightTrafficSourceType", "columnType": "DIMENSION"},
                    {"name": "views", "columnType": "METRIC"}
                ],
                "rows": [
                    ["EXT_URL", 61],
                    ["ADVERTISING", 203],
                    ["YT_SEARCH", 40],
                    ["NO_LINK_OTHER", 80]
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(
            traffic_type_rows(&report),
            vec![
                ("EXT_URL".to_string(), 61),
                ("ADVERTISING".to_string(), 203),
                ("YT_SEARCH".to_string(), 40),
                ("NO_LINK_OTHER".to_string(), 80),
            ]
        );
        // An empty report — a video Analytics has not seen yet — reads as
        // no rows, not an error.
        let empty: AnalyticsReport = serde_json::from_str(r#"{"rows": []}"#).unwrap();
        assert!(traffic_type_rows(&empty).is_empty());
    }

    /// The EXT_URL detail report returns the referring URL per row.
    #[test]
    fn traffic_detail_report_reads_referrers() {
        let report: AnalyticsReport = serde_json::from_str(
            r#"{
                "rows": [
                    ["https://www.reddit.com/r/music/comments/abc/", 40],
                    ["https://t.me/s/channel", 2],
                    ["discord.com", 1]
                ]
            }"#,
        )
        .unwrap();
        let rows = traffic_detail_rows(&report);
        assert_eq!(rows.len(), 3);
        assert_eq!(ext_domain(&rows[0].0).as_deref(), Some("reddit.com"));
        assert_eq!(ext_domain(&rows[1].0).as_deref(), Some("t.me"));
        assert_eq!(ext_domain(&rows[2].0).as_deref(), Some("discord.com"));
    }

    /// Scheme, credentials, port, path and a leading www. all fold to the
    /// bare domain; a non-URL detail answers None.
    #[test]
    fn ext_domain_normalization() {
        assert_eq!(
            ext_domain("HTTPS://WWW.Reddit.COM/r/music/").as_deref(),
            Some("reddit.com")
        );
        assert_eq!(
            ext_domain("https://m.reddit.com:443/r/x?y=1#z").as_deref(),
            Some("m.reddit.com")
        );
        assert_eq!(
            ext_domain("instagram.com").as_deref(),
            Some("instagram.com")
        );
        assert_eq!(
            ext_domain("https://user:pw@sub.example.co.uk/a/b").as_deref(),
            Some("sub.example.co.uk")
        );
        assert_eq!(ext_domain(""), None);
        assert_eq!(ext_domain("localhost"), None);
        assert_eq!(ext_domain("   "), None);
    }
}
