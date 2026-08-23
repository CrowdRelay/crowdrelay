// External growth-metric ingress and the derived operator read model.
//
// The HTTP layer only checks transport shape: what a movement means is decided
// by the domain, and what is stored is decided by the repository. Nothing here
// computes a delta, and nothing here writes SQL.

/// Observations arriving further ahead of our clock than this are rejected
/// rather than quietly accepted: a future-dated point would sit at the head of
/// the window and distort every derived comparison behind it.
const GROWTH_METRIC_CLOCK_SKEW: Duration = Duration::minutes(5);

/// Refusing observations older than the window they could influence. A deeper
/// backfill is a deliberate operation, not something an adapter should be able
/// to do by retrying with an old timestamp.
const GROWTH_METRIC_MAX_BACKFILL: Duration = Duration::days(400);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowthMetricSeriesRequest {
    platform: MetricPlatform,
    metric_key: String,
    subject_kind: Option<String>,
    subject_id: Option<Uuid>,
    display_name: String,
    direction: MetricDirection,
    value_tier: MetricValueTier,
    expected_interval_hours: u32,
    active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowthMetricPointRequest {
    series_id: Uuid,
    captured_at: OffsetDateTime,
    value: i64,
    source: String,
}

fn valid_metric_text(value: &str, max_length: usize) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.len() == value.len() && value.chars().count() <= max_length
}

fn parse_growth_metric_subject(
    kind: Option<&str>,
    id: Option<Uuid>,
) -> Result<Option<GrowthMetricSubject>, ()> {
    match (kind, id) {
        (None, None) => Ok(None),
        (Some(kind), Some(id)) => match kind {
            "event" => Ok(Some(GrowthMetricSubject::Event(EventId::from_uuid(id)))),
            "city" => Ok(Some(GrowthMetricSubject::City(CityId::from_uuid(id)))),
            "release_plan" => Ok(Some(GrowthMetricSubject::ReleasePlan(
                ReleasePlanId::from_uuid(id),
            ))),
            "content_source" => Ok(Some(GrowthMetricSubject::ContentSource(
                ContentSourceId::from_uuid(id),
            ))),
            "beacon" => Ok(Some(GrowthMetricSubject::Beacon(BeaconId::from_uuid(id)))),
            _ => Err(()),
        },
        // A half-specified subject is a client bug, not a series without one.
        _ => Err(()),
    }
}

fn validate_growth_metric_series(
    request: GrowthMetricSeriesRequest,
) -> Result<UpsertGrowthMetricSeries, ()> {
    if !valid_metric_text(&request.metric_key, 64)
        || !valid_metric_text(&request.display_name, 120)
        || request.expected_interval_hours == 0
        || request.expected_interval_hours > 720
    {
        return Err(());
    }
    let subject =
        parse_growth_metric_subject(request.subject_kind.as_deref(), request.subject_id)?;
    Ok(UpsertGrowthMetricSeries {
        platform: request.platform,
        metric_key: request.metric_key,
        subject,
        display_name: request.display_name,
        direction: request.direction,
        value_tier: request.value_tier,
        expected_interval_hours: request.expected_interval_hours,
        active: request.active,
    })
}

fn validate_growth_metric_point(
    request: GrowthMetricPointRequest,
) -> Result<RecordGrowthMetricPoint, ()> {
    let now = OffsetDateTime::now_utc();
    if !valid_metric_text(&request.source, 64)
        || request.value < 0
        || request.captured_at > now + GROWTH_METRIC_CLOCK_SKEW
        || request.captured_at < now - GROWTH_METRIC_MAX_BACKFILL
    {
        return Err(());
    }
    Ok(RecordGrowthMetricPoint {
        series_id: GrowthMetricSeriesId::from_uuid(request.series_id),
        captured_at: request.captured_at,
        value: request.value,
        source: request.source,
    })
}

pub async fn upsert_growth_metric_series(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GrowthMetricSeriesRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let command = match validate_growth_metric_series(request) {
        Ok(command) => command,
        Err(()) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_growth_metric_series(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_growth_metric_point(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GrowthMetricPointRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let command = match validate_growth_metric_point(request) {
        Ok(command) => command,
        Err(()) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .record_growth_metric_point(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn growth_metric_trends(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_growth_metric_trends(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(series) => private_json(StatusCode::OK, GrowthMetricTrendsResponse { series }),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[derive(Debug, Serialize)]
struct GrowthMetricTrendsResponse {
    series: Vec<GrowthMetricTrendView>,
}

/// What the agent can actually see off-platform.
///
/// A missing feed is the most expensive kind of silence: nothing is stale,
/// nothing is anomalous, and none of that is evidence that anything is fine. It
/// is reported as a state so an operator sees "Spotify: not connected" instead
/// of an empty list.
pub async fn growth_metric_coverage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_growth_metric_trends(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(series) => {
            let observed: Vec<(MetricPlatform, bool)> = series
                .iter()
                .map(|view| (view.platform, view.stale))
                .collect();
            private_json(
                StatusCode::OK,
                GrowthMetricCoverageResponse {
                    platforms: off_platform_coverage(&observed),
                },
            )
        }
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[derive(Debug, Serialize)]
struct GrowthMetricCoverageResponse {
    platforms: Vec<FeedCoverage>,
}

/// The play ledger: what the agent committed to, what it did, and what each
/// number is allowed to prove.
///
/// Every claim carries `claim` and `claim_means`, and an unsettled or
/// unanswerable one carries `evidence_reason` rather than an absent field. A
/// consumer that reads only `effect` will find nothing on a claim that could
/// not be made, which is the intended failure: there is no number there to
/// misread.
pub async fn play_ledger(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_play_ledger(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(plays) => private_json(StatusCode::OK, PlayLedgerResponse { plays }),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[derive(Debug, Serialize)]
struct PlayLedgerResponse {
    plays: Vec<PlayLedgerEntry>,
}
