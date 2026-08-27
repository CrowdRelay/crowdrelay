// Reply triage read model — one endpoint that shows operators which inbound
// replies need human review and how recent replies were classified.
//
// This is a read model, not a pipeline. Every row comes from
// `viryaos_reply_classifications`, which the worker populates. The operator
// sees:
// - Replies waiting for human review (NeedsHuman), newest first.
// - Recent auto-classifications, newest first.
//
// No new state, no new writes, no new migrations.

/// The complete reply triage view, in one response.
#[derive(Debug, Serialize)]
pub struct ReplyTriageView {
    /// Replies the classifier routed to human review, newest first.
    pub needs_human: Vec<ReplyTriageEntry>,
    /// Recent auto-classifications, newest first.
    pub recent_auto: Vec<ReplyTriageEntry>,
    /// Summary counts.
    pub summary: ReplyTriageSummary,
}

#[derive(Debug, Serialize)]
pub struct ReplyTriageEntry {
    pub id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub target_kind: String,
    pub reply_text: String,
    pub previous_disposition: Option<String>,
    pub classification_result: String,
    pub classified_disposition: Option<String>,
    pub human_review_reason: Option<String>,
    pub confidence_basis_points: i32,
    pub matched_rules: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub classified_at: OffsetDateTime,
}

#[derive(Debug, FromRow, Serialize)]
pub struct ReplyTriageSummary {
    pub needs_human_count: i64,
    pub auto_positive_count: i64,
    pub auto_declined_count: i64,
    pub auto_do_not_contact_count: i64,
    pub pending_count: i64,
}

#[derive(Debug, FromRow)]
struct ReplyTriageRow {
    id: uuid::Uuid,
    target_id: uuid::Uuid,
    target_kind: String,
    reply_text: String,
    previous_disposition: Option<String>,
    classification_result: String,
    classified_disposition: Option<String>,
    human_review_reason: Option<String>,
    confidence_basis_points: i32,
    matched_rules: serde_json::Value,
    classified_at: OffsetDateTime,
}

const NEEDS_HUMAN_LIMIT: i64 = 50;
const RECENT_AUTO_LIMIT: i64 = 30;

pub async fn reply_triage_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ops.workspace_id().into_uuid();
    let pool = &state.database;

    let needs_human = sqlx::query_as::<_, ReplyTriageRow>(
        r#"
        SELECT id, target_id, target_kind, reply_text, previous_disposition,
               classification_result, classified_disposition, human_review_reason,
               confidence_basis_points, matched_rules, classified_at
        FROM viryaos_reply_classifications
        WHERE workspace_id = $1
          AND classification_result = 'needs_human'
        ORDER BY classified_at DESC
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(NEEDS_HUMAN_LIMIT)
    .fetch_all(pool)
    .await;

    let recent_auto = sqlx::query_as::<_, ReplyTriageRow>(
        r#"
        SELECT id, target_id, target_kind, reply_text, previous_disposition,
               classification_result, classified_disposition, human_review_reason,
               confidence_basis_points, matched_rules, classified_at
        FROM viryaos_reply_classifications
        WHERE workspace_id = $1
          AND classification_result = 'auto'
          AND classified_disposition IS NOT NULL
        ORDER BY classified_at DESC
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(RECENT_AUTO_LIMIT)
    .fetch_all(pool)
    .await;

    let summary = sqlx::query_as::<_, ReplyTriageSummary>(
        r#"
        SELECT
            count(*) FILTER (WHERE classification_result = 'needs_human')::bigint AS needs_human_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'positive')::bigint AS auto_positive_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'declined')::bigint AS auto_declined_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'do_not_contact')::bigint AS auto_do_not_contact_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition IS NULL)::bigint AS pending_count
        FROM viryaos_reply_classifications
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await;

    match (needs_human, recent_auto, summary) {
        (Ok(nh), Ok(ra), Ok(s)) => {
            let view = ReplyTriageView {
                needs_human: nh.into_iter().map(row_to_entry).collect(),
                recent_auto: ra.into_iter().map(row_to_entry).collect(),
                summary: s,
            };
            private_json(StatusCode::OK, view)
        }
        _ => {
            tracing::warn!("could not load reply triage view");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

fn row_to_entry(row: ReplyTriageRow) -> ReplyTriageEntry {
    ReplyTriageEntry {
        id: row.id,
        target_id: row.target_id,
        target_kind: row.target_kind,
        reply_text: row.reply_text,
        previous_disposition: row.previous_disposition,
        classification_result: row.classification_result,
        classified_disposition: row.classified_disposition,
        human_review_reason: row.human_review_reason,
        confidence_basis_points: row.confidence_basis_points,
        matched_rules: row.matched_rules,
        classified_at: row.classified_at,
    }
}
