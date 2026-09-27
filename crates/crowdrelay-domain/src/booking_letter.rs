//! The letter a booking target actually receives.
//!
//! Same rule as `gig_letter`: the words are composed here, at the moment the
//! action is written, so the approval shows exactly what the promoter reads
//! and the executor sends `draft.body` verbatim. Before this existed the
//! event carried `template_key` and `venue_evidence` and left the writing to
//! whatever ran downstream — which meant the operator approved "booking
//! contact: X" and a stranger received a letter nobody had read.
//!
//! Two phases, two languages. The initial letter asks for a slot inside the
//! derived window; the follow-up re-asks without re-proposing. Polish when
//! the city is Polish — the letter is for the person reading it, not for the
//! workspace that sent it.

use serde::{Deserialize, Serialize};
use time::Date;

use crate::booking::BookingOutreachPhase;
use crate::gig_letter::{LetterLanguage, SenderIdentity};

/// A finished letter — the wire shape every letter executor reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingLetter {
    pub subject: String,
    pub body: String,
}

/// Everything the letter is composed from.
#[derive(Clone, Debug)]
pub struct BookingLetterInput<'a> {
    /// What the recipient reads it in — chosen from the room's country, not
    /// the band's locale.
    pub language: LetterLanguage,
    pub sender: &'a SenderIdentity,
    /// The booking target's display name — a room, a promoter, a festival.
    pub target_name: &'a str,
    /// The city the ask is about.
    pub city: &'a str,
    /// The derived window, when one exists. A follow-up carries `None` — it
    /// re-asks the question rather than re-deriving the dates.
    pub proposed_window: Option<(Date, Date)>,
    /// The evidence line measured when the request was raised — "12 shows
    /// in the last year, 4 comparable acts on its bills". `None` when the
    /// room has no measured record, and the letter simply does not cite one.
    pub first_line_fact: Option<&'a str>,
    pub phase: BookingOutreachPhase,
}

/// Why a letter could not be composed — each a missing fact, shown to the
/// operator rather than translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingLetterRefusal {
    NoCity,
    NoTarget,
    NoSenderName,
}

impl BookingLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoCity => "the letter has no city to name",
            Self::NoTarget => "the letter has nobody to address",
            Self::NoSenderName => "this workspace has no name to sign with",
        }
    }
}

/// Subjects stay well under the executor's 220 cap — a subject a mail client
/// truncates is a subject nobody read.
const MAX_SUBJECT: usize = 160;

