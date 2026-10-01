//! The letters a representation or booking-agent approach sends.
//!
//! Both are composed here — at the moment the band asks for the approach —
//! so the words an operator approves are the words a stranger reads. An
//! executor downstream sends `draft.body` verbatim and refuses a payload
//! whose draft is missing or empty; nothing on the send side may write a
//! line on the band's behalf. The incident that taught this pattern is the
//! gig letter's: a band approved one sentence and a promoter read five
//! paragraphs nobody at the band had seen.
//!
//! The register is deliberately plain. Neither contact has a language on
//! record, so the letters are English — the lingua franca of this business
//! and an explicit choice rather than an accident. The listing and the
//! draw evidence are the band's own material: presented, never inflated,
//! and nothing is claimed that the inputs do not state.

use serde::{Deserialize, Serialize};

use crate::booking_agent::AgentDrawEvidence;
use crate::gig_letter::SenderIdentity;
use crate::listing::BandListing;

/// A finished approach letter — the same `{subject, body}` shape the gig
/// letter, the invite and every outward draft carries, because the outward
/// gate's identical-draft check and `draft_revision`'s revisable fields
/// both already look for `draft.subject` and `draft.body`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApproachLetter {
    pub subject: String,
    pub body: String,
}

/// Why an approach letter could not be composed. Each is a missing fact
/// rather than a formatting problem — a letter without the fact it exists
/// to state is not a letter, it is a template wearing a pitch's clothes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApproachLetterRefusal {
    /// The recipient has no name — a letter that cannot greet honestly
    /// should not greet at all.
    NoTargetName,
    /// A representation approach with no listing has no pitch: the listing
    /// is the entire substance of the introduction.
    NoListing,
    /// The agent has no name — same greeting honesty as `NoTargetName`.
    NoAgentName,
    /// A booking-agent application with no measured draw evidence is a cold
    /// pitch, and a cold pitch is exactly what the gate upstream refuses.
    NoEvidence,
    /// The workspace has no act name — there is nobody to introduce.
    NoSenderName,
}

impl ApproachLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTargetName => "the letter has nobody to greet",
            Self::NoListing => "the letter has no listing to present",
            Self::NoAgentName => "the letter has no agent to greet",
            Self::NoEvidence => "the letter has no measured draw to argue from",
            Self::NoSenderName => "the letter has no act to introduce",
        }
    }
}

/// Everything the representation letter is composed from.
#[derive(Clone, Debug)]
pub struct RepresentationLetterInput<'a> {
    pub sender: &'a SenderIdentity,
    /// The contact's display name — agent or label.
    pub target_name: &'a str,
    /// The redacted listing, exactly as an admitted reader would see it:
    /// supported claims only.
    pub listing: &'a BandListing,
    /// One line of the band's own words under the listing, when given.
    pub note: Option<&'a str>,
}

/// Everything the booking-agent application is composed from.
#[derive(Clone, Debug)]
pub struct BookingAgentLetterInput<'a> {
    pub sender: &'a SenderIdentity,
    pub agent_name: &'a str,
    /// The agency, where the agent declared one — the greeting stays honest
    /// either way.
    pub agency: Option<&'a str>,
    /// The draw snapshot measured inside the request's lock — the numbers
    /// the approval saw, so the letter may cite them and nothing else.
    /// Each reading is absent only when it could not be taken; an absent
    /// reading is skipped in the letter, never printed as zero.
    pub evidence: &'a AgentDrawEvidence,
    pub note: Option<&'a str>,
}

