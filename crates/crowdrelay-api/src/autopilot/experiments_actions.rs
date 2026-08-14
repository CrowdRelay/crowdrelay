pub async fn create_experiment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExperimentRequest>,
) -> Response {
    if request.slug.is_empty()
        || request.slug.len() > 128
        || !(2..=8).contains(&request.variants.len())
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
    let command = CreateExperiment {
        slug: request.slug,
        metric: request.metric,
        variants: request
            .variants
            .into_iter()
            .map(|variant| CreateExperimentVariant {
                key: variant.key,
                allocation_basis_points: variant.allocation_basis_points,
            })
            .collect(),
        start: request.start,
    };
    match state
        .autopilot
        .create_experiment(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::CREATED, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn assign_experiment(
    State(state): State<AppState>,
    Path(experiment_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ExperimentAssignmentRequest>,
) -> Response {
    let Ok(experiment_id) = Uuid::parse_str(&experiment_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    if request.assignment_key.trim().is_empty() || request.assignment_key.len() > 200 {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }

    match assign_experiment_variant(
        &state.autopilot,
        state.ops.workspace_id(),
        ExperimentId::from_uuid(experiment_id),
        &request.assignment_key,
    )
    .await
    {
        Ok(assignment) => private_json(StatusCode::OK, assignment),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_experiment_observation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExperimentObservationRequest>,
) -> Response {
    if request.conversions_delta > request.exposures_delta || request.value_minor_delta < 0 {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = ExperimentObservation {
        experiment_id: ExperimentId::from_uuid(request.experiment_id),
        variant_id: ExperimentVariantId::from_uuid(request.variant_id),
        exposures_delta: request.exposures_delta,
        conversions_delta: request.conversions_delta,
        value_minor_delta: request.value_minor_delta,
        observed_at: request.observed_at,
    };
    match state
        .autopilot
        .record_experiment_observation(
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

pub async fn assign_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<AssignActionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let action_id = match Uuid::parse_str(&action_id) {
        Ok(value) => AutopilotActionId::from_uuid(value),
        Err(_) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::unprocessable(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let member_key = payload.member_key.trim().to_ascii_lowercase();
    if member_key.len() < 2
        || member_key.len() > 48
        || !member_key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Problem::unprocessable(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .assign_action(
            state.ops.workspace_id(),
            action_id,
            &member_key,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn approve_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_action(state, headers, action_id, true).await
}

pub async fn cancel_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_action(state, headers, action_id, false).await
}

async fn mutate_action(
    state: AppState,
    headers: HeaderMap,
    action_id: String,
    approve: bool,
) -> Response {
    let action_id = match Uuid::parse_str(&action_id) {
        Ok(value) => AutopilotActionId::from_uuid(value),
        Err(_) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let result = if approve {
        state
            .autopilot
            .approve_action(
                state.ops.workspace_id(),
                action_id,
                &idempotency_key,
                request_id_value.as_ref(),
            )
            .await
    } else {
        state
            .autopilot
            .cancel_action(
                state.ops.workspace_id(),
                action_id,
                &idempotency_key,
                request_id_value.as_ref(),
            )
            .await
    };
    match result {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
