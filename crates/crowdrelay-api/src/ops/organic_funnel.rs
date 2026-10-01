#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrganicFunnelQuery {
    action_id: Option<Uuid>,
    campaign_id: Option<Uuid>,
    days: Option<i32>,
    limit: Option<i64>,
}

pub async fn organic_funnel(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<OrganicFunnelQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let days = query.days.unwrap_or(90);
    if !(1..=90).contains(&days) {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let limit = match page_size(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(request_id_value),
    };
    let now = OffsetDateTime::now_utc();
    match run_limited(
        &state.read_budget,
        state.ops.operation_timeout,
        crowdrelay_infra::organic_funnel::read(
            &state.ops.pool,
            state.ops.workspace_id().into_uuid(),
            query.action_id,
            query.campaign_id,
            days,
            limit,
            now,
        ),
    )
    .await
    {
        Ok(items) => private_json(
            StatusCode::OK,
            json!({
                "as_of":now.format(&time::format_description::well_known::Rfc3339).ok(),
                "window_days":days,"credit_basis":"canonical_last_tracked_click",
                "activation_basis":"deliberate_first_party_engagement_with_current_consent_excluding_sessions",
                "visitor_basis":"distinct_browser_ids_including_unclassified_previews",
                "causal_claim":false,"items":items
            }),
        ),
        Err(error) => error.into_response(request_id_value),
    }
}
