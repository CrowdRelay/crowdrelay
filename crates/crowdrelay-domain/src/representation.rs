//! Who a band may approach about representation, and on what terms.
//!
//! The listing (`crate::listing`) is what the band publishes; this is the
//! other half of §4h-12 — the approach, which is where the platform stops
//! being a profile host and becomes the sender. Because the agent's address
//! stays hidden and the mail leaves our infrastructure, the quality bar is
//! ours: an approach with no evidence behind it, or one to a contact who
//! never opted in, is refused rather than sent.
//!
//! Three gates, in order of how fundamental they are:
//!
//! **Consent.** `accepts_outreach` and `do_not_contact` are not decoration.
//! An agent who asked to hear from bands is a product; an agent whose
//! address gets mailed by strangers is spam that burns the contact asset.
//! The flag only holds when a basis was stated — "we met at the showcase"
//! is a basis, a bare tick is not.
//!
//! **Evidence.** The approach carries the published listing — that *is* the
//! pitch. No published listing means nothing to send, so the approach is
//! refused rather than going out empty.
//!
//! **Scarcity.** A band that can mail fifty agents will, and fifty thin
//! pitches is how a scene closes. The allowance is small on purpose: each
//! approach should be a decision, not a blast.

/// Approaches a band may send per calendar month.
///
/// Small deliberately — the same Pareto argument as the outreach ceilings:
/// the fourth approach a band writes is more considered than the fortieth.
/// A calendar month keeps the reset legible ("you get more on the first")
/// rather than making the band reason about a rolling window.
pub const MONTHLY_APPROACH_ALLOWANCE: u32 = 4;

/// One approach per agent per season (§4h-10).
///
/// A room gets a pitch per show and a promoter per night, but an agent is
/// asked to carry the band's year — a second knock inside the season is not
/// persistence, it is the sound of a band that does not know what an
/// application is. `viryaos_booking_agents.approached_at` and the `approach`
/// ledger both count: whichever says we knocked inside the window, the door
/// stays shut. A refusal keeps it shut until `refused_until` passes.
pub const AGENT_APPROACH_SEASON_DAYS: i64 = 120;

/// The draw numbers an agent pitch stands on.
///
/// The band's own confirmed-show ledger — paid buyers, interested fans, the
/// shows behind them, and the city they draw hardest in. This is the
/// application, not an attachment: the approach mail cites these numbers
/// verbatim, so an approach without them has no pitch to send.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
pub struct DrawEvidence {
    /// Distinct paid buyers across the workspace's published and completed
    /// shows — the counter's own ticket ledger.
    pub paid_buyers: i64,
    /// Fans who registered interest in the band's shows.
    pub interested_fans: i64,
    /// The published and completed shows the numbers came from.
    pub confirmed_shows: i64,
    /// The city the band draws hardest in, where one is known.
    pub top_city: Option<String>,
}

impl DrawEvidence {
    /// Whether the numbers are worth an agent's reading. The bar is honesty,
    /// not size: real paid buyers and real interested fans, both above zero.
    #[must_use]
    pub const fn pitch_worthy(&self) -> bool {
        self.paid_buyers > 0 && self.interested_fans > 0
    }
}

/// The extra gates an agent approach answers, gathered by the caller.
///
/// `None` on the request means the target is a label — a label listens to a
/// scene, an agent bets on numbers, so the season door and the draw ledger
/// are agent-specific. When the target is an agent the caller owes all
/// three facts; the gate does not fetch its own evidence.
#[derive(Clone, Debug)]
pub struct AgentGate {
    /// `viryaos_booking_agents.refused_until` covers today — the agent's own
    /// answer closed the door until then.
    pub door_closed: bool,
    /// The registry's `approached_at`, or an `approach` interaction on this
    /// target, fell inside the season window — the season is spent either
    /// way.
    pub approached_this_season: bool,
    /// The workspace's measured draw — zeros when there are no shows on
    /// the books, which is exactly as pitch-worthy as it sounds.
    pub draw: DrawEvidence,
}

/// Per-workspace tuning for the representation context. Empty on purpose:
/// the allowance lives in `MONTHLY_APPROACH_ALLOWANCE` and the gates in
/// `review_approach`, so there is nothing here to turn — the policy row
/// carries posture (`require_approval`), not knobs. The type exists so a
/// `representation` policy row parses like every other context's.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepresentationPolicy {}

