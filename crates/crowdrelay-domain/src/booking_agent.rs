//! The booking agent — the booking graph's third entity (§4h-10, §12-5).
//!
//! A venue sells the band a room, a promoter sells it a night, and an agent
//! sells the band: the ask is representation for a season, not a slot on one
//! date. That inversion is why this gate is the strictest on the outward
//! surface — the agent's decision is made on draw evidence, so the proof *is*
//! the pitch, and an approach with thin or absent numbers is refused rather
//! than sent. A weak first approach is worse than none: agents talk to each
//! other, and a refusal closes the door for a season.
//!
//! Three gates, in the order they are checked:
//!
//! **Standing.** `do_not_contact` is the hardest line in the system and an
//! inactive agent is not a prospect. The route must be verified — a human
//! confirmed the address when the screened intake promoted the contact, and a
//! reply re-confirms it.
//!
//! **Season.** One approach per agent per `APPROACH_SEASON_DAYS` days. A
//! decline stamps `refused_until` and binds harder than the ordinary season
//! wait: a sent approach is a spent approach, but an answered no is a closed
//! door, so the refusal is checked first and carries its own sentence.
//!
//! **Evidence.** The numbers the agent decides on — shows actually played,
//! paid tickets behind them, and whether the buyers are a crowd rather than
//! one bulk order. Every required reading must exist (a metric that could not
//! be read is `None`, never zero) and clear a deliberately modest floor: the
//! gate's job is to refuse *no* evidence, not to rank good evidence — the
//! pitch carries the real figures either way.

use serde::{Deserialize, Serialize};
use time::{Date, Duration, OffsetDateTime};

/// The season an approach spends: one letter per agent per hundred and
/// twenty days, and a decline closes the door for the same length.
///
/// `refused_until` is a `date` — a refusal is filed by the day it landed —
/// while `approached_at` is the timestamp of the send. The season is the
/// same length either way; the two columns exist because "we wrote" and
/// "they answered no" are different facts.
pub const APPROACH_SEASON_DAYS: i64 = 120;

/// The evidence floor, named so review can argue the numbers instead of
/// digging them out of a predicate. Deliberately a modest club draw — two
/// played shows, fifty paid tickets, twenty distinct buyers — because the
/// gate exists to refuse the empty pitch, not to decide what a good one
/// looks like.
pub const MIN_SHOWS_PLAYED_12M: i64 = 2;
pub const MIN_PAID_TICKETS_12M: i64 = 50;
pub const MIN_DISTINCT_BUYERS_12M: i64 = 20;

/// The first-party draw readings the pitch is made of. Every field is
/// optional for the same reason `EvidencePacket`'s are: a number the
/// workspace cannot answer stays `None`, because a zero invented for it
/// reads exactly like a zero it measured — and here the difference decides
/// whether a door opens or closes.
///
/// Serialized into the action payload at request time: the approval screen
/// and the send read the same snapshot, so the letter never argues from
/// numbers the approver did not see.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentDrawEvidence {
    /// Shows actually played — published or completed, started — in the
    /// last twelve months.
    pub shows_played_12m: Option<i64>,
    /// Paid ticket orders to those shows. Real money from real people —
    /// the number an agent has no reason to discount.
    pub paid_tickets_12m: Option<i64>,
    /// Distinct buyers behind the paid orders: a crowd, not one bulk
    /// purchase padded out.
    pub distinct_buyers_12m: Option<i64>,
    /// Buyers with two or more paid orders in the window — whether the
    /// room's crowd comes back.
    pub repeat_buyers_12m: Option<i64>,
    /// Distinct cities the played shows were in — where the reach lives.
    pub cities_reached_12m: Option<i64>,
    /// The best-attended single show's paid tickets — the headline a
    /// promoter-grade reader reaches for first.
    pub best_show_paid_tickets_12m: Option<i64>,
    /// When these were read. A number without one is a number from any
    /// time.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub as_of: Option<OffsetDateTime>,
}

