// Bandsintown tracker counts: the first growth metric CrowdRelay reads from
// outside itself.
//
// Every other series in `growth_metrics` measures something this system already
// owns — tickets sold, signal sessions, merch orders. Those are tills. They say
// whether people bought, never whether anybody new is listening, so the agent
// could not tell a play that worked from a play that did nothing.
//
// A tracker is a person who asked Bandsintown to tell them when this artist
// plays. That is closer to intent than a follower count, and the credential and
// the artist identity are already here for the calendar sync, so this costs one
// extra request per source poll and no new configuration.
//
// Two deliberate properties:
//
// 1. It is non-fatal. A tracker read that fails must never mark the event
//    source as failing — the calendar is what the public site serves, and no
//    metric is worth degrading it for.
// 2. The observation timestamp is truncated to the hour. Bandsintown reports no
//    observation time of its own, so polling four times an hour would otherwise
//    write four points claiming four distinct observations of one number. The
//    unique constraint on `(workspace_id, series_id, captured_at)` then makes a
//    re-poll inside the same hour a no-op rather than a fabricated data point.

impl EventSyncWorker {
    /// Reads the artist's public tracker count and appends it to that source's
    /// `bandsintown/trackers` series, declaring the series on first sight.
    async fn sync_bandsintown_trackers(
        &self,
        source: &EventSourceRow,
    ) -> Result<i64, EventSyncError> {
        let app_id =
            resolve_bandsintown_app_id(self.bandsintown_api_key.as_deref(), source.app_id.as_str());
        let mut url = Url::parse(&format!(
            "https://rest.bandsintown.com/artists/{}",
            encode_path_segment(&source.artist_name)
        ))
        .map_err(|_| EventSyncError::InvalidSource)?;
        url.query_pairs_mut().append_pair("app_id", app_id);

        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| EventSyncError::ProviderUnavailable)?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(EventSyncError::ProviderAuthentication(
                response.status().as_u16(),
            ));
        }
        if !response.status().is_success() {
            return Err(EventSyncError::ProviderStatus(response.status().as_u16()));
        }

        let body = read_limited_body(response).await?;
        let artist = serde_json::from_slice::<BandsintownArtist>(&body)
            .map_err(|_| EventSyncError::InvalidProviderPayload)?;
        // Bandsintown answers an unknown artist with 200 and a body carrying no
        // tracker count. Treating that as zero would record a real collapse
        // from a lookup miss, so an absent count is a payload error and the
        // series simply receives no point for this hour.
        let trackers = normalize_tracker_count(artist.tracker_count)
            .ok_or(EventSyncError::InvalidProviderPayload)?;

        record_bandsintown_trackers(&self.pool, source, trackers, OffsetDateTime::now_utc()).await?;
        Ok(trackers)
    }
}

/// The dead-feed threshold for the trackers series, not its polling cadence.
///
/// Points can land as often as hourly, but declaring an hourly expectation
/// would report a single missed poll as a dead feed. A day of total silence is
/// a real finding; a gap between two polls is not.
const TRACKER_SERIES_INTERVAL_HOURS: i32 = 24;

#[derive(Debug, Deserialize)]
struct BandsintownArtist {
    tracker_count: Option<serde_json::Value>,
}

/// Accepts the count only where it is a whole, non-negative number.
///
/// The field arrives as JSON with no schema guarantee, and the points table
/// refuses negatives, so a malformed value is rejected here rather than turned
/// into a constraint violation that would fail the whole sync.
fn normalize_tracker_count(value: Option<serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::Number(number) => number.as_i64().filter(|count| *count >= 0),
        // Some provider responses quote the number. Read it, but never parse a
        // float or an arbitrary string into a metric.
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok().filter(|c| *c >= 0),
        _ => None,
    }
}

async fn record_bandsintown_trackers(
    pool: &PgPool,
    source: &EventSourceRow,
    trackers: i64,
    observed_at: OffsetDateTime,
) -> Result<(), EventSyncError> {
    // The series is scoped to the event source, not to the workspace: a
    // workspace may sync more than one artist, and a workspace-level
    // `bandsintown/trackers` series would interleave two artists' numbers into
    // one timeline that no trend calculation could untangle afterwards.
    sqlx::query(
        r#"
        WITH series AS (
            INSERT INTO viryaos_growth_metric_series (
                workspace_id, platform, metric_key, subject_kind, subject_id,
                display_name, direction, value_tier, expected_interval_hours, active
            )
            VALUES (
                $1, 'bandsintown', 'trackers', 'event_source', $2,
                left('Bandsintown trackers — ' || $3, 120),
                'higher_is_better', 'intermediate', $4, true
            )
            ON CONFLICT (workspace_id, platform, metric_key, subject_kind, subject_id)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                active = true
            RETURNING id
        )
        INSERT INTO viryaos_growth_metric_points (
            workspace_id, series_id, captured_at, value, source
        )
        SELECT $1, series.id, date_trunc('hour', $5::timestamptz), $6, 'event_sync'
        FROM series
        ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING
        "#,
    )
    .bind(source.workspace_id)
    .bind(source.id)
    .bind(&source.artist_name)
    .bind(TRACKER_SERIES_INTERVAL_HOURS)
    .bind(observed_at)
    .bind(trackers)
    .execute(pool)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

#[cfg(test)]
mod tracker_tests {
    use super::*;

    #[test]
    fn whole_non_negative_counts_are_accepted() {
        assert_eq!(
            normalize_tracker_count(Some(serde_json::json!(1_284))),
            Some(1_284)
        );
        assert_eq!(normalize_tracker_count(Some(serde_json::json!(0))), Some(0));
    }

    #[test]
    fn quoted_counts_are_read() {
        assert_eq!(
            normalize_tracker_count(Some(serde_json::json!("1284"))),
            Some(1_284)
        );
    }

    #[test]
    fn absent_count_is_not_zero() {
        // The failure this guards: an unknown artist answers 200 with no
        // count, and recording zero would read as every tracker leaving.
        assert_eq!(normalize_tracker_count(None), None);
        assert_eq!(normalize_tracker_count(Some(serde_json::Value::Null)), None);
    }

    #[test]
    fn malformed_counts_are_refused() {
        assert_eq!(normalize_tracker_count(Some(serde_json::json!(-4))), None);
        assert_eq!(normalize_tracker_count(Some(serde_json::json!(12.5))), None);
        assert_eq!(
            normalize_tracker_count(Some(serde_json::json!("many"))),
            None
        );
        assert_eq!(normalize_tracker_count(Some(serde_json::json!({}))), None);
    }

    #[test]
    fn artist_payload_reads_the_tracker_count() {
        let artist: BandsintownArtist = serde_json::from_slice(
            br#"{"id":"1","name":"VIRYA","tracker_count":842,"upcoming_event_count":3}"#,
        )
        .expect("artist payload parses");
        assert_eq!(normalize_tracker_count(artist.tracker_count), Some(842));
    }
}
