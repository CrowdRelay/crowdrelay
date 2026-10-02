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
use crate::tracked_link::TrackedLink;
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
    /// Where the target listens to it — a tracked redirect, never the bare
    /// destination. `None` refuses the letter (`NoPitch`): a pitch without a
    /// link asks a stranger to go hunting, and an untracked one teaches the
    /// ledger nothing about whether the letter worked.
    pub pitch_url: Option<TrackedLink>,
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

/// "Dzień dobry, {name}," — the name in the nominative, as the application
/// letter greets. A bare "Dzień dobry," made every letter of a template
/// identical, and dispatch refuses a draft that already went out: on
/// 2026-09-28, 38 of 39 approved show letters were refused. The nominative
/// is no vocative guess; an empty name keeps the bare greeting.
#[must_use]
pub fn greeting_pl(display_name: &str) -> String {
    let name = salutation_name(display_name);
    if name.is_empty() {
        "Dzień dobry,".to_owned()
    } else {
        format!("Dzień dobry, {name},")
    }
}

/// Subjects stay well under the executor's 220 cap — a subject a mail client
/// truncates is a subject nobody read.
pub(crate) const MAX_SUBJECT: usize = 160;

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
    let url = input.pitch_url.as_ref().map(TrackedLink::as_str);
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
        if url.is_none() {
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
    let Some(url) = url else {
        return Err(OutreachLetterRefusal::NoPitch);
    };
    if title.is_empty() {
        return Err(OutreachLetterRefusal::NoPitch);
    }
    Ok(match (input.language, input.phase, input.target_kind) {
        (LetterLanguage::English, OutreachPhase::Initial, OutreachTargetKind::Playlist) => {
            playlist_initial_en(input, target, act, title, url)
        }
        (LetterLanguage::Polish, OutreachPhase::Initial, OutreachTargetKind::Playlist) => {
            playlist_initial_pl(input, act, title, url)
        }
        (LetterLanguage::English, OutreachPhase::Initial, _) => {
            initial(input, target, act, title, url, ask)
        }
        (LetterLanguage::English, OutreachPhase::FollowUp, _) => {
            follow_up(input, target, act, title, url, ask)
        }
        (LetterLanguage::Polish, OutreachPhase::Initial, _) => initial_pl(input, act, title, url),
        (LetterLanguage::Polish, OutreachPhase::FollowUp, _) => {
            follow_up_pl(input, act, title, url)
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
        // An organiser's ask lives in its own template — this arm answers so
        // the match stays whole, and is unreachable by construction.
        OutreachTargetKind::Organiser | OutreachTargetKind::Agent | OutreachTargetKind::Label => "",
    }
}

/// A playlist curator does not need the band's mini-bio, a CRM disclaimer or
/// "playlist consideration" language. Research supplies the specific opener;
/// this composer only says what the band has and gives the one useful link.
fn playlist_initial_pl(
    input: &OutreachLetterInput<'_>,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        greeting_pl(input.target_name),
        String::new(),
        format!(
            "Mamy nowy numer — {title}. Jeśli pasuje do profilu playlisty, zostawiamy go tutaj:"
        ),
        url.to_owned(),
    ];
    lines.extend(sign_off_pl(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn playlist_initial_en(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!("We have a new track — {title}. If it fits the playlist, here it is:"),
        url.to_owned(),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn initial_pl(
    input: &OutreachLetterInput<'_>,
    act: &str,
    title: &str,
    url: &str,
) -> OutreachLetter {
    let mut lines = vec![
        greeting_pl(input.target_name),
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
        greeting_pl(input.target_name),
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

/// "Piszemy w imieniu {act}{ ({home})}{ — zespołu grającego {style} —}".
///
/// The city stays in the nominative because declining a city name without a
/// dictionary is a guess ("z Wrocław" is not Polish). It used to be kept that
/// way with "z miasta Wrocław", which no Polish writer says and which a press
/// contact reads as a template on the first line. A parenthesis is how Polish
/// press notes carry a place, and it takes the nominative. The style follows
/// "grającego" because a genre name in the accusative reads as written —
/// "zespołu metalcore, modern metal" did not.
pub(crate) fn introduction_pl(sender: &SenderIdentity, act: &str) -> String {
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
            format!("Piszemy w imieniu {act} ({home}) — zespołu grającego {style} —")
        }
        (Some(style), None) => format!("Piszemy w imieniu {act} — zespołu grającego {style} —"),
        (None, Some(home)) => format!("Piszemy w imieniu {act} ({home})"),
        (None, None) => format!("Piszemy w imieniu {act}"),
    }
}

pub(crate) fn sign_off_pl(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Pozdrawiamy,".to_owned(), act.to_owned()];
    if let Some(link) = &sender.site_url {
        lines.push(link.as_str().to_owned());
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
    lines.push(format!(
        "Listen: {}",
        input.pitch_url.as_ref().map_or("", |l| l.as_str())
    ));
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
        format!(
            "Listen: {}",
            input.pitch_url.as_ref().map_or("", |l| l.as_str())
        ),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Follow-up: {act} — gig request"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn organiser_initial_pl(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        greeting_pl(target),
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
    lines.push(format!(
        "Do posłuchania: {}",
        input.pitch_url.as_ref().map_or("", |l| l.as_str())
    ));
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
    target: &str,
    act: &str,
) -> OutreachLetter {
    let mut lines = vec![
        greeting_pl(target),
        String::new(),
        "Krótko wracamy do poprzedniej wiadomości — propozycja zagrania u Was \
         nadal stoi. Jeśli to nie dla Was, krótkie „nie” też bardzo nam pomoże."
            .to_owned(),
        String::new(),
        format!(
            "Do posłuchania: {}",
            input.pitch_url.as_ref().map_or("", |l| l.as_str())
        ),
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
pub(crate) fn format_show_date_en(date: Date) -> String {
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
pub(crate) fn format_show_date_pl(date: Date) -> String {
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
    if let Some(link) = &sender.site_url {
        lines.push(link.as_str().to_owned());
    }
    lines
}

/// "I am writing from {act}{, a {style} act}{ from {home_city}}" — each part
/// present only when the tenant's own records say it.
pub(crate) fn introduction(sender: &SenderIdentity, act: &str) -> String {
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
pub(crate) fn sign_off(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Best,".to_owned(), act.to_owned()];
    if let Some(link) = &sender.site_url {
        lines.push(link.as_str().to_owned());
    }
    lines
}

pub(crate) fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests;