/// The state an approach is decided on. Everything the gate needs, gathered
/// by the caller — the function itself is pure so the rule is testable and
/// the same check runs at request time and again at dispatch.
#[derive(Clone, Debug)]
pub struct ApproachRequest {
    /// The contact's own consent flag. For a representation contact it is
    /// only honest when `has_acceptance_basis` is also true.
    pub accepts_outreach: bool,
    /// Whether a basis was stated for the consent — the schema enforces it
    /// for representation kinds; the gate re-asks so a stale row cannot
    /// slip through a gap between the two.
    pub has_acceptance_basis: bool,
    pub do_not_contact: bool,
    pub active: bool,
    pub verified: bool,
    /// Whether the band's listing is currently published — the approach
    /// sends the listing, so without one there is nothing to send.
    pub listing_published: bool,
    /// The agent-specific gates — `Some` when the target is an agent,
    /// `None` for a label. See [`AgentGate`].
    pub agent_gate: Option<AgentGate>,
    /// Approaches already sent this calendar month.
    pub approaches_used_this_month: u32,
    /// The cap — `MONTHLY_APPROACH_ALLOWANCE` today, a parameter so a
    /// per-workspace override can arrive without rewriting the gate.
    pub allowance: u32,
}

/// Why an approach was refused. Carries a band-facing sentence, because the
/// caller's job is to show it rather than translate a code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApproachRefusal {
    /// The contact said never. The hardest line in the system — nothing
    /// overrides it.
    DoNotContact,
    /// `accepts_outreach` is false — nobody has stated why this contact
    /// accepts approaches, so none may send.
    NotAccepted,
    /// The flag is true but the basis is missing — consent asserted with no
    /// reason is consent not established.
    AcceptanceWithoutBasis,
    /// The contact route was never confirmed real.
    NotVerified,
    /// The contact is archived or switched off.
    Inactive,
    /// The agent declined before and `refused_until` has not passed — the
    /// door is the answer, and it does not get argued with inside the season.
    AgentDoorClosed,
    /// One approach per agent per season — the registry or the ledger says
    /// we already knocked inside the window.
    AgentApproachedThisSeason,
    /// An agent decides on draw. With no paid buyers and no interested fans
    /// on record the pitch would be a claim, not evidence — so it does not
    /// send.
    InsufficientDrawEvidence,
    /// No published listing. The listing is the pitch's evidence; refusing
    /// is cheaper than sending an agent an empty profile.
    ListingNotPublished,
    /// This month's approaches are spent.
    AllowanceExhausted { allowance: u32 },
}

impl ApproachRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::DoNotContact => {
                "this contact asked not to be contacted — that flag binds, whatever the \
                 basis for reaching them was"
                    .to_owned()
            }
            Self::NotAccepted => {
                "this contact has not accepted approaches — say why they would hear from \
                 you before the platform mails them"
                    .to_owned()
            }
            Self::AcceptanceWithoutBasis => {
                "the opt-in has no stated basis — write down why this contact accepts \
                 approaches, or do not send"
                    .to_owned()
            }
            Self::NotVerified => {
                "this contact's address was never confirmed — verify the route before \
                 an approach"
                    .to_owned()
            }
            Self::Inactive => "this contact is inactive".to_owned(),
            Self::AgentDoorClosed => "this agent declined and the season's door is still closed — \
                 a refusal is the answer, not a reason to knock again"
                .to_owned(),
            Self::AgentApproachedThisSeason => {
                "this agent was approached this season — one application per \
                 season is what keeps it an application"
                    .to_owned()
            }
            Self::InsufficientDrawEvidence => {
                "the pitch is the numbers — no paid tickets or interested \
                 fans on record means there is nothing to cite yet"
                    .to_owned()
            }
            Self::ListingNotPublished => {
                "the listing is the pitch — publish it before approaching, or there is \
                 nothing to send"
                    .to_owned()
            }
            Self::AllowanceExhausted { allowance } => format!(
                "this month's approaches are spent — {allowance} a month is what keeps \
                 each one a considered letter rather than a blast"
            ),
        }
    }
}

