// Delivery-fault ingestion — the ear half of deliverability.
//
// The sending provider reports bounces and spam complaints; this layer checks
// transport shape only. What a fault means for the workspace's ceiling is
// decided by `crowdrelay_domain::deliverability` at evaluation time, and what
// a hard bounce means for one address is decided by the repository. Nothing
// here computes rates and nothing here writes SQL.

/// Bounds mirror migration 0101's CHECK constraints, so an out-of-range
/// reference is refused at the boundary rather than truncated into the ledger.
const MAX_PROVIDER_REFERENCE: usize = 200;

/// A fault is counted over a rolling thirty-day window, so an `occurred_at`
/// far outside that window is either a clock worth fixing or an attempt to
/// park a complaint where it pollutes the halt for months. Receipts may drain
/// for up to seven days; five minutes of skew covers a slow workflow's clock.
const MAX_DELIVERY_FAULT_AGE: time::Duration = time::Duration::days(7);
const DELIVERY_FAULT_CLOCK_SKEW: time::Duration = time::Duration::minutes(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryFaultRequest {
    /// Exactly one of `target_id` / `contact_email`. Webhooks from providers
    /// report addresses, not our ids; the admin surface already knows its
    /// own targets.
    target_id: Option<Uuid>,
    contact_email: Option<String>,
    fault: DeliveryFault,
    /// The provider's own reference, where it gave one. Webhooks retry; the
    /// dedupe on this reference is what makes a retried complaint a replay
    /// rather than a second halt nobody earned.
    provider_reference: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

fn validate_delivery_fault(
    request: DeliveryFaultRequest,
) -> Result<RecordDeliveryFault, ()> {
    let subject = match (request.target_id, request.contact_email) {
        (Some(uuid), None) => DeliveryFaultSubject::Target(OutreachTargetId::from_uuid(uuid)),
        (None, Some(email)) => {
            let trimmed = email.trim().to_ascii_lowercase();
            if trimmed.is_empty()
                || trimmed.len() > 320
                || !trimmed.contains('@')
            {
                return Err(());
            }
            DeliveryFaultSubject::ContactEmail(trimmed)
        }
        // Exactly one. Both is ambiguous; neither is unaddressable.
        _ => return Err(()),
    };
    let reference = match request.provider_reference.as_deref() {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() || value.chars().count() > MAX_PROVIDER_REFERENCE {
                return Err(());
            }
            Some(trimmed.to_string())
        }
        None => None,
    };
    // The halt is only ever as fresh as the last report, and only as honest
    // as its dates. A fault dated outside the window it feeds is refused
    // rather than clamped: a silently moved timestamp is a number nobody can
    // argue with later.
    let now = OffsetDateTime::now_utc();
    if request.occurred_at > now + DELIVERY_FAULT_CLOCK_SKEW
        || request.occurred_at < now - MAX_DELIVERY_FAULT_AGE
    {
        return Err(());
    }
    Ok(RecordDeliveryFault {
        subject,
        fault: request.fault,
        provider_reference: reference,
        occurred_at: request.occurred_at,
    })
}

/// Executor-facing ingestion on the internal surface.
///
/// Deliverability reporting is executor work: the n8n branch that receives the
/// provider webhook holds the commerce credential and no admin key. Requiring
/// the admin key here would hand a bounce relay authority over ninety-seven
/// admin routes to post one row.
pub async fn record_delivery_fault(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DeliveryFaultRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    // A fault report without an identity is indistinguishable from a replay:
    // the Idempotency-Key is what makes the second delivery of one complaint a
    // no-op rather than a counted event.
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let command = match validate_delivery_fault(request) {
        Ok(command) => command,
        Err(()) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .record_delivery_fault(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
