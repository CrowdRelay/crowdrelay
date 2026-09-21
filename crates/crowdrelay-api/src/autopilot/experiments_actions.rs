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
    body: axum::body::Bytes,
) -> Response {
    // Raw bytes rather than `Option<Json<_>>`: a malformed revision body must
    // fail loudly, not silently degrade into an approve of the original draft.
    let (revision, remember) = if body.is_empty() {
        (None, None)
    } else {
        match serde_json::from_slice::<ApproveActionRequest>(&body) {
            Ok(request) => (request.revision, request.remember),
            Err(_) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        }
    };
    // Field names are checkable here — the allowlist is static. Naming the
    // field beats the repository's static refusal, and `RevisionRefusal`
    // already wrote the sentence.
    if let Some(revision) = &revision {
        for field in revision.keys() {
            if !crowdrelay_domain::draft_revision::REVISABLE_FIELDS.contains(&field.as_str()) {
                return Problem::conflict_owned(
                    crowdrelay_domain::draft_revision::RevisionRefusal::FieldNotRevisable {
                        field: field.clone(),
                    }
                    .message()
                    .into(),
                    request_id(&headers),
                )
                .private()
                .into_response();
            }
            if revision[field].trim().is_empty() {
                return Problem::conflict_owned(
                    crowdrelay_domain::draft_revision::RevisionRefusal::FieldEmptied {
                        field: field.clone(),
                    }
                    .message()
                    .into(),
                    request_id(&headers),
                )
                .private()
                .into_response();
            }
        }
    }
    mutate_action(state, headers, action_id, true, revision, remember).await
}

