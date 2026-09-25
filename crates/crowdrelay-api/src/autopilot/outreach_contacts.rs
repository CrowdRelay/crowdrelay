// The outreach conversation list: every outreach contact with where the
// conversation stands, read from the interaction ledger.
//
// The act's press, radio, venue and agent outreach lived in its own sheets;
// the ledger held 270 messages and 82 answers for 128 contacts, and no
// console read could list them — a reply had no row to attach to and a
// follow-up had nowhere to be decided. This is that list. It is a read
// model: `outreach_targets` for who, `outreach_interactions` for what was
// said and when. The one write that goes with it (`record_outreach_written`)
// is the act saying "I wrote back", which is the only way a conversation
// leaves "your turn" when the act answers from its own mailbox.
//
// A conversation's state comes from its latest message:
// - `closed`: the contact is inactive or do-not-contact, or its latest
//   message is a refusal (`declined`, `do_not_contact`);
// - `your_turn`: the latest message is theirs;
// - `waiting_on_them`: the latest message is the act's;
// - `not_contacted`: no message on record.
// Unlike the triage view's `waiting_on_you`, `your_turn` has no age limit
// here: an answer from July that nobody returned is still the act's move,
// and the row says how old it is.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachContactsQuery {
    state: Option<String>,
    kind: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct OutreachContactsView {
    pub contacts: Vec<OutreachContact>,
    /// Every contact by state, whatever the filter and the limit cut.
    pub counts: OutreachContactCounts,
}

#[derive(Debug, FromRow, Serialize)]
pub struct OutreachContact {
    pub target_id: Uuid,
    pub display_name: String,
    pub target_kind: String,
    pub state: String,
    /// The latest message either way; `None` when nothing is on record.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_message_at: Option<OffsetDateTime>,
    /// Their latest answer's disposition and, when the source named one, its
    /// own label — only when the latest message is theirs.
    pub answer_disposition: Option<String>,
    pub reply_label: Option<String>,
    pub messages_sent: i64,
    pub answers: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_written_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_answered_at: Option<OffsetDateTime>,
}

#[derive(Debug, Default, Serialize)]
pub struct OutreachContactCounts {
    pub your_turn: i64,
    pub waiting_on_them: i64,
    pub not_contacted: i64,
    pub closed: i64,
    pub total: i64,
}

const OUTREACH_CONTACT_STATES: [&str; 4] =
    ["your_turn", "waiting_on_them", "not_contacted", "closed"];
const OUTREACH_CONTACTS_DEFAULT_LIMIT: u32 = 200;
const OUTREACH_CONTACTS_MAX_LIMIT: u32 = 500;

const OUTREACH_CONVERSATIONS_CTE: &str = r#"
    WITH conversation AS (
        SELECT target.id AS target_id, target.display_name,
               target.target_kind::text AS target_kind,
               target.active, target.do_not_contact,
               latest.direction AS last_direction,
               latest.disposition AS last_disposition,
               latest.metadata AS last_metadata,
               latest.occurred_at AS last_message_at,
               COALESCE(tally.sent, 0) AS messages_sent,
               COALESCE(tally.answers, 0) AS answers,
               tally.last_written_at, tally.last_answered_at
        FROM outreach_targets AS target
        LEFT JOIN LATERAL (
            SELECT message.direction, message.disposition, message.metadata, message.occurred_at
            FROM outreach_interactions AS message
            WHERE message.workspace_id = $1 AND message.target_id = target.id
            ORDER BY message.occurred_at DESC, message.id DESC
            LIMIT 1
        ) AS latest ON true
        LEFT JOIN LATERAL (
            SELECT count(*) FILTER (WHERE message.direction = 'outbound')::bigint AS sent,
                   count(*) FILTER (WHERE message.direction = 'inbound')::bigint AS answers,
                   max(message.occurred_at) FILTER (WHERE message.direction = 'outbound') AS last_written_at,
                   max(message.occurred_at) FILTER (WHERE message.direction = 'inbound') AS last_answered_at
            FROM outreach_interactions AS message
            WHERE message.workspace_id = $1 AND message.target_id = target.id
        ) AS tally ON true
        WHERE target.workspace_id = $1
    ), staged AS (
        SELECT conversation.*,
               CASE
                   WHEN NOT active OR do_not_contact
                     OR (last_direction = 'inbound'
                         AND last_disposition IN ('declined', 'do_not_contact'))
                   THEN 'closed'
                   WHEN last_direction = 'inbound' THEN 'your_turn'
                   WHEN last_direction = 'outbound' THEN 'waiting_on_them'
                   ELSE 'not_contacted'
               END AS state
        FROM conversation
    )
