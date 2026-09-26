// The outreach funnel (WS5): one read model that answers "what did the
// outward effort actually produce" per channel — proposals minted, asks
// approved and sent, replies that came back, and the verdicts on them.
//
// The numbers matter because they separate growth from motion: the same
// `autopilot_actions` ledger that counts a promoter pitch also counts a
// team assignment email, and a funnel that cannot tell them apart reads
// internal housekeeping as outreach. `internal` is its own channel here
// precisely so the 51%-of-successes finding can never hide inside a total
// again.
//
// Every count is grouped twice: the trailing 28 days and all time. The
// 28-day window is the comparison the weekly growth report reads; all-time
// is the day-0 baseline the production review measured against.
//
// Reply verdicts come from the interaction ledgers, not the actions table —
// a send logged from the band's own sheet is still a send, and the funnel
// would lie if it only counted what the autopilot dispatched. `sheet_logged`
// names that distinction rather than merging it into "sent".

/// The contexts that contact somebody outside the workspace, grouped the
/// way the funnel reports them. Internal-only contexts (content supply,
/// show operations, the team mailer kinds) are counted under `internal`
/// rather than dropped, so the split is visible instead of hidden.
const OUTWARD_CONTEXTS: &[&str] = &[
    "outreach",
    "booking_opportunity",
    "booking_agent",
    "beacon",
    "representation",
    "live_opportunity",
];

/// The action kinds whose success means "we emailed ourselves" — the
/// bookkeeping sends that once made half of all successful actions. They
/// are counted under `internal` wherever their context lives.
const INTERNAL_KINDS: &[&str] = &["team.assignment.email"];

/// The outbox event types that carry a real send, grouped by funnel
/// channel. `sent` in a channel's row means the outward event left the
/// ledger; `delivered`/`dead` say whether transport answered.
const CHANNEL_EVENTS: &[(&str, &[&str])] = &[
    (
        "outreach",
        &[
            "crowdrelay.outreach.requested",
            "crowdrelay.outreach.reply_requested",
        ],
    ),
    (
        "booking",
        &[
            "crowdrelay.booking.outreach_requested",
            "crowdrelay.gig.outreach_requested",
        ],
    ),
    (
        "booking_agent",
        &[
            "crowdrelay.booking_agent.approach_requested",
            "crowdrelay.booking_agent.reply_requested",
        ],
    ),
    (
        "beacon",
        &[
            "crowdrelay.beacon.outreach_requested",
            "crowdrelay.beacon.invite_batch_requested",
            "crowdrelay.beacon.invite_delivery_requested",
        ],
    ),
    ("latarnik", &["crowdrelay.latarnik.invite_requested"]),
    (
        "representation",
        &["crowdrelay.representation.approach_requested"],
    ),
    (
        "live_opportunity",
        &[
            "crowdrelay.opportunity.application_requested",
            "crowdrelay.opportunity.terms_accepted",
            "crowdrelay.opportunity.terms_countered",
            "crowdrelay.opportunity.counterparty_report_issued",
        ],
    ),
    (
        "internal",
        &["crowdrelay.team.assignment_email_requested"],
    ),
];

