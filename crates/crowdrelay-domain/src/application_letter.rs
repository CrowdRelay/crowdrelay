//! The letter a festival or showcase actually receives.
//!
//! Same rule as `outreach_letter`, `booking_letter` and `gig_letter`: the
//! words are composed here, at the moment the action is written, so the
//! approval shows exactly what the organiser reads and the executor sends
//! `draft.body` verbatim. Before this existed the event carried the
//! opportunity's facts and left the writing to whatever ran downstream —
//! which meant the operator approved "send a show application" and the
//! organiser received a pitch nobody had read, assembled from environment
//! variables outside any approval.
//!
//! The letter says only what the tenant's own records say: the act's name,
//! its declared style, the city it has played most, its own site, and —
//! when a release plan carries a listen link — where to hear the music.
//! Missing facts shorten the letter or refuse it; nothing is invented.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::gig_letter::{LetterLanguage, SenderIdentity};
use crate::live_opportunities::LiveOpportunityKind;

/// A finished letter — the wire shape every letter executor reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationLetter {
    pub subject: String,
    pub body: String,
}

/// Everything the letter is composed from.
#[derive(Clone, Debug)]
pub struct ApplicationLetterInput<'a> {
    /// Which language the organiser reads — the travel band decides:
    /// `poland` writes Polish, every other band writes English.
    pub language: LetterLanguage,
    pub sender: &'a SenderIdentity,
    /// The opportunity's own title — "Summerfest 2027 open call".
    pub opportunity_title: &'a str,
    /// Who runs it — the festival, the showcase, the contest.
    pub organization: &'a str,
    pub kind: LiveOpportunityKind,
    /// When the call closes; carried into the letter only when set.
    pub deadline: Option<OffsetDateTime>,
    /// The release being pointed at, when one carries a listen link.
    pub pitch_title: Option<&'a str>,
    /// Where the organiser listens to it.
    pub pitch_url: Option<&'a str>,
}

/// Why a letter could not be composed — each a missing fact, shown to the
/// operator rather than translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationLetterRefusal {
    /// The opportunity row carried no title — nothing to apply to by name.
    NoTitle,
    /// The opportunity row carried no organiser — nobody to address.
    NoOrganization,
    NoSenderName,
}

impl ApplicationLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTitle => "the opportunity carries no title",
            Self::NoOrganization => "the opportunity names no organiser",
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
pub fn compose_application_letter(
    input: &ApplicationLetterInput<'_>,
) -> Result<ApplicationLetter, ApplicationLetterRefusal> {
    let title = input.opportunity_title.trim();
    let organization = input.organization.trim();
    let act = input.sender.act_name.trim();
    if title.is_empty() {
        return Err(ApplicationLetterRefusal::NoTitle);
    }
    if organization.is_empty() {
        return Err(ApplicationLetterRefusal::NoOrganization);
    }
    if act.is_empty() {
        return Err(ApplicationLetterRefusal::NoSenderName);
    }
    Ok(match input.language {
        LetterLanguage::Polish => polish(input, title, organization, act),
        LetterLanguage::English => english(input, title, organization, act),
    })
}

/// What the act is applying for, in the words the letter uses.
fn ask(kind: LiveOpportunityKind, language: LetterLanguage) -> &'static str {
    match (kind, language) {
        (LiveOpportunityKind::Festival, LetterLanguage::English) => "a slot at",
        (LiveOpportunityKind::Showcase, LetterLanguage::English) => "a showcase slot at",
        (LiveOpportunityKind::ReviewContest, LetterLanguage::English) => "a place in",
        (LiveOpportunityKind::SupportSlot, LetterLanguage::English) => "a support slot at",
        (LiveOpportunityKind::Festival, LetterLanguage::Polish) => "slot na",
        (LiveOpportunityKind::Showcase, LetterLanguage::Polish) => "slot showcase na",
        (LiveOpportunityKind::ReviewContest, LetterLanguage::Polish) => "miejsce w",
        (LiveOpportunityKind::SupportSlot, LetterLanguage::Polish) => "slot supportowy na",
    }
}

