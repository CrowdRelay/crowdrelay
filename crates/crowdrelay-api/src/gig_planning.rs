//! The gig proposal, as a console read.
//!
//! Two routes, one for a band and one for a roster, both producing the same
//! shape of answer: what to do, or why not, and the reasons either way.
//!
//! # A refusal is part of the answer, not an error
//!
//! Both routes return 200 with refusals in the body. A city that did not clear
//! the bar is information — "nobody has played a room here" tells the band what
//! to go and find — and returning it as a 4xx would make the console treat the
//! most useful half of the output as a failure to render.
//!
//! The only genuine error here is not being able to read the evidence.

use axum::{
    Json,
    extract::{Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::gig_plan::{GigPlan, TenantIntent, plan_gig};
use crowdrelay_domain::roster_plan::{RosterRefusal, RosterRun, plan_roster_run};
use crowdrelay_infra::gig_outreach::{
    GigOutreachError, GigOutreachOutcome, approve_gig_proposal as approve_proposal,
};
use crowdrelay_infra::gig_planning::{city_opportunities, roster_opportunity, stated_intent};
use crowdrelay_infra::organization_settings::{
    KEY_ROSTER_PACKAGES_PER_PERIOD, OrganizationSettingsRepository, PACKAGES_PER_PERIOD_RANGE,
};
use crowdrelay_infra::tenant_settings::TenantSettingsRepository;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

/// How many proposals a band is shown at once.
///
/// Three is a choice somebody can make in a minute. Ten is a list, and the
/// operator's own word for a list of things they have not decided about was
/// "flooded" — the planner ranks, so the cut is safe.
const MAX_PROPOSALS: usize = 3;

#[derive(Debug, Deserialize)]
pub struct PlanParams {
    /// A one-off override of the stored intent, for asking "what would this
    /// look like if we were booking?". Absent — the normal case — uses what the
    /// band stored in the console.
    intent: Option<String>,
}

/// Which intent this request plans against.
///
/// Precedence, and each step is deliberate:
///
/// 1. A **recognised** query parameter wins. It is somebody typing an intent at
///    the moment they ask, which is as explicit as a statement gets.
/// 2. Otherwise the **stored** setting — what the band last said in the
///    console. This is the normal path, and it is why the setting exists: a
///    band that sets "heads down" must stop receiving proposals without having
///    to remember a query string.
/// 3. Otherwise `Unstated`, which proposes on evidence and says the timing is
///    unverified.
///
/// An **unrecognised** parameter falls through to the stored value rather than
/// resolving to `Unstated`. A typo in a URL must never quietly discard what the
/// band actually stated — that is how a heads-down band starts getting gig
/// proposals again and nobody can say why.
fn resolve_intent(param: Option<&str>, stored: TenantIntent) -> TenantIntent {
    param.and_then(TenantIntent::parse).unwrap_or(stored)
}

/// One city that produced no proposal, with the sentence explaining it.
#[derive(Debug, Serialize)]
struct PassedOver {
    city: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct BandPlanResponse {
    /// Ranked, strongest first, capped at `MAX_PROPOSALS`.
    proposals: Vec<GigPlan>,
    /// Every city considered and not proposed. The band's own judgement often
    /// beats the bar, and they cannot apply it to a city they were never told
    /// about.
    passed_over: Vec<PassedOver>,
    /// How many cities were looked at. Without it, an empty answer is
    /// indistinguishable from a planner that never ran.
    cities_considered: usize,
    /// The intent this plan was made against, in its stored form. Returned
    /// because every proposal's `fits_intent` sentence depends on it: a band
    /// reading "you said the release is the focus" must be able to see that
    /// this is what the system has on file, and change it if it is wrong.
    intent: &'static str,
    /// Whether the intent came from the stored setting rather than from the
    /// query string. False for a one-off override, so the console never shows a
    /// preview as though it were the band's settled answer.
    intent_is_stored: bool,
}

/// `GET /v1/control-plane/gig-plan` — what this band should book next.
pub async fn band_gig_plan(
    State(state): State<crate::AppState>,
    Query(params): Query<PlanParams>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let now = OffsetDateTime::now_utc();

    let settings = TenantSettingsRepository::new(state.database.clone());
    let stored = match stated_intent(&settings, workspace_id).await {
        Ok(stored) => stored,
        Err(error) => {
            // The stored intent is not a detail of the answer, it is what the
            // band asked the planner to respect. Planning without it could
            // hand gig proposals to a band that said it is recording, so the
            // read failing fails the request.
            tracing::warn!(%error, "stated intent read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let intent = resolve_intent(params.intent.as_deref(), stored);

    let opportunities = match city_opportunities(&state.database, workspace_id, now).await {
        Ok(opportunities) => opportunities,
        Err(error) => {
            tracing::warn!(%error, "gig plan evidence read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    let considered = opportunities.len();
    let mut proposals = Vec::new();
    let mut passed_over = Vec::new();
    for opportunity in &opportunities {
        match plan_gig(opportunity, intent) {
            Ok(plan) => proposals.push(plan),
            Err(refusal) => passed_over.push(PassedOver {
                city: opportunity.city.clone(),
                reason: refusal.message(),
            }),
        }
    }

    // Ranked by the audience a proposal actually reaches, which is the number
    // the band is optimising. The domain decides *whether* a city qualifies;
    // ordering the qualifiers is a presentation choice and lives here.
    proposals.sort_by(|left, right| {
        right
            .reach
            .reachable
            .saturating_add(right.reach.added_by_co_bill)
            .cmp(
                &left
                    .reach
                    .reachable
                    .saturating_add(left.reach.added_by_co_bill),
            )
            // Stable by city so the same evidence always produces the same
            // order — a band re-reading yesterday's list and seeing a
            // different one stops trusting both.
            .then_with(|| left.city.cmp(&right.city))
    });
    proposals.truncate(MAX_PROPOSALS);

    (
        StatusCode::OK,
        Json(BandPlanResponse {
            proposals,
            passed_over,
            cities_considered: considered,
            intent: intent.as_str(),
            intent_is_stored: intent == stored,
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveProposalRequest {
    /// The city whose proposal the band is approving. The proposal itself is
    /// recomputed rather than replayed from the screen: evidence that moved
    /// between the read and the click wins, and the band is told what moved.
    city: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApproveProposalResponse {
    Queued {
        action_id: Uuid,
        city: String,
        venue: String,
        /// Everybody who will receive it. All of them, or none — the contact
        /// governor reserves the set inside one transaction.
        recipients: Vec<String>,
        /// The first line of the letter, which is the proposal's strongest
        /// reason. Returned so the band reads what the promoter will read.
        opening_line: String,
    },
    /// The same key already produced this outreach. The stored status travels
    /// because it may already have run.
    Replayed { action_id: Uuid, status: String },
    /// The proposal no longer holds. A real answer, not an error: the sentence
    /// says what changed and what to do about it.
    Refused { refused: String },
}

/// `POST /v1/control-plane/gig-plan/approve` — yes, write to these people.
///
/// One action for the whole room (§12-6, 4G.4). Approving once is approving:
/// the action is queued rather than parked for a second approval on another
/// screen, because the band has just read the reasons, the caveats and the
/// names. Every gate still runs again at dispatch.
pub async fn approve_gig_proposal(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ApproveProposalRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(idempotency_key) = headers
        .get(&crate::IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        // Required rather than generated here: a retried click must not become
        // a second letter to the same promoters, and only the caller knows
        // which click this is.
        return Problem::bad_request(request_id_value).into_response();
    };

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match approve_proposal(
        &state.database,
        workspace_id,
        request.city.trim(),
        &idempotency_key,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(GigOutreachOutcome::Queued {
            action_id,
            city,
            venue,
            recipients,
            opening_line,
        }) => (
            StatusCode::OK,
            Json(ApproveProposalResponse::Queued {
                action_id,
                city,
                venue,
                recipients,
                opening_line,
            }),
        )
            .into_response(),
        Ok(GigOutreachOutcome::Replayed { action_id, status }) => (
            StatusCode::OK,
            Json(ApproveProposalResponse::Replayed { action_id, status }),
        )
            .into_response(),
        Err(GigOutreachError::Refused(sentence)) => (
            StatusCode::OK,
            Json(ApproveProposalResponse::Refused { refused: sentence }),
        )
            .into_response(),
        Err(GigOutreachError::NotFound) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(GigOutreachError::Database(error)) => {
            tracing::warn!(%error, "gig proposal approval failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RosterPlanParams {
    organization_id: Uuid,
    /// A one-off override of the stored capacity, for asking what a bigger or
    /// smaller period would look like. Absent — the normal case — uses the
    /// number the manager set (4G.2b).
    packages_this_period: Option<u16>,
}

/// How many packages this plan is sized for, and where that number came from.
///
/// Neither source may be invented. A planner that picks a default produces a
/// plan nobody agreed to staff, and the manager discovers that when the third
/// package needs people who are already busy.
fn resolve_packages(param: Option<u16>, stored: Option<u16>) -> Option<u16> {
    param.or(stored)
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RosterPlanResponse {
    Planned(RosterRun),
    /// A refusal is a real answer and is returned as one, with the sentence.
    Refused {
        refused: String,
    },
    /// Not a refusal: a question. The roster has never said how many packages
    /// it can run, and no plan can be sized until it does. Separated from
    /// `Refused` because the two need different consoles — a refusal is read
    /// and accepted, this one is answered in a single field and the plan
    /// appears.
    NeedsSetting {
        needs_setting: &'static str,
        message: String,
    },
}

/// `GET /v1/admin/roster-plan` — what this roster should do next.
///
/// Admin rather than control-plane: an organisation spans workspaces, and the
/// control-plane surface is scoped to one tenant by construction. A roster read
/// that accepted a workspace token would be one tenant reading its labelmates'
/// audiences.
pub async fn roster_gig_plan(
    State(state): State<crate::AppState>,
    Query(params): Query<RosterPlanParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();

    let organization_settings = OrganizationSettingsRepository::new(state.database.clone());
    let stored = match organization_settings
        .packages_this_period(params.organization_id)
        .await
    {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(%error, "roster capacity read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let Some(packages_this_period) = resolve_packages(params.packages_this_period, stored) else {
        return (
            StatusCode::OK,
            Json(RosterPlanResponse::NeedsSetting {
                needs_setting: KEY_ROSTER_PACKAGES_PER_PERIOD,
                message: format!(
                    "Nobody has said how many packages this roster can run in a period, so \
                     there is nothing to size a plan against. Set it between {} and {} and \
                     the plan appears — it is the one number the planner will not invent, \
                     because a plan nobody agreed to staff costs more than no plan.",
                    PACKAGES_PER_PERIOD_RANGE.start(),
                    PACKAGES_PER_PERIOD_RANGE.end()
                ),
            }),
        )
            .into_response();
    };

    let opportunity = match roster_opportunity(
        &state.database,
        params.organization_id,
        packages_this_period,
        now,
    )
    .await
    {
        Ok(opportunity) => opportunity,
        Err(error) => {
            tracing::warn!(%error, "roster plan evidence read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    let body = match plan_roster_run(&opportunity) {
        Ok(run) => RosterPlanResponse::Planned(run),
        Err(refusal) => RosterPlanResponse::Refused {
            refused: RosterRefusal::message(&refusal),
        },
    };
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::gig_plan::GigRefusal;

    /// The normal path: no parameter, so the plan runs against what the band
    /// stored. Without this, the setting exists and nothing reads it.
    #[test]
    fn the_stored_intent_is_what_a_plain_request_plans_against() {
        assert_eq!(
            resolve_intent(None, TenantIntent::HeadsDown),
            TenantIntent::HeadsDown
        );
        assert_eq!(
            resolve_intent(None, TenantIntent::Unstated),
            TenantIntent::Unstated
        );
    }

    /// An explicit, recognised parameter is somebody stating an intent at the
    /// moment they ask, and it wins.
    #[test]
    fn a_recognised_parameter_overrides_the_stored_intent() {
        assert_eq!(
            resolve_intent(Some("booking_shows"), TenantIntent::Unstated),
            TenantIntent::BookingShows
        );
        assert_eq!(
            resolve_intent(Some("working_a_release"), TenantIntent::BookingShows),
            TenantIntent::WorkingARelease
        );
        assert_eq!(
            resolve_intent(Some("heads_down"), TenantIntent::BookingShows),
            TenantIntent::HeadsDown
        );
    }

    /// The bug this ordering exists to prevent: a typo in a URL discarding what
    /// the band stated, so a band that said it is recording gets proposals
    /// again and nobody can say why.
    #[test]
    fn a_typo_keeps_the_stored_intent_rather_than_discarding_it() {
        assert_eq!(
            resolve_intent(Some("headsdown"), TenantIntent::HeadsDown),
            TenantIntent::HeadsDown
        );
        assert_eq!(
            resolve_intent(Some(""), TenantIntent::WorkingARelease),
            TenantIntent::WorkingARelease
        );
        assert_eq!(
            resolve_intent(Some("Booking_Shows"), TenantIntent::HeadsDown),
            TenantIntent::HeadsDown
        );
    }

    /// The console must be able to tell a settled answer from a preview.
    #[test]
    fn an_override_is_distinguishable_from_the_stored_answer() {
        let stored = TenantIntent::HeadsDown;
        let overridden = resolve_intent(Some("booking_shows"), stored);
        assert_ne!(overridden, stored);
        assert_eq!(resolve_intent(None, stored), stored);
    }

    /// §4G.2b: the roster's capacity is stated, never invented. Both sources
    /// are a person saying a number; absent stays absent all the way to the
    /// answer, which asks for it instead of sizing a plan nobody agreed to.
    #[test]
    fn a_roster_capacity_comes_from_a_person_or_from_nowhere() {
        assert_eq!(resolve_packages(None, Some(3)), Some(3));
        assert_eq!(resolve_packages(Some(5), Some(3)), Some(5));
        assert_eq!(resolve_packages(Some(5), None), Some(5));
        assert_eq!(resolve_packages(None, None), None);
    }

    #[test]
    fn a_refusal_is_a_sentence_a_band_can_read() {
        let sentence = GigRefusal::NoRoomOnRecord.message();
        assert!(sentence.len() > 40);
        assert!(!sentence.contains("Err("));
    }
}
