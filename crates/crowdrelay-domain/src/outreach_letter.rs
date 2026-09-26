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
use time::OffsetDateTime;

use crate::gig_letter::{LetterLanguage, SenderIdentity};
use crate::outreach::{OutreachPhase, OutreachTargetKind};
use time::Date;

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
    /// The act's nearest confirmed show — date and the city it is in. An
    /// organiser reads "they have dates on the books" as the reason the ask
    /// is serious; every other kind ignores it. `None` writes no citation —
    /// a gig request with no show on record still reads true.
    pub next_show: Option<(Date, String)>,
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

/// The name a letter greets: the contact's display name without the role
/// the contact list appended to it. On 2026-09-26 drafts opened "Hi Kamil
/// Dráb — hudební dramaturg," and "Dzień dobry, Rockowa Noc — organizator,":
/// the list's " — role" suffix, read aloud as part of the name.
#[must_use]
pub fn salutation_name(display_name: &str) -> &str {
    let name = display_name.trim();
    [" — ", " – ", " - ", " | "]
        .iter()
        .filter_map(|separator| name.find(separator))
        .min()
        .and_then(|end| name.get(..end))
        .map_or(name, str::trim)
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
    let target = salutation_name(input.target_name);
    let act = input.sender.act_name.trim();
    let title = input.pitch_title.trim();
    let url = input.pitch_url.trim();
    if target.is_empty() {
        return Err(OutreachLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(OutreachLetterRefusal::NoSenderName);
    }
    // An organiser is asked for a slot, not a review — the ask names the act
    // and its calendar, so the propose-a-release skeleton does not fit. The
    // listen link still carries the newest material; a missing one refuses.
    if input.target_kind == OutreachTargetKind::Organiser {
        if url.is_empty() {
            return Err(OutreachLetterRefusal::NoPitch);
        }
        return Ok(match (input.language, input.phase) {
            (LetterLanguage::English, OutreachPhase::Initial) => {
                organiser_initial_en(input, target, act)
            }
            (LetterLanguage::English, OutreachPhase::FollowUp) => {
                organiser_follow_up_en(input, target, act)
            }
            (LetterLanguage::Polish, OutreachPhase::Initial) => {
                organiser_initial_pl(input, target, act)
            }
            (LetterLanguage::Polish, OutreachPhase::FollowUp) => {
                organiser_follow_up_pl(input, target, act)
            }
        });
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
        (LetterLanguage::Polish, OutreachPhase::Initial) => initial_pl(input, act, title, url),
        (LetterLanguage::Polish, OutreachPhase::FollowUp) => follow_up_pl(input, act, title, url),
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
        // An organiser's ask lives in its own template — this arm answers so
        // the match stays whole, and is unreachable by construction.
        OutreachTargetKind::Organiser | OutreachTargetKind::Agent | OutreachTargetKind::Label => "",
    }
}

fn initial_pl(
    input: &OutreachLetterInput<'_>,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        // A Polish letter greets without the name: "Dzień dobry, Anna
        // Laskiewicz" is a form field read aloud, and declining a name into
        // the vocative without a dictionary is a guess.
        "Dzień dobry,".to_owned(),
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
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        // A Polish letter greets without the name: "Dzień dobry, Anna
        // Laskiewicz" is a form field read aloud, and declining a name into
        // the vocative without a dictionary is a guess.
        "Dzień dobry,".to_owned(),
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
        // Composed by the organiser templates, not the release skeleton.
        OutreachTargetKind::Organiser | OutreachTargetKind::Agent | OutreachTargetKind::Label => {
            return None;
        }
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

/// The organiser ask: a slot, not a review. The act's next confirmed show is
/// cited when the calendar carries one — "we play {city} on {date}" is the
/// proof the request is real, and the fact a contested gig made letters
/// claim the band lived there is why the sentence names where the show is,
/// never where the band is from.
fn organiser_initial_en(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "{} and we would love to be considered for a slot on your bill.",
            introduction(input.sender, act),
        ),
    ];
    if let Some((date, city)) = &input.next_show {
        let when_where = match city.trim() {
            "" => format_show_date_en(*date),
            city => format!("{}, {}", format_show_date_en(*date), city),
        };
        lines.push(String::new());
        lines.push(format!("Next confirmed show: {when_where}."));
    }
    lines.push(String::new());
    lines.push(format!("Listen: {}", input.pitch_url.trim()));
    lines.push(String::new());
    lines
        .push("If it is not a fit, no worries — just say so and we will not follow up.".to_owned());
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — gig request"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn organiser_follow_up_en(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        "A quick follow-up on the note below — the offer to play stands. \
         If it is not a fit, a short \"no\" is just as useful as a yes."
            .to_owned(),
        String::new(),
        format!("Listen: {}", input.pitch_url.trim()),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Follow-up: {act} — gig request"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn organiser_initial_pl(
    input: &OutreachLetterInput<'_>,
    _target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        // Same vocative rule as the release letter: "Dzień dobry," alone.
        "Dzień dobry,".to_owned(),
        String::new(),
        format!(
            "{} i chcielibyśmy zapytać o możliwość zagrania u Was.",
            introduction_pl(input.sender, act),
        ),
    ];
    if let Some((date, city)) = &input.next_show {
        let when_where = match city.trim() {
            "" => format_show_date_pl(*date),
            city => format!("{}, {}", format_show_date_pl(*date), city),
        };
        lines.push(String::new());
        lines.push(format!("Najbliższy potwierdzony koncert: {when_where}."));
    }
    lines.push(String::new());
    lines.push(format!("Do posłuchania: {}", input.pitch_url.trim()));
    lines.push(String::new());
    lines.push(
        "Jeśli to nie dla Was, nic nie szkodzi — wystarczy krótka odpowiedź \
         i nie będziemy się więcej odzywać."
            .to_owned(),
    );
    lines.extend(sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — propozycja koncertu"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn organiser_follow_up_pl(
    input: &OutreachLetterInput<'_>,
    _target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        "Dzień dobry,".to_owned(),
        String::new(),
        "Krótko wracamy do poprzedniej wiadomości — propozycja zagrania u Was \
         nadal stoi. Jeśli to nie dla Was, krótkie „nie” też bardzo nam pomoże."
            .to_owned(),
        String::new(),
        format!("Do posłuchania: {}", input.pitch_url.trim()),
    ];
    lines.extend(sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(
            format!("Przypomnienie: {act} — propozycja koncertu"),
            MAX_SUBJECT,
        ),
        body: lines.join("\n"),
    }
}

