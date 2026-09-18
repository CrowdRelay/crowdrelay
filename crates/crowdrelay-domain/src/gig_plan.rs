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

impl TenantIntent {
    /// The stored form. One vocabulary for the query string, the settings row
    /// and the console, so a band that picks "heads down" in one place is heads
    /// down in all three.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unstated => "unstated",
            Self::BookingShows => "booking_shows",
            Self::WorkingARelease => "working_a_release",
            Self::HeadsDown => "heads_down",
        }
    }

    /// Parses a stored or submitted value.
    ///
    /// Returns `None` for anything unrecognised rather than falling back to a
    /// variant. The caller decides what an unreadable value means, and the two
    /// callers decide differently: a settings write rejects it, while a read
    /// keeps whatever the tenant last stated rather than silently demoting a
    /// stated intent to `Unstated`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "unstated" => Some(Self::Unstated),
            "booking_shows" => Some(Self::BookingShows),
            "working_a_release" => Some(Self::WorkingARelease),
            "heads_down" => Some(Self::HeadsDown),
            _ => None,
        }
    }

    /// Every variant, for a console that must offer all of them.
    ///
    /// Served rather than retyped in the UI: an intent the band cannot select
    /// is an intent the planner will never respect.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [
            Self::Unstated,
            Self::BookingShows,
            Self::WorkingARelease,
            Self::HeadsDown,
        ]
    }

    /// What the band reads next to the choice.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Unstated => {
                "No stated plan. Proposals arrive on evidence and say the timing is unverified."
            }
            Self::BookingShows => "Actively looking for shows. Proposals arrive freely.",
            Self::WorkingARelease => {
                "A release is the focus. Shows are proposed as launch nights, not as detours."
            }
            Self::HeadsDown => "Writing or recording. No gig proposals until this changes.",
        }
    }
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
    /// How many distinct acts billed at this room — on record, all-time,
    /// the same count `city_venues` and the booking snapshot report — have a
    /// genre overlapping this tenant's. Deliberately not a subset of
    /// `shows_last_12_months`: the registry's bills reach further back than
    /// the activity window, and one room must answer with one number
    /// wherever it is asked. The strongest single fact in the proposal, and
    /// the one a promoter recognises fastest.
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
    /// Whatever the caller uses to find this promoter's row again, opaque to
    /// this module.
    ///
    /// A display name is not an identity: two people who book the same city
    /// can both be "Anna", and the booking list is unique on the contact
    /// address rather than on the name. Ranking by name and addressing by name
    /// meant the second Anna was silently never written to, while the console
    /// showed both — which breaks the one promise this feature makes, that
    /// everybody who books the room hears about the night.
    pub key: String,
    pub name: String,
    /// 0..=100. Derived from recorded outcomes, not from a guess.
    pub relationship_score: u16,
    /// Whether the last approach to them got an answer of any kind.
    pub answered_last_time: bool,
    pub has_route: bool,
}

/// One promoter on a proposal: who the band reads, and what the letter resolves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromoterContact {
    pub key: String,
    pub name: String,
}

/// An act that could share the bill, and what it brings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoBillAct {
    /// The act's workspace, because a display name is not an identity — two
    /// roster siblings can share one name, and "ask them onto the bill" has to
    /// name a single act.
    pub workspace: crate::WorkspaceId,
    pub name: String,
    /// Consented, reachable people this act has in this city. The number that
    /// decides whether a co-bill adds a room or splits one.
    pub reachable_here: u32,
    /// How many of `reachable_here` are already the tenant's people here —
    /// measured, not reconstructed from the basis-point share, so the
    /// arithmetic a promoter reads loses nobody to rounding.
    pub shared_with_tenant: u32,
    /// Share of the two audiences that is the same people, in basis points.
    /// High overlap is not a package — it is one show with two names on it.
    pub audience_overlap_basis_points: u16,
    /// Whether this act has already agreed to be proposed. An act that has not
    /// is a suggestion to *ask*, never a commitment to announce.
    pub consented_to_share_bills: bool,
}

/// What the planner knows about one city.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CityOpportunity {
    /// The catalogue row this evidence belongs to. `city` is the slug, and a
    /// slug is only unique per country — two catalogues rows can share one —
    /// so identity is the id, and the string is for reading.
    pub city_id: crate::CityId,
    pub city: String,
    /// Where the catalogue pins the city. `None` on either axis means the
    /// city cannot be routed — it still proposes as a night, it just cannot
    /// join a corridor, the same way an unmeasured reach is not a zero.
    #[serde(default)]
    pub latitude: Option<f64>,
    /// Paired with `latitude` — the schema binds the two to NULL-or-both.
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Consented fans inside the radius they chose. `None` means the city
    /// cannot be measured — no coordinates on record — which is not the same
    /// claim as a measured zero.
    pub reachable_fans: Option<u32>,
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
    /// The city cannot be sized — no coordinates on record to measure
    /// against. Distinct from a measured zero: the audience may be large,
    /// we just cannot say.
    AudienceNotMeasurable,
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
            Self::AudienceNotMeasurable => {
                "we cannot size the audience here — the city has no coordinates on record,                  so there is nothing to measure reach against. The show may still be worth                  playing; the data just cannot say"
                    .to_owned()
            }
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
    /// people — measured per act against the tenant. Two billed acts who
    /// share fans with *each other* still count that part twice: the sum of
    /// pairwise additions is an upper bound on the union, which is the honest
    /// direction to err — a package promising less than it could reach would
    /// undersell the night.
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
    /// The catalogue id of `city` — what an approval names. The slug is only
    /// unique per country, so it cannot stand in for identity.
    pub city_id: crate::CityId,
    pub city: String,
    pub venue: String,
    /// Who to write to, strongest relationship first. Each entry carries the
    /// key the caller resolves, because a display name is not an identity.
    pub contact: Vec<PromoterContact>,
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