/// Compose the representation introduction.
///
/// The listing is the pitch: the claims are printed with their own basis
/// so the reader can check each number against the band, and a claim the
/// band could not support never reached the redacted listing in the first
/// place. Nothing is added to what the band published.
pub fn compose_representation_letter(
    input: &RepresentationLetterInput<'_>,
) -> Result<ApproachLetter, ApproachLetterRefusal> {
    let target_name = input.target_name.trim();
    if target_name.is_empty() {
        return Err(ApproachLetterRefusal::NoTargetName);
    }
    let listing = input.listing;
    if listing.act_name.trim().is_empty() {
        return Err(ApproachLetterRefusal::NoListing);
    }
    let sender = input.sender;
    if sender.act_name.trim().is_empty() {
        return Err(ApproachLetterRefusal::NoSenderName);
    }

    let mut lines = vec![
        format!("Dear {target_name},"),
        String::new(),
        introduction(sender, listing),
    ];
    if !listing.genre_tags.is_empty() {
        lines.push(format!("The act runs {}.", listing.genre_tags.join(", ")));
    }
    if !listing.cities.is_empty() {
        lines.push(format!(
            "Cities on the listing: {}.",
            listing.cities.join(", ")
        ));
    }
    if !listing.claims.is_empty() {
        lines.push("On record, each with where the number comes from:".to_owned());
        for claim in &listing.claims {
            match claim.value {
                Some(value) => lines.push(format!("- {}: {value} ({})", claim.label, claim.basis)),
                // An unsupported claim is the band saying "we do not know
                // this yet" — print it as such rather than skip it: the
                // listing's honesty is the pitch.
                None => lines.push(format!(
                    "- {}: not yet measured ({})",
                    claim.label, claim.basis
                )),
            }
        }
    }
    if !listing.seeking.is_empty() {
        lines.push(format!("Looking for: {}.", listing.seeking.join(", ")));
    }
    if let Some(note) = input.note.map(str::trim).filter(|n| !n.is_empty()) {
        lines.push(String::new());
        lines.push(note.to_owned());
    }
    lines.push(String::new());
    lines.push(
        "Happy to send dates, draws or the full listing — whichever is most useful to read."
            .to_owned(),
    );
    lines.extend(sign_off(sender));
    Ok(ApproachLetter {
        subject: format!("{} — representation enquiry", listing.act_name),
        body: lines.join("\n"),
    })
}

/// Compose the booking-agent application.
///
/// The numbers are the pitch: each line of the snapshot is printed as
/// measured, a missing reading is absent rather than zero, and the season
/// ask is stated plainly. The letter may not inflate — it cites the
/// evidence the lock measured and nothing more.
pub fn compose_booking_agent_letter(
    input: &BookingAgentLetterInput<'_>,
) -> Result<ApproachLetter, ApproachLetterRefusal> {
    let agent_name = input.agent_name.trim();
    if agent_name.is_empty() {
        return Err(ApproachLetterRefusal::NoAgentName);
    }
    let evidence = input.evidence;
    // The three readings the pitch cannot run without — the request's gate
    // already refuses them, and a composer that trusted the caller ran the
    // gate would be a composer that sends a cold pitch the day somebody
    // forgets.
    let shows = evidence
        .shows_played_12m
        .ok_or(ApproachLetterRefusal::NoEvidence)?;
    let paid = evidence
        .paid_tickets_12m
        .ok_or(ApproachLetterRefusal::NoEvidence)?;
    let buyers = evidence
        .distinct_buyers_12m
        .ok_or(ApproachLetterRefusal::NoEvidence)?;
    let sender = input.sender;
    if sender.act_name.trim().is_empty() {
        return Err(ApproachLetterRefusal::NoSenderName);
    }

    let greeting = match input.agency.map(str::trim).filter(|a| !a.is_empty()) {
        Some(agency) => format!("Dear {agent_name} ({agency}),"),
        None => format!("Dear {agent_name},"),
    };
    let mut lines = vec![
        greeting,
        String::new(),
        introduction(sender, &listing_stub(sender)),
    ];
    lines.push(String::new());
    lines.push(
        "We are applying for booking representation for the coming season. What the \
         band's own ledger measures over the last twelve months:"
            .to_owned(),
    );
    lines.push(format!("- {shows} shows played"));
    lines.push(format!("- {paid} paid tickets"));
    lines.push(format!("- {buyers} distinct buyers"));
    if let Some(repeat) = evidence.repeat_buyers_12m {
        lines.push(format!("- {repeat} of them came back for more"));
    }
    if let Some(cities) = evidence.cities_reached_12m {
        lines.push(format!("- {cities} cities reached"));
    }
    if let Some(best) = evidence.best_show_paid_tickets_12m {
        lines.push(format!("- best single night: {best} paid tickets"));
    }
    if let Some(note) = input.note.map(str::trim).filter(|n| !n.is_empty()) {
        lines.push(String::new());
        lines.push(note.to_owned());
    }
    lines.push(String::new());
    lines.push(
        "If the numbers are worth a conversation on your side, we can send full draws, \
         dates and whatever else helps you read them."
            .to_owned(),
    );
    lines.extend(sign_off(sender));
    Ok(ApproachLetter {
        subject: format!("{} — booking representation", sender.act_name),
        body: lines.join("\n"),
    })
}

