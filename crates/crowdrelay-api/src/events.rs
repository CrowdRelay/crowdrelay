//! Public event pages, durable fan interest, calendar export and conversion endpoints.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    EventCache, IdempotencyKey, ListFanEventInterests, MAX_PUBLIC_EVENT_LIMIT,
    RegisterEventInterest, RegisterEventInterestCommand, RegisterEventInterestCommandArgs,
    RepositoryError, RequestId,
};
use crowdrelay_domain::{
    CampaignId, EventAction, EventActionKind, EventSlug, PublicEvent, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::{
    IDEMPOTENCY_KEY, Problem, X_REQUEST_ID,
    acquisition::{attribution_visitor, fan_session_from_headers, referrer_host},
    request_id,
};

const PUBLIC_CACHE: &str = "public, max-age=60, stale-while-revalidate=600";
const PRIVATE_NO_STORE: &str = "private, no-store";

/// Closure that accepts an event action for asynchronous batched persistence.
pub type EventActionSubmitter = Arc<dyn Fn(EventAction) + Send + Sync>;
/// Closure that returns a point-in-time snapshot of event action ingestion counters.
pub type EventActionMetricsReader = Arc<dyn Fn() -> EventActionMetricsSnapshot + Send + Sync>;

/// Point-in-time counters for the event action ingestion pipeline.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventActionMetricsSnapshot {
    /// Total event actions accepted by the bounded buffer.
    pub queued: u64,
    /// Total event actions durably written to PostgreSQL.
    pub persisted: u64,
    /// Total event actions dropped under overload or shutdown.
    pub dropped: u64,
    /// Total event actions lost after a bounded persistence failure.
    pub persistence_failed: u64,
}

/// Dependencies and cached event data used by public event routes.
#[derive(Clone)]
pub struct EventState {
    workspace_id: WorkspaceId,
    cache: Arc<EventCache>,
    register_interest: RegisterEventInterest,
    list_fan_interests: ListFanEventInterests,
    action_submitter: EventActionSubmitter,
    action_metrics_reader: EventActionMetricsReader,
}

impl EventState {
    /// Creates event route state for one trusted workspace.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        cache: Arc<EventCache>,
        register_interest: RegisterEventInterest,
        list_fan_interests: ListFanEventInterests,
        action_submitter: EventActionSubmitter,
        action_metrics_reader: EventActionMetricsReader,
    ) -> Self {
        Self {
            workspace_id,
            cache,
            register_interest,
            list_fan_interests,
            action_submitter,
            action_metrics_reader,
        }
    }

    #[must_use]
    /// Returns a point-in-time snapshot of event-action buffer metrics.
    pub fn metrics_snapshot(&self) -> EventActionMetricsSnapshot {
        (self.action_metrics_reader)()
    }
}

/// Optional query parameters for the public event listing endpoint.
#[derive(Debug, Deserialize)]
pub struct EventListQuery {
    #[serde(default = "default_event_limit")]
    limit: u32,
}

/// Attribution query parameters appended to event conversion redirects.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct AttributionQuery {
    campaign_id: Option<CampaignId>,
}

const fn default_event_limit() -> u32 {
    50
}

#[derive(Debug, Serialize)]
struct EventListResponse {
    events: Vec<PublicEvent>,
}

/// Lists cacheable published events for the configured workspace.
pub async fn list_events(
    State(state): State<crate::AppState>,
    query: Result<Query<EventListQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return Problem::bad_request(request_id(&headers)).into_response(),
    };
    if !(1..=MAX_PUBLIC_EVENT_LIMIT).contains(&query.limit) {
        return Problem::bad_request(request_id(&headers)).into_response();
    }

    let events = state
        .events
        .cache
        .list(state.events.workspace_id, query.limit);
    let etag = events_etag(&events);
    let Ok(etag_header) = HeaderValue::from_str(&etag) else {
        tracing::error!("generated event-list ETag was not a valid HTTP header");
        return Problem::internal(request_id(&headers)).into_response();
    };
    if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)),
                (ETAG, etag_header),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)),
            (ETAG, etag_header),
        ],
        Json(EventListResponse { events }),
    )
        .into_response()
}

/// Returns one cacheable published event by slug.
pub async fn get_event(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(event) = resolve_event(&state.events, raw_slug) else {
        return Problem::not_found(request_id_value).into_response();
    };

    let etag = event_etag(&event);
    let Ok(etag_header) = HeaderValue::from_str(&etag) else {
        tracing::error!(event_id = %event.id, "generated event ETag was not a valid HTTP header");
        return Problem::internal(request_id_value).into_response();
    };
    if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)),
                (ETAG, etag_header),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)),
            (ETAG, etag_header),
        ],
        Json(event),
    )
        .into_response()
}