"#;

pub async fn list_outreach_contacts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OutreachContactsQuery>,
) -> Response {
    let bad_state = query
        .state
        .as_deref()
        .is_some_and(|value| !OUTREACH_CONTACT_STATES.contains(&value));
    let bad_kind = query
        .kind
        .as_deref()
        .is_some_and(|value| value.trim().is_empty() || value.chars().count() > 40);
    let limit = query.limit.unwrap_or(OUTREACH_CONTACTS_DEFAULT_LIMIT);
    if bad_state || bad_kind || limit == 0 || limit > OUTREACH_CONTACTS_MAX_LIMIT {
        return Problem::bad_request_because(
            "state must be one of your_turn, waiting_on_them, not_contacted, closed; kind at most 40 characters; limit 1 to 500",
            request_id(&headers),
        )
        .private()
        .into_response();
    }
    let workspace_id = state.ops.workspace_id().into_uuid();
    let pool = &state.database;

    let contacts = sqlx::query_as::<_, OutreachContact>(&format!(
        "{OUTREACH_CONVERSATIONS_CTE}
        SELECT target_id, display_name, target_kind, state, last_message_at,
               CASE WHEN last_direction = 'inbound' THEN last_disposition END AS answer_disposition,
               CASE WHEN last_direction = 'inbound' THEN
                   NULLIF(btrim(COALESCE(last_metadata->>'result',
                                         last_metadata->>'response_type')), '')
               END AS reply_label,
               messages_sent, answers, last_written_at, last_answered_at
        FROM staged
        WHERE ($2::text IS NULL OR state = $2)
          AND ($3::text IS NULL OR target_kind = $3)
        ORDER BY CASE state
                     WHEN 'your_turn' THEN 0
                     WHEN 'waiting_on_them' THEN 1
                     WHEN 'not_contacted' THEN 2
                     ELSE 3
                 END,
                 (last_direction = 'inbound' AND last_disposition = 'positive') DESC,
                 last_message_at ASC NULLS LAST,
                 display_name, target_id
        LIMIT $4"
    ))
    .bind(workspace_id)
    .bind(query.state.as_deref())
    .bind(query.kind.as_deref().map(str::trim))
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await;

    let counts = sqlx::query_as::<_, (String, i64)>(&format!(
        "{OUTREACH_CONVERSATIONS_CTE}
        SELECT state, count(*)::bigint FROM staged GROUP BY state"
    ))
    .bind(workspace_id)
    .fetch_all(pool)
    .await;

    match (contacts, counts) {
        (Ok(contacts), Ok(rows)) => {
            let mut counts = OutreachContactCounts::default();
            for (state, count) in rows {
                match state.as_str() {
                    "your_turn" => counts.your_turn = count,
                    "waiting_on_them" => counts.waiting_on_them = count,
                    "not_contacted" => counts.not_contacted = count,
                    _ => counts.closed += count,
                }
                counts.total += count;
            }
            private_json(StatusCode::OK, OutreachContactsView { contacts, counts })
        }
        _ => {
            tracing::warn!("could not load the outreach contact list");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachWrittenRequest {
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

/// The act wrote to this contact from its own mailbox. Refused (409) for a
/// do-not-contact target, and for a time in the future.
pub async fn record_outreach_written(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<OutreachWrittenRequest>,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    if request.occurred_at > OffsetDateTime::now_utc() + time::Duration::minutes(5) {
        return Problem::bad_request_because("occurred_at is in the future", request_id(&headers))
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
        .record_outreach_written(
            state.ops.workspace_id(),
            RecordOutreachWritten {
                target_id: OutreachTargetId::from_uuid(target_id),
                occurred_at: request.occurred_at,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
