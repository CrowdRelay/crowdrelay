// Venue/promoter discovery ingress and the operator's confirmation queue.
//
// Transport shape only. Whether a prospect is admissible is decided by the
// domain rule on write, whether it becomes a bookable target is decided by an
// operator's confirm click. Nothing here screens and nothing here writes SQL.

/// A discovery sweep posts bounded batches so one call can never become an
/// unbounded transaction; the repository enforces the same bound again.
const MAX_BOOKING_CANDIDATE_BATCH: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingCandidateRequest {
    target_kind: BookingTargetKind,
    display_name: String,
    city_slug: Option<String>,
    route_kind: crowdrelay_domain::booking_discovery::RouteKind,
    route_value: String,
    source: String,
    source_reference: String,
    evidence: Option<String>,
    fit_basis_points: u16,
    #[serde(default)]
    paid_to_apply: bool,
    route_is_published: bool,
    capacity: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingCandidateBatchRequest {
    candidates: Vec<BookingCandidateRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingCandidateQuery {
    status: Option<String>,
    limit: Option<u32>,
}

fn valid_text(value: &str, max_length: usize) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && value.chars().count() <= max_length
}

fn validate_booking_candidate(
    request: BookingCandidateRequest,
) -> Result<BookingCandidateInput, ()> {
    if !valid_text(&request.display_name, 200)
        || !valid_text(&request.route_value, 2048)
        || !valid_text(&request.source, 64)
        || !valid_text(&request.source_reference, 2048)
        || request.fit_basis_points > 10_000
        || request
            .city_slug
            .as_ref()
            .is_some_and(|slug| !valid_text(slug, 80))
        || request
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.chars().count() > 4000)
    {
        return Err(());
    }
    Ok(BookingCandidateInput {
        kind: request.target_kind,
        display_name: request.display_name,
        city_slug: request.city_slug,
        route_kind: request.route_kind,
        route_value: request.route_value,
        source: request.source,
        source_reference: request.source_reference,
        evidence: request.evidence,
        fit_basis_points: request.fit_basis_points,
        paid_to_apply: request.paid_to_apply,
        route_is_published: request.route_is_published,
        capacity: request.capacity,
    })
}

async fn ingest_batch(
    state: AppState,
    headers: HeaderMap,
    batch: BookingCandidateBatchRequest,
) -> Response {
    if batch.candidates.is_empty() || batch.candidates.len() > MAX_BOOKING_CANDIDATE_BATCH {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut candidates = Vec::with_capacity(batch.candidates.len());
    for candidate in batch.candidates {
        match validate_booking_candidate(candidate) {
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
        .ingest_booking_candidates(
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

/// Executor-facing ingestion on the internal surface with the commerce key —
/// the same authority split Phase 9 made for playlist sweeps.
pub async fn ingest_booking_candidates_internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BookingCandidateBatchRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    ingest_batch(state, headers, request).await
}

/// Operator import path for prospects found by hand.
pub async fn ingest_booking_candidates_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BookingCandidateBatchRequest>,
) -> Response {
    ingest_batch(state, headers, request).await
}

/// One human confirmation turns a published email route into a bookable
/// target. This is the button that ends "the negotiation machinery starves".
pub async fn confirm_booking_candidate(
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
        .confirm_booking_candidate(
            state.ops.workspace_id(),
            OutreachOpportunityId::from_uuid(candidate_id),
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn list_booking_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BookingCandidateQuery>,
) -> Response {
    if query
        .status
        .as_ref()
        .is_some_and(|status| !matches!(status.as_str(), "admitted" | "refused" | "promoted"))
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .list_booking_candidates(
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