/// Records a ticket conversion action and redirects to the ticket provider.
pub async fn ticket_redirect(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let attribution = match query {
        Ok(Query(value)) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    event_redirect(
        state,
        raw_slug,
        headers,
        attribution.campaign_id,
        EventActionKind::TicketClick,
        |event| event.ticket_url.clone(),
    )
}

/// Records a listen conversion action and redirects to the music destination.
pub async fn listen_redirect(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let attribution = match query {
        Ok(Query(value)) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    event_redirect(
        state,
        raw_slug,
        headers,
        attribution.campaign_id,
        EventActionKind::ListenClick,
        |event| event.listen_url.clone(),
    )
}

fn event_redirect(
    state: crate::AppState,
    raw_slug: String,
    headers: HeaderMap,
    campaign_id: Option<CampaignId>,
    action: EventActionKind,
    destination: impl FnOnce(&PublicEvent) -> Option<String>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(event) = resolve_event(&state.events, raw_slug) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    let Some(destination) = destination(&event) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    let Ok(location) = HeaderValue::from_str(&destination) else {
        tracing::error!(event_id = %event.id, "stored event URL is not a valid response header");
        return Problem::internal(request_id_value)
            .private()
            .into_response();
    };

    submit_action(&state.events, &headers, &event, action, campaign_id);
    (
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
        ],
    )
        .into_response()
}

/// Generates an iCalendar download for a published event.
pub async fn calendar(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let attribution = match query {
        Ok(Query(value)) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(event) = resolve_event(&state.events, raw_slug) else {
        return Problem::not_found(request_id_value).into_response();
    };

    submit_action(
        &state.events,
        &headers,
        &event,
        EventActionKind::CalendarDownload,
        attribution.campaign_id,
    );

    let filename = format!("virya-{}.ics", event.slug.as_str());
    let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    else {
        return Problem::internal(request_id_value).into_response();
    };

    (
        StatusCode::OK,
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/calendar; charset=utf-8"),
            ),
            (CONTENT_DISPOSITION, disposition),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
        ],
        render_ics(&event),
    )
        .into_response()
}

/// JSON body accepted by the event interest registration endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventInterestRequest {
    campaign_id: Option<CampaignId>,
    #[serde(default = "default_interest_source")]
    source: String,
}

fn default_interest_source() -> String {
    "event_page".to_owned()
}

/// Registers idempotent concert interest for the current fan session.
pub async fn register_interest(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<EventInterestRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(event_slug) = EventSlug::parse(raw_slug) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    if state
        .events
        .cache
        .resolve(state.events.workspace_id, &event_slug)
        .is_none()
    {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }

    let Some(fan_session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Json(payload) = match payload {
        Ok(payload) => payload,
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
    let command = match RegisterEventInterestCommand::new(RegisterEventInterestCommandArgs {
        workspace_id: state.events.workspace_id,
        event_slug,
        fan_session,
        idempotency_key,
        request_id: command_request_id,
        campaign_id: payload.campaign_id,
        visitor_id: attribution_visitor(&headers),
        source: payload.source,
    }) {
        Ok(command) => command,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };

    match state.events.register_interest.execute(&command).await {
        Ok(result) => {
            let status = if result.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(result)).into_response()
        }
        Err(error) => repository_problem(error, request_id_value).into_response(),
    }
}

/// Optional query parameters for the authenticated fan's event listing endpoint.
#[derive(Debug, Deserialize)]
pub struct FanEventQuery {
    #[serde(default = "default_event_limit")]
    limit: u32,
}

/// Lists private event interests for the current fan session.
pub async fn my_events(
    State(state): State<crate::AppState>,
    query: Result<Query<FanEventQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    if !(1..=MAX_PUBLIC_EVENT_LIMIT).contains(&query.limit) {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };

    match state
        .events
        .list_fan_interests
        .execute(state.events.workspace_id, &session, query.limit)
        .await
    {
        Ok(events) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(events),
        )
            .into_response(),
        Err(error) => repository_problem(error, request_id_value).into_response(),
    }
}

/// Queues a non-critical event-page view measurement.
pub async fn track_view(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    track_action(state, raw_slug, query, headers, EventActionKind::PageView)
}

/// Queues a non-critical event-share measurement.
pub async fn track_share(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    track_action(state, raw_slug, query, headers, EventActionKind::ShareClick)
}

fn track_action(
    state: crate::AppState,
    raw_slug: String,
    query: Result<Query<AttributionQuery>, QueryRejection>,
    headers: HeaderMap,
    action: EventActionKind,
) -> Response {
    let request_id_value = request_id(&headers);
    let attribution = match query {
        Ok(Query(value)) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(event) = resolve_event(&state.events, raw_slug) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };

    submit_action(
        &state.events,
        &headers,
        &event,
        action,
        attribution.campaign_id,
    );
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response()
}

fn resolve_event(state: &EventState, raw_slug: String) -> Option<PublicEvent> {
    let slug = EventSlug::parse(raw_slug).ok()?;
    state.cache.resolve(state.workspace_id, &slug)
}

