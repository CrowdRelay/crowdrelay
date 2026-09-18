//! The letter a promoter actually receives (O.1).
//!
//! # Why this is here and not in the executor
//!
//! It used to be JavaScript inside the n8n workflow. The console showed the
//! band an opening line and a list of reasons; the promoter received several
//! paragraphs nobody at the band had read — the room's name, a self-description,
//! a closing ask and a signature, all composed after the approval. Approving one
//! sentence and sending five is the single best reason not to press a button.
//!
//! Composing here fixes three things at once:
//!
//! * The payload carries the finished letter, so the console can show exactly
//!   what will be sent.
//! * `draft_revision::revisable_fields` can offer `subject` and `body` for
//!   editing, because they are now fields rather than something an executor
//!   will invent later.
//! * The identical-draft refusal in `gate_outward_emission` can compare drafts,
//!   which it cannot do for text that does not exist yet.
//!
//! # Nothing about the band is invented
//!
//! The workflow hardcoded "We are Virya, a modern metal band from Wroclaw" and
//! two Virya URLs — one tenant's identity baked into shared machinery. Here
//! every part of that comes from [`SenderIdentity`], and each part is optional:
//! an unset style produces "We are X" rather than a guess at what they sound
//! like, and an unset site produces no link line rather than a dead one. A
//! sentence nobody can support is worse than a shorter letter.

use serde::{Deserialize, Serialize};

/// Which of the two letters this is.
///
/// They are not interchangeable. The proposal asks a room for a night that does
/// not exist; the ask confirms a named labelmate for a slot the promoter already
/// offered on a night that is already held. Rendering the second as the first
/// announces a show that was never in question.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LetterKind {
    #[default]
    Proposal,
    SupportSlotAsk,
}

/// Who the letter is from, as the tenant's own records describe them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SenderIdentity {
    /// The act's name — `workspaces.name`. The one part that is never absent.
    pub act_name: String,
    /// The operator's own declaration of what the act sounds like (5.21),
    /// e.g. "modern metal". Absent means the letter says nothing about it.
    pub style: Option<String>,
    /// Where the act is from, when the tenant has said. Absent means the
    /// sentence stops earlier.
    pub home_city: Option<String>,
    /// The public site, from `member_site_base_url`. Absent means no link line
    /// rather than a link to nowhere.
    pub site_url: Option<String>,
}

/// Everything the letter is composed from.
#[derive(Clone, Debug)]
pub struct LetterInput<'a> {
    pub kind: LetterKind,
    pub sender: &'a SenderIdentity,
    /// The room, named so a promoter who books two knows which is meant.
    pub venue: &'a str,
    /// The proposal's strongest reason, already a sentence.
    pub opening_line: &'a str,
    /// Every reason, in rank order. The first is the one `opening_line`
    /// already states and is not repeated as a bullet.
    pub reasons: &'a [String],
    /// The labelmate being put forward. Required for a support-slot ask.
    pub support_act: Option<&'a str>,
    /// The night the slot belongs to, as the letter should print it.
    /// Required for a support-slot ask.
    pub show_date: Option<&'a str>,
}

/// A finished letter.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GigLetter {
    pub subject: String,
    pub body: String,
}

/// Why a letter could not be composed.
///
/// Each is a missing fact rather than a formatting problem, and each is the
/// band's sentence: the caller shows it instead of translating it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LetterRefusal {
    NoVenue,
    NoOpeningLine,
    NoReasons,
    NoSenderName,
    AskWithoutAct,
    AskWithoutDate,
}

impl LetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoVenue => "the letter has no room to name",
            Self::NoOpeningLine => "the proposal produced no opening line",
            Self::NoReasons => "there is no evidence to write from",
            Self::NoSenderName => "this workspace has no name to sign with",
            Self::AskWithoutAct => "a support-slot ask has to name the act being put forward",
            Self::AskWithoutDate => "a support-slot ask has to name the night the slot is on",
        }
    }
}

/// Subjects are capped well under the 220 the executor allowed, because a
/// subject a mail client truncates is a subject nobody read.
const MAX_SUBJECT: usize = 160;

