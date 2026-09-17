//! "Organise a gig at X with Y, and write to Z — here is why."
//!
//! # What this is for
//!
//! Bands, rosters and labels live from shows. Everything else this product
//! does — content, fans, reach — either feeds a show or feeds off one. So the
//! single most valuable sentence the system can produce is a specific,
//! defensible proposal for the next one: a room, a date window, a bill, the
//! people to write to, and the reasons.
//!
//! The reasons are the feature. A ranked list of rooms is a spreadsheet
//! anybody could build; "this room, because three acts from your genre played
//! it this year, it holds 300, you can reach 240 consented people within
//! 50 km, and the booker answered mail 18 days ago" is a decision somebody can
//! act on or argue with. Both are useful. A score is neither.
//!
//! # Reasons are derived from the decision, never written beside it
//!
//! Every [`Reason`] here is produced by the same check that contributed to the
//! proposal being made at all. Nothing composes an explanation separately from
//! the evaluation, because two code paths that both describe one decision drift
//! — and the first time they disagree, the explanation is the one a person
//! believed.
//!
//! # It has to be allowed to say no
//!
//! The operator's complaint about the rest of the system was being "flooded
//! with tasks I don't really fully understand". A proposal engine that always
//! proposes is that failure in its most expensive form: a band spends a week
//! chasing a room that was never going to work, and stops trusting the next
//! suggestion, which might have been the good one.
//!
//! So this refuses more often than it proposes. It refuses when there is no
//! room on record, when nobody is contactable, when there are too few people
//! within reach to fill anything, when a show is already booked there, and
//! when the tenant's own stated plan says this is not what they are working on
//! right now. Each refusal names what would change it.
//!
//! # What it deliberately does not do
//!
//! It does not price a ticket, book anything, or contact anybody. It produces
//! a proposal a person approves, and every downstream action stays gated the
//! way it already is. It also never invents a fact: every number here arrives
//! measured, and a fact that is not on record is absent rather than assumed.

use serde::{Deserialize, Serialize};

/// Below this many reachable people, a show in that city is a favour somebody
/// is doing you rather than a gig.
///
/// Reachable means consented and inside the radius the fan themselves chose —
/// people the band could actually tell. Fifty is not a capacity target; it is
/// the point below which the proposal stops being about the audience and
/// starts being about hope. A band can still play a city below it. The system
/// just will not claim the data supports it.
pub const MINIMUM_REACHABLE_FOR_A_GIG: u32 = 50;

/// A room with no event in this long reads as dormant rather than quiet.
///
/// Nine months covers a seasonal programme and a summer break. Past it, the
/// honest position is that we do not know the room is still running shows, and
/// proposing it would spend the band's week finding out.
pub const DORMANT_ROOM_DAYS: u16 = 270;

/// A booking contact older than this is a lead, not a route.
///
/// People leave venues constantly. Four months is roughly how long a booker's
/// address stays good in a scene this size, and past it the proposal says
/// "verify this first" instead of pretending the route is live.
pub const CONTACT_STALE_DAYS: u16 = 120;

/// What the tenant said they are working on.
///
/// This is the half the system could never see. A band mid-album does not want
/// a gig proposal; a band with a release in six weeks wants nothing else. Left
/// unset, the planner proposes on evidence alone — which is correct, and worse,
/// because "correct and badly timed" is how a suggestion engine becomes noise.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantIntent {
    /// No stated plan. Propose on evidence; say that the timing is unverified.
    #[default]
    Unstated,
    /// Actively looking for shows. Propose freely.
    BookingShows,
    /// A release is the focus. A show still serves it — a launch night is a
    /// show — but the proposal has to say so rather than competing with it.
    WorkingARelease,
    /// Writing or recording. Shows are a distraction the tenant asked not to
    /// have, and the planner respects that even when the evidence is good.
    HeadsDown,
}

