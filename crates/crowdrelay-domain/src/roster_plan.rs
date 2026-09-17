//! The same question as `gig_plan`, asked by somebody who has several acts.
//!
//! # Why a roster is not a band with a bigger number
//!
//! A band asks *where do we play next*. A roster or a label asks four
//! questions a band never has to:
//!
//! 1. **Which of my acts should play this city?** Two acts, one strong city,
//!    one night — somebody headlines and somebody does not.
//! 2. **Whose turn is it?** An act that has not played in eight months while
//!    the flagship plays weekly is a roster problem, not a scheduling detail.
//!    It is also the thing a manager gets fired over.
//! 3. **Does this pairing add a room or split one?** Two acts with the same
//!    audience in a city is one show with two names on the poster and a door
//!    split two ways.
//! 4. **How much of this can we actually run?** A label with thirty acts
//!    cannot run thirty campaigns, and a plan that ignores that is a wish.
//!
//! # The cheapest gig is the one somebody else already booked
//!
//! The highest-value move in this module is not proposing a new night. It is
//! noticing that one act already has a confirmed show with an open support
//! slot, and that a labelmate is strong in that city. The room is booked, the
//! promoter is committed, the date is set — the only thing missing is a name.
//!
//! So a support-slot fill outranks a new booking whenever both are available,
//! and the planner says so. That is the Pareto move for a roster: most of the
//! value for almost none of the work, and it is the one thing a roster can do
//! that a band on its own cannot.
//!
//! # Headliner is decided by local draw, never by catalogue rank
//!
//! The act with the most reachable people *in that city* headlines, even when
//! the label considers another act bigger. This will be unpopular occasionally
//! and it is still right: the headliner's job is to fill the room, and the
//! room is in one city. A label that overrides this is welcome to — it is
//! their roster — but the system will not pretend the catalogue order is
//! audience evidence.
//!
//! # It refuses, for the same reason `gig_plan` does
//!
//! Past the period's package cap, the answer is that the roster is full. A
//! plan nobody can staff is not ambition, it is a list that teaches the
//! manager to stop reading the list.

use serde::{Deserialize, Serialize};

use crate::gig_plan::{CityOpportunity, GigRefusal, Reason, TenantIntent, plan_gig};

/// Months without a show past which an act is starved rather than resting.
///
/// Six months is two seasons. Past it a roster's newer acts stop building the
/// live history that is the only thing that gets them a better slot, and the
/// investment the roster made in signing them stops compounding.
pub const STARVED_AFTER_MONTHS: u16 = 6;

/// How much local draw a starved act may give away and still win the slot.
///
/// Fairness never overrides a real gap. An act 40% behind on local reach does
/// not headline because it is their turn — the room still has to fill, and a
/// half-empty night sets that act back further than waiting would have. Inside
/// this margin the two acts are close enough that the tie-break may be
/// something other than the number.
pub const STARVED_ACT_MARGIN_BASIS_POINTS: u16 = 2_000;

/// Overlap past which a pairing is one audience, not two.
///
/// At 60% shared, the second act is bringing forty people the first did not
/// have and taking half the door. That is a favour between friends, which is a
/// fine reason to do it and not a reason for the system to propose it.
pub const PAIRING_OVERLAP_CEILING_BASIS_POINTS: u16 = 6_000;

