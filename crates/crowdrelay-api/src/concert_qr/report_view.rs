// The T+7 report view: what the counterparty's email looks like. When the
// report has been issued this renders the durable artifact itself — the
// exact payload n8n mailed — never a re-derivation. Before it exists, the
// same facts are composed live so the band can see what the night will say.
//
// The numbers keep the report's evidence contract: observed is first-party
// room evidence only, inferred is reach that does not prove presence, and
// evidence gaps are named rather than zeroed. The queries deliberately
// mirror `issue_post_show_report` (infra) — if that composition changes,
// this preview must change with it or the page stops telling the truth.

#[derive(Debug, FromRow)]
struct ReportEventRow {
    id: Uuid,
    slug: String,
    title: String,
    city: Option<String>,
    venue: Option<String>,
    starts_at: OffsetDateTime,
    timezone: String,
    counterparty_name: Option<String>,
    counterparty_email: Option<String>,
    acts: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventReportResponse {
    /// True once `issue_post_show_report` has emitted — `report` then IS the
    /// mailed artifact. False means the report below is a live preview of
    /// what issuance would compose.
    issued: bool,
    /// `delivered_at` when the mail left, else the emission's `created_at` —
    /// `delivery_status` says which.
    issued_at: Option<String>,
    /// The outbox row's status (`delivered`/`pending`/`processing`/`dead`)
    /// when issued — "sent" is only honest once `delivered`.
    delivery_status: Option<String>,
    event: serde_json::Value,
    report: serde_json::Value,
    recipients: serde_json::Value,
    honesty_contract: serde_json::Value,
}

/// `GET /v1/control-plane/events/{event_slug}/report` — the artifact preview.
pub async fn control_plane_event_report(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match crate::ops::hold(
        &state.read_budget,
        load_report_facts(&state.concert_qr, &event_slug),
    )
    .await
    {
        Ok(Some(facts)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(ControlPlaneEventReportResponse {
                issued: facts.issued_at.is_some(),
                issued_at: facts.issued_at.map(format_time),
                delivery_status: facts.delivery_status,
                event: facts.event,
                report: facts.report,
                recipients: facts.recipients,
                honesty_contract: facts.honesty_contract,
            }),
        )
            .into_response(),
        Ok(None) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "control-plane event report query failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

struct ReportFacts {
    issued_at: Option<OffsetDateTime>,
    delivery_status: Option<String>,
    event: serde_json::Value,
    report: serde_json::Value,
    recipients: serde_json::Value,
    honesty_contract: serde_json::Value,
}

async fn load_report_facts(
    state: &ConcertQrState,
    event_slug: &str,
) -> Result<Option<ReportFacts>, sqlx::Error> {
    let workspace_id = state.workspace_id.into_uuid();
    let Some(event) = sqlx::query_as::<_, ReportEventRow>(
        r#"
        SELECT event.id, event.slug, event.title, city.name AS city, event.venue,
            event.starts_at, event.timezone,
            event.counterparty_name, event.counterparty_email,
            (SELECT jsonb_agg(jsonb_build_object('slug', act.act_slug, 'name', act.act_name)
                     ORDER BY act.position, act.act_slug)
             FROM event_acts AS act
             WHERE act.workspace_id = event.workspace_id
               AND act.event_id = event.id) AS acts
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.slug = $2
          AND event.status IN ('published','completed')
        "#,
    )
    .bind(workspace_id)
    .bind(event_slug)
    .fetch_optional(&state.database)
    .await?
    else {
        return Ok(None);
    };

    // The issued artifact wins: the mailed payload is the report, byte for
    // byte. Keyed by event_id inside the payload — the emission's only
    // durable link back to the night. These rows are exempt from terminal
    // outbox retention (the artifact IS the record), so this lookup stays
    // valid for the life of the tenant.
    let issued = sqlx::query_as::<
        _,
        (
            serde_json::Value,
            String,
            Option<OffsetDateTime>,
            OffsetDateTime,
        ),
    >(
        r#"
        SELECT payload, status, delivered_at, created_at
        FROM outbox_events
        WHERE workspace_id = $1
          AND event_type = 'crowdrelay.show.post_show_report_due'
          AND payload->>'event_id' = $2
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event.id.to_string())
    .fetch_optional(&state.database)
    .await?;

    if let Some((payload, status, delivered_at, emitted)) = issued {
        let object = payload.as_object().cloned().unwrap_or_default();
        return Ok(Some(ReportFacts {
            issued_at: Some(delivered_at.unwrap_or(emitted)),
            delivery_status: Some(status),
            event: object
                .get("event")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            report: object
                .get("report")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            recipients: object
                .get("recipients")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            honesty_contract: object
                .get("honesty_contract")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }));
    }

    // Not issued yet — compose the preview from the same evidence.
    let event_id = event.id;
    let numbers = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2) AS checkins_total,
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND c.identity_source = 'session') AS checkins_session,
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND c.identity_source = 'email_claim') AS checkins_email_claim,
            (SELECT count(*) FROM concert_checkins AS c
             JOIN fans AS fan
               ON fan.workspace_id = c.workspace_id AND fan.id = c.fan_id
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND fan.created_at >= (
                   SELECT starts_at - interval '6 hours'
                   FROM events WHERE workspace_id = $1 AND id = $2
               )
               AND fan.created_at <= (
                   SELECT starts_at + interval '12 hours'
                   FROM events WHERE workspace_id = $1 AND id = $2
               )) AS new_fan_records,
            (SELECT count(*) FROM admission_passes AS p
             WHERE p.workspace_id = $1 AND p.event_id = $2
               AND p.status = 'redeemed') AS passes_redeemed,
            (SELECT count(*) FROM event_action_events AS a
             WHERE a.workspace_id = $1 AND a.event_id = $2
               AND a.action = 'ticket_click') AS ticket_clicks,
            (SELECT count(DISTINCT lower(o.buyer_email)) FROM ticket_orders AS o
             JOIN ticket_sales AS s
               ON s.workspace_id = o.workspace_id AND s.id = o.ticket_sale_id
             WHERE o.workspace_id = $1 AND s.event_id = $2
               AND o.status IN ('paid', 'partially_refunded')) AS paid_buyers,
            (SELECT count(*) FROM event_interests AS i
             WHERE i.workspace_id = $1 AND i.event_id = $2) AS interested_fans
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    let act_clicks = sqlx::query_as::<_, (Option<String>, i64)>(
        r#"
        SELECT act_slug, count(*) AS clicks
        FROM event_action_events
        WHERE workspace_id = $1 AND event_id = $2 AND action = 'ticket_click'
        GROUP BY act_slug
        ORDER BY clicks DESC, act_slug
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let campaigns = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<OffsetDateTime>,
            Option<i32>,
            Option<i32>,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT slug, template_key, status, scheduled_at,
               recipient_count, delivered_count, completed_at
        FROM communication_campaigns
        WHERE workspace_id = $1 AND content->>'event_id' = $2
        ORDER BY scheduled_at NULLS LAST, slug
        "#,
    )
    .bind(workspace_id)
    .bind(event_id.to_string())
    .fetch_all(&state.database)
    .await?;