/// The state an agent approach is decided on. Gathered by the caller so the
/// function is pure — the same check runs at request time and again inside
/// the dispatch transaction, where the flags may have moved. `evidence` is a
/// value rather than a borrow: the snapshot is `Copy`, and the gate owning
/// it keeps the request free of a lifetime the caller cannot hold across a
/// transaction boundary.
#[derive(Clone, Copy, Debug)]
pub struct AgentApproachRequest {
    pub active: bool,
    pub do_not_contact: bool,
    /// Whether the route was ever confirmed: set at promotion (a human
    /// confirmed the screened contact's address) and refreshed by every
    /// inbound reply. A row that arrived any other way never had its route
    /// confirmed, and the gate refuses it.
    pub route_verified: bool,
    /// When the last approach went out, if one did.
    pub approached_at: Option<OffsetDateTime>,
    /// The day a decline's season ends, if one is on file.
    pub refused_until: Option<Date>,
    /// An approach already queued but unsent. A second ask before the first
    /// was answered is the blast failure the season exists to prevent.
    pub approach_pending: bool,
    pub evidence: AgentDrawEvidence,
    pub now: OffsetDateTime,
}

/// Why an approach was refused. Carries the band-facing sentence — the
/// caller shows it rather than translating a code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentApproachRefusal {
    /// The agent said never. Nothing overrides it.
    DoNotContact,
    /// The record is archived or switched off.
    Inactive,
    /// The route was never confirmed — nobody has shown this address
    /// reaches the agent.
    RouteUnverified,
    /// They declined inside the season — the door is closed until the day
    /// the refusal set.
    SeasonalRefusal { until: Date },
    /// We already wrote inside the season — the next approach is not a new
    /// letter, it is a nagging one.
    SeasonWait { next_at: OffsetDateTime },
    /// A letter is already queued. Approving a second one before the first
    /// sent is how a considered pitch becomes a blast.
    ApproachPending,
    /// The draw readings a pitch needs do not exist or do not clear the
    /// floor. `field` names what fell short so the sentence can say it.
    InsufficientDraw { field: &'static str },
}

impl AgentApproachRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::DoNotContact => {
                "this agent asked not to be contacted — that flag binds, whatever the \
                 season would allow"
                    .to_owned()
            }
            Self::Inactive => "this agent is inactive".to_owned(),
            Self::RouteUnverified => {
                "this agent's address was never confirmed — the route needs a human \
                 to vouch for it before the platform mails it"
                    .to_owned()
            }
            Self::SeasonalRefusal { until } => format!(
                "they declined — a refusal closes the door for {APPROACH_SEASON_DAYS} \
                 days, so this door opens again on {until}"
            ),
            Self::SeasonWait { next_at } => format!(
                "one approach a season is the rule — the letter that went out spends \
                 it until {}, and a nagging second ask closes the door it knocks on",
                next_at.date()
            ),
            Self::ApproachPending => {
                "an approach to this agent is already waiting on an approve".to_owned()
            }
            Self::InsufficientDraw { field } => format!(
                "the approach is the evidence, and the {field} evidence is not there — \
                 an agent decides on real numbers, and a thin pitch closes the door \
                 it knocks on"
            ),
        }
    }
}

/// The day a decline's season ends: the refusal's own date plus the season,
/// never `now`'s — a reply recorded late closes the door from when they
/// answered, not from when we filed it.
#[must_use]
pub fn refusal_until(occurred_at: OffsetDateTime) -> Date {
    occurred_at.date() + Duration::days(APPROACH_SEASON_DAYS)
}

/// Decides whether an approach may send.
///
/// The order is the order of the gates: standing before season before
/// evidence. An agent on `do_not_contact` never hears the other reasons —
/// the refusal it gets back is the one that matters.
///
/// # Errors
///
/// The first refusal in gate order.
pub fn review_agent_approach(request: &AgentApproachRequest) -> Result<(), AgentApproachRefusal> {
    if request.do_not_contact {
        return Err(AgentApproachRefusal::DoNotContact);
    }
    if !request.active {
        return Err(AgentApproachRefusal::Inactive);
    }
    if !request.route_verified {
        return Err(AgentApproachRefusal::RouteUnverified);
    }
    let today = request.now.date();
    if let Some(until) = request.refused_until
        && until >= today
    {
        return Err(AgentApproachRefusal::SeasonalRefusal { until });
    }
    if let Some(approached) = request.approached_at {
        let next_at = approached + Duration::days(APPROACH_SEASON_DAYS);
        if next_at > request.now {
            return Err(AgentApproachRefusal::SeasonWait { next_at });
        }
    }
    if request.approach_pending {
        return Err(AgentApproachRefusal::ApproachPending);
    }
    let evidence = request.evidence;
    // Presence first, then the floor. A reading that is absent fails before
    // one that merely came back low — "we could not see it" is a different
    // sentence from "it is not enough", and the missing one is the worse
    // claim to send.
    let shows = evidence
        .shows_played_12m
        .ok_or(AgentApproachRefusal::InsufficientDraw {
            field: "shows played",
        })?;
    let paid = evidence
        .paid_tickets_12m
        .ok_or(AgentApproachRefusal::InsufficientDraw {
            field: "paid ticket",
        })?;
    let buyers = evidence
        .distinct_buyers_12m
        .ok_or(AgentApproachRefusal::InsufficientDraw {
            field: "distinct buyer",
        })?;
    if shows < MIN_SHOWS_PLAYED_12M {
        return Err(AgentApproachRefusal::InsufficientDraw {
            field: "shows played",
        });
    }
    if paid < MIN_PAID_TICKETS_12M {
        return Err(AgentApproachRefusal::InsufficientDraw {
            field: "paid ticket",
        });
    }
    if buyers < MIN_DISTINCT_BUYERS_12M {
        return Err(AgentApproachRefusal::InsufficientDraw {
            field: "distinct buyer",
        });
    }
    Ok(())
}