/// A room, as the registry and the researched sheet know it.
///
/// Every field is `Option` where the fact may genuinely be unknown, because
/// this whole module's value depends on the difference between "zero" and "we
/// have not measured that". A capacity of `None` is a room we cannot size; a
/// capacity of `Some(0)` would be a room that holds nobody.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VenueEvidence {
    pub name: String,
    /// Shows any tenant has marked at this room in the last twelve months.
    pub shows_last_12_months: u16,
    /// Of those, how many were by acts whose genre overlaps this tenant's.
    /// The strongest single fact in the proposal, and the one a promoter
    /// recognises fastest.
    pub comparable_acts: u16,
    pub capacity: Option<u32>,
    /// Mean paid orders per ticketed show at this room. `None` when no marked
    /// show there sold tickets through us — unmeasured, not empty.
    pub typical_draw: Option<u32>,
    /// How long ago somebody confirmed the booking address still works.
    /// `None` means nobody ever has.
    pub contact_verified_days_ago: Option<u16>,
    /// Days since the room's most recent event on record. `None` means we have
    /// never seen one, which is different from a long gap.
    pub days_since_last_event: Option<u16>,
    /// Whether a contactable booking route exists at all.
    pub has_booking_route: bool,
}

/// Somebody who books in this city, and how well they know the tenant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromoterRef {
    pub name: String,
    /// 0..=100. Derived from recorded outcomes, not from a guess.
    pub relationship_score: u16,
    /// Whether the last approach to them got an answer of any kind.
    pub answered_last_time: bool,
    pub has_route: bool,
}

/// An act that could share the bill, and what it brings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoBillAct {
    pub name: String,
    /// Consented, reachable people this act has in this city. The number that
    /// decides whether a co-bill adds a room or splits one.
    pub reachable_here: u32,
    /// Share of the two audiences that is the same people, in basis points.
    /// High overlap is not a package — it is one show with two names on it.
    pub audience_overlap_basis_points: u16,
    /// Whether this act has already agreed to be proposed. An act that has not
    /// is a suggestion to *ask*, never a commitment to announce.
    pub consented_to_share_bills: bool,
}

/// What the planner knows about one city.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CityOpportunity {
    pub city: String,
    /// Consented fans inside the radius they chose.
    pub reachable_fans: u32,
    /// Fans with recorded activity in the last 30 days.
    pub active_fans_30d: u32,
    /// `None` means never played here — not "played here zero months ago".
    pub months_since_show: Option<u16>,
    /// A show already on the calendar here. A city with one booked is not a
    /// gap, whatever else it scores.
    pub has_upcoming_show: bool,
    pub venue: Option<VenueEvidence>,
    pub promoters: Vec<PromoterRef>,
    pub co_bill: Vec<CoBillAct>,
}

/// One fact that contributed to the proposal, in words a promoter would use.
///
/// Carried as structured data rather than a rendered sentence so the console
/// and the outreach draft can phrase it for their own audience without either
/// one inventing a fact the other did not have.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Reason {
    /// Acts from this genre already play this room.
    ComparableActsPlayedHere { count: u16, of_shows: u16 },
    /// There are people here who asked to hear from the band.
    ReachableAudience { reachable: u32 },
    /// The room's own draw, measured through our ticketing.
    RoomDraws { typical_draw: u32 },
    /// The band has an audience here and has not played it.
    NeverPlayedButHasFans { reachable: u32 },
    /// A long time since the last show, with fans still active.
    OverdueReturn { months: u16, active_30d: u32 },
    /// A co-bill that adds people rather than splitting them.
    CoBillAddsAudience { act: String, adds_reachable: u32 },
    /// Somebody here knows the band and answered last time.
    WarmPromoter { name: String },
    /// The room is running shows right now.
    RoomIsActive { days_since_last_event: u16 },
}

/// Why no proposal was made. Each one names what would change it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GigRefusal {
    /// A show is already booked here.
    AlreadyBooked,
    /// No room on record in this city.
    NoRoomOnRecord,
    /// A room, but no way to reach anybody who books it.
    NoContactableRoute { venue: String },
    /// The room has not hosted anything in a long time, or ever.
    RoomLooksDormant {
        venue: String,
        days_since_last_event: Option<u16>,
    },
    /// Too few people within reach for a show to be about the audience.
    TooFewReachable { reachable: u32, floor: u32 },
    /// The tenant said they are not booking right now.
    NotWhatTheTenantIsDoing { intent: TenantIntent },
}