/// One act on the roster, as the planner needs to see it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterAct {
    pub name: String,
    /// What this act said it is doing. Respected exactly as it is for a band:
    /// an act that is recording is not proposed shows, whatever the roster
    /// would prefer.
    pub intent: TenantIntent,
    /// `None` means never played — an act with no live history at all, which
    /// is a different problem from one that has gone quiet.
    pub months_since_last_show: Option<u16>,
    /// Reachable, consented people this act has, per city.
    pub reach_by_city: Vec<CityReach>,
    /// Share of this act's audience that overlaps each other act **in one
    /// city**, in basis points. Absent means unmeasured, and unmeasured is
    /// treated as unknown rather than as zero — pairing two acts on the
    /// assumption their audiences are separate is exactly the mistake this
    /// number exists to prevent.
    ///
    /// Per city, because that is where it is applied. Two acts can share 5% of
    /// their audiences nationally and 90% in one city where both are local;
    /// judging that city's bill on the national number puts two acts with one
    /// audience on one poster and splits the door two ways, which is precisely
    /// what `PAIRING_OVERLAP_CEILING_BASIS_POINTS` exists to stop.
    pub overlap_with: Vec<ActOverlap>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CityReach {
    pub city: String,
    /// Identity, not the slug: two catalogue cities can share a slug.
    pub city_id: crate::CityId,
    /// `None` means this city cannot be measured for the act — distinct from
    /// a measured zero.
    pub reachable: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActOverlap {
    pub other_act: String,
    /// The city the share was measured in. Identity, not the slug.
    pub city_id: crate::CityId,
    pub overlap_basis_points: u16,
}

impl RosterAct {
    /// `None` means the act's reach in this city was never measured or the
    /// city cannot be measured — a caller deciding on it must not read that
    /// as a counted zero.
    #[must_use]
    pub fn reach_in(&self, city: crate::CityId) -> Option<u32> {
        self.reach_by_city
            .iter()
            .find(|reach| reach.city_id == city)
            .and_then(|reach| reach.reachable)
    }

    /// `None` when the two acts have never been measured against each other
    /// **in this city**. A share measured somewhere else is not an answer
    /// here, and reading it as one is how two acts with one local audience end
    /// up on one poster.
    #[must_use]
    pub fn overlap_with_act(&self, other: &str, city: crate::CityId) -> Option<u16> {
        self.overlap_with
            .iter()
            .find(|overlap| overlap.other_act == other && overlap.city_id == city)
            .map(|overlap| overlap.overlap_basis_points)
    }

    #[must_use]
    pub fn is_starved(&self) -> bool {
        self.months_since_last_show
            .is_none_or(|months| months >= STARVED_AFTER_MONTHS)
    }

    /// Whether this act is available to be proposed at all.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.intent != TenantIntent::HeadsDown
    }
}

/// A confirmed show belonging to one of the roster's acts, with room on the
/// bill.
///
/// The cheapest gig in the system: the room is booked, the promoter is
/// committed and the date is set. Filling it costs an ask.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpenSupportSlot {
    pub city: String,
    /// Identity, not the slug: two catalogue cities can share a slug.
    pub city_id: crate::CityId,
    pub venue: String,
    /// The act whose show this is. They are not a candidate for their own
    /// support slot.
    pub headliner: String,
    /// How far out, in days. A slot three days away is not fillable by an act
    /// that has to travel and rehearse.
    pub days_until_show: u16,
}

/// Slots closer than this are not proposed.
///
/// Ten days covers an ask, an answer, a rehearsal and a van. Filling a slot
/// four days out is possible and it is the promoter's call to make, not a
/// suggestion a system should generate.
pub const MINIMUM_SLOT_LEAD_DAYS: u16 = 10;

/// Everything the roster planner sees.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterOpportunity {
    pub acts: Vec<RosterAct>,
    /// Cities with their evidence, reused verbatim from the band planner so
    /// the two cannot disagree about what makes a city worth playing.
    pub cities: Vec<CityOpportunity>,
    pub open_slots: Vec<OpenSupportSlot>,
    /// How many packages this roster can actually run in the period. Caps
    /// outvote ambition: a plan nobody can staff teaches the manager to stop
    /// reading the plan.
    pub packages_this_period: u16,
}

/// What the roster should do, in the order it should do it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RosterMove {
    /// Put an act into a show that already exists. Cheapest move available.
    FillSupportSlot {
        city: String,
        venue: String,
        headliner: String,
        /// The labelmate to ask.
        support: String,
        adds_reachable: u32,
        days_until_show: u16,
        reasons: Vec<Reason>,
    },
    /// Book a new night, with a headliner chosen by local draw.
    BookPackage {
        city: String,
        venue: String,
        headliner: String,
        /// Empty when no labelmate adds audience without splitting it.
        support: Option<String>,
        /// Who the letter goes to, each carrying the key that resolves the
        /// booking row — a display name is not an identity, and two people who
        /// book one city can share one.
        contact: Vec<crate::gig_plan::PromoterContact>,
        combined_reachable: u32,
        reasons: Vec<Reason>,
        caveats: Vec<String>,
        /// Present when the headliner won on fairness rather than on reach
        /// alone, so a manager can see that and overrule it.
        fairness_note: Option<String>,
    },
}

impl RosterMove {
    /// Cities are one-per-move: a roster cannot run two of its own shows in
    /// one city in one period without competing with itself.
    #[must_use]
    pub fn city(&self) -> &str {
        match self {
            Self::FillSupportSlot { city, .. } | Self::BookPackage { city, .. } => city,
        }
    }

    /// Every act named. Used to keep one act out of two moves.
    #[must_use]
    pub fn acts(&self) -> Vec<&str> {
        match self {
            Self::FillSupportSlot {
                headliner, support, ..
            } => vec![headliner.as_str(), support.as_str()],
            Self::BookPackage {
                headliner, support, ..
            } => {
                let mut names = vec![headliner.as_str()];
                if let Some(support) = support {
                    names.push(support.as_str());
                }
                names
            }
        }
    }
}