impl GigPlan {
    /// The first line of the outreach, built from the reason that carried the
    /// proposal (4G.4).
    ///
    /// # Why the draft does not compose its own opening
    ///
    /// The reasons exist because a proposal and the letter it becomes must
    /// state the same fact. A draft that writes its own first line is a second
    /// code path describing one decision, and the first time the two disagree
    /// the promoter has the wrong one in their inbox while the console shows
    /// the right one.
    ///
    /// So this renders `reasons[0]` — already ordered strongest first by
    /// `plan_gig`, and never empty — and nothing else. A fact a promoter can
    /// check beats a sentence about how excited anybody is: "eleven acts from
    /// this genre played your room last year" is an argument, "we love your
    /// venue" is noise, and a promoter reading forty of these a week can tell
    /// the difference in one line.
    ///
    /// It is a fact, not a finished email. The tenant's own voice writes the
    /// rest — this module has never read a word the band wrote and must not
    /// pretend to.
    #[must_use]
    pub fn opening_line(&self) -> String {
        let city = &self.city;
        let venue = &self.venue;
        match self.reasons.first() {
            Some(Reason::ComparableActsPlayedHere { count, .. }) => {
                // The count is all-time billed acts while `of_shows` is the
                // last twelve months — "X of the last N shows" could claim a
                // subset the number does not guarantee, so the sentence states
                // the record.
                if *count == 1 {
                    format!("One act from our genre has played {venue} on record.")
                } else {
                    format!("{count} acts from our genre have played {venue} on record.")
                }
            }
            Some(Reason::ReachableAudience { reachable }) => format!(
                "{reachable} people around {city} asked us to tell them when we play nearby."
            ),
            Some(Reason::RoomDraws { typical_draw }) => format!(
                "Ticketed shows at {venue} average {typical_draw} paid tickets, which is the \
                 size we are playing to."
            ),
            Some(Reason::NeverPlayedButHasFans { reachable }) => format!(
                "{reachable} people around {city} asked to hear when we play nearby, and we \
                 have never played the city."
            ),
            Some(Reason::OverdueReturn { months, active_30d }) => format!(
                "We last played {city} {months} months ago, and {active_30d} people there \
                 have been active with us in the last month."
            ),
            Some(Reason::CoBillAddsAudience {
                act,
                adds_reachable,
            }) => format!(
                "A bill with {act} reaches {adds_reachable} people around {city} that we do \
                 not reach on our own."
            ),
            Some(Reason::WarmPromoter { name }) => {
                format!("{name} — we spoke before, and we are looking at {city} again.")
            }
            Some(Reason::RoomIsActive {
                days_since_last_event,
            }) => format!(
                "{venue} had something on {days_since_last_event} days ago, and we are \
                 looking at {city}."
            ),
            // Unreachable by construction: `plan_gig` never returns a proposal
            // with no reasons. Stated rather than unwrapped, because a panic
            // here would be an outreach the band cannot send and cannot
            // diagnose, and this sentence is still true.
            None => format!("We are looking at {city}, and {venue} is the room."),
        }
    }
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
    // Unmeasurable is its own refusal, not a zero. A city without
    // coordinates may have thousands of fans; reporting "0 asked to hear
    // from you" would be a measurement the system never made.
    let Some(reachable_fans) = opportunity.reachable_fans else {
        return Err(GigRefusal::AudienceNotMeasurable);
    };
    if reachable_fans < MINIMUM_REACHABLE_FOR_A_GIG {
        return Err(GigRefusal::TooFewReachable {
            reachable: reachable_fans,
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
        reachable: reachable_fans,
    });

    if let Some(draw) = venue.typical_draw {
        reasons.push(Reason::RoomDraws { typical_draw: draw });
    }

    match opportunity.months_since_show {
        None => reasons.push(Reason::NeverPlayedButHasFans {
            reachable: reachable_fans,
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
        // Past the pairing ceiling the two audiences are one audience — the
        // same rule the roster's support picker applies, because a bill that
        // adds nobody new just splits a door.
        if act.audience_overlap_basis_points
            > crate::roster_plan::PAIRING_OVERLAP_CEILING_BASIS_POINTS
        {
            continue;
        }
        let adds = act.reachable_here.saturating_sub(act.shared_with_tenant);
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

    // One promoter, one entry — deduplicated by key, never by name. Two
    // people who book the same city can share a display name, and collapsing
    // them by name drops one of the recipients the letter promised to reach.
    // A list that reads "Anna, Anna" is correct when there are two Annas; the
    // console can disambiguate them, and the letter addresses both.
    let mut seen_keys = std::collections::HashSet::new();
    let contact: Vec<PromoterContact> = ranked
        .iter()
        .filter(|promoter| seen_keys.insert(promoter.key.clone()))
        .map(|promoter| PromoterContact {
            key: promoter.key.clone(),
            name: promoter.name.clone(),
        })
        .collect();

    Ok(GigPlan {
        city_id: opportunity.city_id,
        city: opportunity.city.clone(),
        venue: venue.name.clone(),
        contact,
        invite_to_bill,
        reasons,
        reach: ReachEstimate {
            reachable: reachable_fans,
            added_by_co_bill,
            room_typical_draw: venue.typical_draw,
            basis,
        },
        fits_intent,
        caveats,
    })
}

// The tests live in `gig_plan/tests.rs` and are included here rather than
// written inline: this module carries the reasoning behind every bound it
// holds, and the file crossed the 1200-line source ratchet. The split keeps
// the prose and the proofs in one compilation unit while each file stays
// readable — the same pattern the autopilot repository uses.
include!("gig_plan/tests.rs");
