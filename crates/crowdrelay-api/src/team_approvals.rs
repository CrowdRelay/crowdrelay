// §6-C: the one-click approvals the crew digest and `team.assignment.email`
// payloads link to. GET renders the ask — never mutates — and POST applies
// the verdict the form carries. The token is the whole credential, so the
// page says plainly what answering does, and a link that outlived its window
// says that instead of deciding anyway.

use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_domain::{
    AutopilotActionId,
    team_approval_token::{self, ApprovalTokenError},
};
use serde::Deserialize;

use crate::{AppState, Problem, request_id};

/// `GET`/`POST /v1/public/approvals/{token}` — a pending ask, answered from
/// the crew's inbox: the mailed token is the whole credential, GET renders
/// and POST decides, and the same URL serves both so the link cannot drift
/// from the decision. Merged into the public router, which is why it lives
/// here rather than inline in `routing.rs` (that file sits at its size
/// ratchet).
pub(crate) fn public_routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/v1/public/approvals/{token}",
        axum::routing::get(public_approval_page).post(public_approval_decide),
    )
}

#[derive(Deserialize)]
pub(crate) struct ApprovalVerdict {
    verdict: String,
}

/// `GET /v1/public/approvals/{token}` — the ask, in the crew's language, with
/// one form per answer. Reads only; the decision lives behind POST because a
/// link previewer must never be a vote.
pub(crate) async fn public_approval_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (claims, view) = match resolve(&state, &token).await {
        Ok(resolved) => resolved,
        Err(failure) => return failure.respond(&headers),
    };
    let locale = crew_locale(&state).await;
    if view.status != "awaiting_approval" {
        return page(StatusCode::OK, &decided_panel(&view.status, locale));
    }
    page(StatusCode::OK, &ask_panel(&claims, &view, locale))
}

/// `POST /v1/public/approvals/{token}` — `verdict=approve|skip`, decided
/// through the same repository transition the admin approve/cancel endpoints
/// run, stamped `email-link` so the ledger records the door it came through.
pub(crate) async fn public_approval_decide(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: axum::http::HeaderMap,
    Form(form): Form<ApprovalVerdict>,
) -> Response {
    if !matches!(form.verdict.as_str(), "approve" | "skip") {
        return Problem::bad_request_because(
            "verdict must be 'approve' or 'skip'",
            request_id(&headers),
        )
        .private()
        .into_response();
    }
    let (claims, view) = match resolve(&state, &token).await {
        Ok(resolved) => resolved,
        Err(failure) => return failure.respond(&headers),
    };
    let locale = crew_locale(&state).await;
    if view.status != "awaiting_approval" {
        return page(StatusCode::OK, &decided_panel(&view.status, locale));
    }

    let workspace_id = state.ops.workspace_id();
    let action_id = AutopilotActionId::from_uuid(claims.action_id);
    let idempotency_key =
        match IdempotencyKey::parse(format!("email-approval-{}-{}", action_id, form.verdict)) {
            Ok(key) => key,
            Err(_) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        };
    let parsed_request = request_id(&headers).and_then(|value| RequestId::parse(value).ok());
    let result = match form.verdict.as_str() {
        "approve" => {
            state
                .autopilot
                .approve_action_from_email_link(
                    workspace_id,
                    action_id,
                    &idempotency_key,
                    parsed_request.as_ref(),
                )
                .await
        }
        _ => {
            state
                .autopilot
                .skip_action_from_email_link(
                    workspace_id,
                    action_id,
                    &idempotency_key,
                    parsed_request.as_ref(),
                )
                .await
        }
    };
    match result {
        Ok(mutation) => {
            // A replayed key answers with the action's live status — a second
            // click on the same button is the same answer, not a conflict.
            if mutation.replayed {
                return page(StatusCode::OK, &decided_panel(&mutation.status, locale));
            }
            page(
                StatusCode::OK,
                &answered_panel(&form.verdict, &mutation.status, locale),
            )
        }
        Err(error) => {
            use crowdrelay_application::RepositoryError;
            match error {
                RepositoryError::NotFound => Problem::not_found(request_id(&headers))
                    .private()
                    .into_response(),
                // The status guard raced a real decision — report the state
                // the action is actually in rather than an error page.
                RepositoryError::Conflict | RepositoryError::ConflictBecause(_) => {
                    page(StatusCode::OK, &decided_panel("decided elsewhere", locale))
                }
                _ => Problem::service_unavailable(request_id(&headers))
                    .private()
                    .into_response(),
            }
        }
    }
}

/// Token decode then action load — the two steps every method shares, with
/// the failure mapping §6-C fixes: malformed or forged is a 404 (the link
/// proves nothing), expired is a 410 (the window is the message).
async fn resolve(
    state: &AppState,
    token: &str,
) -> Result<
    (
        team_approval_token::ApprovalTokenClaims,
        crowdrelay_infra::autopilot::ApprovalLinkView,
    ),
    ResolveFailure,
> {
    let claims = match team_approval_token::decode(
        token,
        &state.team_approval_key,
        time::OffsetDateTime::now_utc(),
    ) {
        Ok(claims) => claims,
        Err(ApprovalTokenError::Expired) => return Err(ResolveFailure::Expired),
        Err(_) => return Err(ResolveFailure::NotFound),
    };
    let view = state
        .autopilot
        .approval_link_view(
            state.ops.workspace_id(),
            AutopilotActionId::from_uuid(claims.action_id),
        )
        .await;
    match view {
        Ok(Some(view)) => Ok((claims, view)),
        Ok(None) => Err(ResolveFailure::NotFound),
        Err(error) => {
            tracing::warn!(%error, "approval link view failed");
            Err(ResolveFailure::Unavailable)
        }
    }
}