/// Everything the reply to a booking agent is composed from.
///
/// A reply knows less than an application: the interaction ledger records
/// the disposition the operator filed — `received`, `positive` or `signed`
/// — not the words the agent actually wrote. The draft is therefore a
/// scaffold, shaped by the disposition and deliberately short: the approver
/// completes it against the real thread, and the send goes verbatim as
/// always, so what the card shows is what the agent gets.
#[derive(Clone, Debug)]
pub struct BookingAgentReplyInput<'a> {
    pub sender: &'a SenderIdentity,
    pub agent_name: &'a str,
    /// The agency, where the agent declared one — the greeting stays honest
    /// either way.
    pub agency: Option<&'a str>,
    /// `received`, `positive` or `signed` — the only dispositions a reply
    /// is ever drafted for; a decline and a do-not-contact ask nothing.
    pub reply_disposition: &'a str,
}

/// Compose the booking-agent reply scaffold.
///
/// `positive` and `signed` get the move-it-forward shape — thank them and
/// offer the concrete next step; a bare `received` gets the acknowledgement
/// that keeps the thread alive while the operator reads the actual reply.
/// Neither shape quotes their words: the ledger never held them, and a
/// scaffold that pretended otherwise would be the letter lying.
pub fn compose_booking_agent_reply_scaffold(
    input: &BookingAgentReplyInput<'_>,
) -> Result<ApproachLetter, ApproachLetterRefusal> {
    let agent_name = input.agent_name.trim();
    if agent_name.is_empty() {
        return Err(ApproachLetterRefusal::NoAgentName);
    }
    let sender = input.sender;
    if sender.act_name.trim().is_empty() {
        return Err(ApproachLetterRefusal::NoSenderName);
    }

    let greeting = match input.agency.map(str::trim).filter(|a| !a.is_empty()) {
        Some(agency) => format!("Dear {agent_name} ({agency}),"),
        None => format!("Dear {agent_name},"),
    };
    let middle = match input.reply_disposition {
        "signed" => {
            "Thank you — that is wonderful news. We would love to talk through \
             what the season looks like together: dates, territories, whatever \
             helps you plan. Happy to send full draws or set up a call, whichever \
             suits you best."
        }
        "positive" => {
            "Thank you — great to hear. Happy to send full draws, current \
             availability, or set up a short call — whichever is most useful \
             on your side."
        }
        _ => {
            "Thank you for getting back to us — much appreciated. We will come \
             back to you with the details shortly."
        }
    };
    let mut lines = vec![greeting, String::new(), middle.to_owned()];
    lines.extend(sign_off(sender));
    Ok(ApproachLetter {
        subject: format!("Re: {} — booking representation", sender.act_name.trim()),
        body: lines.join("\n"),
    })
}

/// One shared opening sentence for both letters — who is writing, what the
/// act is called, what it sounds like and where it is from, each part
/// present only when the workspace declared it.
fn introduction(sender: &SenderIdentity, listing: &BandListing) -> String {
    let act = if listing.act_name.trim().is_empty() {
        sender.act_name.trim()
    } else {
        listing.act_name.trim()
    };
    let mut first = format!("I work with {act}");
    if let Some(style) = sender
        .style
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        first.push_str(&format!(", a {style} act"));
    }
    if let Some(city) = sender
        .home_city
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        first.push_str(&format!(" out of {city}"));
    }
    first.push('.');
    first
}

