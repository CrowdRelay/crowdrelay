//! The letter a show opportunity sends: "we play {city} on {date}", to a
//! contact who can list, mention or patronise that one night.
//!
//! # Why this exists
//!
//! The supply refresh has written `event_autopilot` opportunities — one per
//! upcoming show per press, radio, creator and patronage contact — since the
//! event lifecycle landed, each keyed `event.press.v1` and friends. No letter
//! by that name was ever written. The candidate picked its template from the
//! target's kind alone, so every show opportunity composed the catalogue
//! pitch instead: "we would love to submit {album} for coverage". On
//! 2026-09-27 thirty-four of those sat in the approval queue for the Gorzów
//! show on 17 October, and not one of them mentioned Gorzów, a date or a
//! ticket. Three had already been sent.
//!
//! The letter below says what the opportunity is about. Who it goes to is
//! decided upstream (`outreach_supply`): only contacts whose address is in
//! the show's own country, because a show in Gorzów is not news to a festival
//! in Bratislava.
//!
//! Organisers are not written here. An organiser is asked for a slot, and
//! `outreach_letter`'s organiser templates already cite the next show.

use time::Date;

use crate::gig_letter::{LetterLanguage, SenderIdentity};
use crate::outreach::{OutreachPhase, OutreachTargetKind};
use crate::outreach_letter::{
    MAX_SUBJECT, OutreachLetter, format_show_date_en, format_show_date_pl, introduction,
    introduction_pl, salutation_name, sign_off, sign_off_pl, truncate,
};

/// Everything a show letter is composed from. Every field is a fact the
/// event row or the registry holds; nothing here is inferred.
#[derive(Clone, Debug)]
pub struct ShowPitchInput<'a> {
    pub sender: &'a SenderIdentity,
    pub target_name: &'a str,
    pub target_kind: OutreachTargetKind,
    pub phase: OutreachPhase,
    pub language: LetterLanguage,
    pub show_date: Date,
    /// The city the show is in, as the city registry names it.
    pub city: &'a str,
    /// The room, when the event row names one.
    pub venue: Option<&'a str>,
    /// Where to buy a ticket, when the event row carries a link.
    pub ticket_url: Option<&'a str>,
    /// The newest listen link, when there is one. A show letter stands on
    /// its date; the link is supporting material, not the subject.
    pub listen_url: Option<&'a str>,
}

/// Why a show letter could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowPitchRefusal {
    NoTarget,
    NoSenderName,
    NoCity,
    /// The contact's kind has no ask about a single show: a playlist, a
    /// support slot, an endorsement, an agent or a label. An organiser is
    /// answered by the organiser letter instead.
    NoShowAsk,
}

impl ShowPitchRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTarget => "the letter has nobody to address",
            Self::NoSenderName => "this workspace has no name to sign with",
            Self::NoCity => "the show has no city on record",
            Self::NoShowAsk => "this kind of contact has nothing to do with one show",
        }
    }
}

/// What the contact is asked to do with the show. `None` for every kind
/// that has no such ask — see [`ShowPitchRefusal::NoShowAsk`].
fn ask_en(kind: OutreachTargetKind) -> Option<&'static str> {
    Some(match kind {
        OutreachTargetKind::Press => {
            "Sharing it in case it fits your event listings or local coverage."
        }
        OutreachTargetKind::Radio => {
            "Sharing it in case it fits a mention on air or your event listings."
        }
        OutreachTargetKind::Creator => {
            "Sharing it in case it is a night you would like to film or feature."
        }
        OutreachTargetKind::MediaPatronage => {
            "We would like to ask whether you would take media patronage of it."
        }
        OutreachTargetKind::Playlist
        | OutreachTargetKind::SupportSlot
        | OutreachTargetKind::Endorsement
        | OutreachTargetKind::Organiser
        | OutreachTargetKind::Agent
        | OutreachTargetKind::Label => return None,
    })
}

fn ask_pl(kind: OutreachTargetKind) -> Option<&'static str> {
    Some(match kind {
        OutreachTargetKind::Press => {
            "Dajemy znać na wypadek, gdyby pasowało to do Waszego kalendarza wydarzeń albo relacji."
        }
        OutreachTargetKind::Radio => {
            "Dajemy znać na wypadek, gdyby pasowało to do zapowiedzi na antenie albo kalendarza wydarzeń."
        }
        OutreachTargetKind::Creator => {
            "Dajemy znać na wypadek, gdyby był to wieczór, który chcielibyście nagrać albo pokazać."
        }
        OutreachTargetKind::MediaPatronage => {
            "Chcielibyśmy zapytać, czy objęlibyście ten koncert patronatem medialnym."
        }
        OutreachTargetKind::Playlist
        | OutreachTargetKind::SupportSlot
        | OutreachTargetKind::Endorsement
        | OutreachTargetKind::Organiser
        | OutreachTargetKind::Agent
        | OutreachTargetKind::Label => return None,
    })
}

