//! Operator surface for the invitation (P.1).
//!
//! Two routes and one rule between them: the read says who could be asked and
//! why the rest cannot, and the write asks exactly one person. There is no
//! batch endpoint on purpose — "invite everybody who qualifies" is the shape
//! that turns a professional courtesy into a mailshot, and the people on the
//! other end of this list would notice within the hour.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::contact_research::{
    ResearchError, queue_contact_research, queue_target_research, record_hook_for_beacon,
};
use crowdrelay_infra::latarnik::{
    InviteError, approve_latarnik_invite, dual_role_review, preview_latarnik_invite,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

/// The caller's idempotency key.
///
/// Read here rather than borrowed from `autopilot::validation`, which keeps it
/// private to that include chain. One rule either way: a write that can be
/// retried must carry a key, and a missing one is a bad request rather than a
/// silently fresh invitation.
fn idempotency_key(
    headers: &HeaderMap,
) -> Result<crowdrelay_application::IdempotencyKey, Box<Response>> {
    let raw = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    crowdrelay_application::IdempotencyKey::parse(raw).map_err(|_| {
        Box::new(
            Problem::bad_request_because(
                "this write needs an Idempotency-Key header so a retry cannot send twice",
                request_id(headers),
            )
            .private()
            .into_response(),
        )
    })
}

const PRIVATE_NO_STORE: &str = "private, no-store";

/// `GET /v1/control-plane/contacts/dual-role`
///
/// Everybody the band works with, with both roles resolved: who already hears
/// the dates, who could be asked, and — for everybody else — the sentence
/// saying why not.
pub async fn dual_role_contacts(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    // Whether there is anything to tell people is a question about the
    // calendar, and the per-contact answer is computed at approval time. The
    // read reports eligibility on the assumption that a reason exists, so the
    // operator sees the relationship rules rather than an empty screen on a
    // quiet week; the approval refuses if the reason turns out not to be there.
    match dual_role_review(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        now,
        true,
    )
    .await
    {
        Ok(review) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(review),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "dual-role contact read failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// What the band found out about a person: one dated, sourced thing they did.
#[derive(serde::Deserialize)]
pub struct ResearchBody {
    fact: String,
    #[serde(default)]
    praise: Option<String>,
    source_url: String,
    /// `YYYY-MM-DD`: when the thing happened or was published.
    observed_on: String,
    #[serde(default)]
    language: Option<String>,
}

fn parse_day(value: &str) -> Option<time::Date> {
    let mut parts = value.trim().splitn(3, '-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = time::Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

/// `PUT /v1/control-plane/contacts/{beacon_id}/research`
///
/// Records what the band read of this person lately, so a letter can open with
/// it. Nobody is written to as a stranger: until a recent, sourced fact is on
/// file the eligibility rule holds the person as "not read yet". The research
/// agent's result and a person's note come through the same checks (https
/// source, recent, no hype, no links), and the same source again replaces the
/// earlier row, so repeating the call is safe.
pub async fn record_research(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ResearchBody>,
) -> Response {
    let Ok(beacon_id) = Uuid::parse_str(&beacon_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let today = OffsetDateTime::now_utc().date();
    let Some(observed_on) = parse_day(&body.observed_on) else {
        return (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": "observed_on is a date, YYYY-MM-DD" })),
        )
            .into_response();
    };
    match record_hook_for_beacon(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        beacon_id,
        &body.fact,
        body.praise.as_deref(),
        &body.source_url,
        observed_on,
        body.language.as_deref().unwrap_or("pl"),
        "operator",
        today,
    )
    .await
    {
        Ok(hook) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "recorded": true,
                "fact": hook.fact,
                "observed_on": hook.observed_on.to_string(),
            })),
        )
            .into_response(),
        Err(ResearchError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(ResearchError::Refused(sentence)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": sentence })),
        )
            .into_response(),
        Err(ResearchError::Database(error)) => {
            tracing::warn!(%error, "contact research write failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// `POST /v1/control-plane/contacts/{beacon_id}/research/request`
///
/// Sends the research agent to read one person's recent work. Contacts nobody,
/// so no approval is involved; it is still bounded: only people who would
/// otherwise be askable are researched, and one person at most once a week.
/// Returns the queued task id, or `{"refused": "<sentence>"}`.
pub async fn request_research(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(beacon_id) = Uuid::parse_str(&beacon_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    match queue_contact_research(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        beacon_id,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(task_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "queued": task_id })),
        )
            .into_response(),
        Err(ResearchError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(ResearchError::Refused(sentence)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": sentence })),
        )
            .into_response(),
        Err(ResearchError::Database(error)) => {
            tracing::warn!(%error, "contact research request failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// `POST /v1/control-plane/contacts/targets/{target_id}/research/request`
///
/// The same research for an outreach target that is not a beacon (press, radio,
/// playlists): the engine pitches those too, and nobody is pitched unread.
pub async fn request_target_research(
    State(state): State<crate::AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    match queue_target_research(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        target_id,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(task_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "queued": task_id })),
        )
            .into_response(),
        Err(ResearchError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(ResearchError::Refused(sentence)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": sentence })),
        )
            .into_response(),
        Err(ResearchError::Database(error)) => {
            tracing::warn!(%error, "target research request failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// `GET /v1/control-plane/contacts/{beacon_id}/latarnik-invite/preview`
///
/// The letter this person would receive, composed by the same code and checked
/// by the same rules as the approval, and **not queued**. The `POST` below is
/// the approval itself (it queues the action as already approved), so the only
/// way to approve what was read is to read it first. A refusal comes back as
/// `{"refused": "<sentence>"}` exactly as it does from the `POST`.
pub async fn preview_invite(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(beacon_id) = Uuid::parse_str(&beacon_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    match preview_latarnik_invite(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        beacon_id,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(preview) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(preview),
        )
            .into_response(),
        Err(InviteError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(InviteError::Refused(sentence)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": sentence })),
        )
            .into_response(),
        Err(InviteError::Database(error)) => {
            tracing::warn!(%error, "latarnik invite preview failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// `POST /v1/control-plane/contacts/{beacon_id}/latarnik-invite`
///
/// Asks one person, once. Every refusal comes back as 200 with its sentence:
/// "this person unsubscribed" is an answer, not a server fault.
pub async fn invite_to_latarnik(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(beacon_id) = Uuid::parse_str(&beacon_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match approve_latarnik_invite(
        &state.database,
        state.ops.workspace_id().into_uuid(),
        beacon_id,
        &idempotency_key,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(outcome) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "outcome": format!("{outcome:?}") })),
        )
            .into_response(),
        Err(InviteError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(InviteError::Refused(sentence)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "refused": sentence })),
        )
            .into_response(),
        Err(InviteError::Database(error)) => {
            tracing::warn!(%error, "latarnik invite failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}
