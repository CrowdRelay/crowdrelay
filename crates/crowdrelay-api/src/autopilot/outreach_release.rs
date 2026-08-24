pub async fn upsert_beacon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BeaconRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.beacon_id.is_none())
        || request.display_name.trim().is_empty()
        || request.display_name.len() > 240
        || request.relationship_score > 100
        || request.relevance_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request
            .contact_email
            .as_ref()
            .is_some_and(|value| !valid_booking_email(value))
        || request.destination_url.as_ref().is_some_and(|value| {
            let trimmed = value.trim();
            trimmed.is_empty() || trimmed.len() > 2048
        })
        || request.source_url.as_ref().is_some_and(|value| {
            let trimmed = value.trim();
            trimmed.is_empty() || trimmed.len() > 2048
        })
        || !request.metadata.is_object()
        // One home city, one way of naming it. Accepting both keys would let a
        // caller send two different cities and never learn which one won.
        || (request.city_id.is_some()
            && request
                .city_slug
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()));
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let confidence = match Confidence::from_basis_points(request.confidence_basis_points) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    // Operator surfaces hold the public city list, which is keyed by slug and
    // deliberately carries no ids. Resolve here through the same cached snapshot
    // the public endpoint serves, so a staff panel can assign a home city
    // without an admin city read of its own.
    let city_id = match request.city_id {
        Some(city_id) => Some(CityId::from_uuid(city_id)),
        None => match request
            .city_slug
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            None => None,
            Some(slug) => {
                let Ok(slug) = CitySlug::parse(slug) else {
                    return Problem::bad_request(request_id(&headers))
                        .private()
                        .into_response();
                };
                match state.acquisition.city_id_for_slug(&slug).await {
                    Ok(Some(city_id)) => Some(city_id),
                    // An unknown city is an operator mistake, not an outage:
                    // fail the write rather than silently creating a beacon
                    // with no home city and a dead local radar.
                    Ok(None) | Err(ListCitiesError::InvalidLimit { .. }) => {
                        return Problem::bad_request(request_id(&headers))
                            .private()
                            .into_response();
                    }
                    Err(ListCitiesError::Repository(error)) => {
                        return repository_problem(error, request_id(&headers));
                    }
                }
            }
        },
    };
    let command = UpsertBeacon {
        beacon_id: request.beacon_id.map(BeaconId::from_uuid),
        city_id,
        kind: request.beacon_kind,
        display_name: request.display_name,
        contact_email: request.contact_email,
        destination_url: request.destination_url,
        source_url: request.source_url,
        active: request.active,
        verified: request.verified,
        accepts_outreach: request.accepts_outreach,
        do_not_contact: request.do_not_contact,
        relationship_score: request.relationship_score,
        relevance_basis_points: request.relevance_basis_points,
        confidence,
        metadata: request.metadata,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_beacon(
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

pub async fn record_beacon_reply(
    State(state): State<AppState>,
    Path(beacon_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<BeaconReplyRequest>,
) -> Response {
    let Ok(beacon_id) = Uuid::parse_str(&beacon_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordBeaconReply {
        beacon_id: BeaconId::from_uuid(beacon_id),
        event_id: EventId::from_uuid(request.event_id),
        disposition: request.disposition,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_beacon_reply(
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

pub async fn upsert_outreach_target(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachTargetRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.target_id.is_none())
        || request.priority > 100
        || request.relationship_score > 100
        || !valid_booking_name(&request.display_name)
        || !valid_booking_email(&request.contact_email);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }

    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertOutreachTarget {
        target_id: request.target_id.map(OutreachTargetId::from_uuid),
        kind: request.target_kind,
        display_name: request.display_name,
        contact_email: request.contact_email,
        priority: request.priority,
        relationship_score: request.relationship_score,
        active: request.active,
        verified: request.verified,
        accepts_outreach: request.accepts_outreach,
        do_not_contact: request.do_not_contact,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_outreach_target(
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

pub async fn upsert_outreach_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachOpportunityRequest>,
) -> Response {
    let invalid = !valid_market_source(&request.source)
        || request.subject_key.trim().is_empty()
        || request.subject_key.len() > 200
        || request.template_key.trim().is_empty()
        || request.template_key.len() > 160
        || !matches!(
            request.subject_kind.as_str(),
            "release" | "event" | "catalogue" | "band"
        )
        || request.relevance_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > Duration::days(90);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let confidence = match Confidence::from_basis_points(request.confidence_basis_points) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertOutreachOpportunity {
        opportunity_id: request.opportunity_id.map(OutreachOpportunityId::from_uuid),
        target_id: OutreachTargetId::from_uuid(request.target_id),
        source: request.source,
        subject_kind: request.subject_kind,
        subject_key: request.subject_key,
        template_key: request.template_key,
        relevance_basis_points: request.relevance_basis_points,
        confidence,
        active: request.active,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    };
    match state
        .autopilot
        .upsert_outreach_opportunity(
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

pub async fn record_outreach_reply(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<OutreachReplyRequest>,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordOutreachReply {
        target_id: OutreachTargetId::from_uuid(target_id),
        opportunity_id: request.opportunity_id.map(OutreachOpportunityId::from_uuid),
        disposition: request.disposition,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_outreach_reply(
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

pub async fn upsert_release_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReleasePlanRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.release_id.is_none())
        || request.source_key.trim().is_empty()
        || request.source_key.len() > 160
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request
            .listen_url
            .as_ref()
            .is_some_and(|url| url.trim().is_empty() || url.len() > 1000);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertReleasePlan {
        release_id: request.release_id.map(ReleasePlanId::from_uuid),
        source_key: request.source_key,
        title: request.title,
        release_at: request.release_at,
        listen_url: request.listen_url,
        active: request.active,
        assets_ready: request.assets_ready,
        communication_enabled: request.communication_enabled,
        press_enabled: request.press_enabled,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_release_plan(
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

pub async fn upsert_team_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.opportunity_id.is_none())
        || !valid_market_source(&request.source)
        || request.external_key.trim().is_empty()
        || request.external_key.len() > 240
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request.organization.trim().is_empty()
        || request.organization.len() > 240
        || request
            .destination_url
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 1000)
        || request
            .contact_email
            .as_ref()
            .is_some_and(|value| !valid_booking_email(value))
        || request.fit_basis_points > 10_000
        || request.reputation_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request.strategic_value_basis_points > 10_000
        || !valid_currency(&request.currency)
        || request.expected_fee_minor < 0
        || request.estimated_cost_minor < 0
        || request.application_fee_minor < 0
        || request.funding_amount_minor < 0
        || request.own_contribution_minor < 0
        || request.country_code.as_ref().is_some_and(|code| {
            code.len() != 2 || !code.bytes().all(|byte| byte.is_ascii_uppercase())
        })
        || !request.metadata.is_object()
        || (matches!(request.opportunity_kind, TeamOpportunityKind::Funding)
            && request.deadline.is_none());
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let confidence = match Confidence::from_basis_points(request.confidence_basis_points) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertTeamOpportunity {
        opportunity_id: request.opportunity_id.map(TeamOpportunityId::from_uuid),
        kind: request.opportunity_kind,
        source: request.source,
        external_key: request.external_key,
        title: request.title,
        organization: request.organization,
        destination_url: request.destination_url,
        contact_email: request.contact_email,
        verified_destination: request.verified_destination,
        fit_basis_points: request.fit_basis_points,
        reputation_basis_points: request.reputation_basis_points,
        confidence,
        currency: request.currency,
        expected_fee_minor: request.expected_fee_minor,
        estimated_cost_minor: request.estimated_cost_minor,
        application_fee_minor: request.application_fee_minor,
        requires_contract: request.requires_contract,
        exclusive: request.exclusive,
        eligible: request.eligible,
        funding_amount_minor: request.funding_amount_minor,
        own_contribution_minor: request.own_contribution_minor,
        deadline: request.deadline,
        event_starts_at: request.event_starts_at,
        country_code: request.country_code,
        travel_band: request.travel_band,
        metadata: request.metadata,
        strategic_value_basis_points: request.strategic_value_basis_points,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_team_opportunity(
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

pub async fn record_team_opportunity_progress(
    State(state): State<AppState>,
    Path(opportunity_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityProgressRequest>,
) -> Response {
    let Ok(opportunity_id) = Uuid::parse_str(&opportunity_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordTeamOpportunityProgress {
        opportunity_id: TeamOpportunityId::from_uuid(opportunity_id),
        progress: request.progress,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_team_opportunity_progress(
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

/// Records what the promoter has said about one opportunity.
///
/// The only way a negotiation ever starts. Everything the agent does afterwards
/// — the floor, the counter, the refusals — hangs off a number a human read in
/// an email and wrote down here.
pub async fn record_team_opportunity_terms(
    State(state): State<AppState>,
    Path(opportunity_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityTermsRequest>,
) -> Response {
    let Ok(opportunity_id) = Uuid::parse_str(&opportunity_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let position = match request.position {
        PromoterPositionRequest::Offer => PromoterPosition::Offer {
            fee_minor: request.offered_fee_minor,
        },
        PromoterPositionRequest::Withdrawn => PromoterPosition::Withdrawn,
    };
    // A window that has already closed would open a negotiation the next cycle
    // immediately expires, which reads to an operator as the agent losing a
    // show it never had a chance at.
    let invalid = request.offered_fee_minor < 0
        || !valid_currency(&request.currency)
        || request.responds_by <= OffsetDateTime::now_utc();
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordTeamOpportunityTerms {
        opportunity_id: TeamOpportunityId::from_uuid(opportunity_id),
        position,
        currency: request.currency,
        responds_by: request.responds_by,
    };
    match state
        .autopilot
        .record_team_opportunity_terms(
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

/// Records that a human submitted the Spotify editorial pitch.
///
/// The form has no API, so this is the only thing that stops the chasing. The
/// agent never sets it: it would be recording a submission it cannot see.
pub async fn complete_editorial_pitch(
    State(state): State<AppState>,
    Path(release_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(release_id) = Uuid::parse_str(&release_id) else {
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
        .complete_editorial_pitch(
            state.ops.workspace_id(),
            ReleasePlanId::from_uuid(release_id),
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Records a curator's placement claim, or what one public read of it found.
///
/// The only way a placement enters the system. A claim counts toward nothing
/// until a read confirms it, and a confirmation that disappears inside the
/// verification window suppresses the curator behind it.
pub async fn record_playlist_placement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PlaylistPlacementRequest>,
) -> Response {
    let invalid = request.playlist_external_id.trim().is_empty()
        || request.playlist_external_id.len() > 200
        || request.track_external_id.trim().is_empty()
        || request.track_external_id.len() > 200;
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let observation = match request.report {
        PlacementReportRequest::Claimed => None,
        PlacementReportRequest::Present => Some(PlacementObservation::Present),
        PlacementReportRequest::Absent => Some(PlacementObservation::Absent),
        PlacementReportRequest::Unreadable => Some(PlacementObservation::Unreadable),
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordPlaylistPlacement {
        opportunity_id: OutreachOpportunityId::from_uuid(request.opportunity_id),
        playlist_external_id: request.playlist_external_id,
        track_external_id: request.track_external_id,
        observation,
    };
    match state
        .autopilot
        .record_playlist_placement(
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

pub async fn upsert_content_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ContentSourceRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.source_id.is_none())
        || request.source_key.trim().is_empty()
        || request.source_key.len() > 200
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request.expires_at <= request.occurred_at
        || request.expires_at - request.occurred_at > Duration::days(90)
        || !request.metadata.is_object();
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertContentSource {
        source_id: request.source_id.map(ContentSourceId::from_uuid),
        kind: request.source_kind,
        source_key: request.source_key,
        title: request.title,
        occurred_at: request.occurred_at,
        expires_at: request.expires_at,
        metadata: request.metadata,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_content_source(
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