/// How `resolve` can fail, kept small so the handlers' `Result` stays cheap —
/// the response is built at the call site, not carried in the error.
enum ResolveFailure {
    /// The token's window has closed — a mailed link past its expiry is gone,
    /// not missing.
    Expired,
    /// Malformed, forged, or pointing at nothing — the link proves nothing
    /// about this caller.
    NotFound,
    /// The action's view could not be produced.
    Unavailable,
}

impl ResolveFailure {
    fn respond(self, headers: &axum::http::HeaderMap) -> Response {
        match self {
            Self::Expired => Problem::gone(request_id(headers)),
            Self::NotFound => Problem::not_found(request_id(headers)),
            Self::Unavailable => Problem::service_unavailable(request_id(headers)),
        }
        .private()
        .into_response()
    }
}

async fn crew_locale(state: &AppState) -> crowdrelay_application::autopilot::BriefingLocale {
    let tag =
        crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(state.database.clone())
            .crew_locale(state.ops.workspace_id().into_uuid())
            .await
            .unwrap_or_default();
    crowdrelay_application::autopilot::BriefingLocale::from_tag(&tag)
}

/// Everything the clicker needs to answer the ask: what kind of work it is,
/// the same summary and detail the briefing panel shows, the deadline, and
/// one plain form per verdict. No JavaScript — the link must work from a
/// mail client's own HTML viewer.
fn ask_panel(
    claims: &team_approval_token::ApprovalTokenClaims,
    view: &crowdrelay_infra::autopilot::ApprovalLinkView,
    locale: crowdrelay_application::autopilot::BriefingLocale,
) -> String {
    use crowdrelay_application::autopilot::BriefingLocale;
    let briefing = serde_json::from_value::<
        crowdrelay_application::autopilot::AutopilotActionPayload,
    >(view.payload.clone())
    .map(|payload| payload.briefing().localized(locale));
    let (kind, title, detail, deadline) = match briefing {
        Ok(briefing) => (
            crowdrelay_infra::autopilot::friendly_action_title(&view.action_kind, locale),
            briefing.summary,
            briefing.why_it_matters,
            briefing.deadline_note,
        ),
        Err(_) => (
            crowdrelay_infra::autopilot::friendly_action_title(&view.action_kind, locale),
            view.action_kind.clone(),
            String::new(),
            String::new(),
        ),
    };
    let due = claims
        .expires_at
        .format(&time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute] UTC"
        ))
        .unwrap_or_default();
    let (approve_label, skip_label, due_label) = match locale {
        BriefingLocale::Pl => ("Zatwierdź", "Pomiń", "Termin"),
        BriefingLocale::En => ("Approve", "Skip", "Due"),
    };
    let deadline_line = if deadline.is_empty() {
        format!("<p class=\"due\">{due_label}: {due}</p>")
    } else {
        format!(
            "<p class=\"due\">{due_label}: {}</p>",
            escape_html(&deadline)
        )
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{CSS}</style></head><body><main><p class=\"kind\">{kind}</p><h1>{title}</h1><p class=\"detail\">{detail}</p>{deadline_line}<form method=\"post\"><button name=\"verdict\" value=\"approve\" class=\"approve\">{approve_label}</button><button name=\"verdict\" value=\"skip\" class=\"skip\">{skip_label}</button></form></main></body></html>",
        title = escape_html(&title),
        kind = escape_html(&kind),
        detail = escape_html(&detail),
    )
}

fn decided_panel(
    status: &str,
    locale: crowdrelay_application::autopilot::BriefingLocale,
) -> String {
    use crowdrelay_application::autopilot::BriefingLocale;
    let heading = match locale {
        BriefingLocale::Pl => "Już zdecydowane",
        BriefingLocale::En => "Already decided",
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{heading}</title><style>{CSS}</style></head><body><main><h1>{heading}</h1><p class=\"detail\">already decided: {}</p></main></body></html>",
        escape_html(status)
    )
}

fn answered_panel(
    verdict: &str,
    status: &str,
    locale: crowdrelay_application::autopilot::BriefingLocale,
) -> String {
    use crowdrelay_application::autopilot::BriefingLocale;
    let heading = match (locale, verdict) {
        (BriefingLocale::Pl, "approve") => "Zatwierdzone",
        (BriefingLocale::Pl, _) => "Pominięte",
        (BriefingLocale::En, "approve") => "Approved",
        (BriefingLocale::En, _) => "Skipped",
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{heading}</title><style>{CSS}</style></head><body><main><h1>{heading}</h1><p class=\"detail\">{}</p></main></body></html>",
        escape_html(status)
    )
}

const CSS: &str = "body{font-family:system-ui,sans-serif;background:#fafafa;color:#222;margin:0}main{max-width:34rem;margin:3rem auto;padding:0 1.25rem}.kind{text-transform:uppercase;letter-spacing:.06em;color:#777;font-size:.8rem}h1{font-size:1.4rem;line-height:1.3}.detail{white-space:pre-wrap;color:#444}.due{color:#666;font-size:.9rem}form{display:flex;gap:.75rem;margin-top:1.5rem}button{font-size:1rem;padding:.7rem 1.6rem;border-radius:.4rem;border:1px solid #bbb;cursor:pointer}.approve{background:#1a7f37;border-color:#1a7f37;color:#fff}.skip{background:#fff;color:#333}";

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(status: StatusCode, body: &str) -> Response {
    (status, Html(body.to_owned())).into_response()
}