/// "17 Oct 2026" — day, the month's short English name, the year.
fn format_show_date_en(date: Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .get(usize::from(u8::from(date.month()) - 1))
        .copied()
        .unwrap_or_default();
    format!("{} {} {}", date.day(), month, date.year())
}

/// "17 października 2026" — the Polish month in the genitive, as a date is
/// read after a number, with the year.
fn format_show_date_pl(date: Date) -> String {
    const MONTHS: [&str; 12] = [
        "stycznia",
        "lutego",
        "marca",
        "kwietnia",
        "maja",
        "czerwca",
        "lipca",
        "sierpnia",
        "września",
        "października",
        "listopada",
        "grudnia",
    ];
    let month = MONTHS
        .get(usize::from(u8::from(date.month()) - 1))
        .copied()
        .unwrap_or_default();
    format!("{} {} {}", date.day(), month, date.year())
}

/// Everything a thread follow-up is composed from.
///
/// Separate from `OutreachLetterInput` because this letter is *about* a gap:
/// the import kept the date of the act's handwritten message and nothing
/// else, so the letter may name that date and no more. Giving it the pitch
/// fields would invite a sentence the record cannot support.
#[derive(Clone, Debug)]
pub struct ThreadFollowUpInput<'a> {
    pub sender: &'a SenderIdentity,
    /// The target's display name — a playlist, a station, an editor.
    pub target_name: &'a str,
    /// The mailbox this goes to — the only address signal a target carries,
    /// and what picks the language.
    pub contact_email: &'a str,
    /// When the act's handwritten message went out, from the import ledger.
    pub thread_started_at: OffsetDateTime,
}