/// Why a city produced no move.
///
/// Two different facts that a manager must be able to tell apart. `Refused`
/// means the city itself did not clear the bar and the reason is about the
/// city. `NoActFree` means the city was fine and every act was already
/// committed elsewhere in this period — which is a cap problem, not a city
/// problem, and answering it by researching venues would be answering the
/// wrong question.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CitySkipped {
    Refused(GigRefusal),
    NoActFree,
}

impl CitySkipped {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Refused(refusal) => refusal.message(),
            Self::NoActFree => "the city is fine — every act who could play it is already \
                 booked somewhere else this period. Raise the period's package count, or \
                 this one waits"
                .to_owned(),
        }
    }
}

/// Why the roster planner produced nothing.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RosterRefusal {
    /// Every act said it is not playing right now.
    NoActIsAvailable,
    /// The period's packages are already committed.
    PeriodIsFull { cap: u16 },
    /// Acts are available and no city cleared the bar. The per-city refusals
    /// travel so the manager sees why rather than only that.
    NothingClearedTheBar {
        per_city: Vec<(String, CitySkipped)>,
    },
}

impl RosterRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NoActIsAvailable => {
                "every act on the roster said they are writing or recording. Nothing is \
                 proposed until one of them is booking again"
                    .to_owned()
            }
            Self::PeriodIsFull { cap } => format!(
                "this period's {cap} packages are already committed. More would be a list \
                 rather than a plan — the next one waits for the next period"
            ),
            Self::NothingClearedTheBar { per_city } => {
                let named = per_city
                    .iter()
                    .take(3)
                    .map(|(city, refusal)| format!("{city}: {}", refusal.message()))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("no city cleared the bar this period. {named}")
            }
        }
    }
}

/// The plan, ordered cheapest-first.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterRun {
    pub moves: Vec<RosterMove>,
    /// Acts that could have played and did not fit in the period's cap. Named
    /// so a manager can see who is waiting rather than discovering it in three
    /// months when somebody asks why they have not played.
    pub waiting: Vec<String>,
    /// Cities considered and refused, with the reason. The manager's own
    /// judgement often beats the bar, and they cannot apply it to a city they
    /// were never told about.
    pub considered_and_refused: Vec<(String, CitySkipped)>,
}

/// Picks the act for a city by local draw, with fairness as a tie-break.
///
/// Returns the act and, when fairness decided it, the sentence explaining
/// that — because a manager who sees the second-biggest draw headlining needs
/// to know whether the system made a mistake or a choice.
fn choose_headliner<'a>(
    acts: &[&'a RosterAct],
    city: crate::CityId,
) -> Option<(&'a RosterAct, Option<String>)> {
    let best = acts
        .iter()
        .max_by(|left, right| {
            left.reach_in(city)
                .cmp(&right.reach_in(city))
                // Stable by name so the same roster always produces the same
                // plan. A plan that reshuffles between runs reads as a new
                // plan and gets re-litigated every time.
                .then_with(|| right.name.cmp(&left.name))
        })
        .copied()?;
    // An act nobody can measure cannot headline — `None` reads as no
    // candidate here, which is what a zero already meant.
    let best_reach = best.reach_in(city)?;
    if best_reach == 0 {
        return None;
    }

    // Fairness only inside the margin. An act well behind on local reach does
    // not headline because it is their turn: the room still has to fill, and a
    // half-empty night sets that act back further than waiting would have.
    let floor = u32::try_from(
        u64::from(best_reach)
            * u64::from(10_000_u16.saturating_sub(STARVED_ACT_MARGIN_BASIS_POINTS))
            / 10_000,
    )
    .unwrap_or(best_reach);

    let starved_contender = acts
        .iter()
        .copied()
        .filter(|act| act.is_starved() && act.name != best.name)
        .filter(|act| {
            act.reach_in(city)
                .is_some_and(|reach| reach >= floor && reach > 0)
        })
        .max_by(|left, right| {
            left.reach_in(city)
                .cmp(&right.reach_in(city))
                .then_with(|| right.name.cmp(&left.name))
        });

    match starved_contender {
        Some(starved) if !best.is_starved() => {
            let note = match starved.months_since_last_show {
                Some(months) => format!(
                    "{} headlines over {} on fairness: their local draw is within {}% and \
                     they have not played in {months} months. Overrule it if the room needs \
                     the bigger name.",
                    starved.name,
                    best.name,
                    STARVED_ACT_MARGIN_BASIS_POINTS / 100
                ),
                None => format!(
                    "{} headlines over {} on fairness: their local draw is within {}% and \
                     they have never played a show on record. Overrule it if the room needs \
                     the bigger name.",
                    starved.name,
                    best.name,
                    STARVED_ACT_MARGIN_BASIS_POINTS / 100
                ),
            };
            Some((starved, Some(note)))
        }
        _ => Some((best, None)),
    }
}