/// Decides whether an approach may send.
///
/// The order is the order of the gates: consent before evidence before
/// scarcity. A contact on `do_not_contact` never hears the other reasons —
/// the refusal it gets back is the one that matters.
///
/// # Errors
///
/// The first refusal in gate order.
pub fn review_approach(request: &ApproachRequest) -> Result<(), ApproachRefusal> {
    if request.do_not_contact {
        return Err(ApproachRefusal::DoNotContact);
    }
    if !request.accepts_outreach {
        return Err(ApproachRefusal::NotAccepted);
    }
    if !request.has_acceptance_basis {
        return Err(ApproachRefusal::AcceptanceWithoutBasis);
    }
    if !request.verified {
        return Err(ApproachRefusal::NotVerified);
    }
    if !request.active {
        return Err(ApproachRefusal::Inactive);
    }
    // The season door answers before the pitch is even looked at: an agent
    // who already declined, or who already heard from us this season, never
    // reaches the evidence question.
    if let Some(agent) = &request.agent_gate {
        if agent.door_closed {
            return Err(ApproachRefusal::AgentDoorClosed);
        }
        if agent.approached_this_season {
            return Err(ApproachRefusal::AgentApproachedThisSeason);
        }
    }
    if !request.listing_published {
        return Err(ApproachRefusal::ListingNotPublished);
    }
    if let Some(agent) = &request.agent_gate
        && !agent.draw.pitch_worthy()
    {
        return Err(ApproachRefusal::InsufficientDrawEvidence);
    }
    if request.approaches_used_this_month >= request.allowance {
        return Err(ApproachRefusal::AllowanceExhausted {
            allowance: request.allowance,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permitted() -> ApproachRequest {
        ApproachRequest {
            accepts_outreach: true,
            has_acceptance_basis: true,
            do_not_contact: false,
            active: true,
            verified: true,
            listing_published: true,
            agent_gate: None,
            approaches_used_this_month: 0,
            allowance: MONTHLY_APPROACH_ALLOWANCE,
        }
    }

    #[test]
    fn a_consenting_verified_contact_with_a_published_listing_passes() {
        assert_eq!(review_approach(&permitted()), Ok(()));
    }

    #[test]
    fn do_not_contact_wins_over_everything() {
        let mut request = permitted();
        request.do_not_contact = true;
        request.approaches_used_this_month = u32::MAX;
        request.listing_published = false;
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::DoNotContact)
        );
    }

    #[test]
    fn an_unconsented_contact_is_refused() {
        let mut request = permitted();
        request.accepts_outreach = false;
        assert_eq!(review_approach(&request), Err(ApproachRefusal::NotAccepted));
    }

    #[test]
    fn consent_without_a_basis_is_not_consent() {
        let mut request = permitted();
        request.has_acceptance_basis = false;
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::AcceptanceWithoutBasis)
        );
    }

    #[test]
    fn an_unverified_route_is_refused() {
        let mut request = permitted();
        request.verified = false;
        assert_eq!(review_approach(&request), Err(ApproachRefusal::NotVerified));
    }

    #[test]
    fn no_published_listing_means_nothing_to_send() {
        let mut request = permitted();
        request.listing_published = false;
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::ListingNotPublished)
        );
    }

    #[test]
    fn the_allowance_exhausts_at_the_limit() {
        let mut request = permitted();
        request.approaches_used_this_month = MONTHLY_APPROACH_ALLOWANCE - 1;
        assert_eq!(review_approach(&request), Ok(()));
        request.approaches_used_this_month = MONTHLY_APPROACH_ALLOWANCE;
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::AllowanceExhausted {
                allowance: MONTHLY_APPROACH_ALLOWANCE
            })
        );
    }

    const EMPTY_DRAW: DrawEvidence = DrawEvidence {
        paid_buyers: 0,
        interested_fans: 0,
        confirmed_shows: 0,
        top_city: None,
    };

    fn draw() -> DrawEvidence {
        DrawEvidence {
            paid_buyers: 180,
            interested_fans: 900,
            confirmed_shows: 6,
            top_city: Some("Wrocław".to_owned()),
        }
    }

    fn agent() -> AgentGate {
        AgentGate {
            door_closed: false,
            approached_this_season: false,
            draw: draw(),
        }
    }

    #[test]
    fn an_agent_with_evidence_and_an_open_season_passes() {
        let mut request = permitted();
        request.agent_gate = Some(agent());
        assert_eq!(review_approach(&request), Ok(()));
    }

    #[test]
    fn a_closed_door_answers_before_the_pitch_is_read() {
        let mut request = permitted();
        let mut gate = agent();
        gate.door_closed = true;
        request.listing_published = false;
        gate.draw = EMPTY_DRAW;
        request.agent_gate = Some(gate);
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::AgentDoorClosed)
        );
    }

    #[test]
    fn a_second_knock_inside_the_season_is_refused() {
        let mut request = permitted();
        let mut gate = agent();
        gate.approached_this_season = true;
        request.agent_gate = Some(gate);
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::AgentApproachedThisSeason)
        );
    }

    #[test]
    fn an_agent_pitch_without_draw_is_refused() {
        let mut request = permitted();
        let mut gate = agent();
        gate.draw = EMPTY_DRAW;
        request.agent_gate = Some(gate);
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::InsufficientDrawEvidence)
        );
        let mut gate = agent();
        gate.draw = DrawEvidence {
            paid_buyers: 0,
            ..draw()
        };
        request.agent_gate = Some(gate);
        assert_eq!(
            review_approach(&request),
            Err(ApproachRefusal::InsufficientDrawEvidence)
        );
    }

    #[test]
    fn a_label_never_faces_the_agent_gates() {
        let request = permitted();
        assert_eq!(review_approach(&request), Ok(()));
    }

    #[test]
    fn every_refusal_says_something_a_band_can_act_on() {
        for refusal in [
            ApproachRefusal::DoNotContact,
            ApproachRefusal::NotAccepted,
            ApproachRefusal::AcceptanceWithoutBasis,
            ApproachRefusal::NotVerified,
            ApproachRefusal::Inactive,
            ApproachRefusal::ListingNotPublished,
            ApproachRefusal::AgentDoorClosed,
            ApproachRefusal::AgentApproachedThisSeason,
            ApproachRefusal::InsufficientDrawEvidence,
            ApproachRefusal::AllowanceExhausted { allowance: 4 },
        ] {
            assert!(refusal.message().len() > 20, "too terse: {refusal:?}");
        }
    }
}
