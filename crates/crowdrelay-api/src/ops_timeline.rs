// Request/correlation timeline is kept out of `ops.rs` to preserve the
// control-plane source-size ratchet. This file is included into the `ops`
// module, so it deliberately shares its private database state and helpers.

#[derive(Debug, Serialize)]
pub struct OperationTimeline {
    request_id: String,
    events: Vec<OperationTimelineEvent>,
}
#[derive(Debug, Serialize, FromRow)]
pub struct OperationTimelineEvent {
    occurred_at: OffsetDateTime,
    source: String,
    kind: String,
    status: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
}

/// Returns a metadata-only timeline for one request/correlation identifier.
///
/// Payloads, webhook URLs, actor details, free-form operator details and audit
/// metadata are deliberately excluded so an operator can trace a flow without
/// exposing fan data or secrets.
pub async fn operation_timeline(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(raw_request_id): Path<String>,
) -> Response {
    let timeline_request_id = match parse_timeline_request_id(&raw_request_id) {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    match run_with_timeout(
        state.ops.operation_timeout,
        load_operation_timeline(&state.ops, &timeline_request_id),
    )
    .await
    {
        Ok(events) if events.is_empty() => {
            Problem::not_found(request_id(&headers)).private().into_response()
        }
        Ok(events) => private_json(
            StatusCode::OK,
            OperationTimeline {
                request_id: timeline_request_id,
                events,
            },
        ),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

async fn load_operation_timeline(
    state: &OpsState,
    timeline_request_id: &str,
) -> Result<Vec<OperationTimelineEvent>, OpsError> {
    sqlx::query_as::<_, OperationTimelineEvent>(
        r#"
        SELECT occurred_at, source, kind, status, target_type, target_id
        FROM (
            SELECT
                occurred_at,
                'audit'::text AS source,
                action::text AS kind,
                NULL::text AS status,
                target_type::text AS target_type,
                target_id::text AS target_id
            FROM audit_events
            WHERE workspace_id = $1 AND request_id = $2

            UNION ALL

            SELECT
                created_at AS occurred_at,
                'outbox'::text AS source,
                event_type::text AS kind,
                status::text AS status,
                'outbox_event'::text AS target_type,
                id::text AS target_id
            FROM outbox_events
            WHERE workspace_id = $1 AND request_id = $2

            UNION ALL

            SELECT
                delivery.created_at AS occurred_at,
                'delivery'::text AS source,
                endpoint.name::text AS kind,
                delivery.status::text AS status,
                'webhook_delivery'::text AS target_type,
                delivery.id::text AS target_id
            FROM webhook_deliveries AS delivery
            JOIN outbox_events AS outbox
              ON outbox.workspace_id = delivery.workspace_id
             AND outbox.id = delivery.outbox_event_id
            JOIN webhook_endpoints AS endpoint
              ON endpoint.workspace_id = delivery.workspace_id
             AND endpoint.id = delivery.endpoint_id
            WHERE delivery.workspace_id = $1 AND outbox.request_id = $2

            UNION ALL

            SELECT
                created_at AS occurred_at,
                'operator'::text AS source,
                action::text AS kind,
                NULL::text AS status,
                target_type::text AS target_type,
                target_id::text AS target_id
            FROM operator_actions
            WHERE workspace_id = $1 AND request_id = $2
        ) AS timeline
        ORDER BY occurred_at ASC, source ASC, target_id ASC NULLS LAST
        LIMIT 250
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(timeline_request_id)
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

fn parse_timeline_request_id(value: &str) -> Result<String, OpsError> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte)))
    .then(|| value.to_owned())
    .ok_or(OpsError::BadRequest)
}
