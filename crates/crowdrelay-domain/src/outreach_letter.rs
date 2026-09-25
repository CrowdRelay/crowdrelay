//! The letter an outreach target actually receives.
//!
//! Same rule as `booking_letter` and `gig_letter`: the words are composed
//! here, at the moment the action is written, so the approval shows exactly
//! what the curator, editor or station reads and the executor sends
//! `draft.body` verbatim. Before this existed the event carried
//! `template_key` and `evidence` and left the writing to whatever ran
//! downstream — which meant the operator approved "outreach contact: X" and
//! a stranger received a pitch nobody had read, with the pitch itself
//! resolved from environment variables outside any approval.
//!
//! The pitch is the tenant's own catalogue: the release plan that carries a
//! listen link. A workspace with no listenable release gets no letter — the
//! composer refuses rather than pitching "our latest release" at nothing.
//! Agents and labels are not composed here at all: §4h-12 routes them
//! through the approach path, where the published listing is the pitch.

use serde::{Deserialize, Serialize};

use crate::gig_letter::{LetterLanguage, SenderIdentity};
use crate::outreach::{OutreachPhase, OutreachTargetKind};

/// A finished letter — the wire shape every letter executor reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutreachLetter {
    pub subject: String,
    pub body: String,
}

/// Everything the letter is composed from.
#[derive(Clone, Debug)]
pub struct OutreachLetterInput<'a> {
    pub sender: &'a SenderIdentity,
    /// The target's display name — a playlist, a station, an editor.
    pub target_name: &'a str,
    /// What the target is being asked for — playlist consideration, airplay,
    /// coverage. Shapes the ask line.
    pub target_kind: OutreachTargetKind,
    /// The release being pitched — a title from the tenant's release plans.
    pub pitch_title: &'a str,
    /// Where the target listens to it.
    pub pitch_url: &'a str,
    pub phase: OutreachPhase,
    /// The language the recipient reads. See [`language_for_contact`].
    pub language: LetterLanguage,
}

/// The language to write to a contact in, from the one fact the registry
/// holds about where they are: the domain of their address. A `.pl` address
/// reads Polish; anything else, including a free-mail address that says
/// nothing about its owner, reads English.
///
/// In production on 2026-09-25, 99 of the act's 196 pitchable contacts had
/// a `.pl` address — Polish zines, radio shows and webzines that received an
/// English pitch from a Polish band. A whitelist, like
/// [`LetterLanguage::for_country`]: a guess we have no copy for would be an
/// English letter under a Polish label.
#[must_use]
pub fn language_for_contact(contact_email: &str) -> LetterLanguage {
    let domain = contact_email
        .rsplit_once('@')
        .map_or("", |(_, domain)| domain)
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.ends_with(".pl") {
        LetterLanguage::Polish
    } else {
        LetterLanguage::English
    }
}

/// Why a letter could not be composed — each a missing fact, shown to the
/// operator rather than translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachLetterRefusal {
    NoTarget,
    NoSenderName,
    /// No release plan carries a listen link — there is nothing to pitch.
    NoPitch,
    /// Agents and labels are approached through the listing, not pitched.
    RepresentationTarget,
}

impl OutreachLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTarget => "the letter has nobody to address",
            Self::NoSenderName => "this workspace has no name to sign with",
            Self::NoPitch => "no active release plan carries a listen link",
            Self::RepresentationTarget => {
                "agents and labels are approached through the listing, not pitched"
            }
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
pub fn compose_outreach_letter(
    input: &OutreachLetterInput<'_>,
) -> Result<OutreachLetter, OutreachLetterRefusal> {
    let target = input.target_name.trim();
    let act = input.sender.act_name.trim();
    let title = input.pitch_title.trim();
    let url = input.pitch_url.trim();
    if target.is_empty() {
        return Err(OutreachLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(OutreachLetterRefusal::NoSenderName);
    }
    let Some(ask) = purpose(input.target_kind) else {
        return Err(OutreachLetterRefusal::RepresentationTarget);
    };
    if title.is_empty() || url.is_empty() {
        return Err(OutreachLetterRefusal::NoPitch);
    }
    Ok(match (input.language, input.phase) {
        (LetterLanguage::English, OutreachPhase::Initial) => {
            initial(input, target, act, title, url, ask)
        }
        (LetterLanguage::English, OutreachPhase::FollowUp) => {
            follow_up(input, target, act, title, url, ask)
        }
        (LetterLanguage::Polish, OutreachPhase::Initial) => {
            initial_pl(input, target, act, title, url)
        }
        (LetterLanguage::Polish, OutreachPhase::FollowUp) => {
            follow_up_pl(input, target, act, title, url)
        }
    })
}

/// The Polish ask, completing "chcielibyśmy zaproponować Wam {title} …".
/// Same whitelist as [`purpose`]; the kinds `purpose` refuses never get here.
fn purpose_pl(kind: OutreachTargetKind) -> &'static str {
    match kind {
        OutreachTargetKind::Playlist => "do Waszej playlisty",
        OutreachTargetKind::Radio => "do anteny",
        OutreachTargetKind::Press => "do omówienia lub recenzji",
        OutreachTargetKind::Creator => "do wspólnego materiału albo prezentacji",
        OutreachTargetKind::SupportSlot => "i zapytać o granie jako support",
        OutreachTargetKind::Endorsement => "z prośbą o rekomendację",
        OutreachTargetKind::MediaPatronage => "z prośbą o patronat medialny",
        OutreachTargetKind::Agent | OutreachTargetKind::Label => "",
    }
}