/// `GET /v1/control-plane/autopilot/actions/{action_id}/sent`
///
/// What this action actually said, and to whom (O.4). The overview answers
/// "did it work"; this answers the two questions an operator asks before
/// pressing the button a second time. Both facts have been on disk since the
/// emission was written and nothing read them back.
pub async fn action_sent_record(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(action_id) = Uuid::parse_str(&action_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    match crowdrelay_infra::sent_record::sent_record(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        action_id,
    )
    .await
    {
        Ok(Some(record)) => private_json(StatusCode::OK, record),
        Ok(None) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "sent record read failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// Most actions one call may approve.
///
/// Bounded because the batch runs one approval per id and each is a write.
/// Fifty is comfortably more than a week's queue at the attention budget's
/// twenty, so the cap never stands between an operator and a clear board.
const MAX_APPROVAL_BATCH: usize = 50;

/// `POST /v1/control-plane/autopilot/actions/approve`
///
/// Approve several parked actions in one call.
///
/// # Why this exists
///
/// Approvals expire at 72 hours and the only way to answer one was to answer
/// it alone. Against a queue the agent refills every cycle that is a race the
/// operator loses: measured in production, seven drafts, four approvals, four
/// hundred and twelve opportunities, zero posts published.
///
/// # Why it takes ids and not a filter
///
/// "Approve everything in this context" is the shape that makes an approval
/// meaningless — the operator would be granting authority over work they have
/// not read, which is what the queue exists to prevent. Naming the ids means
/// they saw them.
///
/// # Why it is not atomic
///
/// Partial success is the honest result. Ten approvals where two have already
/// expired should approve eight and say which two did not; refusing all ten
/// because of the two would make the batch less useful than the single call
/// it replaces. Each id carries its own outcome and its own idempotency key
/// derived from the caller's, so a retry re-approves nothing.
pub async fn approve_actions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<ApproveActionsRequest>(&body) else {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    };
    if request.action_ids.is_empty() || request.action_ids.len() > MAX_APPROVAL_BATCH {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);

    let mut results = Vec::with_capacity(request.action_ids.len());
    for action_id in request.action_ids {
        // One key per action, derived from the caller's. A shared key would
        // make the second approval in the batch a replay of the first.
        let Ok(key) =
            IdempotencyKey::parse(format!("{}:{action_id}", idempotency_key.as_str()).as_str())
        else {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        };
        let approved = state
            .autopilot
            .approve_action(
                state.ops.workspace_id(),
                AutopilotActionId::from_uuid(action_id),
                &key,
                request_id_value.as_ref(),
                None,
            )
            .await;
        match approved {
            Ok(mutation) => {
                let remembered = match &request.remember {
                    Some(remember) => {
                        match remember_action_target(
                            &state,
                            AutopilotActionId::from_uuid(action_id),
                            remember,
                        )
                        .await
                        {
                            Ok(outcome) => Some(outcome),
                            Err(problem) => return *problem,
                        }
                    }
                    None => None,
                };
                results.push(serde_json::json!({
                    "action_id": action_id,
                    "approved": true,
                    "mutation": mutation,
                    "remembered": remembered,
                }));
            }
            // A refusal is reported beside its id rather than ending the
            // batch: the operator needs to know which ones did not go.
            Err(error) => results.push(serde_json::json!({
                "action_id": action_id,
                "approved": false,
                "reason": error.to_string(),
            })),
        }
    }
    private_json(StatusCode::OK, serde_json::json!({ "results": results }))
}

pub async fn cancel_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_action(state, headers, action_id, false, None, None).await
}

/// Records that a human handled this finding outside the system.
///
/// The "done ourselves" button, and a first-class outcome rather than a
/// dismissal: an opportunity a human took is a success. The decision leaves
/// the queue and the brief, and any action of it still parked is withdrawn so
/// the agent cannot send what somebody already did by hand.
pub async fn mark_decision_handled_externally(
    State(state): State<AppState>,
    Path(decision_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(decision_id) = Uuid::parse_str(&decision_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .mark_decision_handled_externally(
            state.ops.workspace_id(),
            AutopilotDecisionId::from_uuid(decision_id),
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Approves every pitch in one free-reach wave.
///
/// The whole reason waves exist: an operator reads one batch and says yes once.
/// Approving forty pitches individually is how a human stops approving.
pub async fn approve_outreach_wave(
    State(state): State<AppState>,
    Path(wave_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(wave_id) = Uuid::parse_str(&wave_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .approve_outreach_wave(
            state.ops.workspace_id(),
            wave_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// P.5: one "yes" over a synced post's whole relay ladder — the owned-audience
/// push plus one community relay per admitted community, released together.
pub async fn approve_relay_ladder(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(source_id) = Uuid::parse_str(&source_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .approve_relay_ladder(
            state.ops.workspace_id(),
            source_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// P.4: one "yes" over a show's whole growth ladder — releases the rungs
/// already parked for the event and pre-authorizes the ones not yet decided.
/// The domain's per-lever evidence gates still apply; this only removes the
/// repeated human gate.
pub async fn approve_show_ladder(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(event_id) = Uuid::parse_str(&event_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .approve_show_ladder(
            state.ops.workspace_id(),
            crowdrelay_domain::EventId::from_uuid(event_id),
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Cancels the still-queued rungs a post's relay ladder released. A rung a
/// person approved on its own keeps its approval; a rung already running or
/// finished keeps its record.
pub async fn revoke_relay_ladder(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(source_id) = Uuid::parse_str(&source_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .revoke_relay_ladder(
            state.ops.workspace_id(),
            source_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Withdraws the ladder approval: rungs not yet decided ask individually
/// again, and rungs the ladder released but that have not executed yet are
/// cancelled. A rung approved on its own keeps its approval.
pub async fn revoke_show_ladder(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(event_id) = Uuid::parse_str(&event_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .revoke_show_ladder(
            state.ops.workspace_id(),
            crowdrelay_domain::EventId::from_uuid(event_id),
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

async fn mutate_action(
    state: AppState,
    headers: HeaderMap,
    action_id: String,
    approve: bool,
    revision: Option<std::collections::BTreeMap<String, String>>,
    remember: Option<RememberRequest>,
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
                revision.as_ref(),
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
        Ok(result) => {
            // Only after the approval itself succeeded. A grant written
            // beside a refused approval would be authority the operator
            // never actually gave, and it would outlive the mistake by
            // ninety days.
            let remembered = match remember {
                Some(remember) => {
                    match remember_action_target(&state, action_id, &remember).await {
                        Ok(outcome) => Some(outcome),
                        Err(problem) => return *problem,
                    }
                }
                None => None,
            };
            private_json(
                StatusCode::OK,
                serde_json::json!({ "mutation": result, "remembered": remembered }),
            )
        }
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Writes the standing approval an operator asked for with `remember`.
///
/// The target comes from the action's own payload, never from the request:
/// "approve this and stop asking about it" must not be able to become a grant
/// over something else. An action with no target a grant could cover is
/// reported as such rather than silently approved-without-remembering — the
/// operator asked for two things and got one.
async fn remember_action_target(
    state: &AppState,
    action_id: AutopilotActionId,
    remember: &RememberRequest,
) -> Result<serde_json::Value, Box<Response>> {
    let workspace_id = state.ops.workspace_id().into_uuid();
    let target = crowdrelay_infra::standing_approvals::grant_target_for_action(
        &state.database,
        workspace_id,
        action_id.into_uuid(),
    )
    .await;
    let Ok(target) = target else {
        return Err(Box::new(Problem::service_unavailable(None).into_response()));
    };
    let Some((action_kind, target_key, class)) = target else {
        return Ok(serde_json::json!({
            "granted": false,
            "reason": "this action has no target a standing approval could cover",
        }));
    };
    match crowdrelay_infra::standing_approvals::grant(
        &state.database,
        workspace_id,
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: &action_kind,
            target_key: &target_key,
            class,
            granted_by: "operator:control_plane",
            days: remember
                .days
                .unwrap_or(crowdrelay_domain::standing_approval::DEFAULT_GRANT_DAYS),
            note: remember
                .note
                .as_deref()
                .map(str::trim)
                .filter(|note| !note.is_empty()),
        },
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(view) => Ok(serde_json::json!({ "granted": true, "approval": view })),
        Err(error) => Ok(serde_json::json!({
            "granted": false,
            "reason": error.to_string(),
        })),
    }
}

/// `GET /v1/control-plane/autopilot/community-relays`
///
/// The community-relay batch feed: one row per piece of content's whole
/// community spread — the image, the target list, the cadence — rather than
/// the per-community delivery cards the batches replaced.
pub async fn list_community_relays(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match read(
        &state,
        1,
        state
            .autopilot
            .load_community_relays(state.ops.workspace_id()),
    )
    .await
    {
        Ok(batches) => private_json(StatusCode::OK, batches),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Approves a community relay batch: the content's whole spread at once.
///
/// Deliveries already parked release into the queue; drafts still landing
/// queue under the standing answer without asking again. The executor drips
/// them at `interval_seconds` — the cadence the card showed — and an
/// `interval_seconds` body field overrides it inside the table's floor.
pub async fn approve_community_relay(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(source_id) = Uuid::parse_str(&source_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    // Raw bytes rather than `Option<Json<_>>`: a malformed override must fail
    // loudly, not silently approve at the default cadence.
    let interval_seconds = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<CommunityRelayApproveRequest>(&body) {
            Ok(request) => request.interval_seconds,
            Err(_) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .approve_community_relay(
            state.ops.workspace_id(),
            source_id,
            interval_seconds,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Revokes a community relay batch — "stop the rest of this spread".
///
/// Parked deliveries lose their ask, queued deliveries are cancelled, and the
/// posts still waiting in the drip are cancelled. A post already on Reddit
/// keeps its record.
pub async fn revoke_community_relay(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(source_id) = Uuid::parse_str(&source_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .revoke_community_relay(
            state.ops.workspace_id(),
            source_id,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
