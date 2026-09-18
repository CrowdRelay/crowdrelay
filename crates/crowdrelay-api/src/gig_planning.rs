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
use crowdrelay_domain::gig_plan::{GigPlan, GigRefusal, TenantIntent, plan_gig};
use crowdrelay_domain::roster_plan::{RosterRefusal, RosterRun, plan_roster_run};
use crowdrelay_domain::venue_seed::{self, ResearchSubject};
use crowdrelay_infra::band_listing::PostgresBandListingRepository;
use crowdrelay_infra::gig_outreach::{
    GigOutreachError, GigOutreachOutcome, SEND_CHANNEL_MISSING, SupportSlotAskOutcome,
    approve_gig_proposal as approve_proposal, approve_support_slot_ask as approve_ask,
    gig_outreach_is_sendable,
};
use crowdrelay_infra::gig_planning::{
    city_opportunities, proposal_track_record, roster_opportunity, stated_intent,
};
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
    /// The catalogue row's id — a slug is only unique per country, so the id
    /// is the identity.
    city_id: Uuid,
    /// The catalogue slug.
    city: String,
    /// The name a person reads — "Wrocław", not "wroclaw".
    city_name: String,
    reason: String,
    /// When the refusal is something research can fix, the question to go and
    /// answer — a prompt the operator pastes into whatever AI they already
    /// use, whose answer lands back in the venues sheet (4G.6). Absent for
    /// refusals research cannot change: "you said you are recording" is not a
    /// missing fact.
    research_brief: Option<String>,
}

/// The capacity band a room search should stay inside for this city.
///
/// A room bigger than the consented audience is a proposal built on strangers
/// showing up; one under fifty is a pub night, not a gig. The band is the
/// honest ceiling — nobody can sell more tickets than people who asked to
/// hear about them. Rounded to fifties because the sheet's capacities are
/// estimates and false precision reads as measured fact.
fn capacity_band(reachable: u32) -> Option<(u32, u32)> {
    if reachable == 0 {
        return None;
    }
    let low = (reachable / 4).max(50) / 50 * 50;
    let high = reachable.div_ceil(50) * 50;
    Some((low, high.max(low)))
}

/// A proposal as the console reads it: the domain's plan plus the city's
/// display name, flattened so the payload is the plan itself. `city` stays
/// the catalogue slug — it is the key an approval names — while `city_name`
/// is what the band reads.
#[derive(Debug, Serialize)]
struct ProposalView {
    city_name: String,
    #[serde(flatten)]
    plan: GigPlan,
}

/// The same for a track-record entry.
#[derive(Debug, Serialize)]
struct OutcomeView {
    city_name: String,
    #[serde(flatten)]
    outcome: crowdrelay_infra::gig_planning::ProposalOutcome,
}

#[derive(Debug, Serialize)]
struct TrackRecordView {
    proposals: Vec<OutcomeView>,
    by_reason: Vec<crowdrelay_infra::gig_planning::ReasonScore>,
}

/// Every city the response mentions, resolved to the catalogue's display
/// names in one read — keyed by id because a slug is only unique per country
/// and `WHERE slug = ANY(...)` could hand back the wrong country's name. A
/// city with no catalogue row displays as its slug — ugly, but honest, and
/// it cannot happen for a city the evidence read produced.
async fn city_names(
    pool: &sqlx::PgPool,
    city_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, String>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM cities WHERE id = ANY($1)")
        .bind(city_ids)
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().collect())
}

fn display_name<'a>(
    names: &'a std::collections::HashMap<Uuid, String>,
    city_id: Uuid,
    slug: &'a str,
) -> &'a str {
    names.get(&city_id).map_or(slug, String::as_str)
}

#[derive(Debug, Serialize)]
struct BandPlanResponse {
    /// Ranked, strongest first, capped at `MAX_PROPOSALS`.
    proposals: Vec<ProposalView>,
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
    /// What approved proposals actually produced, and which reasons were on
    /// the ones that worked (4G.5). Read beside the new proposals: a reason
    /// that has produced a show before deserves to be read differently from
    /// one that has never been tested.
    track_record: TrackRecordView,
    /// What this act says it sounds like, or absent when it has never said
    /// (§4h-8 / 5.21).
    ///
    /// Returned beside the proposals because it is the input the roster's
    /// package matcher will judge a shared bill on, and an act that has not
    /// declared one cannot be paired at all — the console can ask for it at
    /// the moment the band is reading about rooms rather than in a settings
    /// screen nobody opens.
    #[serde(skip_serializing_if = "Option::is_none")]
    act_style: Option<String>,
    /// Whether approving a proposal would actually send anything.
    ///
    /// False means no connected sender advertises gig outreach, so the
    /// approval is refused rather than queued — the console must say so on the
    /// proposal instead of offering a button its own backend will decline.
    can_send: bool,
    /// The sentence to show when `can_send` is false. Absent when it is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    send_blocked_reason: Option<&'static str>,
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