/// What the agent said back. Kept deliberately small — the disposition is a
/// fact the operator files, not a reading the system infers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingAgentReplyDisposition {
    /// A reply exists but needed no classification.
    Received,
    /// They want to talk.
    Positive,
    /// They took the act on — the outcome the whole feature exists for.
    Signed,
    /// No for this season — stamps `refused_until`.
    Declined,
    /// Never again — stamps `do_not_contact` and the governor row so every
    /// other contact path honours it too.
    DoNotContact,
}

impl BookingAgentReplyDisposition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Positive => "positive",
            Self::Signed => "signed",
            Self::Declined => "declined",
            Self::DoNotContact => "do_not_contact",
        }
    }

    /// Parse the stored representation written by [`Self::as_str`].
    /// `none` — the interactions table's default — is stored but is not a
    /// disposition a caller may file.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "received" => Some(Self::Received),
            "positive" => Some(Self::Positive),
            "signed" => Some(Self::Signed),
            "declined" => Some(Self::Declined),
            "do_not_contact" => Some(Self::DoNotContact),
            _ => None,
        }
    }
}

/// Per-workspace tuning for the `booking_agent` context. Empty on purpose,
/// the same way `RepresentationPolicy` is: the season lives in
/// `APPROACH_SEASON_DAYS`, the floor in the `MIN_*` constants and the gate in
/// `review_agent_approach` — the policy row carries posture
/// (`require_approval`), not knobs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BookingAgentPolicy {}

#[cfg(test)]
mod tests {
    use super::*;

    // The `time` crate's macros feature is off in this crate, so a fixed
    // instant is built from the epoch — the tests only need a stable now.
    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH.saturating_add(Duration::days(20_700));

    fn evidence() -> AgentDrawEvidence {
        AgentDrawEvidence {
            shows_played_12m: Some(4),
            paid_tickets_12m: Some(140),
            distinct_buyers_12m: Some(96),
            repeat_buyers_12m: Some(21),
            cities_reached_12m: Some(3),
            best_show_paid_tickets_12m: Some(58),
            as_of: Some(NOW),
        }
    }

    fn permitted(evidence: AgentDrawEvidence) -> AgentApproachRequest {
        AgentApproachRequest {
            active: true,
            do_not_contact: false,
            route_verified: true,
            approached_at: None,
            refused_until: None,
            approach_pending: false,
            evidence,
            now: NOW,
        }
    }

    #[test]
    fn a_verified_agent_with_real_numbers_passes() {
        assert_eq!(review_agent_approach(&permitted(evidence())), Ok(()));
    }