/// Composes the one nudge a hand-started thread gets, in the contact's
/// language.
///
/// The letter never says what the first message asked for, because the
/// import does not record it — a letter that guessed would be the machine
/// inventing words under the operator's name.
///
/// # Errors
///
/// Returns the missing fact, same contract as `compose_outreach_letter`.
pub fn compose_thread_followup_letter(
    input: &ThreadFollowUpInput<'_>,
) -> Result<OutreachLetter, OutreachLetterRefusal> {
    let target = salutation_name(input.target_name);
    let act = input.sender.act_name.trim();
    if target.is_empty() {
        return Err(OutreachLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(OutreachLetterRefusal::NoSenderName);
    }
    Ok(match language_for_contact(input.contact_email) {
        LetterLanguage::Polish => thread_followup_pl(input, target, act),
        LetterLanguage::English => thread_followup_en(input, target, act),
    })
}

fn thread_followup_en(input: &ThreadFollowUpInput<'_>, target: &str, act: &str) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "Following up on my message from {} — happy to send anything \
             that makes a reply easier.",
            format_thread_date_en(input.thread_started_at),
        ),
        String::new(),
        "If now is not the time, a short \"no\" helps just as much.".to_owned(),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Following up — {act}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn thread_followup_pl(input: &ThreadFollowUpInput<'_>, target: &str, act: &str) -> OutreachLetter {
    let mut lines = vec![
        format!("Cześć {target},"),
        String::new(),
        format!(
            "Nawiązuję do mojej wiadomości z {} — chętnie doszlę wszystko, \
             co ułatwi odpowiedź.",
            format_thread_date_pl(input.thread_started_at),
        ),
        String::new(),
        "Jeśli to nie ten moment, krótkie „nie” też bardzo pomaga.".to_owned(),
    ];
    lines.extend(thread_sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Nawiązanie — {act}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

/// "12 Sep" — day number and the month's short English name.
fn format_thread_date_en(at: OffsetDateTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .get(usize::from(u8::from(at.month()) - 1))
        .copied()
        .unwrap_or_default();
    format!("{} {}", at.day(), month)
}

/// "12 września" — the Polish month names in the genitive, which is how a
/// date is read after "z".
fn format_thread_date_pl(at: OffsetDateTime) -> String {
    const MONTHS: [&str; 12] = [
        "stycznia",
        "lutego",
        "marca",
        "kwietnia",
        "maja",
        "czerwca",
        "lipca",
        "sierpnia",
        "września",
        "października",
        "listopada",
        "grudnia",
    ];
    let month = MONTHS
        .get(usize::from(u8::from(at.month()) - 1))
        .copied()
        .unwrap_or_default();
    format!("{} {}", at.day(), month)
}

/// The Polish closing block — same shape as `sign_off`, in the language the
/// rest of the letter is in.
fn thread_sign_off_pl(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Pozdrawiam,".to_owned(), act.to_owned()];
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
            next_show: None,
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

    fn thread_input<'a>(sender: &'a SenderIdentity, email: &'a str) -> ThreadFollowUpInput<'a> {
        ThreadFollowUpInput {
            sender,
            target_name: "Radio Zet",
            contact_email: email,
            thread_started_at: OffsetDateTime::from_unix_timestamp(1_758_000_000)
                .expect("a fixed timestamp"),
        }
    }

    #[test]
    fn a_thread_followup_names_the_message_date_and_no_more() {
        let sender = sender();
        // 1_758_000_000 is 2025-09-16.
        let letter = compose_thread_followup_letter(&thread_input(&sender, "music@zine.example"))
            .expect("a complete input composes");
        assert!(letter.body.contains("Hi Radio Zet,"));
        assert!(letter.body.contains("my message from 16 Sep"));
        assert!(!letter.body.contains("Listen:"));
        assert_eq!(letter.subject, "Following up — VIRYA");
    }

    #[test]
    fn a_polish_mailbox_gets_the_polish_followup() {
        let sender = sender();
        let letter = compose_thread_followup_letter(&thread_input(&sender, "redakcja@radio.pl"))
            .expect("a complete input composes");
        assert!(letter.body.contains("Cześć Radio Zet,"));
        assert!(letter.body.contains("wiadomości z 16 września"));
        assert!(letter.body.contains("Pozdrawiam,"));
        assert_eq!(letter.subject, "Nawiązanie — VIRYA");
        // And the same sender, an international mailbox: English.
        let en = compose_thread_followup_letter(&thread_input(&sender, "desk@station.fm"))
            .expect("a complete input composes");
        assert!(en.body.contains("Following up on my message"));
    }

    #[test]
    fn a_thread_followup_still_refuses_anonymous() {
        let sender = sender();
        let no_target = ThreadFollowUpInput {
            target_name: " ",
            ..thread_input(&sender, "music@zine.example")
        };
        assert_eq!(
            compose_thread_followup_letter(&no_target),
            Err(OutreachLetterRefusal::NoTarget)
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
    fn the_greeting_drops_the_role_the_contact_list_appended() {
        assert_eq!(
            salutation_name("Kamil Dráb — hudební dramaturg"),
            "Kamil Dráb"
        );
        assert_eq!(salutation_name("Metal Noise - review blog"), "Metal Noise");
        assert_eq!(salutation_name("  Radio 357  "), "Radio 357");
        assert_eq!(
            salutation_name("Rock-Radio"),
            "Rock-Radio",
            "a hyphen inside a name stays"
        );
        let sender = sender();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            target_name: "Kamil Dráb — hudební dramaturg",
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        })
        .expect("composes");
        assert!(
            letter.body.starts_with("Hi Kamil Dráb,\n"),
            "{}",
            letter.body
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
        assert!(letter.body.starts_with("Dzień dobry,\n"));
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
        // An organiser answers to its own templates, not the release ask —
        // the loops above cover the kinds `purpose` and `purpose_pl` serve.
        assert!(purpose(OutreachTargetKind::Organiser).is_none());
        assert!(purpose_pl(OutreachTargetKind::Organiser).is_empty());
    }

    #[test]
    fn an_organiser_is_asked_to_play_not_to_review() {
        let sender = sender();
        let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            target_name: "uROCK Młodych — organizator",
            target_kind: OutreachTargetKind::Organiser,
            next_show: Some((show, "Gorzów Wielkopolski".to_owned())),
            ..input(
                &sender,
                OutreachTargetKind::Organiser,
                OutreachPhase::Initial,
            )
        })
        .expect("a complete input composes");
        assert!(
            letter.body.starts_with("Hi uROCK Młodych,"),
            "{}",
            letter.body
        );
        assert!(
            letter
                .body
                .contains("VIRYA, a modern metal act from Wrocław")
        );
        assert!(letter.body.contains("considered for a slot on your bill"));
        assert!(
            letter
                .body
                .contains("Next confirmed show: 17 Oct 2026, Gorzów Wielkopolski."),
            "{}",
            letter.body
        );
        assert!(letter.body.contains("Listen: https://virya.music/f/rytual"));
        assert!(letter.body.contains("will not follow up"));
        assert!(letter.body.ends_with("https://virya.music"));
        assert_eq!(letter.subject, "VIRYA — gig request");
        // The failure this kind exists to end: no review request to a
        // festival organiser.
        let lower = letter.body.to_lowercase();
        assert!(!lower.contains("review"), "{lower}");
        assert!(!lower.contains("coverage"), "{lower}");
        assert!(!lower.contains("submit"), "{lower}");
        // The city named in the letter is where the band is *from* — the
        // show city appears only inside the citation.
        assert!(!letter.body.contains("act from Gorzów"), "{}", letter.body);
    }

    #[test]
    fn an_organiser_letter_without_a_booked_show_still_reads_true() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::Initial,
        ))
        .expect("a gig ask needs no citation");
        assert!(letter.body.contains("considered for a slot on your bill"));
        assert!(!letter.body.contains("confirmed show"), "{}", letter.body);
        // A show with no city on record cites the date alone.
        let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            next_show: Some((show, String::new())),
            ..input(
                &sender,
                OutreachTargetKind::Organiser,
                OutreachPhase::Initial,
            )
        })
        .expect("composes");
        assert!(
            letter.body.contains("Next confirmed show: 17 Oct 2026."),
            "{}",
            letter.body
        );
    }

    #[test]
    fn a_polish_organiser_is_asked_for_a_slot_in_polish() {
        let sender = sender();
        let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
        let letter = compose_outreach_letter(&OutreachLetterInput {
            language: LetterLanguage::Polish,
            next_show: Some((show, "Gorzów Wielkopolski".to_owned())),
            ..input(
                &sender,
                OutreachTargetKind::Organiser,
                OutreachPhase::Initial,
            )
        })
        .expect("a complete input composes");
        assert!(letter.body.starts_with("Dzień dobry,\n"));
        assert!(letter.body.contains(
            "Piszemy w imieniu VIRYA — zespołu modern metal z miasta Wrocław — i chcielibyśmy \
             zapytać o możliwość zagrania u Was."
        ));
        assert!(letter.body.contains(
            "Najbliższy potwierdzony koncert: 17 października 2026, Gorzów Wielkopolski."
        ));
        assert!(
            letter
                .body
                .contains("Do posłuchania: https://virya.music/f/rytual")
        );
        assert!(letter.body.contains("i nie będziemy się więcej odzywać"));
        assert!(
            letter
                .body
                .ends_with("Pozdrawiamy,\nVIRYA\nhttps://virya.music")
        );
        assert!(!letter.body.contains("recenzj"), "{}", letter.body);
        assert!(!letter.body.contains("omówienia"), "{}", letter.body);
        assert_eq!(letter.subject, "VIRYA — propozycja koncertu");
    }

    #[test]
    fn an_organiser_follow_up_re_asks_the_slot() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::FollowUp,
        ))
        .expect("a follow-up composes");
        assert!(letter.body.contains("the offer to play stands"));
        assert_eq!(letter.subject, "Follow-up: VIRYA — gig request");
        let polish = compose_outreach_letter(&OutreachLetterInput {
            language: LetterLanguage::Polish,
            ..input(
                &sender,
                OutreachTargetKind::Organiser,
                OutreachPhase::FollowUp,
            )
        })
        .expect("composes");
        assert!(polish.body.contains("propozycja zagrania"));
        assert_eq!(polish.subject, "Przypomnienie: VIRYA — propozycja koncertu");
    }

    #[test]
    fn an_organiser_still_refuses_without_a_listen_link() {
        let sender = sender();
        let no_pitch = OutreachLetterInput {
            pitch_url: "",
            ..input(
                &sender,
                OutreachTargetKind::Organiser,
                OutreachPhase::Initial,
            )
        };
        assert_eq!(
            compose_outreach_letter(&no_pitch),
            Err(OutreachLetterRefusal::NoPitch)
        );
    }
}