/// The booking-agent path has no listing — the introduction only needs the
/// act name, so a stub listing hands `introduction` the sender's name.
fn listing_stub(sender: &SenderIdentity) -> BandListing {
    BandListing {
        act_name: sender.act_name.clone(),
        genre_tags: Vec::new(),
        cities: Vec::new(),
        claims: Vec::new(),
        published_dates: Vec::new(),
        seeking: Vec::new(),
        visibility: crate::listing::ListingVisibility::AdmittedReaders,
    }
}

/// The closing block — the act's name and, where the tenant declared one,
/// its own site. No link line when there is no link to give.
fn sign_off(sender: &SenderIdentity) -> Vec<String> {
    let mut lines = vec![String::new(), format!("— {}", sender.act_name.trim())];
    if let Some(link) = &sender.site_url {
        lines.push(link.as_str().to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::{ListedClaim, ListingVisibility};
    use crate::tracked_link::TrackedLink;
    use crate::value_tier::MetricValueTier;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "Virya".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some(TrackedLink::for_site(
                "https://virya.example",
                &crate::SmartLinkSlug::parse("site").unwrap(),
            )),
        }
    }

    fn listing() -> BandListing {
        BandListing {
            act_name: "Virya".to_owned(),
            genre_tags: vec!["deathcore".to_owned(), "metalcore".to_owned()],
            cities: vec!["Wrocław".to_owned(), "Warszawa".to_owned()],
            claims: vec![
                ListedClaim {
                    label: "paid ticket buyers".to_owned(),
                    value: Some(184),
                    tier: MetricValueTier::Downstream,
                    basis: "ticket orders".to_owned(),
                },
                ListedClaim {
                    label: "reachable fans in Wrocław".to_owned(),
                    value: None,
                    tier: MetricValueTier::Intermediate,
                    basis: "our own check-ins".to_owned(),
                },
            ],
            published_dates: Vec::new(),
            seeking: vec!["booking agent".to_owned()],
            visibility: ListingVisibility::AdmittedReaders,
        }
    }

    fn evidence() -> AgentDrawEvidence {
        AgentDrawEvidence {
            shows_played_12m: Some(9),
            paid_tickets_12m: Some(184),
            distinct_buyers_12m: Some(141),
            repeat_buyers_12m: Some(23),
            cities_reached_12m: Some(4),
            best_show_paid_tickets_12m: Some(61),
            as_of: None,
        }
    }

    #[test]
    fn representation_letter_presents_the_listing() {
        let letter = compose_representation_letter(&RepresentationLetterInput {
            sender: &sender(),
            target_name: "Aga Agency",
            listing: &listing(),
            note: Some("we met after the Wrocław show"),
        })
        .expect("composes");
        assert_eq!(letter.subject, "Virya — representation enquiry");
        assert!(letter.body.contains("Dear Aga Agency,"));
        assert!(
            letter
                .body
                .contains("paid ticket buyers: 184 (ticket orders)")
        );
        assert!(
            letter
                .body
                .contains("reachable fans in Wrocław: not yet measured")
        );
        assert!(letter.body.contains("Looking for: booking agent."));
        assert!(letter.body.contains("we met after the Wrocław show"));
        assert!(letter.body.contains("— Virya"));
        assert!(letter.body.contains("https://virya.example"));
    }

    #[test]
    fn representation_letter_refuses_missing_facts() {
        assert_eq!(
            compose_representation_letter(&RepresentationLetterInput {
                sender: &sender(),
                target_name: "",
                listing: &listing(),
                note: None,
            }),
            Err(ApproachLetterRefusal::NoTargetName)
        );
        let mut blank = listing();
        blank.act_name = String::new();
        assert_eq!(
            compose_representation_letter(&RepresentationLetterInput {
                sender: &sender(),
                target_name: "Aga",
                listing: &blank,
                note: None,
            }),
            Err(ApproachLetterRefusal::NoListing)
        );
        let no_sender = SenderIdentity {
            act_name: String::new(),
            style: None,
            home_city: None,
            site_url: None,
        };
        assert_eq!(
            compose_representation_letter(&RepresentationLetterInput {
                sender: &no_sender,
                target_name: "Aga",
                listing: &listing(),
                note: None,
            }),
            Err(ApproachLetterRefusal::NoSenderName)
        );
    }

    #[test]
    fn booking_agent_letter_cites_the_measured_evidence() {
        let letter = compose_booking_agent_letter(&BookingAgentLetterInput {
            sender: &sender(),
            agent_name: "Jan Kowalski",
            agency: Some("Stage Left"),
            evidence: &evidence(),
            note: None,
        })
        .expect("composes");
        assert_eq!(letter.subject, "Virya — booking representation");
        assert!(letter.body.contains("Dear Jan Kowalski (Stage Left),"));
        assert!(letter.body.contains("9 shows played"));
        assert!(letter.body.contains("184 paid tickets"));
        assert!(letter.body.contains("141 distinct buyers"));
        assert!(letter.body.contains("23 of them came back for more"));
        assert!(letter.body.contains("4 cities reached"));
        assert!(letter.body.contains("best single night: 61 paid tickets"));
    }

    #[test]
    fn booking_agent_letter_refuses_a_thin_pitch() {
        let thin = AgentDrawEvidence {
            distinct_buyers_12m: None,
            ..evidence()
        };
        assert_eq!(
            compose_booking_agent_letter(&BookingAgentLetterInput {
                sender: &sender(),
                agent_name: "Jan",
                agency: None,
                evidence: &thin,
                note: None,
            }),
            Err(ApproachLetterRefusal::NoEvidence)
        );
    }

    #[test]
    fn agent_reply_scaffold_thanks_and_offers_the_next_step() {
        let letter = compose_booking_agent_reply_scaffold(&BookingAgentReplyInput {
            sender: &sender(),
            agent_name: "Jan Kowalski",
            agency: Some("Stage Left"),
            reply_disposition: "positive",
        })
        .expect("composes");
        assert_eq!(letter.subject, "Re: Virya — booking representation");
        assert!(letter.body.contains("Dear Jan Kowalski (Stage Left),"));
        assert!(letter.body.contains("Thank you"));
        assert!(letter.body.contains("— Virya"));

        let holding = compose_booking_agent_reply_scaffold(&BookingAgentReplyInput {
            sender: &sender(),
            agent_name: "Agent Y",
            agency: None,
            reply_disposition: "received",
        })
        .expect("composes");
        assert!(holding.body.contains("getting back"));
        assert!(holding.body.contains("Dear Agent Y,"));
    }

    #[test]
    fn agent_reply_scaffold_refuses_missing_facts() {
        assert_eq!(
            compose_booking_agent_reply_scaffold(&BookingAgentReplyInput {
                sender: &sender(),
                agent_name: "  ",
                agency: None,
                reply_disposition: "positive",
            }),
            Err(ApproachLetterRefusal::NoAgentName)
        );
        let no_sender = SenderIdentity {
            act_name: String::new(),
            style: None,
            home_city: None,
            site_url: None,
        };
        assert_eq!(
            compose_booking_agent_reply_scaffold(&BookingAgentReplyInput {
                sender: &no_sender,
                agent_name: "Jan",
                agency: None,
                reply_disposition: "received",
            }),
            Err(ApproachLetterRefusal::NoSenderName)
        );
    }

    #[test]
    fn an_empty_default_draft_decodes_and_is_empty() {
        // A parked action queued before the draft field existed decodes to
        // the default — empty strings — and dispatch refuses it rather than
        // sending a letter nobody composed.
        let draft = ApproachLetter::default();
        assert!(draft.subject.trim().is_empty() && draft.body.trim().is_empty());
    }
}