    // Bounded: the evidence read is one query per city, up to forty cities,
    // and it holds a connection for as long as it runs. Unbounded, one slow
    // read is a connection the ticketing path cannot have.
    let opportunities = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            city_opportunities(&state.database, workspace_id, now),
        ),
    )
    .await
    {
        Ok(Ok(opportunities)) => opportunities,
        Ok(Err(error)) => {
            tracing::warn!(%error, "gig plan evidence read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("gig plan evidence read timed out");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    let track_record = match proposal_track_record(&state.database, workspace_id).await {
        Ok(record) => record,
        Err(error) => {
            // The same rule as the evidence read above: the track record is
            // part of the answer now — a console that renders proposals beside
            // a silently-absent history would let the band believe nothing has
            // ever been tried.
            tracing::warn!(%error, "gig plan track record read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    // The band's own word for what it plays — the research brief names the
    // genre so the sheet that comes back is about rooms that book this band,
    // not rooms in general. A band that never stated one gets a brief that
    // leaves the genre to the operator rather than inventing it.
    // A read that failed is not a band without a genre. Swallowing the error
    // produced a research brief that quietly asked for "rooms that book acts
    // in the band's genre" — weaker, with nothing anywhere saying why — so the
    // failure fails the request like every other evidence read here.
    let listing = match PostgresBandListingRepository::new(state.database.clone())
        .load(workspace_id)
        .await
    {
        Ok(listing) => listing,
        Err(error) => {
            tracing::warn!(%error, "band listing read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let genre = listing
        .map(|listing| listing.genre_tags.join(" / "))
        .filter(|joined| !joined.is_empty());

    let act_style = match settings.act_style(workspace_id).await {
        Ok(style) => style,
        Err(error) => {
            tracing::warn!(%error, "act style read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    // Asked here so the console can grey the approve button rather than
    // discovering the refusal after the band has read three proposals and
    // decided. The approval asks again — this is the warning, not the gate.
    let can_send = match gig_outreach_is_sendable(&state.database, workspace_id).await {
        Ok(sendable) => sendable,
        Err(error) => {
            tracing::warn!(%error, "gig outreach sendability read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };

    let considered = opportunities.len();
    let mut proposals = Vec::new();
    let mut passed_over = Vec::new();
    // What each refusal would need researched, held until the catalogue names
    // are known. The brief is text a person pastes into an AI, and "venues in
    // wroclaw" is a worse question than "venues in Wrocław" — the slug is our
    // key, not a place anybody writes.
    let mut pending_briefs: Vec<(usize, ResearchSubject, u32)> = Vec::new();
    for opportunity in &opportunities {
        match plan_gig(opportunity, intent) {
            Ok(plan) => proposals.push(plan),
            Err(refusal) => {
                let subject = match &refusal {
                    GigRefusal::NoRoomOnRecord => Some(ResearchSubject::Rooms),
                    GigRefusal::NoContactableRoute { venue } => {
                        Some(ResearchSubject::BookingContact {
                            venue: venue.clone(),
                        })
                    }
                    _ => None,
                };
                if let Some(subject) = subject {
                    pending_briefs.push((
                        passed_over.len(),
                        subject,
                        opportunity.reachable_fans.unwrap_or(0),
                    ));
                }
                passed_over.push(PassedOver {
                    city_id: opportunity.city_id.into_uuid(),
                    city: opportunity.city.clone(),
                    // Filled after the loop, when every slug the response
                    // mentions is known and one lookup resolves them all.
                    city_name: String::new(),
                    reason: refusal.message(),
                    research_brief: None,
                });
            }
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

    // One lookup resolves every city the response mentions to its display
    // name — keyed by id since a slug can name a city in each of two
    // countries. The slug stays on the payload for reading; the id is the key
    // an approval names.
    let city_ids: Vec<Uuid> = proposals
        .iter()
        .map(|proposal| proposal.city_id.into_uuid())
        .chain(passed_over.iter().map(|entry| entry.city_id))
        .chain(track_record.proposals.iter().map(|outcome| outcome.city_id))
        .collect();
    let names = match city_names(&state.database, &city_ids).await {
        Ok(names) => names,
        Err(error) => {
            tracing::warn!(%error, "gig plan city-name read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
    };
    for entry in &mut passed_over {
        entry.city_name = display_name(&names, entry.city_id, &entry.city).to_owned();
    }
    for (index, subject, reachable) in pending_briefs {
        let Some(entry) = passed_over.get_mut(index) else {
            continue;
        };
        entry.research_brief = Some(venue_seed::research_brief(
            &subject,
            // The name a person would type, not the catalogue key.
            &entry.city_name,
            genre.as_deref(),
            // The band sizing a room to its draw only matters when the ask is
            // "find rooms"; finding the booking contact for a named room does
            // not re-ask the capacity question.
            match subject {
                ResearchSubject::Rooms => capacity_band(reachable),
                ResearchSubject::BookingContact { .. } => None,
            },
        ));
    }

    (
        StatusCode::OK,
        Json(BandPlanResponse {
            proposals: proposals
                .into_iter()
                .map(|plan| ProposalView {
                    city_name: display_name(&names, plan.city_id.into_uuid(), &plan.city)
                        .to_owned(),
                    plan,
                })
                .collect(),
            passed_over,
            cities_considered: considered,
            intent: intent.as_str(),
            intent_is_stored: intent == stored,
            act_style,
            can_send,
            send_blocked_reason: (!can_send).then_some(SEND_CHANNEL_MISSING),
            track_record: TrackRecordView {
                proposals: track_record
                    .proposals
                    .into_iter()
                    .map(|outcome| OutcomeView {
                        city_name: display_name(&names, outcome.city_id, &outcome.city).to_owned(),
                        outcome,
                    })
                    .collect(),
                by_reason: track_record.by_reason,
            },
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveProposalRequest {
    /// The catalogue id of the city whose proposal the band is approving —
    /// the same `city_id` the proposal was served with. A slug would do for
    /// most cities, but the catalogue is only unique per country and an
    /// approval is exactly where a wrong-city resolution cannot be afforded.
    /// The proposal itself is recomputed rather than replayed from the
    /// screen: evidence that moved between the read and the click wins, and
    /// the band is told what moved.
    city_id: Uuid,
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
        request.city_id,
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

/// `POST /v1/admin/roster-plan/support-slot-ask` — the roster operator's yes:
/// the labelmate is named, and the headliner's own promoter gets the letter.
///
/// Admin rather than control-plane for the same reason the plan read is: the
/// ask names two workspaces, and both must be the organisation's. What the
/// approval actually recomputes is on the other side — the slot, the pairing
/// arithmetic, the sender, the names — so a screen that went stale refuses
/// rather than writing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportSlotAskRequest {
    /// The organisation both acts belong to. The route is admin-scoped
    /// because a roster spans workspaces; this id is what the membership
    /// check is made against, not a hint.
    organization_id: Uuid,
    /// The catalogue id of the city the show is in — the same `city_id` the
    /// slot was read with. Ids, not slugs, everywhere an approval names
    /// something: the catalogue is only unique per country.
    city_id: Uuid,
    /// The act whose published show holds the open slot — the workspace the
    /// letter leaves from.
    headliner_workspace_id: Uuid,
    /// The labelmate being put forward for the slot.
    support_workspace_id: Uuid,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SupportSlotAskResponse {
    Queued {
        action_id: Uuid,
        /// Who holds the room and the night.
        headliner: String,
        /// Who is being put forward for the slot.
        support: String,
        city: String,
        venue: String,
        /// The night as the letter states it.
        show_date: String,
        /// Everybody who will receive it — the headliner's own promoter
        /// contacts for that city, all of them or none.
        recipients: Vec<String>,
        /// The first line of the letter. Returned so the operator reads what
        /// the promoter will read.
        opening_line: String,
    },
    /// The same key already produced this ask. The stored status travels
    /// because it may already have run.
    Replayed { action_id: Uuid, status: String },
    /// The ask no longer holds — the slot is gone, the support fills nothing,
    /// the pairing splits one crowd, or a letter for this show is already in
    /// flight. A real answer, not an error: the sentence says what changed.
    Refused { refused: String },
}

/// The write half of the cheapest move the roster planner makes. One open
/// slot produces one letter; approving once is approving, and every gate is
/// re-run at dispatch.
pub async fn approve_support_slot_ask(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<SupportSlotAskRequest>, JsonRejection>,
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
        // Required rather than generated: a retried click must not become a
        // second letter to the same promoters, and only the caller knows
        // which click this is.
        return Problem::bad_request(request_id_value).into_response();
    };

    match approve_ask(
        &state.database,
        request.organization_id,
        request.headliner_workspace_id,
        request.support_workspace_id,
        request.city_id,
        &idempotency_key,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(SupportSlotAskOutcome::Queued {
            action_id,
            headliner,
            support,
            city,
            venue,
            show_date,
            recipients,
            opening_line,
        }) => (
            StatusCode::OK,
            Json(SupportSlotAskResponse::Queued {
                action_id,
                headliner,
                support,
                city,
                venue,
                show_date,
                recipients,
                opening_line,
            }),
        )
            .into_response(),
        Ok(SupportSlotAskOutcome::Replayed { action_id, status }) => (
            StatusCode::OK,
            Json(SupportSlotAskResponse::Replayed { action_id, status }),
        )
            .into_response(),
        Err(GigOutreachError::Refused(sentence)) => (
            StatusCode::OK,
            Json(SupportSlotAskResponse::Refused { refused: sentence }),
        )
            .into_response(),
        Err(GigOutreachError::NotFound) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(GigOutreachError::Database(error)) => {
            tracing::warn!(%error, "support-slot ask approval failed");
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

    // The widest read on this surface: one funnel per act, one query per city
    // inside each. Sixty acts of forty cities is thousands of round trips, and
    // without a bound one roster's plan holds a connection for as long as that
    // takes. The timeout says so out loud instead of degrading the whole API.
    let opportunity = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            roster_opportunity(
                &state.database,
                params.organization_id,
                packages_this_period,
                now,
            ),
        ),
    )
    .await
    {
        Ok(Ok(opportunity)) => opportunity,
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster plan evidence read failed");
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("roster plan evidence read timed out");
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