fn submit_action(
    state: &EventState,
    headers: &HeaderMap,
    event: &PublicEvent,
    action: EventActionKind,
    campaign_id: Option<CampaignId>,
) {
    let Ok(event_action) = EventAction::new(
        state.workspace_id,
        event.id,
        action,
        campaign_id,
        attribution_visitor(headers),
        referrer_host(headers),
        OffsetDateTime::now_utc(),
    ) else {
        return;
    };
    (state.action_submitter)(event_action);
}

fn render_ics(event: &PublicEvent) -> String {
    let starts_at = format_ics_time(event.starts_at);
    let fallback_end = event
        .starts_at
        .checked_add(time::Duration::hours(3))
        .unwrap_or(event.starts_at);
    let ends_at = format_ics_time(event.ends_at.unwrap_or(fallback_end));
    let description = event.description.as_deref().unwrap_or("Virya live");
    let location = [event.venue.as_deref(), event.venue_address.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//CrowdRelay//Virya Signal//EN\r\nCALSCALE:GREGORIAN\r\nBEGIN:VEVENT\r\nUID:{}@virya.music\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:{}\r\nDESCRIPTION:{}\r\nLOCATION:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        event.id,
        format_ics_time(OffsetDateTime::now_utc()),
        starts_at,
        ends_at,
        ics_escape(&event.title),
        ics_escape(description),
        ics_escape(&location),
    )
}

fn format_ics_time(value: OffsetDateTime) -> String {
    let value = value.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
    )
}

fn ics_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace(['\r', '\n'], "\\n")
}

fn event_etag(event: &PublicEvent) -> String {
    format!(
        "\"event-{}-{}\"",
        event.id,
        event.updated_at.unix_timestamp_nanos()
    )
}

fn events_etag(events: &[PublicEvent]) -> String {
    let mut digest = Sha256::new();
    for event in events {
        digest.update(event.id.to_string().as_bytes());
        digest.update([0]);
        digest.update(event.slug.as_str().as_bytes());
        digest.update([0]);
        digest.update(event.updated_at.unix_timestamp_nanos().to_be_bytes());
    }
    format!(
        "\"events-{}-{}\"",
        events.len(),
        hex::encode(digest.finalize())
    )
}

fn etag_matches(candidate: Option<&HeaderValue>, expected: &str) -> bool {
    candidate
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*"
                    || candidate == expected
                    || candidate.strip_prefix("W/") == Some(expected)
            })
        })
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Problem {
    match error {
        RepositoryError::Unavailable => Problem::service_unavailable(request_id),
        RepositoryError::NotFound => Problem::not_found(request_id),
        RepositoryError::Conflict => Problem::conflict(request_id),
        RepositoryError::Unexpected => Problem::internal(request_id),
    }
    .private()
}

#[cfg(test)]
mod tests {
    use crowdrelay_domain::{EventId, EventSlug, PublicEvent};
    use serde_json::Value;
    use time::OffsetDateTime;

    use super::render_ics;

    #[test]
    fn public_event_timestamps_are_rfc3339_strings() -> Result<(), Box<dyn std::error::Error>> {
        let event = PublicEvent {
            id: EventId::new(),
            slug: EventSlug::parse("namyslow-2026")?,
            title: "Sanity Check Tour".to_owned(),
            description: None,
            city: None,
            venue: Some("Art Space Avalon".to_owned()),
            venue_address: None,
            timezone: "Europe/Warsaw".to_owned(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            doors_at: Some(OffsetDateTime::UNIX_EPOCH),
            ends_at: None,
            ticket_url: None,
            listen_url: None,
            image_url: None,
            trailer_url: None,
            external_event_url: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let value = serde_json::to_value(event)?;
        assert_eq!(
            value.get("starts_at"),
            Some(&Value::String("1970-01-01T00:00:00Z".to_owned())),
        );
        assert_eq!(
            value.get("doors_at"),
            Some(&Value::String("1970-01-01T00:00:00Z".to_owned())),
        );
        assert_eq!(value.get("ends_at"), Some(&Value::Null));
        assert_eq!(
            value.get("updated_at"),
            Some(&Value::String("1970-01-01T00:00:00Z".to_owned())),
        );
        Ok(())
    }

    #[test]
    fn calendar_escapes_user_visible_fields() -> Result<(), Box<dyn std::error::Error>> {
        let event = PublicEvent {
            id: EventId::new(),
            slug: EventSlug::parse("wroclaw-2026")?,
            title: "Virya, live".to_owned(),
            description: Some("Line one\nLine two".to_owned()),
            city: None,
            venue: Some("Club; Main".to_owned()),
            venue_address: None,
            timezone: "Europe/Warsaw".to_owned(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            doors_at: None,
            ends_at: None,
            ticket_url: None,
            listen_url: None,
            image_url: None,
            trailer_url: None,
            external_event_url: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let calendar = render_ics(&event);
        assert!(calendar.contains("SUMMARY:Virya\\, live"));
        assert!(calendar.contains("DESCRIPTION:Line one\\nLine two"));
        assert!(calendar.contains("LOCATION:Club\\; Main"));
        assert!(calendar.contains("DTSTART:19700101T000000Z"));
        Ok(())
    }
}
