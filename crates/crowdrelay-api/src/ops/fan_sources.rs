// Fan-source attribution — "where did our fans come from", answered by the
// brain's own evidence ledger rather than by a dashboard guess.
//
// The worker snapshots `attribute_fan_growth` once per cycle (hourly-capped)
// into `fan_source_snapshots`; this read serves those snapshots newest-first.
// The `attribution` blob is the authority — the aggregate columns on the row
// are the same numbers denormalized, returned alongside so a reader never has
// to unpack the blob for the headline figures. Ineffective templates ride in
// `by_template` with zero incremental fans; the surface is for the truth,
// not the highlight reel.

/// One captured attribution reading.
#[derive(Debug, Serialize, FromRow)]
pub(crate) struct FanSourceSnapshot {
    captured_at: OffsetDateTime,
    total_observed_fans: f64,
    total_incremental_fans: f64,
    total_durable_fans: f64,
    resolved_observations: i32,
    /// The complete serialized `FanGrowthAttribution` — by_template,
    /// by_strategy and by_quality breakdowns included.
    attribution: serde_json::Value,
    /// CUSUM regime shifts over the 60-day North Star series, dates resolved
    /// to civil days at write time. `[]` when no shift was detected.
    north_star_shifts: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct FanSourceBrief {
    snapshots: Vec<FanSourceSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FanSourcesQuery {
    limit: Option<i64>,
}

/// Recent attribution snapshots for the workspace, newest first.
///
/// `?limit` is bounded by the shared page-size rules; the default page covers
/// two days of hourly snapshots and the ceiling about four.
pub async fn fan_sources(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<FanSourcesQuery>,
) -> Response {
    let limit = match page_size(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let result = run_limited(
        &state.read_budget,
        state.ops.operation_timeout,
        sqlx::query_as::<_, FanSourceSnapshot>(
            r#"
            SELECT captured_at, total_observed_fans, total_incremental_fans,
                   total_durable_fans, resolved_observations, attribution,
                   north_star_shifts
            FROM fan_source_snapshots
            WHERE workspace_id = $1
            ORDER BY captured_at DESC
            LIMIT $2
            "#,
        )
        .bind(state.ops.workspace_id().into_uuid())
        .bind(limit)
        .fetch_all(&state.ops.pool),
    )
    .await;
    match result {
        Ok(snapshots) => private_json(StatusCode::OK, FanSourceBrief { snapshots }),
        Err(error) => error.into_response(request_id(&headers)),
    }
}