fn initial_pl(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Dzień dobry, {target},"),
        String::new(),
        format!(
            "{} i chcielibyśmy zaproponować Wam {title} {}.",
            introduction_pl(input.sender, act),
            purpose_pl(input.target_kind),
        ),
        String::new(),
        format!("Do posłuchania: {url}"),
        String::new(),
        // Same promise as the English letter, said the way the band says it.
        "Jeśli to nie dla Was, nic nie szkodzi — wystarczy krótka odpowiedź \
         i nie będziemy się więcej odzywać."
            .to_owned(),
    ];
    lines.extend(sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn follow_up_pl(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Dzień dobry, {target},"),
        String::new(),
        format!(
            "Krótko wracamy do poprzedniej wiadomości w sprawie {title}. \
             Jeśli to nie dla Was, krótkie „nie” też bardzo nam pomoże."
        ),
        String::new(),
        format!("Do posłuchania: {url}"),
    ];
    lines.extend(sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Przypomnienie: {act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

/// "Piszemy w imieniu {act}{ — zespołu {style}}{ z miasta {home}}". The city
/// is named after "z miasta" so it stays in the nominative: "z Wrocław" is
/// not Polish, and declining a city name without a dictionary is a guess.
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
            format!("Piszemy w imieniu {act} — zespołu {style} z miasta {home} —")
        }
        (Some(style), None) => format!("Piszemy w imieniu {act} — zespołu {style} —"),
        (None, Some(home)) => format!("Piszemy w imieniu {act} z miasta {home}"),
        (None, None) => format!("Piszemy w imieniu {act}"),
    }
}

fn sign_off_pl(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Pozdrawiamy,".to_owned(), act.to_owned()];
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

/// What the target is being asked for, in the words the letter uses. `None`
/// for the kinds no pitch letter may be written to — the compose checks the
/// refusal, so a new kind answers for itself rather than pitching under a
/// wrong purpose.
fn purpose(kind: OutreachTargetKind) -> Option<&'static str> {
    Some(match kind {
        OutreachTargetKind::Playlist => "playlist consideration",
        OutreachTargetKind::Radio => "airplay",
        OutreachTargetKind::Press => "coverage",
        OutreachTargetKind::Creator => "a feature or collaboration",
        OutreachTargetKind::SupportSlot => "a support slot",
        OutreachTargetKind::Endorsement => "an endorsement",
        OutreachTargetKind::MediaPatronage => "media patronage",
        OutreachTargetKind::Agent | OutreachTargetKind::Label => return None,
    })
}