impl GigRefusal {
    /// What to tell the operator, including what would change the answer.
    ///
    /// A refusal that only says no teaches nothing. Every branch here ends with
    /// the thing that would make this city worth proposing, because the point
    /// of saying no is to stop the band spending a week — not to stop them
    /// asking again.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::AlreadyBooked => {
                "there is already a show on the calendar here — this city is covered, not a gap"
                    .to_owned()
            }
            Self::NoRoomOnRecord => {
                "no room here has hosted a show we know about. Research one, or log a past \
                 show at a room you have played, and it becomes proposable"
                    .to_owned()
            }
            Self::NoContactableRoute { venue } => format!(
                "{venue} looks right and there is no way to reach anybody who books it. \
                 Find the booking address and this becomes a proposal rather than a hunch"
            ),
            Self::RoomLooksDormant {
                venue,
                days_since_last_event,
            } => match days_since_last_event {
                Some(days) => format!(
                    "{venue} has had nothing on for {days} days. It may have stopped \
                     programming; check it is still running before spending a week on it"
                ),
                None => format!(
                    "we have never seen an event at {venue}, so there is nothing to say it \
                     still books shows. One logged event there changes that"
                ),
            },
            Self::TooFewReachable { reachable, floor } => format!(
                "only {reachable} people here asked to hear from you, and below about \
                 {floor} a show is a favour somebody does you rather than a gig. Play it if \
                 you want to — the data does not support calling it a plan"
            ),
            Self::NotWhatTheTenantIsDoing { intent } => match intent {
                TenantIntent::HeadsDown => {
                    "you said you are writing or recording, so this is not a proposal — it \
                     is here if you want it. Change that on the plan and gigs come back"
                        .to_owned()
                }
                _ => "this does not fit what you said you are working on right now".to_owned(),
            },
        }
    }
}

/// How many people a show here could plausibly reach, and on what basis.
///
/// Deliberately a range with its own basis attached rather than a single
/// number. The inputs are a room's historical mean and a consented audience
/// size; presenting their product as a forecast would be a precision neither
/// has, and the first time it missed the band would stop believing the rest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReachEstimate {
    /// People who asked to hear from the band, within reach of this city.
    pub reachable: u32,
    /// Added by co-billed acts, counting only the part that is not the same
    /// people. Union minus intersection.
    pub added_by_co_bill: u32,
    /// What the room itself has historically drawn. `None` when unmeasured —
    /// and the whole estimate is weaker when this is absent, which the basis
    /// says out loud.
    pub room_typical_draw: Option<u32>,
    /// One sentence naming what these numbers are and are not.
    pub basis: String,
}

/// The proposal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GigPlan {
    pub city: String,
    pub venue: String,
    /// Who to write to, strongest relationship first.
    pub contact: Vec<String>,
    /// Acts worth asking onto the bill. Asking, never announcing.
    pub invite_to_bill: Vec<String>,
    /// Ordered strongest first. Never empty — a plan with no reasons is not
    /// produced at all.
    pub reasons: Vec<Reason>,
    pub reach: ReachEstimate,
    /// How this serves what the tenant said they are doing, or that the timing
    /// is unverified because they said nothing.
    pub fits_intent: String,
    /// Stated plainly so the band checks it before acting. The planner knows
    /// what it does not know.
    pub caveats: Vec<String>,
}