    let band = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT normalized_email, COALESCE(display_name, normalized_email)
        FROM workspace_members
        WHERE workspace_id = $1 AND status = 'active'
        ORDER BY display_name
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&state.database)
    .await?;

    let (
        checkins_total,
        checkins_session,
        checkins_email_claim,
        new_fan_records,
        passes_redeemed,
        ticket_clicks,
        paid_buyers,
        interested_fans,
    ) = numbers;

    let mut evidence_gaps: Vec<&str> = Vec::new();
    if checkins_total == 0 && passes_redeemed == 0 {
        evidence_gaps.push("room_attendance_unverified");
    }
    if event.counterparty_email.is_none() {
        evidence_gaps.push("no_counterparty_on_record");
    }
    if band.is_empty() {
        evidence_gaps.push("no_active_band_recipient");
    }
    if campaigns.is_empty() {
        evidence_gaps.push("no_event_campaigns_on_record");
    }

    Ok(Some(ReportFacts {
        issued_at: None,
        delivery_status: None,
        event: serde_json::json!({
            "slug": event.slug,
            "title": event.title,
            "city": event.city,
            "venue": event.venue,
            "starts_at": format_time(event.starts_at),
            "timezone": event.timezone,
            "acts": event.acts.unwrap_or_else(|| serde_json::json!([])),
        }),
        report: serde_json::json!({
            "kind": "post_show_t7",
            "generated_at": null,
            "preview": true,
            "observed": {
                "room_checkins_total": checkins_total,
                "room_checkins_by_session": checkins_session,
                "room_checkins_by_email_claim": checkins_email_claim,
                "new_fan_records_at_show": new_fan_records,
                "admission_passes_redeemed": passes_redeemed,
            },
            "inferred": {
                "paid_ticket_buyers": paid_buyers,
                "interested_fans": interested_fans,
                "ticket_link_clicks": ticket_clicks,
                "ticket_link_clicks_by_act": act_clicks
                    .iter()
                    .map(|(slug, clicks)| serde_json::json!({
                        "act_slug": slug,
                        "clicks": clicks,
                    }))
                    .collect::<Vec<_>>(),
            },
            "campaigns": campaigns
                .iter()
                .map(|row| serde_json::json!({
                    "slug": row.0,
                    "template_key": row.1,
                    "status": row.2,
                    "scheduled_at": row.3,
                    "recipients": row.4,
                    "delivered": row.5,
                    "completed_at": row.6,
                }))
                .collect::<Vec<_>>(),
            "evidence_gaps": evidence_gaps,
        }),
        recipients: serde_json::json!({
            "band": band
                .iter()
                .map(|(email, name)| serde_json::json!({"email": email, "name": name}))
                .collect::<Vec<_>>(),
            "counterparty": match (&event.counterparty_name, &event.counterparty_email) {
                (_, Some(email)) => serde_json::json!({
                    "name": event.counterparty_name,
                    "email": email,
                }),
                _ => serde_json::Value::Null,
            },
        }),
        honesty_contract: serde_json::json!({
            "observed": "first-party room evidence only — QR check-ins and redeemed admission passes",
            "inferred": "suggests reach or attendance but does not prove presence in the room",
            "rules": [
                "never_sum_numbers_across_evidence_classes",
                "state_evidence_gaps_explicitly_do_not_zero_them",
                "do_not_claim_attendance_or_reach_the_records_do_not_support",
                "the_artifact_is_the_whole_report_no_account_required"
            ]
        }),
    }))
}
