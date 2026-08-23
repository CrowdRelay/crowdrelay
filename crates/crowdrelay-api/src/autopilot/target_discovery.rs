// Target discovery ingress and the operator's screening queue.
//
// The adapter calls Spotify and the directories; this layer checks transport
// shape only. Whether a candidate is admissible is decided by the domain, and
// whether it is stored is decided by the repository. Nothing here screens and
// nothing here writes SQL.

/// A discovery sweep posts in bounded batches so one call can never become an
/// unbounded transaction. The repository enforces the same bound; this is the
/// transport-shaped refusal that happens first.
const MAX_CANDIDATE_BATCH: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachCandidateRequest {
    target_kind: OutreachTargetKind,
    display_name: String,
    source: CandidateSource,
    source_reference: String,
    /// The published text the route was read out of. Optional on the wire and
    /// refused by screening when absent, so an adapter that cannot supply it
    /// still reports what it found instead of dropping it silently.
    evidence: Option<String>,
    route_kind: RouteKind,
    route_value: String,
    route_is_published: bool,
    channel_slug: Option<String>,
    fit_basis_points: u16,
    follower_count: Option<u32>,
    engagement_count: Option<u32>,
    #[serde(default)]
    sells_placement: bool,
    #[serde(default)]
    churns_indiscriminately: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachCandidateBatchRequest {
    candidates: Vec<OutreachCandidateRequest>,
    /// What the sweep read before screening anything, where the adapter can
    /// count it. Optional so an adapter that cannot is still a valid client,
    /// and absent on the admin route: a human posting candidates by hand did
    /// not run a sweep and must not be recorded as having run one.
    #[serde(default)]
    sweep: Option<SweepReportRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepReportRequest {
    sources_read: u32,
    items_seen: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionChannelRequest {
    slug: String,
    display_name: String,
    cost_model: ChannelCost,
    submission_url: Option<String>,
    active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachCandidateQuery {
    status: Option<String>,
    limit: Option<u32>,
}

fn valid_candidate_text(value: &str, max_length: usize) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && value.chars().count() <= max_length
}

fn validate_candidate(
    request: OutreachCandidateRequest,
) -> Result<IngestOutreachCandidate, ()> {
    if !valid_candidate_text(&request.display_name, 200)
        || !valid_candidate_text(&request.source_reference, 2048)
        || !valid_candidate_text(&request.route_value, 2048)
        || request.fit_basis_points > 10_000
        || request
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.chars().count() > 4000)
        || request
            .channel_slug
            .as_ref()
            .is_some_and(|slug| !valid_candidate_text(slug, 80))
    {
        return Err(());
    }
    Ok(IngestOutreachCandidate {
        target_kind: request.target_kind,
        display_name: request.display_name,
        source: request.source,
        source_reference: request.source_reference,
        evidence: request.evidence,
        route_kind: request.route_kind,
        route_value: request.route_value,
        route_is_published: request.route_is_published,
        channel_slug: request.channel_slug,
        fit_basis_points: request.fit_basis_points,
        follower_count: request.follower_count,
        engagement_count: request.engagement_count,
        sells_placement: request.sells_placement,
        churns_indiscriminately: request.churns_indiscriminately,
    })
}

/// Executor-facing ingestion.
///
/// Discovery is executor work, so it belongs on the internal surface with the
/// commerce credential every other executor route uses. Requiring the admin key
/// would hand an adapter authority over ninety-seven admin routes to post a
/// list of playlist contacts, which is exactly the blurring the surfaces exist
/// to prevent.
///
/// Unlike the admin route this accepts an **empty** batch. A sweep that read
/// published text and found no usable route is a real answer, and it is the
/// answer the barren-sweep back-off is built on: without a way to report it,
/// an adapter can only stay silent, and silence is indistinguishable from an
/// adapter that crashed.
pub async fn ingest_outreach_candidates_internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachCandidateBatchRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    if request.candidates.len() > MAX_CANDIDATE_BATCH {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    // A sweep cannot report fewer items read than candidates reported: every
    // candidate came out of an item. Refusing the incoherent pair keeps the
    // supply rule from reading `items_seen = 0` as a broken adapter on a sweep
    // that plainly found something.
    let sweep_report = match request.sweep {
        Some(sweep) => match validate_sweep_report(&sweep, request.candidates.len()) {
            Ok(report) => Some(report),
            Err(()) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        },
        None => None,
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut candidates = Vec::with_capacity(request.candidates.len());
    for candidate in request.candidates {
        match validate_candidate(candidate) {
            Ok(candidate) => candidates.push(candidate),
            Err(()) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        }
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .ingest_outreach_candidates(
            state.ops.workspace_id(),
            candidates,
            sweep_report,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Bounds mirror migration 0086's CHECK constraints, so an out-of-range claim
/// is refused at the boundary rather than silently clamped into the timeline.
const MAX_SWEEP_SOURCES_READ: u32 = 1_000;
const MAX_SWEEP_ITEMS_SEEN: u32 = 100_000;

fn validate_sweep_report(
    request: &SweepReportRequest,
    candidates: usize,
) -> Result<OutreachSweepReport, ()> {
    if request.sources_read > MAX_SWEEP_SOURCES_READ || request.items_seen > MAX_SWEEP_ITEMS_SEEN {
        return Err(());
    }
    // Reporting candidates out of nothing read is not a coherent sweep.
    let candidates = u32::try_from(candidates).map_err(|_| ())?;
    if request.items_seen < candidates {
        return Err(());
    }
    // Items with no source behind them is the same incoherence one level up.
    if request.sources_read == 0 && request.items_seen > 0 {
        return Err(());
    }
    Ok(OutreachSweepReport {
        sources_read: request.sources_read,
        items_seen: request.items_seen,
    })
}

pub async fn ingest_outreach_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachCandidateBatchRequest>,
) -> Response {
    // A sweep report belongs to an adapter that swept. Accepting one here would
    // let a hand-posted batch answer "what did the last sweep read", which is
    // the one question this route has no standing to answer.
    if request.candidates.is_empty()
        || request.candidates.len() > MAX_CANDIDATE_BATCH
        || request.sweep.is_some()
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut candidates = Vec::with_capacity(request.candidates.len());
    for candidate in request.candidates {
        match validate_candidate(candidate) {
            Ok(candidate) => candidates.push(candidate),
            Err(()) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        }
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .ingest_outreach_candidates(
            state.ops.workspace_id(),
            candidates,
            None,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn list_outreach_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OutreachCandidateQuery>,
) -> Response {
    if query
        .status
        .as_ref()
        .is_some_and(|status| !matches!(status.as_str(), "admitted" | "promoted" | "refused"))
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .list_outreach_candidates(
            state.ops.workspace_id(),
            query.status,
            query.limit.unwrap_or(50),
        )
        .await
    {
        Ok(candidates) => private_json(StatusCode::OK, candidates),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn confirm_outreach_candidate(
    State(state): State<AppState>,
    Path(candidate_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .confirm_outreach_candidate(
            state.ops.workspace_id(),
            candidate_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_submission_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SubmissionChannelRequest>,
) -> Response {
    if !valid_candidate_text(&request.slug, 80)
        || !valid_candidate_text(&request.display_name, 200)
        || request
            .submission_url
            .as_ref()
            .is_some_and(|url| !valid_candidate_text(url, 2048))
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertSubmissionChannel {
        slug: request.slug,
        display_name: request.display_name,
        cost_model: request.cost_model,
        submission_url: request.submission_url,
        active: request.active,
    };
    match state
        .autopilot
        .upsert_submission_channel(
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