/// Composes the letter, or refuses with the fact that is missing.
///
/// # Errors
///
/// Returns the missing fact. Every refusal here is recoverable by the operator
/// — name the act, set the style, pick the support — which is why none of them
/// is a silent fallback.
pub fn compose_letter(input: &LetterInput<'_>) -> Result<GigLetter, LetterRefusal> {
    let venue = input.venue.trim();
    let opening = input.opening_line.trim();
    let act = input.sender.act_name.trim();
    if venue.is_empty() {
        return Err(LetterRefusal::NoVenue);
    }
    if opening.is_empty() {
        return Err(LetterRefusal::NoOpeningLine);
    }
    if input.reasons.is_empty() {
        return Err(LetterRefusal::NoReasons);
    }
    if act.is_empty() {
        return Err(LetterRefusal::NoSenderName);
    }

    // The first reason is the opening line's own fact. Repeating it as a bullet
    // makes the letter read like a form.
    let rest: Vec<String> = input
        .reasons
        .iter()
        .skip(1)
        .map(|reason| sentence_case(reason.trim()))
        .filter(|reason| !reason.is_empty())
        .collect();

    match input.kind {
        LetterKind::Proposal => Ok(proposal(input, venue, opening, act, &rest)),
        LetterKind::SupportSlotAsk => {
            let support = input
                .support_act
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(LetterRefusal::AskWithoutAct)?;
            let date = input
                .show_date
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(LetterRefusal::AskWithoutDate)?;
            Ok(support_slot_ask(
                input, venue, opening, support, date, &rest,
            ))
        }
    }
}

fn proposal(
    input: &LetterInput<'_>,
    venue: &str,
    opening: &str,
    act: &str,
    rest: &[String],
) -> GigLetter {
    let mut body = vec![
        "Hi,".to_owned(),
        String::new(),
        opening.to_owned(),
        String::new(),
    ];
    body.push(format!(
        "{} and we are putting together a night at {venue}. This letter goes to \
         everyone who books the room at once, so nobody hears about it secondhand.",
        introduction(input.sender, act)
    ));
    body.push(String::new());
    if !rest.is_empty() {
        body.push("Why we think the night works:".to_owned());
        for reason in rest {
            body.push(format!("- {reason}"));
        }
        body.push(String::new());
    }
    body.push(format!(
        "If {venue} has a window in the coming months, tell us what the night \
         needs and we will come back with a concrete offer."
    ));
    body.push(String::new());
    if let Some(site) = site_line(input.sender) {
        body.push(site);
        body.push(String::new());
    }
    body.push("Best,".to_owned());
    body.push(act.to_owned());

    GigLetter {
        subject: truncate(&format!("{act} x {venue} — show proposal")),
        body: body.join("\n"),
    }
}

fn support_slot_ask(
    input: &LetterInput<'_>,
    venue: &str,
    opening: &str,
    support: &str,
    date: &str,
    rest: &[String],
) -> GigLetter {
    let act = input.sender.act_name.trim();
    let mut body = vec![
        "Hi,".to_owned(),
        String::new(),
        opening.to_owned(),
        String::new(),
    ];
    body.push(format!(
        "The night at {venue} on {date} is already ours, and the slot you offered \
         is still open. {support} is the labelmate we want to put in it. This \
         letter goes to everyone who books the room at once, so nobody hears \
         about it secondhand."
    ));
    body.push(String::new());
    if !rest.is_empty() {
        body.push(format!("Why {support} fits the slot:"));
        for reason in rest {
            body.push(format!("- {reason}"));
        }
        body.push(String::new());
    }
    body.push(format!(
        "If {support} works for the slot, say so and they are confirmed — and if \
         you would rather hold it, that answer is just as useful."
    ));
    body.push(String::new());
    body.push("Best,".to_owned());
    body.push(act.to_owned());

    GigLetter {
        subject: truncate(&format!("{support} for the {venue} slot — {date}")),
        body: body.join("\n"),
    }
}

/// "We are X", plus whatever else the tenant has actually said about itself.
///
/// Every clause is conditional on a stored fact. A band that has not declared a
/// style gets a shorter sentence, not a guessed one — the promoter reading it
/// would rather have four true words than a genre somebody's software picked.
fn introduction(sender: &SenderIdentity, act: &str) -> String {
    let style = sender
        .style
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let city = sender
        .home_city
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (style, city) {
        (Some(style), Some(city)) => format!("We are {act}, a {style} band from {city},"),
        (Some(style), None) => format!("We are {act}, a {style} band,"),
        (None, Some(city)) => format!("We are {act}, a band from {city},"),
        (None, None) => format!("We are {act},"),
    }
}

fn site_line(sender: &SenderIdentity) -> Option<String> {
    sender
        .site_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|site| format!("Music: {site}"))
}