fn english(
    input: &ApplicationLetterInput<'_>,
    title: &str,
    organization: &str,
    act: &str,
) -> ApplicationLetter {
    let mut lines = vec![
        format!("Hello {organization},"),
        String::new(),
        format!(
            "{} and we would like to apply for {} {title}.",
            introduction(input.sender, act),
            ask(input.kind, LetterLanguage::English),
        ),
    ];
    if let Some(deadline) = input.deadline {
        lines.push(String::new());
        lines.push(format!(
            "We understand applications close {} — if a different format or earlier date helps, tell us.",
            rfc3339_date(deadline),
        ));
    }
    push_listen(&mut lines, input, LetterLanguage::English);
    lines.push(String::new());
    lines.push(
        "If the fit is not there, a short reply either way is genuinely useful — \
         and we can send stage plans, tech rider or further material immediately."
            .to_owned(),
    );
    lines.extend(sign_off(input.sender, act, LetterLanguage::English));
    ApplicationLetter {
        subject: truncate(format!("{act} — application for {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn polish(
    input: &ApplicationLetterInput<'_>,
    title: &str,
    organization: &str,
    act: &str,
) -> ApplicationLetter {
    let mut lines = vec![
        format!("Dzień dobry, {organization},"),
        String::new(),
        format!(
            "{} i chcielibyśmy zgłosić się na {} {title}.",
            introduction_pl(input.sender, act),
            ask(input.kind, LetterLanguage::Polish),
        ),
    ];
    if let Some(deadline) = input.deadline {
        lines.push(String::new());
        lines.push(format!(
            "Rozumiemy, że zgłoszenia zamykają się {} — jeśli inny format lub wcześniejszy termin jest wygodniejszy, dajcie znać.",
            rfc3339_date(deadline),
        ));
    }
    push_listen(&mut lines, input, LetterLanguage::Polish);
    lines.push(String::new());
    lines.push(
        "Jeśli to do Was nie pasuje, krótka odpowiedź w którąkolwiek stronę jest \
         równie pomocna — scenariusz, rider techniczny i dodatkowe materiały \
         możemy dosłać od razu."
            .to_owned(),
    );
    lines.extend(sign_off(input.sender, act, LetterLanguage::Polish));
    ApplicationLetter {
        subject: truncate(format!("{act} — zgłoszenie na {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

/// The listen line — only when a release plan carries a link. An
/// application with nothing to hear is weaker, not wrong, so it shortens
/// rather than refuses.
fn push_listen(
    lines: &mut Vec<String>,
    input: &ApplicationLetterInput<'_>,
    language: LetterLanguage,
) {
    let (Some(title), Some(url)) = (
        input.pitch_title.map(str::trim).filter(|t| !t.is_empty()),
        input.pitch_url.map(str::trim).filter(|u| !u.is_empty()),
    ) else {
        return;
    };
    lines.push(String::new());
    lines.push(match language {
        LetterLanguage::English => format!("Listen ({title}): {url}"),
        LetterLanguage::Polish => format!("Posłuchaj ({title}): {url}"),
    });
}

/// "I am writing from {act}{, a {style} act}{ from {home_city}}" — each part
/// present only when the tenant's own records say it.
fn introduction(sender: &SenderIdentity, act: &str) -> String {
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
    match (style, home) {
        (Some(style), Some(home)) => {
            format!("I am writing from {act}, a {style} act from {home},")
        }
        (Some(style), None) => format!("I am writing from {act}, a {style} act,"),
        (None, Some(home)) => format!("I am writing from {act}, from {home},"),
        (None, None) => format!("I am writing from {act},"),
    }
}

fn introduction_pl(sender: &SenderIdentity, act: &str) -> String {
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
    match (style, home) {
        (Some(style), Some(home)) => {
            // Nominative city in a parenthesis, style after "grającego": see
            // `outreach_letter::introduction_pl`.
            format!("Piszemy w imieniu {act} ({home}) — zespołu grającego {style},")
        }
        (Some(style), None) => format!("Piszemy w imieniu {act} — zespołu grającego {style},"),
        (None, Some(home)) => format!("Piszemy w imieniu {act} ({home}),"),
        (None, None) => format!("Piszemy w imieniu {act},"),
    }
}

/// The closing block — the act's name and, where the tenant declared one,
/// its own site. No link line when there is no link to give.
fn sign_off(sender: &SenderIdentity, act: &str, language: LetterLanguage) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        match language {
            LetterLanguage::English => "Best,".to_owned(),
            LetterLanguage::Polish => "Pozdrawiamy,".to_owned(),
        },
        act.to_owned(),
    ];
    if let Some(url) = sender
        .site_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        lines.push(url.to_owned());
    }
    lines
}

fn rfc3339_date(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .ok()
        .and_then(|s| s.split('T').next().map(str::to_owned))
        .unwrap_or_else(|| "soon".to_owned())
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
    use time::macros::datetime;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "VIRYA".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music".to_owned()),
        }
    }

    fn input<'a>(
        sender: &'a SenderIdentity,
        language: LetterLanguage,
    ) -> ApplicationLetterInput<'a> {
        ApplicationLetterInput {
            language,
            sender,
            opportunity_title: "Summerfest 2027 open call",
            organization: "Summerfest",
            kind: LiveOpportunityKind::Festival,
            deadline: Some(datetime!(2027-02-01 23:59 UTC)),
            pitch_title: Some("our new single \"Rytuał\""),
            pitch_url: Some("https://virya.music/f/rytual"),
        }
    }

    #[test]
    fn an_application_names_the_opportunity_the_organiser_and_the_act() {
        let sender = sender();
        let letter = compose_application_letter(&input(&sender, LetterLanguage::English))
            .expect("a complete input composes");
        assert!(letter.body.contains("Hello Summerfest,"));
        assert!(
            letter
                .body
                .contains("VIRYA, a modern metal act from Wrocław")
        );
        assert!(
            letter
                .body
                .contains("apply for a slot at Summerfest 2027 open call")
        );
        assert!(letter.body.contains("2027-02-01"));
        assert!(letter.body.contains("Listen (our new single"));
        assert!(letter.body.ends_with("https://virya.music"));
        assert_eq!(
            letter.subject,
            "VIRYA — application for Summerfest 2027 open call"
        );
    }

    #[test]
    fn polish_letters_read_polish() {
        let sender = sender();
        let letter = compose_application_letter(&input(&sender, LetterLanguage::Polish))
            .expect("a polish input composes");
        assert!(letter.body.contains("Dzień dobry, Summerfest,"));
        assert!(
            letter
                .body
                .contains("Piszemy w imieniu VIRYA (Wrocław) — zespołu grającego modern metal,")
        );
        assert!(
            !letter.body.contains(" fit"),
            "no anglicism in a Polish letter"
        );
        assert!(letter.body.contains("zgłosić się na slot na"));
        assert!(letter.body.contains("Posłuchaj (our new single"));
        assert_eq!(
            letter.subject,
            "VIRYA — zgłoszenie na Summerfest 2027 open call"
        );
    }

    #[test]
    fn a_letter_without_a_release_still_applies() {
        let sender = sender();
        let no_pitch = ApplicationLetterInput {
            pitch_title: None,
            pitch_url: None,
            ..input(&sender, LetterLanguage::English)
        };
        let letter =
            compose_application_letter(&no_pitch).expect("no release only shortens the letter");
        assert!(!letter.body.contains("Listen"));
        assert!(letter.body.contains("apply for a slot at"));
    }

    #[test]
    fn missing_facts_refuse_instead_of_filling_in() {
        let sender = sender();
        let no_title = ApplicationLetterInput {
            opportunity_title: " ",
            ..input(&sender, LetterLanguage::English)
        };
        assert_eq!(
            compose_application_letter(&no_title),
            Err(ApplicationLetterRefusal::NoTitle)
        );
        let no_org = ApplicationLetterInput {
            organization: "",
            ..input(&sender, LetterLanguage::English)
        };
        assert_eq!(
            compose_application_letter(&no_org),
            Err(ApplicationLetterRefusal::NoOrganization)
        );
        let no_name = SenderIdentity {
            act_name: String::new(),
            ..sender.clone()
        };
        assert_eq!(
            compose_application_letter(&input(&no_name, LetterLanguage::English)),
            Err(ApplicationLetterRefusal::NoSenderName)
        );
    }

    #[test]
    fn every_kind_has_words_in_both_languages() {
        let sender = sender();
        for kind in [
            LiveOpportunityKind::Festival,
            LiveOpportunityKind::Showcase,
            LiveOpportunityKind::ReviewContest,
            LiveOpportunityKind::SupportSlot,
        ] {
            for language in [LetterLanguage::English, LetterLanguage::Polish] {
                let i = ApplicationLetterInput {
                    kind,
                    ..input(&sender, language)
                };
                assert!(
                    compose_application_letter(&i).is_ok(),
                    "{kind:?}/{language:?} must compose"
                );
            }
        }
    }
}
