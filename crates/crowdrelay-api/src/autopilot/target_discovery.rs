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

pub async fn ingest_outreach_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachCandidateBatchRequest>,
) -> Response {
    if request.candidates.is_empty() || request.candidates.len() > MAX_CANDIDATE_BATCH {
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
