//! Instagram insights for the band's own recent posts: how many people a
//! post reached, and — for a reel — how long they watched.
//!
//! Likes and comments say a post was liked. Reach turns that into a rate
//! (engagement per thousand reached), which is fair to a post the platform
//! showed to fewer people; average watch time is what a hook in the first
//! seconds actually moves. Both feed `content_supply::resonates_for_communities`.
//!
//! Read sparingly: only posts from the last 14 days, and a post's insights at
//! most every six hours — about four reads a post a day, never on every sync.
//! Needs `instagram_manage_insights`; without it the read answers 4xx, which
//! is logged and skipped, never retried in a loop.

use super::*;

/// Insights stop being read after this age: the numbers have settled.
const INSIGHTS_MAX_AGE_DAYS: i64 = 14;
/// Least time between two insight reads of one post.
const INSIGHTS_REFRESH_HOURS: i64 = 6;

#[derive(Debug, Deserialize)]
struct InsightsPage {
    data: Vec<InsightMetric>,
}

#[derive(Debug, Deserialize)]
struct InsightMetric {
    name: String,
    values: Vec<InsightValue>,
}

#[derive(Debug, Deserialize)]
struct InsightValue {
    value: serde_json::Value,
}

/// The metrics asked for a media type. Asking for a metric a type does not
/// have fails the whole request, so the sets are per type.
fn metrics_for(media_type: Option<&str>) -> &'static str {
    match media_type {
        Some("VIDEO") => "reach,saved,shares,views,ig_reels_avg_watch_time",
        _ => "reach,saved,shares",
    }
}

/// The insights a post's resonance reads, as stored in its metadata.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PostInsights {
    pub reach: Option<i64>,
    pub saves: Option<i64>,
    pub shares: Option<i64>,
    pub views: Option<i64>,
    /// Milliseconds, as Instagram reports `ig_reels_avg_watch_time`.
    pub avg_watch_ms: Option<i64>,
}

fn read_insights(page: InsightsPage) -> PostInsights {
    let mut insights = PostInsights::default();
    for metric in page.data {
        let value = metric.values.first().and_then(|v| v.value.as_i64());
        match metric.name.as_str() {
            "reach" => insights.reach = value,
            "saved" => insights.saves = value,
            "shares" => insights.shares = value,
            "views" => insights.views = value,
            "ig_reels_avg_watch_time" => insights.avg_watch_ms = value,
            _ => {}
        }
    }
    insights
}

impl SocialPostSourceSyncWorker {
    /// Refreshes one Instagram post's insights when it is recent and its last
    /// read is stale. Best-effort: failures are logged, the sync continues.
    pub(super) async fn refresh_instagram_insights(
        &self,
        media_id: &str,
        media_type: Option<&str>,
        posted_at: Option<OffsetDateTime>,
    ) {
        let Some(token) = self.facebook_page_access_token.as_deref() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        if posted_at.is_none_or(|at| now - at > time::Duration::days(INSIGHTS_MAX_AGE_DAYS)) {
            return;
        }
        let source_key = format!("instagram:{media_id}");
        let last: Option<Option<OffsetDateTime>> = sqlx::query_scalar(
            r#"
            SELECT (metadata->>'insights_at')::timestamptz
            FROM content_sources
            WHERE workspace_id = $1 AND source_kind = 'social_post' AND source_key = $2
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);
        if last
            .flatten()
            .is_some_and(|at| now - at < time::Duration::hours(INSIGHTS_REFRESH_HOURS))
        {
            return;
        }
        let response = match self
            .http_client
            .get(format!("{GRAPH_API_BASE}/{media_id}/insights"))
            .query(&[("metric", metrics_for(media_type)), ("access_token", token)])
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(error = %error.without_url(), "instagram insights read failed");
                return;
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                status = %response.status(),
                "instagram insights refused (instagram_manage_insights missing?)"
            );
            return;
        }
        let insights = match response.json::<InsightsPage>().await {
            Ok(page) => read_insights(page),
            Err(error) => {
                tracing::warn!(error = %error.without_url(), "instagram insights unreadable");
                return;
            }
        };
        let written = sqlx::query(
            r#"
            UPDATE content_sources
            SET metadata = metadata || jsonb_strip_nulls(jsonb_build_object(
                    'reach', $3::bigint,
                    'saves', $4::bigint,
                    'shares', $5::bigint,
                    'views', $6::bigint,
                    'avg_watch_ms', $7::bigint,
                    'insights_at', to_jsonb(now())
                ))
            WHERE workspace_id = $1 AND source_kind = 'social_post' AND source_key = $2
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(insights.reach)
        .bind(insights.saves)
        .bind(insights.shares)
        .bind(insights.views)
        .bind(insights.avg_watch_ms)
        .execute(&self.pool)
        .await;
        if let Err(error) = written {
            tracing::warn!(error = %error, "instagram insights write failed");
        }
    }
}

#[cfg(test)]
mod insights_tests {
    use super::*;

    #[test]
    fn reels_ask_for_watch_time_and_photos_do_not() {
        assert!(metrics_for(Some("VIDEO")).contains("ig_reels_avg_watch_time"));
        assert!(!metrics_for(Some("IMAGE")).contains("views"));
        assert!(!metrics_for(Some("CAROUSEL_ALBUM")).contains("ig_reels_avg_watch_time"));
    }

    #[test]
    fn the_graph_answer_reads_into_named_numbers() {
        let page: InsightsPage = serde_json::from_value(serde_json::json!({
            "data": [
                { "name": "reach", "values": [{ "value": 1840 }] },
                { "name": "saved", "values": [{ "value": 31 }] },
                { "name": "ig_reels_avg_watch_time", "values": [{ "value": 6120 }] },
                { "name": "something_new", "values": [{ "value": 1 }] }
            ]
        }))
        .expect("insights page");
        assert_eq!(
            read_insights(page),
            PostInsights {
                reach: Some(1840),
                saves: Some(31),
                shares: None,
                views: None,
                avg_watch_ms: Some(6120),
            }
        );
    }
}
