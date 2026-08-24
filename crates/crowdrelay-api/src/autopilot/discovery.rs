use super::*;
use crowdrelay_domain::live_opportunities::{
    LiveOpportunityDiscovery, LiveOpportunityKind, evaluate_live_opportunity_discovery,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityDiscoveryRequest {
    source: String,
    external_key: String,
    title: String,
    destination_url: Option<String>,
    summary: String,
}

pub async fn discover_team_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityDiscoveryRequest>,
) -> Response {
    let invalid = !valid_market_source(&request.source)
        || request.external_key.trim().is_empty()
        || request.external_key.len() > 240
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request.summary.len() > 8_000
        || request
            .summary
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        || request
            .destination_url
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 1_000);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(assessment) = evaluate_live_opportunity_discovery(&LiveOpportunityDiscovery {
        title: &request.title,
        summary: &request.summary,
    }) else {
        return private_json(StatusCode::OK, serde_json::json!({"accepted": false}));
    };
    let kind = match assessment.kind {
        LiveOpportunityKind::Festival => TeamOpportunityKind::Festival,
        LiveOpportunityKind::Showcase => TeamOpportunityKind::Showcase,
        LiveOpportunityKind::ReviewContest => TeamOpportunityKind::ReviewContest,
        LiveOpportunityKind::SupportSlot => TeamOpportunityKind::SupportSlot,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertTeamOpportunity {
        opportunity_id: None,
        kind,
        organization: request.source.clone(),
        source: request.source,
        external_key: request.external_key,
        title: request.title,
        destination_url: request.destination_url,
        contact_email: None,
        verified_destination: false,
        fit_basis_points: assessment.fit_basis_points,
        reputation_basis_points: assessment.reputation_basis_points,
        confidence: assessment.confidence,
        currency: "PLN".to_owned(),
        expected_fee_minor: 0,
        estimated_cost_minor: 0,
        application_fee_minor: 0,
        requires_contract: true,
        exclusive: false,
        eligible: true,
        funding_amount_minor: 0,
        own_contribution_minor: 0,
        deadline: None,
        event_starts_at: None,
        country_code: None,
        travel_band: None,
        metadata: serde_json::json!({
            "discovery": {
                "destination_unverified": true,
                "fee_unverified": true,
                "terms_unverified": true,
                "summary": request.summary,
            }
        }),
        // A name match against a landmark list is a suggestion an operator
        // confirms, never an automatic grant: "Festival" in a title means
        // nothing on its own. Text-based discovery never sets this above
        // Standard.
        strategic_value_basis_points: 0,
        expected_version: 0,
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