/// Composes the letter, or refuses with the fact that is missing.
///
/// # Errors
///
/// Returns the missing fact. Every refusal is recoverable by the operator —
/// which is why none of them is a silent fallback.
pub fn compose_booking_letter(
    input: &BookingLetterInput<'_>,
) -> Result<BookingLetter, BookingLetterRefusal> {
    let city = input.city.trim();
    let target = input.target_name.trim();
    let act = input.sender.act_name.trim();
    if city.is_empty() {
        return Err(BookingLetterRefusal::NoCity);
    }
    if target.is_empty() {
        return Err(BookingLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(BookingLetterRefusal::NoSenderName);
    }
    Ok(match input.phase {
        BookingOutreachPhase::Initial => initial(input, city, act),
        BookingOutreachPhase::FollowUp => follow_up(input, city, act),
    })
}

fn window_phrase(language: LetterLanguage, window: Option<(Date, Date)>) -> Option<String> {
    let (start, end) = window?;
    let en = |d: Date| {
        d.format(time::macros::format_description!(
            "[day] [month repr:short] [year]"
        ))
        .unwrap_or_else(|_| d.to_string())
    };
    let pl = |d: Date| {
        d.format(time::macros::format_description!("[day].[month].[year]"))
            .unwrap_or_else(|_| d.to_string())
    };
    Some(match language {
        LetterLanguage::English => format!("between {} and {}", en(start), en(end)),
        LetterLanguage::Polish => format!("w okresie {} – {}", pl(start), pl(end)),
    })
}

fn initial(input: &BookingLetterInput<'_>, city: &str, act: &str) -> BookingLetter {
    let mut lines = vec![
        match input.language {
            LetterLanguage::English => "Hi,".to_owned(),
            LetterLanguage::Polish => "Cześć,".to_owned(),
        },
        String::new(),
    ];
    if let Some(fact) = input
        .first_line_fact
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        lines.push(fact.to_owned());
        lines.push(String::new());
    }
    let window = window_phrase(input.language, input.proposed_window);
    lines.push(match (input.language, &window) {
        (LetterLanguage::English, Some(window)) => format!(
            "{} and we are putting together shows in {city} — {window} works on our \
             side. If {target} has a slot in that window, tell us what the night needs \
             and we will come back with a concrete offer.",
            introduction(input.sender, act, input.language),
            target = input.target_name.trim(),
        ),
        (LetterLanguage::English, None) => format!(
            "{} and we are putting together shows in {city}. If {target} has a window \
             in the coming months, tell us what the night needs and we will come back \
             with a concrete offer.",
            introduction(input.sender, act, input.language),
            target = input.target_name.trim(),
        ),
        (LetterLanguage::Polish, Some(window)) => format!(
            "{} i planujemy koncerty w {city} — {window} pasuje po naszej \
             stronie. Jeśli w tym okresie jest u Was wolny slot, napiszcie, czego \
             potrzebuje wieczór, a wrócimy z konkretną propozycją.",
            introduction(input.sender, act, input.language),
        ),
        (LetterLanguage::Polish, None) => format!(
            "{} i planujemy koncerty w {city}. Jeśli macie wolny termin w \
             najbliższych miesiącach, napiszcie, czego potrzebuje wieczór, a wrócimy z \
             konkretną propozycją.",
            introduction(input.sender, act, input.language),
        ),
    });
    lines.push(String::new());
    if let Some(site) = input
        .sender
        .site_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.push(match input.language {
            LetterLanguage::English => format!("More about us: {site}"),
            LetterLanguage::Polish => format!("Więcej o nas: {site}"),
        });
        lines.push(String::new());
    }
    lines.push(match input.language {
        LetterLanguage::English => "Best,".to_owned(),
        LetterLanguage::Polish => "Pozdrawiamy,".to_owned(),
    });
    lines.push(act.to_owned());
    let subject = match input.language {
        LetterLanguage::English => format!("{act} — booking in {city}"),
        LetterLanguage::Polish => format!("{act} — booking, {city}"),
    };
    BookingLetter {
        subject: truncate(subject, MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn follow_up(input: &BookingLetterInput<'_>, city: &str, act: &str) -> BookingLetter {
    let lines = [
        match input.language {
            LetterLanguage::English => "Hi,".to_owned(),
            LetterLanguage::Polish => "Cześć,".to_owned(),
        },
        String::new(),
        match input.language {
            LetterLanguage::English => format!(
                "A quick follow-up on our earlier letter — we are still looking at \
                 {city}. If the timing does not work on your side, a short \"no\" is \
                 just as useful as a yes."
            ),
            LetterLanguage::Polish => format!(
                "Wracamy do naszej wcześniejszej wiadomości — {city} nadal jest dla \
                 nas aktualne. Jeśli termin nie pasuje, krótkie \"nie\" jest równie \
                 pomocne."
            ),
        },
        String::new(),
        match input.language {
            LetterLanguage::English => "Best,".to_owned(),
            LetterLanguage::Polish => "Pozdrawiamy,".to_owned(),
        },
        act.to_owned(),
    ];
    let subject = match input.language {
        LetterLanguage::English => format!("{act} — {city}, following up"),
        LetterLanguage::Polish => format!("{act} — {city}, przypomnienie"),
    };
    BookingLetter {
        subject: truncate(subject, MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

/// "We are {act}{, a {style} act}{ from {home_city}}" — each part present only
/// when the tenant's own records say it.
fn introduction(sender: &SenderIdentity, act: &str, language: LetterLanguage) -> String {
    let style = sender
        .style
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let home = sender
        .home_city
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (language, style, home) {
        (LetterLanguage::English, Some(style), Some(home)) => {
            format!("We are {act}, a {style} act from {home},")
        }
        (LetterLanguage::English, Some(style), None) => {
            format!("We are {act}, a {style} act,")
        }
        (LetterLanguage::English, None, Some(home)) => format!("We are {act} from {home},"),
        (LetterLanguage::English, None, None) => format!("We are {act},"),
        (LetterLanguage::Polish, Some(style), Some(home)) => {
            // Nominative city in a parenthesis: see
            // `outreach_letter::introduction_pl`.
            format!("Jesteśmy {act} ({home}), gramy {style},")
        }
        (LetterLanguage::Polish, Some(style), None) => {
            format!("Jesteśmy {act}, gramy {style},")
        }
        (LetterLanguage::Polish, None, Some(home)) => format!("Jesteśmy {act} ({home}),"),
        (LetterLanguage::Polish, None, None) => format!("Jesteśmy {act},"),
    }
}

fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "VIRYA".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music".to_owned()),
        }
    }

    #[test]
    fn an_initial_letter_names_the_city_the_window_and_the_act() {
        let day = Date::from_calendar_date(2026, time::Month::November, 6).unwrap();
        let letter = compose_booking_letter(&BookingLetterInput {
            language: LetterLanguage::English,
            sender: &sender(),
            target_name: "Klub Liverpool",
            city: "Wrocław",
            proposed_window: Some((day, day + time::Duration::days(21))),
            first_line_fact: Some("12 shows in the last year"),
            phase: BookingOutreachPhase::Initial,
        })
        .unwrap();
        assert!(letter.subject.contains("Wrocław"));
        assert!(letter.body.contains("VIRYA"));
        assert!(letter.body.contains("modern metal"));
        assert!(letter.body.contains("12 shows in the last year"));
        assert!(letter.body.contains("6 Nov 2026"));
        assert!(letter.body.contains("virya.music"));
    }

    #[test]
    fn a_polish_room_gets_a_polish_letter() {
        let letter = compose_booking_letter(&BookingLetterInput {
            language: LetterLanguage::Polish,
            sender: &sender(),
            target_name: "Klub Liverpool",
            city: "Wrocław",
            proposed_window: None,
            first_line_fact: None,
            phase: BookingOutreachPhase::Initial,
        })
        .unwrap();
        assert!(letter.body.contains("Jesteśmy VIRYA"));
        assert!(letter.body.contains("Pozdrawiamy"));
    }

    #[test]
    fn a_followup_re_asks_without_reproposing_dates() {
        let letter = compose_booking_letter(&BookingLetterInput {
            language: LetterLanguage::English,
            sender: &sender(),
            target_name: "Klub Liverpool",
            city: "Wrocław",
            proposed_window: None,
            first_line_fact: None,
            phase: BookingOutreachPhase::FollowUp,
        })
        .unwrap();
        assert!(letter.body.contains("follow-up"));
        assert!(letter.subject.contains("following up"));
    }

    #[test]
    fn missing_facts_refuse_instead_of_filling_in() {
        let no_name = SenderIdentity::default();
        assert_eq!(
            compose_booking_letter(&BookingLetterInput {
                language: LetterLanguage::English,
                sender: &no_name,
                target_name: "X",
                city: "Wrocław",
                proposed_window: None,
                first_line_fact: None,
                phase: BookingOutreachPhase::Initial,
            }),
            Err(BookingLetterRefusal::NoSenderName)
        );
        assert_eq!(
            compose_booking_letter(&BookingLetterInput {
                language: LetterLanguage::English,
                sender: &sender(),
                target_name: "X",
                city: "  ",
                proposed_window: None,
                first_line_fact: None,
                phase: BookingOutreachPhase::Initial,
            }),
            Err(BookingLetterRefusal::NoCity)
        );
    }
}
