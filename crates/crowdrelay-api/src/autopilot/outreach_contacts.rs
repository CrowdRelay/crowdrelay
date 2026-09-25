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

// The same conversation CTE opens both statements. It is written out twice,
// not spliced in with `format!`, because a spliced fragment cannot be prepared
// on its own and `sql-result-types.py` would never check either statement;
// `the_two_statements_share_one_conversation_cte` fails if they drift apart.
const OUTREACH_CONTACTS_SQL: &str = r#"
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
    LIMIT $4
"#;

const OUTREACH_CONTACT_COUNTS_SQL: &str = r#"
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
    SELECT state, count(*)::bigint FROM staged GROUP BY state
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

    let contacts = sqlx::query_as::<_, OutreachContact>(OUTREACH_CONTACTS_SQL)
    .bind(workspace_id)
    .bind(query.state.as_deref())
    .bind(query.kind.as_deref().map(str::trim))
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await;

    let counts = sqlx::query_as::<_, (String, i64)>(OUTREACH_CONTACT_COUNTS_SQL)
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

// ── The conversation drawer ─────────────────────────────────────────────────
//
// `GET …/outreach-targets/{target_id}/conversation` — everything the operator
// needs to answer "who do we write to, and why" for one contact: who they
// are, every message either way, the letters the machine drafted or sent,
// and what happens next computed by the same `evaluate_outreach` the cycle
// runs — the drawer explains the engine's own decision, not a second
// opinion of it.

#[derive(Debug, Serialize)]
pub struct OutreachConversationView {
    pub contact: ConversationContact,
    pub timeline: Vec<ConversationMessage>,
    pub letters: Vec<ConversationLetter>,
    pub next: ConversationNext,
}

#[derive(Debug, FromRow, Serialize)]
pub struct ConversationContact {
    pub target_id: Uuid,
    pub display_name: String,
    pub target_kind: String,
    pub contact_email: String,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub version: i64,
    pub last_reply_disposition: String,
}