fn initial(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
    ask: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "{} and we would love to submit {title} for {ask}.",
            introduction(input.sender, act),
        ),
        String::new(),
        format!("Listen: {url}"),
        String::new(),
        // The recipient is a stranger reading a letter from a band, so the
        // promise is said the way the band would say it. It is still the
        // governor's promise: any reply ends the sequence, and at most one
        // follow-up goes to someone who never answers.
        "If it is not a fit, no worries — just say so and we will not follow up.".to_owned(),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn follow_up(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
    ask: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "A quick follow-up on the note below — {title} is still with us for {ask}. \
             If it is not a fit, a short \"no\" is just as useful as a yes."
        ),
        String::new(),
        format!("Listen: {url}"),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Follow-up: {act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
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

/// The closing block — the act's name and, where the tenant declared one,
/// its own site. No link line when there is no link to give.
fn sign_off(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Best,".to_owned(), act.to_owned()];
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

    fn input<'a>(
        sender: &'a SenderIdentity,
        kind: OutreachTargetKind,
        phase: OutreachPhase,
    ) -> OutreachLetterInput<'a> {
        OutreachLetterInput {
            sender,
            target_name: "Metal Playlists Weekly",
            target_kind: kind,
            pitch_title: "our new single \"Rytuał\"",
            pitch_url: "https://virya.music/f/rytual",
            phase,
            language: LetterLanguage::English,
        }
    }

    #[test]
    fn an_initial_pitch_names_the_target_the_release_and_the_ask() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Playlist,
            OutreachPhase::Initial,
        ))
        .expect("a complete input composes");
        assert!(letter.body.contains("Hi Metal Playlists Weekly,"));
        assert!(
            letter
                .body
                .contains("VIRYA, a modern metal act from Wrocław")
        );
        assert!(
            letter
                .body
                .contains("\"Rytuał\" for playlist consideration")
        );
        assert!(letter.body.contains("Listen: https://virya.music/f/rytual"));
        assert!(
            letter
                .body
                .contains("just say so and we will not follow up")
        );
        // A stranger reads this. It is a letter from a band, and it says so
        // in the band's words, not the machinery's.
        assert!(!letter.body.to_lowercase().contains("automat"));
        assert!(letter.body.ends_with("https://virya.music"));
        assert_eq!(letter.subject, "VIRYA — our new single \"Rytuał\"");
    }

    #[test]
    fn a_followup_re_asks_without_rewriting_the_pitch() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Press,
            OutreachPhase::FollowUp,
        ))
        .expect("a follow-up composes");
        assert!(letter.body.contains("quick follow-up"));
        assert!(letter.body.contains("for coverage"));
        assert!(letter.subject.starts_with("Follow-up:"));
    }

    #[test]
    fn missing_facts_refuse_instead_of_filling_in() {
        let sender = sender();
        let no_target = OutreachLetterInput {
            target_name: " ",
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        };
        assert_eq!(
            compose_outreach_letter(&no_target),
            Err(OutreachLetterRefusal::NoTarget)
        );
        let no_pitch = OutreachLetterInput {
            pitch_url: "",
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        };
        assert_eq!(
            compose_outreach_letter(&no_pitch),
            Err(OutreachLetterRefusal::NoPitch)
        );
        let no_name = SenderIdentity {
            act_name: String::new(),
            ..sender.clone()
        };
        let no_sender = input(&no_name, OutreachTargetKind::Press, OutreachPhase::Initial);
        assert_eq!(
            compose_outreach_letter(&no_sender),
            Err(OutreachLetterRefusal::NoSenderName)
        );
    }

    #[test]
    fn representation_targets_are_never_pitched() {
        let sender = sender();
        for kind in [OutreachTargetKind::Agent, OutreachTargetKind::Label] {
            assert_eq!(
                compose_outreach_letter(&input(&sender, kind, OutreachPhase::Initial)),
                Err(OutreachLetterRefusal::RepresentationTarget),
                "{kind:?} must refuse — the listing carries that approach"
            );
        }
    }

    #[test]
    fn a_pl_address_reads_polish_and_everything_else_english() {
        assert_eq!(
            language_for_contact("redakcja@metalrulez.pl"),
            LetterLanguage::Polish
        );
        assert_eq!(
            language_for_contact("Radio@Onet.PL."),
            LetterLanguage::Polish
        );
        assert_eq!(
            language_for_contact("zine@gmail.com"),
            LetterLanguage::English
        );
        assert_eq!(
            language_for_contact("booking@club.de"),
            LetterLanguage::English
        );
        // A `.pl` that is not the domain says nothing.
        assert_eq!(
            language_for_contact("jan.pl@example.com"),
            LetterLanguage::English
        );
        assert_eq!(
            language_for_contact("no-at-sign.pl"),
            LetterLanguage::English
        );
    }

    #[test]
    fn a_polish_pitch_is_polish_end_to_end() {
        let sender = sender();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            language: LetterLanguage::Polish,
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        })
        .expect("a complete input composes");
        assert!(
            letter
                .body
                .starts_with("Dzień dobry, Metal Playlists Weekly,")
        );
        assert!(letter.body.contains(
            "Piszemy w imieniu VIRYA — zespołu modern metal z miasta Wrocław — i chcielibyśmy \
             zaproponować Wam our new single \"Rytuał\" do omówienia lub recenzji."
        ));
        assert!(
            letter
                .body
                .contains("Do posłuchania: https://virya.music/f/rytual")
        );
        assert!(
            letter
                .body
                .ends_with("Pozdrawiamy,\nVIRYA\nhttps://virya.music")
        );
        assert!(
            !letter.body.contains("Hi "),
            "no English left in a Polish letter"
        );
        assert!(letter.body.contains("i nie będziemy się więcej odzywać"));
        assert_eq!(letter.subject, "VIRYA — our new single \"Rytuał\"");
    }

    #[test]
    fn a_polish_follow_up_says_so_in_the_subject() {
        let sender = sender();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            language: LetterLanguage::Polish,
            ..input(&sender, OutreachTargetKind::Radio, OutreachPhase::FollowUp)
        })
        .expect("a complete input composes");
        assert!(letter.subject.starts_with("Przypomnienie: VIRYA"));
        assert!(
            letter
                .body
                .contains("Krótko wracamy do poprzedniej wiadomości")
        );
    }

    #[test]
    fn every_pitchable_kind_has_a_polish_ask() {
        for kind in [
            OutreachTargetKind::Playlist,
            OutreachTargetKind::Radio,
            OutreachTargetKind::Press,
            OutreachTargetKind::Creator,
            OutreachTargetKind::SupportSlot,
            OutreachTargetKind::Endorsement,
            OutreachTargetKind::MediaPatronage,
        ] {
            assert!(purpose(kind).is_some());
            assert!(!purpose_pl(kind).is_empty(), "{kind:?} has no Polish ask");
        }
    }
}