/// Composes the show letter, or refuses with the fact that is missing.
///
/// # Errors
///
/// Returns the missing fact; the caller leaves the draft empty and the
/// executor refuses an empty draft, so a refusal never sends.
pub fn compose_show_pitch_letter(
    input: &ShowPitchInput<'_>,
) -> Result<OutreachLetter, ShowPitchRefusal> {
    let target = salutation_name(input.target_name);
    let act = input.sender.act_name.trim();
    let city = input.city.trim();
    if target.is_empty() {
        return Err(ShowPitchRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(ShowPitchRefusal::NoSenderName);
    }
    if city.is_empty() {
        return Err(ShowPitchRefusal::NoCity);
    }
    let venue = input.venue.map(str::trim).filter(|v| !v.is_empty());
    let tickets = input.ticket_url.map(str::trim).filter(|v| !v.is_empty());
    let listen = input.listen_url.map(str::trim).filter(|v| !v.is_empty());
    match input.language {
        LetterLanguage::English => {
            let ask = ask_en(input.target_kind).ok_or(ShowPitchRefusal::NoShowAsk)?;
            let date = format_show_date_en(input.show_date);
            let place = venue.map_or_else(|| city.to_owned(), |venue| format!("{city} ({venue})"));
            let mut lines = vec![format!("Hi {target},"), String::new()];
            match input.phase {
                OutreachPhase::Initial => {
                    lines.push(format!(
                        "{} and we play {place} on {date}.",
                        introduction(input.sender, act),
                    ));
                    lines.push(ask.to_owned());
                }
                OutreachPhase::FollowUp => {
                    lines.push(format!(
                        "A quick follow-up on the note below about our show in {place} on {date}. \
                         If it is not a fit, a short \"no\" is just as useful as a yes."
                    ));
                }
            }
            push_links(&mut lines, tickets, "Tickets", listen, "Listen");
            if input.phase == OutreachPhase::Initial {
                lines.push(String::new());
                lines.push(
                    "If it is not a fit, no worries — just say so and we will not follow up."
                        .to_owned(),
                );
            }
            lines.extend(sign_off(input.sender, act));
            let subject = format!("{act} — {city}, {date}");
            Ok(OutreachLetter {
                subject: truncate(
                    match input.phase {
                        OutreachPhase::Initial => subject,
                        OutreachPhase::FollowUp => format!("Follow-up: {subject}"),
                    },
                    MAX_SUBJECT,
                ),
                body: lines.join("\n"),
            })
        }
        LetterLanguage::Polish => {
            let ask = ask_pl(input.target_kind).ok_or(ShowPitchRefusal::NoShowAsk)?;
            let date = format_show_date_pl(input.show_date);
            // The city stays in the nominative, the same rule
            // `introduction_pl` follows: declining a city name without a
            // dictionary is a guess. "gramy koncert w mieście Gorzów
            // Wielkopolski" was grammatical and read as a template; a colon
            // and a parenthesis are how an announcement carries the place.
            let (place, place_aside) = venue.map_or_else(
                || (format!(": {city}"), format!(" ({city})")),
                |venue| (format!(": {city}, {venue}"), format!(" ({city}, {venue})")),
            );
            let mut lines = vec!["Dzień dobry,".to_owned(), String::new()];
            match input.phase {
                OutreachPhase::Initial => {
                    lines.push(format!(
                        "{} i {date} gramy koncert{place}.",
                        introduction_pl(input.sender, act),
                    ));
                    lines.push(ask.to_owned());
                }
                OutreachPhase::FollowUp => {
                    lines.push(format!(
                        "Krótko wracamy do poprzedniej wiadomości o koncercie {date}{place_aside}. \
                         Jeśli to nie dla Was, krótkie „nie” też bardzo nam pomoże."
                    ));
                }
            }
            push_links(&mut lines, tickets, "Bilety", listen, "Do posłuchania");
            if input.phase == OutreachPhase::Initial {
                lines.push(String::new());
                lines.push(
                    "Jeśli to nie dla Was, nic nie szkodzi — wystarczy krótka odpowiedź \
                     i nie będziemy się więcej odzywać."
                        .to_owned(),
                );
            }
            lines.extend(sign_off_pl(input.sender, act));
            let subject = format!("{act} — koncert {date}, {city}");
            Ok(OutreachLetter {
                subject: truncate(
                    match input.phase {
                        OutreachPhase::Initial => subject,
                        OutreachPhase::FollowUp => format!("Przypomnienie: {subject}"),
                    },
                    MAX_SUBJECT,
                ),
                body: lines.join("\n"),
            })
        }
    }
}

fn push_links(
    lines: &mut Vec<String>,
    tickets: Option<&str>,
    tickets_label: &str,
    listen: Option<&str>,
    listen_label: &str,
) {
    if tickets.is_none() && listen.is_none() {
        return;
    }
    lines.push(String::new());
    if let Some(url) = tickets {
        lines.push(format!("{tickets_label}: {url}"));
    }
    if let Some(url) = listen {
        lines.push(format!("{listen_label}: {url}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "Virya".to_owned(),
            style: Some("metalcore, modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music".to_owned()),
        }
    }

    fn input<'a>(
        sender: &'a SenderIdentity,
        kind: OutreachTargetKind,
        language: LetterLanguage,
        phase: OutreachPhase,
    ) -> ShowPitchInput<'a> {
        ShowPitchInput {
            sender,
            target_name: "Radio Gorzów — redakcja",
            target_kind: kind,
            phase,
            language,
            show_date: Date::from_calendar_date(2026, time::Month::October, 17).unwrap(),
            city: "Gorzów Wielkopolski",
            venue: Some("MagnetOffOn"),
            ticket_url: Some("https://tickets.example/virya-gorzow"),
            listen_url: Some("https://open.spotify.com/album/x"),
        }
    }

    /// The failure this module exists for: a show opportunity composing a
    /// review request for the album. The letter names the show — city, date,
    /// room, tickets — and asks for nothing about a release.
    #[test]
    fn a_press_show_letter_is_about_the_show_not_the_album() {
        let sender = sender();
        let letter = compose_show_pitch_letter(&input(
            &sender,
            OutreachTargetKind::Press,
            LetterLanguage::English,
            OutreachPhase::Initial,
        ))
        .unwrap();
        assert_eq!(letter.subject, "Virya — Gorzów Wielkopolski, 17 Oct 2026");
        assert!(
            letter.body.starts_with("Hi Radio Gorzów,\n"),
            "{}",
            letter.body
        );
        assert!(
            letter
                .body
                .contains("we play Gorzów Wielkopolski (MagnetOffOn) on 17 Oct 2026")
        );
        assert!(
            letter
                .body
                .contains("Tickets: https://tickets.example/virya-gorzow")
        );
        assert!(!letter.body.contains("submit"), "{}", letter.body);
        assert!(!letter.body.contains("review"), "{}", letter.body);
    }

    #[test]
    fn a_polish_show_letter_keeps_the_city_in_the_nominative() {
        let sender = sender();
        let letter = compose_show_pitch_letter(&input(
            &sender,
            OutreachTargetKind::Radio,
            LetterLanguage::Polish,
            OutreachPhase::Initial,
        ))
        .unwrap();
        assert_eq!(
            letter.subject,
            "Virya — koncert 17 października 2026, Gorzów Wielkopolski"
        );
        assert!(letter.body.starts_with("Dzień dobry,\n"), "{}", letter.body);
        assert!(
            letter.body.contains(
                "i 17 października 2026 gramy koncert: Gorzów Wielkopolski, MagnetOffOn."
            ),
            "{}",
            letter.body
        );
        assert!(letter.body.contains("Bilety: "));
        assert!(letter.body.contains("zapowiedzi na antenie"));
    }

    #[test]
    fn a_follow_up_names_the_show_and_makes_no_new_promise() {
        let sender = sender();
        for language in [LetterLanguage::English, LetterLanguage::Polish] {
            let letter = compose_show_pitch_letter(&input(
                &sender,
                OutreachTargetKind::Press,
                language,
                OutreachPhase::FollowUp,
            ))
            .unwrap();
            assert!(
                letter.subject.contains("Gorzów Wielkopolski"),
                "{}",
                letter.subject
            );
            assert!(
                letter.subject.starts_with("Follow-up: ")
                    || letter.subject.starts_with("Przypomnienie: "),
                "{}",
                letter.subject
            );
        }
    }

    /// Kinds with no ask about one night refuse rather than borrow a
    /// release ask, and a show with no city refuses rather than write
    /// "we play  on 17 Oct".
    #[test]
    fn kinds_without_a_show_ask_and_cityless_shows_refuse() {
        let sender = sender();
        for kind in [
            OutreachTargetKind::Playlist,
            OutreachTargetKind::SupportSlot,
            OutreachTargetKind::Endorsement,
            OutreachTargetKind::Organiser,
        ] {
            assert_eq!(
                compose_show_pitch_letter(&input(
                    &sender,
                    kind,
                    LetterLanguage::English,
                    OutreachPhase::Initial
                )),
                Err(ShowPitchRefusal::NoShowAsk),
                "{kind:?}"
            );
        }
        let mut cityless = input(
            &sender,
            OutreachTargetKind::Press,
            LetterLanguage::English,
            OutreachPhase::Initial,
        );
        cityless.city = " ";
        assert_eq!(
            compose_show_pitch_letter(&cityless),
            Err(ShowPitchRefusal::NoCity)
        );
    }

    #[test]
    fn missing_links_write_no_link_lines() {
        let sender = sender();
        let mut bare = input(
            &sender,
            OutreachTargetKind::Press,
            LetterLanguage::English,
            OutreachPhase::Initial,
        );
        bare.ticket_url = None;
        bare.listen_url = None;
        bare.venue = None;
        let letter = compose_show_pitch_letter(&bare).unwrap();
        assert!(!letter.body.contains("Tickets:"));
        assert!(!letter.body.contains("Listen:"));
        assert!(
            letter
                .body
                .contains("we play Gorzów Wielkopolski on 17 Oct 2026")
        );
    }
}