#[derive(Debug, FromRow)]
struct ConversationMessageRow {
    direction: String,
    phase: String,
    disposition: String,
    source_key: String,
    occurred_at: OffsetDateTime,
    opportunity_id: Option<Uuid>,
    reply_label: Option<String>,
    reply_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConversationMessage {
    pub direction: String,
    pub phase: String,
    /// The answer's disposition — `none` on the act's own messages.
    pub disposition: String,
    /// Who actually wrote it: `sheet` for the import, `you` for a message the
    /// operator logged from their own mailbox, `autopilot` for the machine.
    pub author: String,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    /// The import's own verdict label when it carried one.
    pub reply_label: Option<String>,
    /// The reply's text, when the operator logged it with words.
    pub reply_text: Option<String>,
}

#[derive(Debug, FromRow, Serialize)]
pub struct ConversationLetter {
    pub action_id: Uuid,
    pub status: String,
    pub phase: Option<String>,
    pub template_key: Option<String>,
    pub wave_id: Option<Uuid>,
    pub subject: String,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

/// What happens next for this contact, and the reason an operator can act
/// on. `kind` is one of `in_wave`, `due`, `held`, `your_turn`, `closed`,
/// `none` — the drawer's job is phrasing, not deciding.
#[derive(Debug, Serialize)]
pub struct ConversationNext {
    pub kind: &'static str,
    pub detail: String,
    /// When a held contact comes due again, where the hold has a date.
    #[serde(with = "time::serde::rfc3339::option")]
    pub due_at: Option<OffsetDateTime>,
    pub wave_id: Option<Uuid>,
}

const CONVERSATION_TIMELINE_SQL: &str = r#"
    SELECT message.direction, message.phase, message.disposition, message.source_key,
           message.occurred_at, message.opportunity_id,
           NULLIF(btrim(COALESCE(message.metadata->>'result',
                                 message.metadata->>'response_type')), '') AS reply_label,
           NULLIF(btrim(COALESCE(message.metadata->>'reply_text',
                                 classification.reply_text)), '') AS reply_text
    FROM outreach_interactions AS message
    -- Legacy fallback only: replies recorded before the interaction carried
    -- its own reply_text keep the words solely on the triage row, matched by
    -- the stamp ingress copied from the reply's occurred_at. Once the worker
    -- triages that row it rewrites classified_at to now(), so the fallback
    -- only ever joins rows still pending triage.
    LEFT JOIN reply_classifications AS classification
      ON classification.workspace_id = message.workspace_id
     AND classification.target_id = message.target_id
     AND classification.classified_at = message.occurred_at
     AND message.direction = 'inbound'
     AND message.metadata->>'reply_text' IS NULL
    WHERE message.workspace_id = $1 AND message.target_id = $2
    ORDER BY message.occurred_at, message.id
"#;

const CONVERSATION_LETTERS_SQL: &str = r#"
    SELECT action.id AS action_id, action.status, action.created_at, action.finished_at,
           action.payload->>'phase' AS phase,
           action.payload->>'template_key' AS template_key,
           NULLIF(action.payload->>'wave_id', '')::uuid AS wave_id,
           COALESCE(action.payload->'draft'->>'subject', '') AS subject,
           COALESCE(action.payload->'draft'->>'body', '') AS body
    FROM autopilot_actions AS action
    WHERE action.workspace_id = $1
      AND action.context = 'outreach'
      AND action.payload->>'target_id' = $2::text
    ORDER BY action.created_at DESC
    LIMIT 40
"#;

/// The pitch or follow-up already committed for this contact — a queued,
/// awaiting or processing action has taken its place whether or not it has
/// sent, and the drawer must say so before suggesting the engine do it
/// again.
const CONVERSATION_PENDING_SQL: &str = r#"
    SELECT action.status,
           NULLIF(action.payload->>'wave_id', '')::uuid AS wave_id,
           wave.state AS wave_state
    FROM autopilot_actions AS action
    LEFT JOIN outreach_waves AS wave
      ON wave.workspace_id = action.workspace_id
     AND wave.id = NULLIF(action.payload->>'wave_id', '')::uuid
    WHERE action.workspace_id = $1
      AND action.context = 'outreach'
      AND action.payload->>'target_id' = $2::text
      AND action.status IN ('awaiting_approval', 'queued', 'processing')
    ORDER BY action.created_at DESC
    LIMIT 1
"#;

/// The threads wave this contact's kind would draft into — the "next" answer
/// names the wave by name when one is open, so the operator can go approve
/// it rather than wait for a cycle.
const CONVERSATION_THREADS_WAVE_SQL: &str = r#"
    SELECT wave.id, wave.state
    FROM outreach_waves AS wave
    WHERE wave.workspace_id = $1
      AND wave.anchor_kind = 'threads'
      AND wave.target_kind = $2
      AND wave.settled_at IS NULL
    ORDER BY wave.opened_at DESC
    LIMIT 1
"#;

#[derive(Debug, FromRow)]
struct ConversationPendingRow {
    status: String,
    wave_id: Option<Uuid>,
    wave_state: Option<String>,
}

/// The single-contact read behind the drawer's row click.
pub async fn get_outreach_conversation(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    let pool = &state.database;

    let contact = sqlx::query_as::<_, ConversationContact>(
        r#"
        SELECT id AS target_id, display_name, target_kind::text AS target_kind,
               contact_email, active, verified, accepts_outreach, do_not_contact,
               version, last_reply_disposition
        FROM outreach_targets
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(target_id)
    .fetch_optional(pool)
    .await;

    let contact = match contact {
        Ok(Some(contact)) => contact,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("could not load the outreach contact");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };

    let reads = tokio::try_join!(
        sqlx::query_as::<_, ConversationMessageRow>(CONVERSATION_TIMELINE_SQL)
            .bind(workspace_id)
            .bind(target_id)
            .fetch_all(pool),
        sqlx::query_as::<_, ConversationLetter>(CONVERSATION_LETTERS_SQL)
            .bind(workspace_id)
            .bind(target_id)
            .fetch_all(pool),
        sqlx::query_as::<_, ConversationPendingRow>(CONVERSATION_PENDING_SQL)
            .bind(workspace_id)
            .bind(target_id)
            .fetch_optional(pool),
        sqlx::query_as::<_, (Uuid, String)>(CONVERSATION_THREADS_WAVE_SQL)
            .bind(workspace_id)
            .bind(&contact.target_kind)
            .fetch_optional(pool),
    );
    let state_reads = tokio::try_join!(
        state.autopilot.load_target_outreach_snapshots(
            state.ops.workspace_id(),
            OutreachTargetId::from_uuid(target_id),
        ),
        state.autopilot.load_policies(state.ops.workspace_id()),
    );
    let (timeline, letters, pending, threads_wave) = match reads {
        Ok(results) => results,
        Err(_) => {
            tracing::warn!("could not load the outreach conversation");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };
    let (snapshots, policies) = match state_reads {
        Ok(results) => results,
        Err(_) => {
            tracing::warn!("could not load the outreach evaluator state");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };
    let outreach_policy = policies
        .iter()
        .find_map(|policy| match &policy.config {
            AutopilotPolicyConfig::Outreach(policy) => Some(*policy),
            _ => None,
        })
        .unwrap_or_default();

    let now = OffsetDateTime::now_utc();
    let next = conversation_next(
        &contact,
        &timeline,
        pending.as_ref(),
        threads_wave,
        &snapshots,
        &outreach_policy,
        now,
    );

    private_json(
        StatusCode::OK,
        OutreachConversationView {
            contact,
            timeline: timeline
                .into_iter()
                .map(|row| ConversationMessage {
                    direction: row.direction,
                    phase: row.phase,
                    disposition: row.disposition,
                    author: conversation_author(&row.source_key).to_owned(),
                    occurred_at: row.occurred_at,
                    reply_label: row.reply_label,
                    reply_text: row.reply_text,
                })
                .collect(),
            letters,
            next,
        },
    )
}

/// Who wrote a ledger message, in the drawer's words: `sheet` for the
/// imported rows, `you` for the ones the operator logged, `autopilot` for
/// the machine. Anything else is told straight rather than guessed.
fn conversation_author(source_key: &str) -> &'static str {
    if source_key.starts_with("autopilot:") {
        "autopilot"
    } else if source_key.starts_with("operator:") {
        "you"
    } else {
        "sheet"
    }
}

/// The drawer's headline. The pending action is read first — a letter that
/// is already drafted is the answer, whatever the rules would say about
/// drafting another. Then the live opportunities are run through the same
/// `evaluate_outreach` the cycle runs, so "held" here is held there too.
fn conversation_next(
    contact: &ConversationContact,
    timeline: &[ConversationMessageRow],
    pending: Option<&ConversationPendingRow>,
    threads_wave: Option<(Uuid, String)>,
    snapshots: &[crowdrelay_domain::outreach::OutreachSnapshot],
    policy: &crowdrelay_domain::outreach::OutreachPolicy,
    now: OffsetDateTime,
) -> ConversationNext {
    use crowdrelay_domain::outreach::{
        OutreachDecision, OutreachHoldReason, OutreachPhase,
    };

    if let Some(pending) = pending {
        let detail = match pending.wave_state.as_deref() {
            Some("drafting") => "drafted into a wave that is still filling".to_owned(),
            Some("sealed") => "sealed in a wave — your approval sends it".to_owned(),
            _ if pending.status == "queued" || pending.status == "processing" => {
                "approved — in the send queue".to_owned()
            }
            _ => "awaiting your approval".to_owned(),
        };
        return ConversationNext {
            kind: "in_wave",
            detail,
            due_at: None,
            wave_id: pending.wave_id,
        };
    }

    // The evaluator's own verdict on this contact's live opportunities. A
    // request anywhere wins over every hold, because it is the thing that
    // will actually happen next.
    fn hold_rank(reason: OutreachHoldReason) -> u8 {
        match reason {
            // The drawer reads the most informative hold: a permanent stop
            // outranks a timed one, and a timed one outranks a soft gate.
            OutreachHoldReason::ContactExhausted => 7,
            OutreachHoldReason::AlreadyReplied => 6,
            OutreachHoldReason::IneligibleTarget => 5,
            OutreachHoldReason::Cooldown
            | OutreachHoldReason::FollowUpNotDue
            | OutreachHoldReason::StaleOpportunity
            | OutreachHoldReason::FollowUpLimit => 4,
            _ => 1,
        }
    }
    let mut best_hold: Option<(OutreachHoldReason, &crowdrelay_domain::outreach::OutreachSnapshot)> =
        None;
    for snapshot in snapshots {
        match crowdrelay_domain::outreach::evaluate_outreach(*snapshot, *policy, now) {
            OutreachDecision::Request { phase, .. } => {
                let (detail, due_at) = if snapshot.thread_followup {
                    (
                        match &threads_wave {
                            Some((_, state)) if state == "drafting" => {
                                "follow-up due — drafts into the threads wave being built".to_owned()
                            }
                            // A sealed wave holds only the pitches drafted
                            // before it closed — this contact's letter lands
                            // in the next month's wave, not this one.
                            _ => "follow-up due — lands in the next threads wave".to_owned(),
                        },
                        None,
                    )
                } else {
                    (
                        match phase {
                            OutreachPhase::FollowUp => "a follow-up pitch is due".to_owned(),
                            OutreachPhase::Initial => "a pitch is due".to_owned(),
                        },
                        None,
                    )
                };
                return ConversationNext {
                    kind: "due",
                    detail,
                    due_at,
                    wave_id: threads_wave.map(|(id, _)| id),
                };
            }
            OutreachDecision::Hold(reason) => {
                if best_hold
                    .is_none_or(|(held, _)| hold_rank(reason) > hold_rank(held))
                {
                    best_hold = Some((reason, snapshot));
                }
            }
        }
    }

    if let Some((reason, snapshot)) = best_hold {
        let days = |d: u32| time::Duration::days(i64::from(d));
        let (detail, due_at) = match reason {
            OutreachHoldReason::ContactExhausted => (
                "contact limit reached — they have not answered, and the lifetime cap is spent"
                    .to_owned(),
                None,
            ),
            OutreachHoldReason::AlreadyReplied => (
                "they answered — the reply is logged under their name".to_owned(),
                None,
            ),
            OutreachHoldReason::IneligibleTarget => (
                if contact.do_not_contact {
                    "do not contact — you marked them".to_owned()
                } else if !contact.verified {
                    "held — the address is unverified".to_owned()
                } else if !contact.accepts_outreach {
                    "held — they opted out of outreach".to_owned()
                } else {
                    "held — the contact is inactive".to_owned()
                },
                None,
            ),
            OutreachHoldReason::FollowUpNotDue => {
                let due = if snapshot.thread_followup {
                    snapshot.target_last_outreach_at.map(|at| at + days(policy.thread_followup_after_days))
                } else {
                    snapshot.last_outreach_at.map(|at| at + days(policy.followup_after_days))
                };
                ("follow-up not due yet".to_owned(), due)
            }
            OutreachHoldReason::Cooldown => {
                let due = snapshot
                    .target_last_outreach_at
                    .map(|at| at + days(policy.initial_cooldown_days));
                ("held by the contact cooldown".to_owned(), due)
            }
            OutreachHoldReason::StaleOpportunity => (
                if snapshot.thread_followup {
                    "past the follow-up window — the thread is too old to reopen".to_owned()
                } else {
                    "the opportunity has expired".to_owned()
                },
                None,
            ),
            OutreachHoldReason::FollowUpLimit => (
                "the follow-up already went out — this contact gets exactly one".to_owned(),
                None,
            ),
            OutreachHoldReason::InFlight => ("a pitch is already in flight".to_owned(), None),
            OutreachHoldReason::LowRelevance | OutreachHoldReason::InvalidPolicy | OutreachHoldReason::InvalidSnapshot => {
                ("held by the outreach rules".to_owned(), None)
            }
        };
        return ConversationNext {
            kind: "held",
            detail,
            due_at,
            wave_id: None,
        };
    }

    // No live opportunity — the fallback reads the ledger itself, the same
    // test the supply seed runs.
    if contact.do_not_contact || !contact.active {
        return ConversationNext {
            kind: "closed",
            detail: "do not contact — no automation touches this address".to_owned(),
            due_at: None,
            wave_id: None,
        };
    }
    let latest = timeline.last();
    match latest {
        Some(message) if message.direction == "inbound" => ConversationNext {
            kind: "your_turn",
            detail: "their answer is on record — log it or write back".to_owned(),
            due_at: None,
            wave_id: None,
        },
        Some(message) => {
            let unlinked = message.direction == "outbound" && message.opportunity_id.is_none();
            // The seed's own eligibility test — an answered thread is never
            // picked up, whatever a later unlinked message says, and
            // representation contacts never ride the thread lane at all.
            let eligible = contact.verified
                && contact.accepts_outreach
                && matches!(
                    contact.target_kind.as_str(),
                    "playlist"
                        | "radio"
                        | "press"
                        | "creator"
                        | "support_slot"
                        | "endorsement"
                        | "media_patronage"
                )
                && !matches!(
                    contact.last_reply_disposition.as_str(),
                    "received" | "positive" | "declined"
                );
            let age = now - message.occurred_at;
            if unlinked
                && eligible
                && age >= time::Duration::days(i64::from(policy.thread_followup_after_days))
                && age < time::Duration::days(i64::from(policy.thread_followup_window_days))
            {
                ConversationNext {
                    kind: "held",
                    detail: "inside the follow-up window — the engine picks the thread up on its next cycle".to_owned(),
                    due_at: None,
                    wave_id: None,
                }
            } else if unlinked && !eligible {
                let pitchable_kind = matches!(
                    contact.target_kind.as_str(),
                    "playlist"
                        | "radio"
                        | "press"
                        | "creator"
                        | "support_slot"
                        | "endorsement"
                        | "media_patronage"
                );
                ConversationNext {
                    kind: "held",
                    detail: if !contact.verified {
                        "held — the address is unverified".to_owned()
                    } else if !contact.accepts_outreach {
                        "held — they opted out of outreach".to_owned()
                    } else if !pitchable_kind {
                        "held — agents and labels are approached through the listing, never followed up"
                            .to_owned()
                    } else {
                        "their answer is already on record — the thread lane does not reopen it"
                            .to_owned()
                    },
                    due_at: None,
                    wave_id: None,
                }
            } else if unlinked
                && age < time::Duration::days(i64::from(policy.thread_followup_after_days))
            {
                ConversationNext {
                    kind: "held",
                    detail: "your message is recent — the follow-up window opens at ten days of silence".to_owned(),
                    due_at: Some(
                        message.occurred_at
                            + time::Duration::days(i64::from(policy.thread_followup_after_days)),
                    ),
                    wave_id: None,
                }
            } else if unlinked {
                ConversationNext {
                    kind: "none",
                    detail: "past the sixty-day window — the thread is too old to reopen".to_owned(),
                    due_at: None,
                    wave_id: None,
                }
            } else {
                ConversationNext {
                    kind: "none",
                    detail: "the last word was a pitch the ledger links — its own opportunity owns what happens next".to_owned(),
                    due_at: None,
                    wave_id: None,
                }
            }
        }
        None => ConversationNext {
            kind: "none",
            detail: "never written to".to_owned(),
            due_at: None,
            wave_id: None,
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachWrittenRequest {
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachSuppressionRequest {
    do_not_contact: bool,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

/// "Don't contact" — the drawer's third control, and the honest one: it
/// changes the contact's standing and nothing else, so the timeline never
/// pretends a reply arrived when none did.
pub async fn suppress_outreach_target(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<OutreachSuppressionRequest>,
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
        .suppress_outreach_target(
            state.ops.workspace_id(),
            SuppressOutreachTarget {
                target_id: OutreachTargetId::from_uuid(target_id),
                do_not_contact: request.do_not_contact,
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

#[cfg(test)]
mod outreach_contacts_sql_tests {
    use super::{OUTREACH_CONTACT_COUNTS_SQL, OUTREACH_CONTACTS_SQL};

    fn conversation_cte(sql: &str) -> &str {
        let end = sql.rfind("\n    SELECT").expect("a final SELECT");
        &sql[..end]
    }

    #[test]
    fn the_two_statements_share_one_conversation_cte() {
        assert_eq!(
            conversation_cte(OUTREACH_CONTACTS_SQL),
            conversation_cte(OUTREACH_CONTACT_COUNTS_SQL)
        );
    }
}
