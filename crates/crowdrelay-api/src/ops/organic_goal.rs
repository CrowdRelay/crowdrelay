#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrganicGoalRequest {
    target: i64,
    declared_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrganicExclusionRequest {
    reason: Option<String>,
    declared_by: String,
}

fn organic_actor_ok(actor: &str) -> bool {
    !actor.trim().is_empty() && actor.chars().count() <= 120
}

fn organic_mutation_response(
    result: Result<
        crowdrelay_infra::organic_goal::GoalMutation,
        crowdrelay_infra::organic_goal::GoalError,
    >,
    request: Option<String>,
) -> Response {
    use crowdrelay_infra::organic_goal::GoalError;
    match result {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(GoalError::Conflict) => Problem::conflict(request).private().into_response(),
        Err(GoalError::NotFound) => Problem::not_found(request).private().into_response(),
        Err(GoalError::Database(error)) => OpsError::sqlx(error).into_response(request),
    }
}

pub async fn organic_goal(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    match run_limited(
        &state.read_budget,
        state.ops.operation_timeout,
        crowdrelay_infra::organic_goal::read(
            &state.ops.pool,
            state.ops.workspace_id().into_uuid(),
            now,
        ),
    )
    .await
    {
        Ok(goal) => private_json(
            StatusCode::OK,
            json!({
                "as_of":now.format(&time::format_description::well_known::Rfc3339).ok(),
                "timezone":"Europe/Warsaw","configured":goal.is_some(),"goal":goal,
                "credit_basis":"canonical_first_arrival_with_live_publication_and_tracked_click",
                "causal_claim":false,"staff_detection":"explicit_operator_exclusion",
            }),
        ),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

pub async fn declare_organic_goal(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(request): Json<OrganicGoalRequest>,
) -> Response {
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    if !(1..=1000000).contains(&request.target) || !organic_actor_ok(&request.declared_by) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let rid = request_id(&headers);
    let result = crowdrelay_infra::organic_goal::declare(
        &state.ops.pool,
        state.ops.workspace_id().into_uuid(),
        request.target,
        request.declared_by.trim(),
        &key,
        rid.as_deref(),
        OffsetDateTime::now_utc(),
    )
    .await;
    organic_mutation_response(result, rid)
}

pub async fn set_organic_exclusion(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(fan_id): Path<Uuid>,
    Json(request): Json<OrganicExclusionRequest>,
) -> Response {
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    if !organic_actor_ok(&request.declared_by)
        || request
            .reason
            .as_deref()
            .is_some_and(|r| !matches!(r, "staff" | "manual_invite" | "test"))
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let rid = request_id(&headers);
    let result = crowdrelay_infra::organic_goal::exclude(
        &state.ops.pool,
        state.ops.workspace_id().into_uuid(),
        fan_id,
        request.reason.as_deref(),
        request.declared_by.trim(),
        &key,
        rid.as_deref(),
        OffsetDateTime::now_utc(),
    )
    .await;
    organic_mutation_response(result, rid)
}