/// Decides whether to propose a gig in this city, and says why either way.
///
/// # Errors
///
/// Refuses on an existing booking, no room, no route, a dormant room, too few
/// reachable people, or a tenant who said they are not booking.
#[allow(clippy::too_many_lines)]
pub fn plan_gig(
    opportunity: &CityOpportunity,
    intent: TenantIntent,
) -> Result<GigPlan, GigRefusal> {
    // Order matters and is not arbitrary: cheapest and most certain refusals
    // first, so the reason a band is given is the most actionable true one
    // rather than whichever check happened to run.
    if opportunity.has_upcoming_show {
        return Err(GigRefusal::AlreadyBooked);
    }
    if intent == TenantIntent::HeadsDown {
        return Err(GigRefusal::NotWhatTheTenantIsDoing { intent });
    }
    if opportunity.reachable_fans < MINIMUM_REACHABLE_FOR_A_GIG {
        return Err(GigRefusal::TooFewReachable {
            reachable: opportunity.reachable_fans,
            floor: MINIMUM_REACHABLE_FOR_A_GIG,
        });
    }
    let Some(venue) = opportunity.venue.as_ref() else {
        return Err(GigRefusal::NoRoomOnRecord);
    };

    // A room nobody has seen host anything, or that has hosted nothing in a
    // long time. Both refuse; the message distinguishes them, because "we have
    // never seen one" and "it stopped" need different next steps.
    let dormant = venue
        .days_since_last_event
        .is_none_or(|days| days > DORMANT_ROOM_DAYS);
    if dormant {
        return Err(GigRefusal::RoomLooksDormant {
            venue: venue.name.clone(),
            days_since_last_event: venue.days_since_last_event,
        });
    }

    let contactable_promoters: Vec<&PromoterRef> = opportunity
        .promoters
        .iter()
        .filter(|promoter| promoter.has_route)
        .collect();
    if !venue.has_booking_route && contactable_promoters.is_empty() {
        return Err(GigRefusal::NoContactableRoute {
            venue: venue.name.clone(),
        });
    }

    // ── Reasons, each produced by the check that made it true ───────────────
    //
    // Pushed in the order a promoter weighs them: who else played here, then
    // who we can bring, then what the room does, then timing.
    let mut reasons = Vec::new();

    if venue.comparable_acts > 0 {
        reasons.push(Reason::ComparableActsPlayedHere {
            count: venue.comparable_acts,
            of_shows: venue.shows_last_12_months,
        });
    }

    reasons.push(Reason::ReachableAudience {
        reachable: opportunity.reachable_fans,
    });

    if let Some(draw) = venue.typical_draw {
        reasons.push(Reason::RoomDraws { typical_draw: draw });
    }

    match opportunity.months_since_show {
        None => reasons.push(Reason::NeverPlayedButHasFans {
            reachable: opportunity.reachable_fans,
        }),
        // Twelve months with an audience still active is the clearest "go
        // back" signal there is: they have not forgotten, and they have not
        // been served.
        Some(months) if months >= 12 && opportunity.active_fans_30d > 0 => {
            reasons.push(Reason::OverdueReturn {
                months,
                active_30d: opportunity.active_fans_30d,
            });
        }
        Some(_) => {}
    }

    // ── The bill: union minus intersection ──────────────────────────────────
    //
    // An act whose audience here is the same people is not a package, it is
    // one show with two names on the poster and a split door. Only the part
    // that is genuinely additional counts, and only acts that already agreed
    // to be proposed are named.
    let mut invite_to_bill = Vec::new();
    let mut added_by_co_bill = 0u32;
    for act in &opportunity.co_bill {
        if !act.consented_to_share_bills {
            continue;
        }
        let overlapping = u32::try_from(
            u64::from(act.reachable_here) * u64::from(act.audience_overlap_basis_points) / 10_000,
        )
        .unwrap_or(act.reachable_here);
        let adds = act.reachable_here.saturating_sub(overlapping);
        if adds == 0 {
            continue;
        }
        added_by_co_bill = added_by_co_bill.saturating_add(adds);
        invite_to_bill.push(act.name.clone());
        reasons.push(Reason::CoBillAddsAudience {
            act: act.name.clone(),
            adds_reachable: adds,
        });
    }

    // ── Who to write to ─────────────────────────────────────────────────────
    let mut ranked = contactable_promoters;
    ranked.sort_by(|left, right| {
        right
            .answered_last_time
            .cmp(&left.answered_last_time)
            .then_with(|| right.relationship_score.cmp(&left.relationship_score))
            // Stable by name, so the same evidence always produces the same
            // order. A proposal that reshuffles between runs reads as a new
            // proposal.
            .then_with(|| left.name.cmp(&right.name))
    });
    if let Some(warm) = ranked
        .iter()
        .find(|promoter| promoter.answered_last_time && promoter.relationship_score >= 50)
    {
        reasons.push(Reason::WarmPromoter {
            name: warm.name.clone(),
        });
    }

    if let Some(days) = venue.days_since_last_event {
        reasons.push(Reason::RoomIsActive {
            days_since_last_event: days,
        });
    }

    // ── What we do not know ─────────────────────────────────────────────────
    //
    // Stated on the proposal rather than left for the band to discover. A
    // caveat costs a line; finding it out costs a week.
    let mut caveats = Vec::new();
    if venue.capacity.is_none() {
        caveats.push(format!(
            "We do not know what {} holds, so nothing here says the room is the right size.",
            venue.name
        ));
    }
    if venue.typical_draw.is_none() {
        caveats.push(
            "No show at this room has sold tickets through us, so its draw is unmeasured \
             rather than low."
                .to_owned(),
        );
    }
    match venue.contact_verified_days_ago {
        None => caveats.push(
            "Nobody has confirmed the booking address works. Verify it before writing.".to_owned(),
        ),
        Some(days) if days > CONTACT_STALE_DAYS => caveats.push(format!(
            "The booking contact was last confirmed {days} days ago. People leave venues; \
             check it still reaches somebody."
        )),
        Some(_) => {}
    }
    if venue.comparable_acts == 0 {
        caveats.push(
            "No act from your genre has played here on record. That is not a no — it is \
             the thing a promoter will ask about."
                .to_owned(),
        );
    }

    let fits_intent = match intent {
        TenantIntent::BookingShows => "You said you are booking shows. This is that.".to_owned(),
        TenantIntent::WorkingARelease => {
            "You said the release is the focus. A show here is a launch night rather than a \
             detour — the audience is already listening."
                .to_owned()
        }
        TenantIntent::Unstated => {
            "You have not said what you are working on, so the timing here is unverified. \
             The evidence supports the city; only you know whether now is the moment."
                .to_owned()
        }
        // Refused above; kept exhaustive so a new intent cannot compile without
        // somebody deciding what it means.
        TenantIntent::HeadsDown => String::new(),
    };

    let basis = match venue.typical_draw {
        Some(_) => "Reachable means consented and inside the radius the fan chose. The room's \
                    draw is the mean paid orders per ticketed show there, across every tenant \
                    who has played it — history, not a forecast."
            .to_owned(),
        None => "Reachable means consented and inside the radius the fan chose. The room's \
                 own draw is unmeasured, so this is an audience estimate with no venue \
                 history behind it."
            .to_owned(),
    };

    Ok(GigPlan {
        city: opportunity.city.clone(),
        venue: venue.name.clone(),
        contact: ranked
            .iter()
            .map(|promoter| promoter.name.clone())
            .collect(),
        invite_to_bill,
        reasons,
        reach: ReachEstimate {
            reachable: opportunity.reachable_fans,
            added_by_co_bill,
            room_typical_draw: venue.typical_draw,
            basis,
        },
        fits_intent,
        caveats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue() -> VenueEvidence {
        VenueEvidence {
            name: "Klub X".to_owned(),
            shows_last_12_months: 14,
            comparable_acts: 3,
            capacity: Some(300),
            typical_draw: Some(180),
            contact_verified_days_ago: Some(18),
            days_since_last_event: Some(11),
            has_booking_route: true,
        }
    }

    fn opportunity() -> CityOpportunity {
        CityOpportunity {
            city: "Wrocław".to_owned(),
            reachable_fans: 240,
            active_fans_30d: 60,
            months_since_show: Some(14),
            has_upcoming_show: false,
            venue: Some(venue()),
            promoters: vec![PromoterRef {
                name: "Anna".to_owned(),
                relationship_score: 70,
                answered_last_time: true,
                has_route: true,
            }],
            co_bill: Vec::new(),
        }
    }

    #[test]
    fn a_strong_city_produces_a_plan_with_its_reasons() {
        let plan = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert_eq!(plan.city, "Wrocław");
        assert_eq!(plan.venue, "Klub X");
        assert_eq!(plan.contact, vec!["Anna"]);
        assert!(
            plan.reasons.contains(&Reason::ComparableActsPlayedHere {
                count: 3,
                of_shows: 14
            }),
            "the strongest fact a promoter recognises was not given as a reason"
        );
        assert!(plan.reasons.contains(&Reason::WarmPromoter {
            name: "Anna".to_owned()
        }));
        assert!(!plan.reasons.is_empty());
    }

    /// The failure this module exists to avoid: proposing regardless.
    #[test]
    fn a_city_with_almost_nobody_in_it_is_refused() {
        let mut thin = opportunity();
        thin.reachable_fans = MINIMUM_REACHABLE_FOR_A_GIG - 1;
        assert_eq!(
            plan_gig(&thin, TenantIntent::BookingShows),
            Err(GigRefusal::TooFewReachable {
                reachable: MINIMUM_REACHABLE_FOR_A_GIG - 1,
                floor: MINIMUM_REACHABLE_FOR_A_GIG,
            })
        );
        // Exactly at the floor proposes. An off-by-one here silently removes a
        // band of real cities from every suggestion they ever get.
        let mut at_floor = opportunity();
        at_floor.reachable_fans = MINIMUM_REACHABLE_FOR_A_GIG;
        assert!(plan_gig(&at_floor, TenantIntent::BookingShows).is_ok());
    }

    #[test]
    fn a_city_already_booked_is_not_a_gap() {
        let mut booked = opportunity();
        booked.has_upcoming_show = true;
        assert_eq!(
            plan_gig(&booked, TenantIntent::BookingShows),
            Err(GigRefusal::AlreadyBooked)
        );
    }

    /// The tenant's own plan outranks the evidence. A band that said it is
    /// recording does not want a gig proposal, however good the city looks —
    /// and ignoring that is how a helpful system becomes a noisy one.
    #[test]
    fn a_tenant_who_said_they_are_writing_is_not_pitched_shows() {
        assert_eq!(
            plan_gig(&opportunity(), TenantIntent::HeadsDown),
            Err(GigRefusal::NotWhatTheTenantIsDoing {
                intent: TenantIntent::HeadsDown
            })
        );
        // And the refusal is not a dead end — it says how to turn it back on.
        let message = GigRefusal::NotWhatTheTenantIsDoing {
            intent: TenantIntent::HeadsDown,
        }
        .message();
        assert!(message.contains("Change that"), "no way back: {message}");
    }

    /// Never seen an event and stopped having them need different next steps,
    /// so they must not collapse into one refusal message.
    #[test]
    fn a_dormant_room_and_an_unseen_room_read_differently() {
        let mut quiet = opportunity();
        if let Some(room) = quiet.venue.as_mut() {
            room.days_since_last_event = Some(DORMANT_ROOM_DAYS + 1);
        }
        let stopped = plan_gig(&quiet, TenantIntent::BookingShows).expect_err("refuses");
        assert!(stopped.message().contains("stopped programming"));

        let mut unseen = opportunity();
        if let Some(room) = unseen.venue.as_mut() {
            room.days_since_last_event = None;
        }
        let never = plan_gig(&unseen, TenantIntent::BookingShows).expect_err("refuses");
        assert!(never.message().contains("never seen an event"));
        assert_ne!(stopped.message(), never.message());
    }

    /// A room at the dormancy boundary still proposes. The bound is a bound.
    #[test]
    fn the_dormancy_bound_is_a_boundary_not_a_vibe() {
        let mut edge = opportunity();
        if let Some(room) = edge.venue.as_mut() {
            room.days_since_last_event = Some(DORMANT_ROOM_DAYS);
        }
        assert!(plan_gig(&edge, TenantIntent::BookingShows).is_ok());
    }

    #[test]
    fn a_room_nobody_can_write_to_is_refused_by_name() {
        let mut unreachable = opportunity();
        if let Some(room) = unreachable.venue.as_mut() {
            room.has_booking_route = false;
        }
        unreachable.promoters.clear();
        assert_eq!(
            plan_gig(&unreachable, TenantIntent::BookingShows),
            Err(GigRefusal::NoContactableRoute {
                venue: "Klub X".to_owned()
            })
        );
    }

    /// A promoter with a route rescues a room that has none — the room is the
    /// place, the promoter is the person, and only one of them has to be
    /// reachable.
    #[test]
    fn a_promoter_with_a_route_is_enough_without_the_venues_own() {
        let mut through_promoter = opportunity();
        if let Some(room) = through_promoter.venue.as_mut() {
            room.has_booking_route = false;
        }
        assert!(plan_gig(&through_promoter, TenantIntent::BookingShows).is_ok());
    }

    // ── The bill ────────────────────────────────────────────────────────────

    /// The whole point of a co-bill. An act that brings the same people is one
    /// show with two names on it and a door split two ways.
    #[test]
    fn a_co_bill_that_brings_the_same_people_is_not_invited() {
        let mut same_crowd = opportunity();
        same_crowd.co_bill = vec![CoBillAct {
            name: "Twin".to_owned(),
            reachable_here: 200,
            audience_overlap_basis_points: 10_000,
            consented_to_share_bills: true,
        }];
        let plan = plan_gig(&same_crowd, TenantIntent::BookingShows).expect("proposes");
        assert!(plan.invite_to_bill.is_empty(), "invited its own audience");
        assert_eq!(plan.reach.added_by_co_bill, 0);
    }

    #[test]
    fn a_co_bill_that_adds_people_is_invited_with_the_number_it_adds() {
        let mut complementary = opportunity();
        complementary.co_bill = vec![CoBillAct {
            name: "Other".to_owned(),
            reachable_here: 200,
            // A quarter of their audience is already ours.
            audience_overlap_basis_points: 2_500,
            consented_to_share_bills: true,
        }];
        let plan = plan_gig(&complementary, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(plan.invite_to_bill, vec!["Other"]);
        assert_eq!(plan.reach.added_by_co_bill, 150);
        assert!(plan.reasons.contains(&Reason::CoBillAddsAudience {
            act: "Other".to_owned(),
            adds_reachable: 150
        }));
    }

    /// An act that has not agreed is a suggestion to ask, never a name on a
    /// proposal that somebody might announce.
    #[test]
    fn an_act_that_never_agreed_is_not_put_on_a_bill() {
        let mut unconsented = opportunity();
        unconsented.co_bill = vec![CoBillAct {
            name: "Unasked".to_owned(),
            reachable_here: 300,
            audience_overlap_basis_points: 0,
            consented_to_share_bills: false,
        }];
        let plan = plan_gig(&unconsented, TenantIntent::BookingShows).expect("proposes");
        assert!(plan.invite_to_bill.is_empty());
        assert_eq!(plan.reach.added_by_co_bill, 0);
    }

    // ── What we do not know, said out loud ──────────────────────────────────

    #[test]
    fn everything_unmeasured_becomes_a_caveat_rather_than_a_silence() {
        let mut unknowns = opportunity();
        if let Some(room) = unknowns.venue.as_mut() {
            room.capacity = None;
            room.typical_draw = None;
            room.contact_verified_days_ago = None;
            room.comparable_acts = 0;
        }
        let plan = plan_gig(&unknowns, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(
            plan.caveats.len(),
            4,
            "a silent unknown: {:?}",
            plan.caveats
        );
        assert!(plan.reach.room_typical_draw.is_none());
        assert!(
            plan.reach.basis.contains("unmeasured"),
            "the basis hid that the room has no measured draw"
        );
        // And an unmeasured room must not claim a draw it does not have.
        assert!(
            !plan
                .reasons
                .iter()
                .any(|reason| matches!(reason, Reason::RoomDraws { .. }))
        );
    }

    #[test]
    fn a_stale_contact_is_flagged_and_a_fresh_one_is_not() {
        let mut stale = opportunity();
        if let Some(room) = stale.venue.as_mut() {
            room.contact_verified_days_ago = Some(CONTACT_STALE_DAYS + 1);
        }
        let flagged = plan_gig(&stale, TenantIntent::BookingShows).expect("proposes");
        assert!(
            flagged
                .caveats
                .iter()
                .any(|note| note.contains("People leave venues")),
            "a four-month-old address was presented as a live route"
        );

        // The default fixture's contact is 18 days old and must not be flagged.
        let fresh = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert!(
            !fresh
                .caveats
                .iter()
                .any(|note| note.contains("People leave venues")),
            "a fresh contact was flagged as stale"
        );
    }

    // ── Timing ──────────────────────────────────────────────────────────────

    #[test]
    fn a_city_with_fans_and_no_history_reads_differently_from_an_overdue_one() {
        let mut never = opportunity();
        never.months_since_show = None;
        let first_time = plan_gig(&never, TenantIntent::BookingShows).expect("proposes");
        assert!(
            first_time
                .reasons
                .contains(&Reason::NeverPlayedButHasFans { reachable: 240 })
        );

        let overdue = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert!(overdue.reasons.contains(&Reason::OverdueReturn {
            months: 14,
            active_30d: 60
        }));
    }

    /// A release-focused tenant still gets the proposal, framed as serving the
    /// release rather than competing with it.
    #[test]
    fn a_release_becomes_the_frame_rather_than_a_blocker() {
        let plan = plan_gig(&opportunity(), TenantIntent::WorkingARelease).expect("proposes");
        assert!(plan.fits_intent.contains("launch night"));
    }

    #[test]
    fn an_unstated_intent_says_the_timing_is_unverified() {
        let plan = plan_gig(&opportunity(), TenantIntent::Unstated).expect("proposes");
        assert!(plan.fits_intent.contains("unverified"));
        assert!(!plan.fits_intent.is_empty());
    }

    #[test]
    fn every_refusal_says_what_would_change_it() {
        for refusal in [
            GigRefusal::AlreadyBooked,
            GigRefusal::NoRoomOnRecord,
            GigRefusal::NoContactableRoute {
                venue: "Klub X".to_owned(),
            },
            GigRefusal::RoomLooksDormant {
                venue: "Klub X".to_owned(),
                days_since_last_event: Some(400),
            },
            GigRefusal::RoomLooksDormant {
                venue: "Klub X".to_owned(),
                days_since_last_event: None,
            },
            GigRefusal::TooFewReachable {
                reachable: 12,
                floor: MINIMUM_REACHABLE_FOR_A_GIG,
            },
            GigRefusal::NotWhatTheTenantIsDoing {
                intent: TenantIntent::HeadsDown,
            },
        ] {
            let message = refusal.message();
            assert!(message.len() > 40, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the band: {message}"
            );
        }
    }

    /// Two runs over the same evidence must produce the same proposal, or a
    /// band re-reading yesterday's suggestion sees a different one and stops
    /// trusting both.
    #[test]
    fn the_same_evidence_always_produces_the_same_proposal() {
        let mut many = opportunity();
        many.promoters = vec![
            PromoterRef {
                name: "Zed".to_owned(),
                relationship_score: 60,
                answered_last_time: true,
                has_route: true,
            },
            PromoterRef {
                name: "Ada".to_owned(),
                relationship_score: 60,
                answered_last_time: true,
                has_route: true,
            },
        ];
        let first = plan_gig(&many, TenantIntent::BookingShows).expect("proposes");
        let second = plan_gig(&many, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(first, second);
        assert_eq!(first.contact, vec!["Ada", "Zed"]);
    }
}