    #[test]
    fn do_not_contact_wins_over_everything() {
        let mut request = permitted(evidence());
        request.do_not_contact = true;
        request.refused_until = Some(NOW.date() + Duration::days(30));
        assert_eq!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::DoNotContact)
        );
    }

    #[test]
    fn an_unverified_route_refuses_before_the_season_is_consulted() {
        let mut request = permitted(evidence());
        request.route_verified = false;
        request.refused_until = Some(NOW.date() + Duration::days(30));
        assert_eq!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::RouteUnverified)
        );
    }

    #[test]
    fn a_decline_inside_the_season_refuses_with_its_day() {
        let mut request = permitted(evidence());
        request.refused_until = Some(NOW.date());
        assert_eq!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::SeasonalRefusal { until: NOW.date() })
        );
    }

    #[test]
    fn a_refusal_yesterday_is_history_not_a_wall() {
        let mut request = permitted(evidence());
        request.refused_until = Some(NOW.date() - Duration::days(1));
        assert_eq!(review_agent_approach(&request), Ok(()));
    }

    #[test]
    fn the_season_wait_is_120_days_from_the_send() {
        let mut request = permitted(evidence());
        request.approached_at = Some(NOW - Duration::days(APPROACH_SEASON_DAYS - 1));
        assert!(matches!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::SeasonWait { .. })
        ));
        request.approached_at = Some(NOW - Duration::days(APPROACH_SEASON_DAYS));
        assert_eq!(
            review_agent_approach(&request),
            Ok(()),
            "the season ends on its own day"
        );
    }

    #[test]
    fn the_refusal_outranks_an_old_send() {
        let mut request = permitted(evidence());
        request.approached_at = Some(NOW - Duration::days(140));
        request.refused_until = Some(NOW.date() + Duration::days(10));
        assert!(matches!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::SeasonalRefusal { .. })
        ));
    }

    #[test]
    fn a_queued_letter_refuses_a_second() {
        let mut request = permitted(evidence());
        request.approach_pending = true;
        assert_eq!(
            review_agent_approach(&request),
            Err(AgentApproachRefusal::ApproachPending)
        );
    }

    #[test]
    fn every_missing_reading_refuses_as_absent_not_zero() {
        for name in ["shows played", "paid ticket", "distinct buyer"] {
            let mut evidence = evidence();
            match name {
                "shows played" => evidence.shows_played_12m = None,
                "paid ticket" => evidence.paid_tickets_12m = None,
                _ => evidence.distinct_buyers_12m = None,
            }
            assert_eq!(
                review_agent_approach(&permitted(evidence)),
                Err(AgentApproachRefusal::InsufficientDraw { field: name }),
                "a missing {name} reading must refuse"
            );
        }
    }

    #[test]
    fn the_floor_is_exactly_the_floor() {
        for (field, value) in [
            ("shows played", MIN_SHOWS_PLAYED_12M),
            ("paid ticket", MIN_PAID_TICKETS_12M),
            ("distinct buyer", MIN_DISTINCT_BUYERS_12M),
        ] {
            let mut evidence = evidence();
            match field {
                "shows played" => evidence.shows_played_12m = Some(value),
                "paid ticket" => evidence.paid_tickets_12m = Some(value),
                _ => evidence.distinct_buyers_12m = Some(value),
            }
            assert_eq!(
                review_agent_approach(&permitted(evidence)),
                Ok(()),
                "{field} at the floor passes"
            );
            match field {
                "shows played" => evidence.shows_played_12m = Some(value - 1),
                "paid ticket" => evidence.paid_tickets_12m = Some(value - 1),
                _ => evidence.distinct_buyers_12m = Some(value - 1),
            }
            assert_eq!(
                review_agent_approach(&permitted(evidence)),
                Err(AgentApproachRefusal::InsufficientDraw { field }),
                "{field} under the floor refuses"
            );
        }
    }

    #[test]
    fn a_measured_zero_refuses_like_a_missing_one() {
        let mut evidence = evidence();
        evidence.paid_tickets_12m = Some(0);
        assert_eq!(
            review_agent_approach(&permitted(evidence)),
            Err(AgentApproachRefusal::InsufficientDraw {
                field: "paid ticket"
            })
        );
    }

    #[test]
    fn refusal_until_counts_from_the_answer_not_the_filing() {
        let answered = OffsetDateTime::UNIX_EPOCH.saturating_add(Duration::days(20_650));
        assert_eq!(
            refusal_until(answered),
            answered.date() + Duration::days(APPROACH_SEASON_DAYS)
        );
    }

    #[test]
    fn reply_dispositions_round_trip() {
        for disposition in [
            BookingAgentReplyDisposition::Received,
            BookingAgentReplyDisposition::Positive,
            BookingAgentReplyDisposition::Signed,
            BookingAgentReplyDisposition::Declined,
            BookingAgentReplyDisposition::DoNotContact,
        ] {
            assert_eq!(
                BookingAgentReplyDisposition::parse(disposition.as_str()),
                Some(disposition)
            );
        }
        assert_eq!(BookingAgentReplyDisposition::parse("none"), None);
        assert_eq!(BookingAgentReplyDisposition::parse("booked"), None);
    }
}
