// CRM import proposals: the operator's screen-then-approve gate.
//
// The registry sweep proposes contacts in bulk; these routes let the
// operator read the deterministic verdicts and approve the admitted set in
// batches. This layer checks transport shape only — the screen lives in the
// repository so the answer a list shows is the answer an approve re-runs.

/// One approve call's ceiling, whichever shape it takes.
const MAX_IMPORT_APPROVAL_BATCH: usize = 200;
/// Page bound for the proposal list.
const MAX_IMPORT_PROPOSAL_PAGE: u32 = 500;

/// The kinds the import gate admits — the same list the repository screens
/// against, spelled here so a typo is a 400 and not an empty page.
const IMPORTABLE_TARGET_KINDS: &[&str] = &[
    "press",
    "radio",
    "creator",
    "endorsement",
    "media_patronage",
    "playlist",
    "organiser",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportProposalQuery {
    target_kind: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveImportProposalsRequest {
    ids: Option<Vec<Uuid>>,
    target_kind: Option<String>,
    #[serde(default)]
    all_admitted: bool,
}

pub async fn list_outreach_import_proposals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ImportProposalQuery>,
) -> Response {
    if query
        .target_kind
        .as_deref()
        .is_some_and(|kind| !IMPORTABLE_TARGET_KINDS.contains(&kind))
        || query.limit.is_some_and(|limit| limit == 0)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .list_outreach_import_proposals(
            state.ops.workspace_id(),
            query.target_kind,
            query.limit.unwrap_or(100).min(MAX_IMPORT_PROPOSAL_PAGE),
        )
        .await
    {
        Ok(page) => private_json(StatusCode::OK, page),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn approve_outreach_import_proposals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ApproveImportProposalsRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    // Exactly one selection shape: a named id list, or every admitted row of
    // one kind. Both bounded at two hundred so a mistake is a batch, never
    // the whole registry.
    let selection = match (&request.ids, &request.target_kind, request.all_admitted) {
        (Some(ids), None, false)
            if !ids.is_empty() && ids.len() <= MAX_IMPORT_APPROVAL_BATCH =>
        {
            OutreachImportSelection::Ids(ids.clone())
        }
        (None, Some(kind), true) if IMPORTABLE_TARGET_KINDS.contains(&kind.as_str()) => {
            OutreachImportSelection::AllAdmitted {
                target_kind: kind.clone(),
            }
        }
        _ => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .approve_outreach_import_proposals(
            state.ops.workspace_id(),
            selection,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[cfg(test)]
mod import_proposal_tests {
    use super::*;

    /// The request shape is deliberately exclusive: ids xor all-admitted.
    #[test]
    fn approve_body_accepts_exactly_one_selection() {
        let by_ids: ApproveImportProposalsRequest = serde_json::from_value(serde_json::json!({
            "ids": [uuid::Uuid::now_v7()],
        }))
        .expect("ids shape");
        assert!(by_ids.ids.is_some() && !by_ids.all_admitted);

        let all: ApproveImportProposalsRequest = serde_json::from_value(serde_json::json!({
            "target_kind": "press",
            "all_admitted": true,
        }))
        .expect("all-admitted shape");
        assert!(all.all_admitted && all.target_kind.as_deref() == Some("press"));

        // A bare body names nothing — the handler refuses it rather than
        // approving zero rows silently.
        let empty: ApproveImportProposalsRequest =
            serde_json::from_value(serde_json::json!({})).expect("empty body parses");
        assert!(empty.ids.is_none() && !empty.all_admitted);
    }
}
