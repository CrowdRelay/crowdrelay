/// Body of the manual show-entry endpoint. One form's worth of fields — the
/// sync adapters write richer rows (description, images, provider ids) and
/// this surface deliberately does not: an operator hand-typing a gig needs
/// the night, the room and the door link, not a second CMS.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEventRequest {
    title: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    doors_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    ends_at: Option<OffsetDateTime>,
    venue: Option<String>,
    venue_address: Option<String>,
    /// City is a pair: a name alone cannot pick a row out of the shared
    /// registry, and a code alone says nothing. Both or neither.
    city_name: Option<String>,
    city_country_code: Option<String>,
    city_region: Option<String>,
    /// The venue's clock; absent resolves to the tenant's `crew_timezone`
    /// setting, then 'Europe/Warsaw'.
    timezone: Option<String>,
    ticket_url: Option<String>,
    /// `true` announces the night publicly now; absent or `false` stores a
    /// draft. Internal reads (checklists, gig planning, reports) see it
    /// either way — announcing stays its own deliberate act.
    #[serde(default)]
    publish: bool,
}

/// Creates one show by hand (staff/admin/control-plane).
///
/// The path for a tenant whose label never ran a sync source: every show
/// surface reads `events`, so one honest row is all the machinery needs.
/// Idempotent on `Idempotency-Key` — a retried submit returns the show it
/// already created, not a second one.
///
/// `POST /v1/{admin,staff,control-plane}/events`
pub async fn create_event(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateEventRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match body {
        Ok(value) => value,
        Err(rejection) => {
            let problem = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                Problem::payload_too_large(request_id_value)
            } else {
                Problem::bad_request(request_id_value)
            };
            return problem.private().into_response();
        }
    };
    let idempotency_key = match headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(IdempotencyKey::parse)
    {
        Some(Ok(key)) => key,
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(raw_request_id) = headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
    else {
        return Problem::internal(None).private().into_response();
    };
    let Ok(command_request_id) = RequestId::parse(raw_request_id) else {
        return Problem::internal(None).private().into_response();
    };

    let trim_to_option = |value: Option<String>| {
        value
            .map(|inner| inner.trim().to_owned())
            .filter(|inner| !inner.is_empty())
    };
    let venue = trim_to_option(payload.venue);
    let venue_address = trim_to_option(payload.venue_address);
    let city_name = trim_to_option(payload.city_name);
    let city_region = trim_to_option(payload.city_region);
    let ticket_url = trim_to_option(payload.ticket_url);
    let timezone = trim_to_option(payload.timezone);
    let city_country_code =
        trim_to_option(payload.city_country_code).map(|code| code.to_uppercase());
    let title = payload.title.trim().to_owned();

    let fields_ok =
        crowdrelay_domain::validate_event_write_fields(&crowdrelay_domain::EventWriteFields {
            title: &title,
            timezone: timezone.as_deref(),
            venue: venue.as_deref(),
            venue_address: venue_address.as_deref(),
            ticket_url: ticket_url.as_deref(),
            doors_at: payload.doors_at,
            starts_at: payload.starts_at,
            ends_at: payload.ends_at,
        })
        .is_ok();
    let timezone_known = timezone
        .as_deref()
        .is_none_or(crowdrelay_infra::regional::is_known_iana_timezone);
    // City is a pair — name and country code together or neither — and a
    // region only colours a pair that exists.
    let city_ok = city_name.is_some() == city_country_code.is_some()
        && (city_region.is_none() || city_name.is_some())
        && city_country_code.as_deref().is_none_or(|code| {
            code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase())
        });
    if !fields_ok || !timezone_known || !city_ok {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }

    let command = CreateEventCommand {
        workspace_id: state.events.workspace_id,
        slug_base: crowdrelay_domain::slugify(&title).unwrap_or_else(|| "show".to_owned()),
        title,
        timezone,
        starts_at: payload.starts_at,
        doors_at: payload.doors_at,
        ends_at: payload.ends_at,
        venue,
        venue_address,
        city_name,
        city_country_code,
        city_region,
        ticket_url,
        publish: payload.publish,
        idempotency_key,
        request_id: command_request_id,
    };
    match state.events.create_event.execute(&command).await {
        // Echo the row back — the caller links straight to the night it made
        // without a second fetch.
        Ok(created) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(created),
        )
            .into_response(),
        Err(error) => repository_problem(error, request_id_value).into_response(),
    }
}