/// The best labelmate to put under `headliner` in this city, if any.
///
/// Returns the act and how many people it genuinely adds. An act whose
/// audience here is mostly the same people adds a name to the poster and takes
/// half the door, so it is not proposed — and an act that has never been
/// measured against the headliner is not proposed either, because assuming two
/// audiences are separate is the mistake the overlap number exists to prevent.
fn choose_support<'a>(
    acts: &[&'a RosterAct],
    headliner: &RosterAct,
    city: crate::CityId,
) -> Option<(&'a RosterAct, u32)> {
    acts.iter()
        .copied()
        .filter(|act| act.name != headliner.name)
        .filter_map(|act| {
            let overlap = act.overlap_with_act(&headliner.name, city)?;
            if overlap > PAIRING_OVERLAP_CEILING_BASIS_POINTS {
                return None;
            }
            let reach = act.reach_in(city)?;
            let shared =
                u32::try_from(u64::from(reach) * u64::from(overlap) / 10_000).unwrap_or(reach);
            let adds = reach.saturating_sub(shared);
            (adds > 0).then_some((act, adds))
        })
        .max_by(|(left_act, left_adds), (right_act, right_adds)| {
            left_adds
                .cmp(right_adds)
                .then_with(|| right_act.name.cmp(&left_act.name))
        })
}

