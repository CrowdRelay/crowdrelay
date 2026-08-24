// Operator-declared targets.
//
// The HTTP layer checks transport shape only. What a target means, and whether
// it is being met, is derived from the series by the domain and the repository;
// nothing here computes progress.

/// A target more than a decade out is not a target, and one in the past cannot
/// be worked toward. Both are almost always a unit or timezone mistake.
const MAX_OBJECTIVE_HORIZON_DAYS: i64 = 3_650;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowthObjectiveRequest {
    platform: MetricPlatform,
    metric_key: String,
    scope_kind: String,
    scope_id: Option<Uuid>,
    direction: MetricDirection,
    target_value: i64,
    deadline: OffsetDateTime,
    declared_by: String,
}

fn parse_objective_scope(kind: &str, id: Option<Uuid>) -> Option<ObjectiveScope> {
    match (kind, id) {
        ("workspace", None) => Some(ObjectiveScope::Workspace),
        ("city", Some(id)) => Some(ObjectiveScope::City(CityId::from_uuid(id))),
        ("event", Some(id)) => Some(ObjectiveScope::Event(EventId::from_uuid(id))),
        ("release_plan", Some(id)) => {
            Some(ObjectiveScope::ReleasePlan(ReleasePlanId::from_uuid(id)))
        }
        _ => None,
    }
}

/// Declares a target on a series and freezes the series' current value as its
/// baseline.
///
/// Re-declaring the same series and scope returns the existing target rather
/// than opening a second one: two live targets on one number is two answers to
/// the same question, and a report would get to pick the friendlier one.
pub async fn declare_growth_objective(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GrowthObjectiveRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let now = OffsetDateTime::now_utc();
    let declared_by = request.declared_by.trim().to_owned();
    let Some(scope) = parse_objective_scope(&request.scope_kind, request.scope_id) else {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    };
    if declared_by.is_empty()
        || declared_by.chars().count() > 120
        || !valid_metric_text(&request.metric_key, 64)
        || request.deadline <= now
        || request.deadline > now + Duration::days(MAX_OBJECTIVE_HORIZON_DAYS)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .declare_growth_objective(
            state.ops.workspace_id(),
            DeclareGrowthObjective {
                platform: request.platform,
                metric_key: request.metric_key,
                scope,
                direction: request.direction,
                target_value: request.target_value,
                deadline: request.deadline,
                declared_by,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Retires a target without deleting it.
///
/// A target that was declared and then removed is exactly what a later review
/// needs to see, so the row stays and stops counting.
pub async fn retire_growth_objective(
    State(state): State<AppState>,
    Path(objective_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .retire_growth_objective(
            state.ops.workspace_id(),
            objective_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Every live target and where its own series says it stands.
///
/// The state is derived on read and never stored. A target with no observation
/// is `unmeasurable` with the reason, not `on_track`, and a deadline that
/// passed unmet is `missed` rather than quietly absent.
pub async fn growth_objectives(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_growth_objectives(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(objectives) => private_json(StatusCode::OK, GrowthObjectivesResponse { objectives }),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[derive(Debug, Serialize)]
struct GrowthObjectivesResponse {
    objectives: Vec<GrowthObjectiveView>,
}
