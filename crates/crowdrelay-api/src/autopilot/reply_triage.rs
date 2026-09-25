// Reply triage read model — one endpoint that shows operators which inbound
// replies need human review and how recent replies were classified.
//
// This is a read model, not a pipeline. Every row comes from
// `reply_classifications`, which the worker populates. The operator
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
    /// Contacts whose last word is theirs: they answered and nobody has
    /// written back since. Positive answers first, then the longest wait.
    pub waiting_on_you: Vec<WaitingReply>,
    /// Summary counts.
    pub summary: ReplyTriageSummary,
}

/// One contact who answered and is waiting on the act.
///
/// Read from the outreach interaction ledger, not from
/// `reply_classifications`: a reply recorded through the reply route, a
/// mailbox import or a sheet import never passes the classifier, so on
/// 2026-09-25 sixteen contacts whose last word was theirs (19 positive
/// answers in the ledger) sat behind a triage view that showed nothing.
#[derive(Debug, FromRow, Serialize)]
pub struct WaitingReply {
    pub target_id: uuid::Uuid,
    pub display_name: String,
    pub target_kind: String,
    /// The recorded disposition of their last message.
    pub disposition: String,
    /// What their answer was, in the words the source recorded it with — a
    /// sheet's result column or a mailbox import's response type. `None`
    /// when the source named nothing beyond the disposition.
    pub reply_label: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub replied_at: OffsetDateTime,
    /// The last message the act sent them, before their answer. `None`
    /// when the ledger holds no outbound message to this contact.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_written_at: Option<OffsetDateTime>,
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
    /// What the reader proposed from a negotiation reply's text — a fee the
    /// human confirms through the terms route, never a write the machine made.
    pub proposed_fee_minor: Option<i64>,
    pub proposed_currency: Option<String>,
    pub proposed_opportunity_id: Option<uuid::Uuid>,
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
    /// Every contact waiting on the act, beyond the listed ones too.
    pub waiting_on_you_count: i64,
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
    proposed_fee_minor: Option<i64>,
    proposed_currency: Option<String>,
    proposed_opportunity_id: Option<uuid::Uuid>,
    confidence_basis_points: i32,
    matched_rules: serde_json::Value,
    classified_at: OffsetDateTime,
}

const NEEDS_HUMAN_LIMIT: i64 = 50;
const RECENT_AUTO_LIMIT: i64 = 30;
const WAITING_LIMIT: i64 = 50;
/// An answer older than this is no longer "your turn"; it is a lapsed
/// conversation. Ninety days keeps a season's press round in view.
const WAITING_WINDOW_DAYS: i32 = 90;

/// Each contact's latest message, when it is theirs, it is recent, and it
/// did not close the conversation. A declined or do-not-contact answer asks
/// nothing of the act; an inactive or do-not-contact target is not written
/// to anyway.
const WAITING_ON_YOU_CTE: &str = r#"
    WITH latest AS (
        SELECT DISTINCT ON (interaction.target_id)
               interaction.target_id, interaction.direction, interaction.disposition,
               interaction.occurred_at, interaction.metadata
        FROM outreach_interactions AS interaction
        WHERE interaction.workspace_id = $1
        ORDER BY interaction.target_id, interaction.occurred_at DESC, interaction.id DESC
    ), waiting AS (
        SELECT latest.target_id, target.display_name, target.target_kind,
               latest.disposition,
               NULLIF(btrim(COALESCE(latest.metadata->>'result',
                                     latest.metadata->>'response_type')), '') AS reply_label,
               latest.occurred_at AS replied_at,
               (SELECT max(sent.occurred_at)
                  FROM outreach_interactions AS sent
                 WHERE sent.workspace_id = $1
                   AND sent.target_id = latest.target_id
                   AND sent.direction = 'outbound') AS last_written_at
        FROM latest
        JOIN outreach_targets AS target
          ON target.workspace_id = $1 AND target.id = latest.target_id
        WHERE latest.direction = 'inbound'
          AND latest.disposition NOT IN ('declined', 'do_not_contact')
          AND latest.occurred_at > now() - make_interval(days => $2)
          AND target.active
          AND NOT target.do_not_contact
    )
"#;

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
               proposed_fee_minor, proposed_currency, proposed_opportunity_id,
               confidence_basis_points, matched_rules, classified_at
        FROM reply_classifications
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
               proposed_fee_minor, proposed_currency, proposed_opportunity_id,
               confidence_basis_points, matched_rules, classified_at
        FROM reply_classifications
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

    let waiting_on_you = sqlx::query_as::<_, WaitingReply>(&format!(
        "{WAITING_ON_YOU_CTE}
        SELECT target_id, display_name, target_kind, disposition, reply_label,
               replied_at, last_written_at
        FROM waiting
        ORDER BY (disposition = 'positive') DESC, replied_at ASC, target_id
        LIMIT $3"
    ))
    .bind(workspace_id)
    .bind(WAITING_WINDOW_DAYS)
    .bind(WAITING_LIMIT)
    .fetch_all(pool)
    .await;

    let waiting_on_you_count = sqlx::query_scalar::<_, i64>(&format!(
        "{WAITING_ON_YOU_CTE} SELECT count(*)::bigint FROM waiting"
    ))
    .bind(workspace_id)
    .bind(WAITING_WINDOW_DAYS)
    .fetch_one(pool)
    .await;

    let summary = sqlx::query_as::<_, ReplyTriageSummary>(
        r#"
        SELECT
            count(*) FILTER (WHERE classification_result = 'needs_human')::bigint AS needs_human_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'positive')::bigint AS auto_positive_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'declined')::bigint AS auto_declined_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition = 'do_not_contact')::bigint AS auto_do_not_contact_count,
            count(*) FILTER (WHERE classification_result = 'auto' AND classified_disposition IS NULL)::bigint AS pending_count,
            0::bigint AS waiting_on_you_count
        FROM reply_classifications
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await;

    match (needs_human, recent_auto, waiting_on_you, waiting_on_you_count, summary) {
        (Ok(nh), Ok(ra), Ok(waiting), Ok(waiting_count), Ok(s)) => {
            let view = ReplyTriageView {
                needs_human: nh.into_iter().map(row_to_entry).collect(),
                recent_auto: ra.into_iter().map(row_to_entry).collect(),
                waiting_on_you: waiting,
                summary: ReplyTriageSummary {
                    waiting_on_you_count: waiting_count,
                    ..s
                },
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
        proposed_fee_minor: row.proposed_fee_minor,
        proposed_currency: row.proposed_currency,
        proposed_opportunity_id: row.proposed_opportunity_id,
        confidence_basis_points: row.confidence_basis_points,
        matched_rules: row.matched_rules,
        classified_at: row.classified_at,
    }
}
