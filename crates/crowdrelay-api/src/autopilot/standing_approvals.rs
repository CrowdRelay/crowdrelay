// The operator's standing approvals: grant, list, revoke.
//
// A per-action approval answers "may this post go out". A standing approval
// answers "may this community's posts go out", which is the decision an
// operator actually reaches after reading three drafts from the same place.
// Without somewhere to say it, the only way to say it is to approve each post,
// and the queue expires at 72 hours faster than a person empties it.
//
// What a grant may do is decided in `domain::standing_approval`, and whether a
// grant covers an action is asked by the agent-outcome worker. This layer
// checks transport shape and nothing else.

/// Who a grant made through this surface is attributed to.
///
/// The control plane authenticates with a key rather than a person, the same
/// as every other operator write here (`operator:admin_api_key` on approvals).
/// Naming the surface is honest about what is known; inventing a user id would
/// not be.
const GRANTED_BY: &str = "operator:control_plane";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantStandingApprovalRequest {
    /// The action kind this grant answers for, e.g.
    /// `community.engage.request`. The kind rather than the context: a
    /// context holds several kinds, and an operator who trusted a forum post
    /// has not thereby trusted a press pitch.
    action_kind: String,
    /// The one target covered. A community target id for a community post.
    target_key: String,
    action_class: String,
    /// How long the grant lasts. Bounded by `MAX_GRANT_DAYS`; omitted means
    /// `DEFAULT_GRANT_DAYS`.
    days: Option<i64>,
    /// Why the operator said yes, for whoever reads the row at renewal.
    note: Option<String>,
}

/// `POST /v1/control-plane/autopilot/standing-approvals`
pub async fn grant_standing_approval(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<GrantStandingApprovalRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(class) = crowdrelay_domain::action_class::ActionClass::parse(body.action_class.trim()) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    match crowdrelay_infra::standing_approvals::grant(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: body.action_kind.trim(),
            target_key: body.target_key.trim(),
            class,
            granted_by: GRANTED_BY,
            days: body
                .days
                .unwrap_or(crowdrelay_domain::standing_approval::DEFAULT_GRANT_DAYS),
            note: body.note.as_deref().map(str::trim).filter(|n| !n.is_empty()),
        },
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(view) => private_json(StatusCode::CREATED, view),
        Err(error) => standing_approval_problem(&error, request_id_value.clone()),
    }
}

/// `GET /v1/control-plane/autopilot/standing-approvals`
///
/// Revoked and expired grants are in the list. "Which of these did we turn
/// off, and when" is the question asked after a community goes quiet, and a
/// list that hides them cannot answer it.
pub async fn list_standing_approvals(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    match crowdrelay_infra::standing_approvals::list(
        &state.database,
        state.ops.workspace_id().into_uuid(),
    )
    .await
    {
        Ok(items) => private_json(StatusCode::OK, serde_json::json!({ "items": items })),
        Err(error) => standing_approval_problem(&error, request_id_value.clone()),
    }
}

/// `DELETE /v1/control-plane/autopilot/standing-approvals/{action_kind}/{target_key}`
pub async fn revoke_standing_approval(
    State(state): State<AppState>,
    Path((action_kind, target_key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match crowdrelay_infra::standing_approvals::revoke(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        action_kind.trim(),
        target_key.trim(),
        GRANTED_BY,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => standing_approval_problem(&error, request_id_value.clone()),
    }
}

/// A refusal reaches the operator as the sentence the domain already wrote.
///
/// `GrantError` says why money may not carry a grant and why a duration was
/// refused; turning either into a bare 400 would make the operator guess at a
/// rule that is already written down.
fn standing_approval_problem(
    error: &crowdrelay_infra::standing_approvals::StandingApprovalError,
    request_id_value: Option<String>,
) -> Response {
    use crowdrelay_infra::standing_approvals::StandingApprovalError as E;
    match error {
        E::Refused(refusal) => {
            Problem::conflict_owned(refusal.to_string().into(), request_id_value).into_response()
        }
        E::NotFound => Problem::not_found(request_id_value).into_response(),
        E::Database(error) => {
            tracing::warn!(%error, "standing approval store unavailable");
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}