pub async fn funnel(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id: Uuid = state.ops.workspace_id().into_uuid();
    match load_funnel(&state.ops.pool, workspace_id).await {
        Ok(funnel) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(funnel),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "ops funnel read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

async fn load_funnel(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<serde_json::Value, sqlx::Error> {
    // The action ledger grouped by context: what the system proposed,
    // what is parked on a human, what actually ran, and what lapsed
    // unanswered. `lapsed` is the approval-expired subset of cancelled —
    // the queue-death the standing-grant machinery now answers.
    let actions = sqlx::query(
        r#"
        SELECT context,
               count(*) AS proposed,
               count(*) FILTER (WHERE status = 'awaiting_approval') AS awaiting,
               count(*) FILTER (WHERE status IN ('queued','processing')) AS in_flight,
               count(*) FILTER (WHERE status = 'succeeded') AS succeeded,
               count(*) FILTER (WHERE status = 'failed') AS failed,
               count(*) FILTER (WHERE status = 'cancelled') AS cancelled,
               count(*) FILTER (WHERE status = 'cancelled' AND last_error_kind = 'approval_expired') AS lapsed,
               count(*) FILTER (WHERE status = 'succeeded'
                                 AND finished_at >= now() - interval '28 days') AS succeeded_28d,
               count(*) FILTER (WHERE created_at >= now() - interval '28 days') AS proposed_28d
        FROM autopilot_actions
        WHERE workspace_id = $1
        GROUP BY context
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    // Internal sends get their own row — action kinds whose success is a
    // team inbox, not an outward touch.
    let internal = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT count(*) AS succeeded_all,
               count(*) FILTER (WHERE finished_at >= now() - interval '28 days') AS succeeded_28d
        FROM autopilot_actions
        WHERE workspace_id = $1 AND status = 'succeeded' AND action_kind = ANY($2)
        "#,
    )
    .bind(workspace_id)
    .bind(INTERNAL_KINDS)
    .fetch_one(pool)
    .await?;

    // Replies on the three interaction ledgers — inbound only, by
    // disposition. `positive`/`negotiating`/`declined` are the verdicts the
    // funnel actually answers for; `received` is a reply too.
    let outreach_replies = sqlx::query(
        r#"
        SELECT disposition,
               count(*) AS all_time,
               count(*) FILTER (WHERE occurred_at >= now() - interval '28 days') AS last_28d
        FROM outreach_interactions
        WHERE workspace_id = $1 AND direction = 'inbound'
        GROUP BY disposition
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    let booking_replies = sqlx::query(
        r#"
        SELECT disposition,
               count(*) AS all_time,
               count(*) FILTER (WHERE occurred_at >= now() - interval '28 days') AS last_28d
        FROM booking_interactions
        WHERE workspace_id = $1 AND direction = 'inbound'
        GROUP BY disposition
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    let agent_replies = sqlx::query(
        r#"
        SELECT disposition,
               count(*) AS all_time,
               count(*) FILTER (WHERE occurred_at >= now() - interval '28 days') AS last_28d
        FROM booking_agent_interactions
        WHERE workspace_id = $1 AND direction = 'inbound'
        GROUP BY disposition
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    // Beacon replies stamp the campaign row — no ledger to group. A reply
    // is a disposition that is not 'none'.
    let beacon_replies = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT count(*) FILTER (WHERE last_reply_disposition <> 'none') AS all_time,
               count(*) FILTER (WHERE last_reply_disposition IN ('interested','partner')) AS positive,
               count(*) FILTER (WHERE last_reply_disposition <> 'none'
                                 AND updated_at >= now() - interval '28 days') AS last_28d
        FROM beacon_campaigns
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // Sheet-logged sends: outbound interactions written by the intake
    // importer rather than an autopilot dispatch — `source_key` carries the
    // workbook book prefix (`master:`/`promo:`) the importer stamps. The
    // funnel names them separately so "sent" does not silently mean "the
    // machine sent".
    let sheet_logged = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT count(*) AS all_time,
               count(*) FILTER (WHERE occurred_at >= now() - interval '28 days') AS last_28d
        FROM outreach_interactions
        WHERE workspace_id = $1 AND direction = 'outbound'
          AND (source_key LIKE 'master:%' OR source_key LIKE 'promo:%')
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // The transport ledger per channel: an outward event that is still
    // pending is queued, `delivered` reached the executor, `dead` is the
    // kind of send that needs the outbox surface, not the funnel.
    let sends = sqlx::query(
        r#"
        SELECT event_type,
               count(*) AS all_time,
               count(*) FILTER (WHERE status = 'delivered') AS delivered,
               count(*) FILTER (WHERE status = 'dead') AS dead,
               count(*) FILTER (WHERE status IN ('pending','processing')) AS queued,
               count(*) FILTER (WHERE status = 'delivered'
                                 AND delivered_at >= now() - interval '28 days') AS delivered_28d
        FROM outbox_events
        WHERE workspace_id = $1 AND event_type = ANY($2)
        GROUP BY event_type
        "#,
    )
    .bind(workspace_id)
    .bind(
        CHANNEL_EVENTS
            .iter()
            .flat_map(|(_, types)| types.iter().copied())
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await?;

    let fans = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT count(*) FILTER (WHERE status = 'active') AS active,
               count(*) AS total,
               count(*) FILTER (WHERE created_at >= now() - interval '28 days') AS new_28d
        FROM fans
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    use sqlx::Row;
    let replies_json = |rows: &[sqlx::postgres::PgRow]| -> serde_json::Value {
        let mut by_disposition = serde_json::Map::new();
        let mut total_28d = 0_i64;
        for row in rows {
            let disposition: String = row.get("disposition");
            let all_time: i64 = row.get("all_time");
            let last_28d: i64 = row.get("last_28d");
            total_28d += last_28d;
            by_disposition.insert(
                disposition,
                json!({ "all_time": all_time, "last_28d": last_28d }),
            );
        }
        json!({ "by_disposition": by_disposition, "total_28d": total_28d })
    };

    let mut channels = serde_json::Map::new();
    for row in &actions {
        let context: String = row.get("context");
        let bucket = if OUTWARD_CONTEXTS.contains(&context.as_str()) {
            context.clone()
        } else {
            "internal".to_owned()
        };
        let entry = channels
            .entry(bucket)
            .or_insert_with(|| json!({
                "proposed": 0, "awaiting": 0, "in_flight": 0, "succeeded": 0,
                "failed": 0, "cancelled": 0, "lapsed": 0,
                "succeeded_28d": 0, "proposed_28d": 0,
            }));
        if let Some(entry) = entry.as_object_mut() {
            for key in [
                "proposed",
                "awaiting",
                "in_flight",
                "succeeded",
                "failed",
                "cancelled",
                "lapsed",
                "succeeded_28d",
                "proposed_28d",
            ] {
                let value: i64 = row.get(key);
                let current = entry.get(key).and_then(Value::as_i64).unwrap_or(0);
                entry.insert(key.to_owned(), json!(current + value));
            }
        }
    }
    // The mailer kinds fold into `internal` even when their rows sit in an
    // outward context — the split is by what the action did, not where it
    // was filed.
    if let Some(internal_entry) = channels
        .get_mut("internal")
        .and_then(Value::as_object_mut)
    {
        internal_entry.insert("mailer_succeeded".to_owned(), json!(internal.0));
        internal_entry.insert("mailer_succeeded_28d".to_owned(), json!(internal.1));
    }

    // Transport counts fold into the channel the event belongs to. A
    // channel with zero outward events still gets the zeroed shape so the
    // surface reads uniformly.
    for (channel, event_types) in CHANNEL_EVENTS {
        let entry = channels
            .entry(channel.to_string())
            .or_insert_with(|| json!({
                "proposed": 0, "awaiting": 0, "in_flight": 0, "succeeded": 0,
                "failed": 0, "cancelled": 0, "lapsed": 0,
                "succeeded_28d": 0, "proposed_28d": 0,
            }));
        let mut sent = serde_json::Map::new();
        for key in ["all_time", "delivered", "dead", "queued", "delivered_28d"] {
            sent.insert(key.to_owned(), json!(0));
        }
        for row in &sends {
            let event_type: String = row.get("event_type");
            if event_types.contains(&event_type.as_str()) {
                for key in ["all_time", "delivered", "dead", "queued", "delivered_28d"] {
                    let value: i64 = row.get(key);
                    let current = sent.get(key).and_then(Value::as_i64).unwrap_or(0);
                    sent.insert(key.to_owned(), json!(current + value));
                }
            }
        }
        if let Some(entry) = entry.as_object_mut() {
            entry.insert("sent".to_owned(), Value::Object(sent));
        }
    }

    Ok(json!({
        "window_days": 28,
        "channels": channels,
        "replies": {
            "outreach": replies_json(&outreach_replies),
            "booking": replies_json(&booking_replies),
            "booking_agent": replies_json(&agent_replies),
            "beacon": {
                "all_time": beacon_replies.0,
                "positive": beacon_replies.1,
                "last_28d": beacon_replies.2,
            },
        },
        "sheet_logged_outreach_sends": {
            "all_time": sheet_logged.0,
            "last_28d": sheet_logged.1,
        },
        "fans": {
            "active": fans.0,
            "total": fans.1,
            "new_28d": fans.2,
        },
    }))
}
