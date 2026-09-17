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
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::gig_plan::{GigPlan, TenantIntent, plan_gig};
use crowdrelay_domain::roster_plan::{RosterRefusal, RosterRun, plan_roster_run};
use crowdrelay_infra::gig_planning::{city_opportunities, roster_opportunity};
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
    /// What the tenant says they are working on. Absent means `Unstated`,
    /// which proposes on evidence and says the timing is unverified — never
    /// inferred, because guessing a band is heads-down and silently
    /// withholding gigs is worse than asking.
    intent: Option<String>,
}

fn parse_intent(raw: Option<&str>) -> TenantIntent {
    match raw {
        Some("booking_shows") => TenantIntent::BookingShows,
        Some("working_a_release") => TenantIntent::WorkingARelease,
        Some("heads_down") => TenantIntent::HeadsDown,
        // An unrecognised value reads as unstated rather than as an error. A
        // typo in a query string must not withhold a proposal, and must not
        // silently pick a plan the tenant did not state either.
        _ => TenantIntent::Unstated,
    }
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
}

/// `GET /v1/control-plane/gig-plan` — what this band should book next.
pub async fn band_gig_plan(
    State(state): State<crate::AppState>,
    Query(params): Query<PlanParams>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let intent = parse_intent(params.intent.as_deref());
    let now = OffsetDateTime::now_utc();

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
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RosterPlanParams {
    organization_id: Uuid,
    /// How many packages the roster can actually run this period. Required:
    /// there is no defensible default, and a planner that invents one produces
    /// a plan nobody agreed to staff.
    packages_this_period: u16,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RosterPlanResponse {
    Planned(RosterRun),
    /// A refusal is a real answer and is returned as one, with the sentence.
    Refused {
        refused: String,
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
    let opportunity = match roster_opportunity(
        &state.database,
        params.organization_id,
        params.packages_this_period,
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

    #[test]
    fn an_unknown_intent_is_unstated_rather_than_a_guess() {
        assert_eq!(parse_intent(None), TenantIntent::Unstated);
        assert_eq!(parse_intent(Some("nonsense")), TenantIntent::Unstated);
        assert_eq!(parse_intent(Some("")), TenantIntent::Unstated);
    }

    /// Every variant round-trips, or a band setting "heads down" in the console
    /// silently keeps receiving gig proposals.
    #[test]
    fn every_intent_the_console_can_send_is_understood() {
        assert_eq!(
            parse_intent(Some("booking_shows")),
            TenantIntent::BookingShows
        );
        assert_eq!(
            parse_intent(Some("working_a_release")),
            TenantIntent::WorkingARelease
        );
        assert_eq!(parse_intent(Some("heads_down")), TenantIntent::HeadsDown);
    }

    #[test]
    fn a_refusal_is_a_sentence_a_band_can_read() {
        let sentence = GigRefusal::NoRoomOnRecord.message();
        assert!(sentence.len() > 40);
        assert!(!sentence.contains("Err("));
    }
}