/// Capitalises the first character and leaves the rest alone.
///
/// The reasons are machine-rendered sentences that start lowercase because they
/// are usually embedded mid-sentence. As bullets they start a line.
fn sentence_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn truncate(subject: &str) -> String {
    if subject.chars().count() <= MAX_SUBJECT {
        return subject.to_owned();
    }
    let mut out: String = subject.chars().take(MAX_SUBJECT - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "Virya".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music/".to_owned()),
        }
    }

    fn reasons() -> Vec<String> {
        vec![
            "four comparable acts played this room in the last year".to_owned(),
            "the band has 120 reachable fans in this city".to_owned(),
            "the room has hosted a show every month since March".to_owned(),
        ]
    }

    /// The whole point of the change: the letter exists before the approval,
    /// and it contains every sentence the promoter will read.
    #[test]
    fn the_proposal_is_whole_before_it_is_approved() {
        let sender = sender();
        let reasons = reasons();
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            sender: &sender,
            venue: "Klub X",
            opening_line: "Four comparable acts played Klub X in the last year.",
            reasons: &reasons,
            support_act: None,
            show_date: None,
        })
        .expect("composes");
        assert_eq!(letter.subject, "Virya x Klub X — show proposal");
        assert!(letter.body.starts_with("Hi,\n"));
        assert!(
            letter
                .body
                .contains("We are Virya, a modern metal band from Wrocław,")
        );
        assert!(
            letter
                .body
                .contains("- The band has 120 reachable fans in this city")
        );
        assert!(letter.body.ends_with("Best,\nVirya"));
        // The opening line's own fact is never repeated as a bullet.
        assert_eq!(letter.body.matches("Four comparable acts").count(), 1);
    }

    /// Nothing about the band is invented. An unset style shortens the
    /// sentence; it does not guess a genre.
    #[test]
    fn an_unknown_style_shortens_the_sentence_rather_than_inventing_one() {
        let sender = SenderIdentity {
            act_name: "Nowy Zespół".to_owned(),
            ..SenderIdentity::default()
        };
        let reasons = reasons();
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            sender: &sender,
            venue: "Klub X",
            opening_line: "The room books this kind of night.",
            reasons: &reasons,
            support_act: None,
            show_date: None,
        })
        .expect("composes");
        assert!(
            letter
                .body
                .contains("We are Nowy Zespół, and we are putting together")
        );
        assert!(!letter.body.contains("band from"));
        assert!(
            !letter.body.contains("Music:"),
            "a site nobody configured must not become a link line"
        );
    }

    /// The two letters are not interchangeable: the ask names the act and the
    /// night, and never announces a show as if it were unbooked.
    #[test]
    fn the_ask_names_the_act_the_night_and_the_slot() {
        let sender = sender();
        let reasons = reasons();
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::SupportSlotAsk,
            sender: &sender,
            venue: "Klub X",
            opening_line: "The slot you offered on 12 October is still open.",
            reasons: &reasons,
            support_act: Some("Second Act"),
            show_date: Some("12 October"),
        })
        .expect("composes");
        assert_eq!(
            letter.subject,
            "Second Act for the Klub X slot — 12 October"
        );
        assert!(
            letter
                .body
                .contains("The night at Klub X on 12 October is already ours")
        );
        assert!(letter.body.contains("Why Second Act fits the slot:"));
        assert!(
            !letter.body.contains("putting together a night"),
            "the ask must not announce a night that is already held"
        );
    }

    /// Missing facts refuse by name. A letter with a blank where the act's name
    /// belongs is worse than no letter.
    #[test]
    fn a_missing_fact_refuses_and_says_which() {
        let sender = sender();
        let reasons = reasons();
        let base = LetterInput {
            kind: LetterKind::SupportSlotAsk,
            sender: &sender,
            venue: "Klub X",
            opening_line: "A sentence.",
            reasons: &reasons,
            support_act: None,
            show_date: Some("12 October"),
        };
        assert_eq!(compose_letter(&base), Err(LetterRefusal::AskWithoutAct));

        let empty = SenderIdentity::default();
        assert_eq!(
            compose_letter(&LetterInput {
                kind: LetterKind::Proposal,
                sender: &empty,
                ..base.clone()
            }),
            Err(LetterRefusal::NoSenderName)
        );
        assert_eq!(
            compose_letter(&LetterInput {
                venue: "  ",
                ..base.clone()
            }),
            Err(LetterRefusal::NoVenue)
        );
        assert_eq!(
            compose_letter(&LetterInput {
                reasons: &[],
                ..base
            }),
            Err(LetterRefusal::NoReasons)
        );
    }

    /// One reason is a whole proposal: the opening line says it, and the letter
    /// carries no empty bullet list underneath.
    #[test]
    fn a_single_reason_produces_no_bullet_section() {
        let sender = sender();
        let one = vec!["the room books this kind of night".to_owned()];
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            sender: &sender,
            venue: "Klub X",
            opening_line: "The room books this kind of night.",
            reasons: &one,
            support_act: None,
            show_date: None,
        })
        .expect("composes");
        assert!(!letter.body.contains("Why we think the night works:"));
    }

    /// A subject a mail client cuts is a subject nobody read.
    #[test]
    fn a_long_room_name_does_not_produce_an_unreadable_subject() {
        let sender = sender();
        let reasons = reasons();
        let venue = "A".repeat(400);
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            sender: &sender,
            venue: &venue,
            opening_line: "A sentence.",
            reasons: &reasons,
            support_act: None,
            show_date: None,
        })
        .expect("composes");
        assert!(letter.subject.chars().count() <= MAX_SUBJECT);
        assert!(letter.subject.ends_with('…'));
    }
}