/// Plans the roster's next period.
///
/// # Errors
///
/// Refuses when no act is available, when the period's cap is already zero, or
/// when no city cleared the bar — the last of which carries the per-city
/// reasons so a manager can apply their own judgement.
pub fn plan_roster_run(opportunity: &RosterOpportunity) -> Result<RosterRun, RosterRefusal> {
    let available: Vec<&RosterAct> = opportunity
        .acts
        .iter()
        .filter(|act| act.is_available())
        .collect();
    if available.is_empty() {
        return Err(RosterRefusal::NoActIsAvailable);
    }
    if opportunity.packages_this_period == 0 {
        return Err(RosterRefusal::PeriodIsFull { cap: 0 });
    }

    let mut moves: Vec<RosterMove> = Vec::new();
    let mut refused: Vec<(String, CitySkipped)> = Vec::new();
    let mut committed_acts: Vec<String> = Vec::new();
    let mut committed_cities: Vec<crate::CityId> = Vec::new();

    // ── Support slots first ─────────────────────────────────────────────────
    //
    // The room is booked, the promoter is committed, the date is set. Nothing
    // else in this module is this cheap, so nothing else goes first — and a
    // planner that proposed a new night while a labelmate's slot sat open
    // would be spending the roster's week to buy what an email buys.
    for slot in &opportunity.open_slots {
        if moves.len() >= usize::from(opportunity.packages_this_period) {
            break;
        }
        if slot.days_until_show < MINIMUM_SLOT_LEAD_DAYS {
            continue;
        }
        let Some(headliner) = opportunity
            .acts
            .iter()
            .find(|act| act.name == slot.headliner)
        else {
            continue;
        };
        let candidates: Vec<&RosterAct> = available
            .iter()
            .copied()
            .filter(|act| !committed_acts.contains(&act.name))
            .filter(|act| act.name != slot.headliner)
            .collect();
        let Some((support, adds)) = choose_support(&candidates, headliner, slot.city_id) else {
            continue;
        };

        let mut reasons = vec![Reason::CoBillAddsAudience {
            act: support.name.clone(),
            adds_reachable: adds,
        }];
        reasons.push(Reason::ReachableAudience {
            // `adds > 0` from `choose_support` proves the reach was measured,
            // so the zero here is unreachable rather than a claim.
            reachable: support.reach_in(slot.city_id).unwrap_or(0),
        });
        moves.push(RosterMove::FillSupportSlot {
            city: slot.city.clone(),
            venue: slot.venue.clone(),
            headliner: slot.headliner.clone(),
            support: support.name.clone(),
            adds_reachable: adds,
            days_until_show: slot.days_until_show,
            reasons,
        });
        committed_acts.push(support.name.clone());
        committed_cities.push(slot.city_id);
    }

    // ── Then new nights ─────────────────────────────────────────────────────
    for city in &opportunity.cities {
        if moves.len() >= usize::from(opportunity.packages_this_period) {
            break;
        }
        if committed_cities.contains(&city.city_id) {
            // A roster running two of its own shows in one city in one period
            // is competing with itself for the same room.
            continue;
        }
        let candidates: Vec<&RosterAct> = available
            .iter()
            .copied()
            .filter(|act| !committed_acts.contains(&act.name))
            .collect();
        let Some((headliner, fairness_note)) = choose_headliner(&candidates, city.city_id) else {
            // Every act who could play here is already committed. Recorded
            // rather than skipped: a city that vanishes reads to a manager as
            // a city that was never considered, and they would answer it by
            // researching venues when the actual constraint is the cap.
            refused.push((city.city.clone(), CitySkipped::NoActFree));
            continue;
        };

        // The city's own evidence is judged by the band planner, so a roster
        // and a band can never disagree about whether a city is worth playing.
        // The headliner's reach is what the city is evaluated on, because the
        // headliner is who has to fill it.
        let mut as_seen_by_headliner = city.clone();
        as_seen_by_headliner.reachable_fans = headliner.reach_in(city.city_id);
        let plan = match plan_gig(&as_seen_by_headliner, headliner.intent) {
            Ok(plan) => plan,
            Err(refusal) => {
                refused.push((city.city.clone(), CitySkipped::Refused(refusal)));
                continue;
            }
        };

        let support = choose_support(&candidates, headliner, city.city_id);
        let combined = plan.reach.reachable + support.map_or(0, |(_, adds)| adds);
        let mut reasons = plan.reasons.clone();
        if let Some((act, adds)) = support {
            reasons.push(Reason::CoBillAddsAudience {
                act: act.name.clone(),
                adds_reachable: adds,
            });
        }

        moves.push(RosterMove::BookPackage {
            city: city.city.clone(),
            venue: plan.venue.clone(),
            headliner: headliner.name.clone(),
            support: support.map(|(act, _)| act.name.clone()),
            contact: plan.contact.clone(),
            combined_reachable: combined,
            reasons,
            caveats: plan.caveats.clone(),
            fairness_note,
        });
        committed_cities.push(city.city_id);
        committed_acts.push(headliner.name.clone());
        if let Some((act, _)) = support {
            committed_acts.push(act.name.clone());
        }
    }

    if moves.is_empty() {
        return Err(RosterRefusal::NothingClearedTheBar { per_city: refused });
    }

    // Everybody available who did not get a slot. Named rather than left for a
    // manager to notice in three months when the act asks why.
    let waiting = available
        .iter()
        .filter(|act| !committed_acts.contains(&act.name))
        .map(|act| act.name.clone())
        .collect();

    Ok(RosterRun {
        moves,
        waiting,
        considered_and_refused: refused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gig_plan::{PromoterRef, VenueEvidence};

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

    /// A stable id per fixture name — acts' `reach_by_city` rows pair with
    /// the city fixtures through it, the way production pairs on `city_id`.
    fn city_id(name: &str) -> crate::CityId {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        let hash = hasher.finish();
        crate::CityId::from_uuid(uuid::Uuid::from_u128(
            u128::from(hash) << 64 | u128::from(!hash),
        ))
    }

    fn city(name: &str) -> CityOpportunity {
        CityOpportunity {
            city_id: city_id(name),
            city: name.to_owned(),
            reachable_fans: Some(240),
            active_fans_30d: 60,
            months_since_show: Some(14),
            has_upcoming_show: false,
            venue: Some(venue()),
            promoters: vec![PromoterRef {
                key: "anna".to_owned(),
                name: "Anna".to_owned(),
                relationship_score: 70,
                answered_last_time: true,
                has_route: true,
            }],
            co_bill: Vec::new(),
        }
    }

    fn act(name: &str, reach: &[(&str, u32)], months: Option<u16>) -> RosterAct {
        RosterAct {
            name: name.to_owned(),
            intent: TenantIntent::BookingShows,
            months_since_last_show: months,
            reach_by_city: reach
                .iter()
                .map(|(city, reachable)| CityReach {
                    city: (*city).to_owned(),
                    city_id: city_id(city),
                    reachable: Some(*reachable),
                })
                .collect(),
            overlap_with: Vec::new(),
        }
    }

    /// An overlap measured in one city. Every fixture states the city, because
    /// the planner will not accept a share measured anywhere else.
    fn with_overlap(mut act: RosterAct, other: &str, city: &str, basis_points: u16) -> RosterAct {
        act.overlap_with.push(ActOverlap {
            other_act: other.to_owned(),
            city_id: city_id(city),
            overlap_basis_points: basis_points,
        });
        act
    }

    fn roster(acts: Vec<RosterAct>, cities: Vec<CityOpportunity>) -> RosterOpportunity {
        RosterOpportunity {
            acts,
            cities,
            open_slots: Vec::new(),
            packages_this_period: 4,
        }
    }

    // ── Who headlines ───────────────────────────────────────────────────────

    /// The rule a label will occasionally dislike, and it is still right: the
    /// headliner's job is to fill the room, and the room is in one city.
    #[test]
    fn the_act_with_the_local_draw_headlines_not_the_bigger_name() {
        let run = plan_roster_run(&roster(
            vec![
                // Bigger everywhere else, smaller here.
                act("Flagship", &[("Wrocław", 100), ("Praha", 900)], Some(1)),
                act("Local", &[("Wrocław", 240)], Some(1)),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &run.moves[0] {
            RosterMove::BookPackage { headliner, .. } => assert_eq!(headliner, "Local"),
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    /// Fairness inside the margin, and only inside it.
    #[test]
    fn a_starved_act_wins_a_close_call_and_says_so() {
        let run = plan_roster_run(&roster(
            vec![
                act("Busy", &[("Wrocław", 240)], Some(1)),
                // 10% behind, and has not played in a year.
                act("Starved", &[("Wrocław", 216)], Some(12)),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &run.moves[0] {
            RosterMove::BookPackage {
                headliner,
                fairness_note,
                ..
            } => {
                assert_eq!(headliner, "Starved");
                let note = fairness_note.as_ref().expect("fairness was not explained");
                assert!(note.contains("Overrule it"), "no way to disagree: {note}");
                assert!(note.contains("12 months"));
            }
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    /// A half-empty night sets a starved act back further than waiting would.
    #[test]
    fn fairness_never_overrides_a_real_gap_in_draw() {
        let run = plan_roster_run(&roster(
            vec![
                act("Busy", &[("Wrocław", 240)], Some(1)),
                // 50% behind — far outside the margin.
                act("Starved", &[("Wrocław", 120)], Some(24)),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &run.moves[0] {
            RosterMove::BookPackage {
                headliner,
                fairness_note,
                ..
            } => {
                assert_eq!(headliner, "Busy");
                assert!(fairness_note.is_none());
            }
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    // ── Who supports ────────────────────────────────────────────────────────

    #[test]
    fn a_labelmate_who_adds_people_supports_and_one_who_does_not_is_left_off() {
        let adds = plan_roster_run(&roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                with_overlap(
                    act("Adds", &[("Wrocław", 200)], Some(3)),
                    "Head",
                    "Wrocław",
                    2_000,
                ),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &adds.moves[0] {
            RosterMove::BookPackage {
                support,
                combined_reachable,
                ..
            } => {
                assert_eq!(support.as_deref(), Some("Adds"));
                // 240 + (200 - 20% of 200) = 240 + 160.
                assert_eq!(*combined_reachable, 400);
            }
            other => panic!("expected a booking, got {other:?}"),
        }

        let same_crowd = plan_roster_run(&roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                with_overlap(
                    act("Twin", &[("Wrocław", 200)], Some(3)),
                    "Head",
                    "Wrocław",
                    9_000,
                ),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &same_crowd.moves[0] {
            RosterMove::BookPackage { support, .. } => assert!(support.is_none()),
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    /// Unmeasured is not zero. Pairing two acts on the assumption their
    /// audiences are separate is the mistake the overlap number exists to
    /// prevent, so an unmeasured pair is not proposed.
    #[test]
    fn an_unmeasured_pair_is_not_assumed_to_be_two_audiences() {
        let run = plan_roster_run(&roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                act("Unmeasured", &[("Wrocław", 200)], Some(3)),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &run.moves[0] {
            RosterMove::BookPackage { support, .. } => assert!(support.is_none()),
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    /// An overlap measured in another city is not an answer here.
    ///
    /// The failure this prevents: two acts share little nationally and almost
    /// everything in the one city where both are local. Reading the national
    /// share as the local one puts both on the poster, sells the same ticket
    /// twice and splits the door — exactly what the ceiling exists to stop.
    #[test]
    fn an_overlap_measured_elsewhere_does_not_decide_this_city() {
        let elsewhere = plan_roster_run(&roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                with_overlap(
                    act("Local", &[("Wrocław", 200)], Some(3)),
                    "Head",
                    // Measured in Praha, proposed in Wrocław.
                    "Praha",
                    1_000,
                ),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &elsewhere.moves[0] {
            RosterMove::BookPackage { support, .. } => assert!(
                support.is_none(),
                "a share measured in another city was used to pair a bill here"
            ),
            other => panic!("expected a booking, got {other:?}"),
        }

        // The same pair, measured where the show is, pairs.
        let here = plan_roster_run(&roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                with_overlap(
                    act("Local", &[("Wrocław", 200)], Some(3)),
                    "Head",
                    "Wrocław",
                    1_000,
                ),
            ],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        match &here.moves[0] {
            RosterMove::BookPackage { support, .. } => {
                assert_eq!(support.as_deref(), Some("Local"));
            }
            other => panic!("expected a booking, got {other:?}"),
        }
    }

    // ── The cheapest move ───────────────────────────────────────────────────

    /// The Pareto move for a roster, and the thing a band alone cannot do.
    #[test]
    fn an_open_support_slot_outranks_booking_a_new_night() {
        let mut opportunity = roster(
            vec![
                act("Head", &[("Wrocław", 240), ("Praha", 300)], Some(1)),
                with_overlap(
                    act("Mate", &[("Wrocław", 200), ("Praha", 50)], Some(8)),
                    "Head",
                    "Wrocław",
                    1_000,
                ),
            ],
            vec![city("Praha")],
        );
        opportunity.open_slots = vec![OpenSupportSlot {
            city: "Wrocław".to_owned(),
            city_id: city_id("Wrocław"),
            venue: "Klub X".to_owned(),
            headliner: "Head".to_owned(),
            days_until_show: 30,
        }];
        let run = plan_roster_run(&opportunity).expect("plans");
        assert!(
            matches!(run.moves.first(), Some(RosterMove::FillSupportSlot { .. })),
            "a new booking was proposed ahead of a slot that was already paid for: {:?}",
            run.moves
        );
    }

    /// A slot four days out is the promoter's call, not a suggestion.
    #[test]
    fn a_slot_too_close_to_the_show_is_not_proposed() {
        let mut opportunity = roster(
            vec![
                act("Head", &[("Wrocław", 240)], Some(1)),
                with_overlap(
                    act("Mate", &[("Wrocław", 200)], Some(8)),
                    "Head",
                    "Wrocław",
                    1_000,
                ),
            ],
            Vec::new(),
        );
        opportunity.open_slots = vec![OpenSupportSlot {
            city: "Wrocław".to_owned(),
            city_id: city_id("Wrocław"),
            venue: "Klub X".to_owned(),
            headliner: "Head".to_owned(),
            days_until_show: MINIMUM_SLOT_LEAD_DAYS - 1,
        }];
        assert!(matches!(
            plan_roster_run(&opportunity),
            Err(RosterRefusal::NothingClearedTheBar { .. })
        ));

        opportunity.open_slots[0].days_until_show = MINIMUM_SLOT_LEAD_DAYS;
        assert!(
            plan_roster_run(&opportunity).is_ok(),
            "the bound is a bound"
        );
    }

    // ── Caps and fairness of attention ──────────────────────────────────────

    /// Caps outvote ambition. A plan nobody can staff teaches the manager to
    /// stop reading the plan.
    #[test]
    fn the_period_cap_bounds_the_plan_and_names_who_is_waiting() {
        let mut opportunity = roster(
            vec![
                act("A", &[("Wrocław", 240)], Some(1)),
                act("B", &[("Praha", 240)], Some(1)),
                act("C", &[("Brno", 240)], Some(9)),
            ],
            vec![city("Wrocław"), city("Praha"), city("Brno")],
        );
        opportunity.packages_this_period = 2;
        let run = plan_roster_run(&opportunity).expect("plans");
        assert_eq!(run.moves.len(), 2);
        assert_eq!(run.waiting.len(), 1, "somebody waiting was not named");
    }

    #[test]
    fn a_full_period_refuses_rather_than_listing() {
        let mut opportunity = roster(
            vec![act("A", &[("Wrocław", 240)], Some(1))],
            vec![city("Wrocław")],
        );
        opportunity.packages_this_period = 0;
        assert_eq!(
            plan_roster_run(&opportunity),
            Err(RosterRefusal::PeriodIsFull { cap: 0 })
        );
    }

    /// One act cannot be in two places, and a roster running two of its own
    /// shows in one city competes with itself.
    #[test]
    fn no_act_and_no_city_appears_twice() {
        let run = plan_roster_run(&roster(
            vec![
                act("A", &[("Wrocław", 240), ("Praha", 240)], Some(1)),
                act("B", &[("Wrocław", 230), ("Praha", 230)], Some(1)),
            ],
            vec![city("Wrocław"), city("Praha")],
        ))
        .expect("plans");
        let mut acts: Vec<&str> = run.moves.iter().flat_map(RosterMove::acts).collect();
        let before = acts.len();
        acts.sort_unstable();
        acts.dedup();
        assert_eq!(acts.len(), before, "an act was booked twice");

        let mut cities: Vec<&str> = run.moves.iter().map(RosterMove::city).collect();
        let before = cities.len();
        cities.sort_unstable();
        cities.dedup();
        assert_eq!(cities.len(), before, "a city was booked twice");
    }

    // ── The act's own word ──────────────────────────────────────────────────

    /// An act that said it is recording is not proposed shows, whatever the
    /// roster would prefer. The roster does not get to overrule the act.
    #[test]
    fn an_act_that_is_recording_is_never_proposed_by_its_label() {
        let mut recording = act("Recording", &[("Wrocław", 900)], Some(1));
        recording.intent = TenantIntent::HeadsDown;
        let run = plan_roster_run(&roster(
            vec![recording, act("Available", &[("Wrocław", 100)], Some(1))],
            vec![city("Wrocław")],
        ))
        .expect("plans");
        for move_ in &run.moves {
            assert!(
                !move_.acts().contains(&"Recording"),
                "the label overruled the act's own plan"
            );
        }
    }

    #[test]
    fn a_roster_where_nobody_is_playing_refuses_plainly() {
        let mut quiet = act("Quiet", &[("Wrocław", 240)], Some(1));
        quiet.intent = TenantIntent::HeadsDown;
        assert_eq!(
            plan_roster_run(&roster(vec![quiet], vec![city("Wrocław")])),
            Err(RosterRefusal::NoActIsAvailable)
        );
    }

    // ── Honesty about what was skipped ──────────────────────────────────────

    #[test]
    fn cities_that_were_refused_travel_with_their_reasons() {
        let mut booked = city("Praha");
        booked.has_upcoming_show = true;
        let run = plan_roster_run(&roster(
            // Two acts, so Praha is reached with somebody free to play it and
            // is refused on its own evidence rather than on the cap. With one
            // act this asserted a path it never took.
            vec![
                act("A", &[("Wrocław", 240), ("Praha", 240)], Some(1)),
                act("B", &[("Wrocław", 100), ("Praha", 240)], Some(1)),
            ],
            vec![city("Wrocław"), booked],
        ))
        .expect("plans");
        assert!(
            run.considered_and_refused
                .iter()
                .any(|(city, skipped)| city == "Praha"
                    && matches!(skipped, CitySkipped::Refused(GigRefusal::AlreadyBooked))),
            "a refused city vanished instead of being reported: {:?}",
            run.considered_and_refused
        );
    }

    /// The skip a manager would otherwise answer with the wrong fix.
    #[test]
    fn a_city_skipped_because_everybody_is_busy_says_so() {
        let run = plan_roster_run(&roster(
            vec![act("Only", &[("Wrocław", 240), ("Praha", 240)], Some(1))],
            vec![city("Wrocław"), city("Praha")],
        ))
        .expect("plans");
        assert_eq!(run.moves.len(), 1);
        let (city, skipped) = run
            .considered_and_refused
            .iter()
            .find(|(city, _)| city == "Praha")
            .expect("the second city vanished");
        assert_eq!(city, "Praha");
        assert_eq!(*skipped, CitySkipped::NoActFree);
        assert!(
            skipped
                .message()
                .contains("already \nbooked somewhere else")
                || skipped.message().contains("booked somewhere else")
        );
    }

    #[test]
    fn every_refusal_reads_as_a_sentence() {
        for refusal in [
            RosterRefusal::NoActIsAvailable,
            RosterRefusal::PeriodIsFull { cap: 3 },
            RosterRefusal::NothingClearedTheBar {
                per_city: vec![(
                    "Wrocław".to_owned(),
                    CitySkipped::Refused(GigRefusal::AlreadyBooked),
                )],
            },
        ] {
            let message = refusal.message();
            assert!(message.len() > 40, "too terse: {message}");
            assert!(!message.contains("Err(") && !message.contains("None"));
        }
    }

    /// The same roster must always produce the same plan, or a manager
    /// re-reading last week's run sees a different one and re-litigates it.
    #[test]
    fn the_same_roster_always_produces_the_same_plan() {
        let opportunity = roster(
            vec![
                act("Zed", &[("Wrocław", 240)], Some(1)),
                act("Ada", &[("Wrocław", 240)], Some(1)),
            ],
            vec![city("Wrocław")],
        );
        assert_eq!(plan_roster_run(&opportunity), plan_roster_run(&opportunity));
    }
}
