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
use time::OffsetDateTime;

/// The language the promoter reads (O.6).
///
/// The letter was English for everybody. Virya books rooms in Poland, so every
/// proposal so far has reached a Polish promoter in a foreign language — the
/// first impression a stranger forms of a band, and one nobody chose.
///
/// Two languages, because two is what can be written honestly here. A room in a
/// country neither covers gets English, which is the lingua franca of booking
/// and an explicit fallback rather than an accident.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LetterLanguage {
    #[default]
    English,
    Polish,
}

impl LetterLanguage {
    /// The language for a room in this country, by ISO 3166-1 alpha-2 code.
    ///
    /// Deliberately a whitelist. Guessing from a country code we have no copy
    /// for would produce an English letter wearing a Polish label, and the
    /// promoter would be the one to discover it.
    #[must_use]
    pub fn for_country(code: &str) -> Self {
        match code.trim().to_ascii_uppercase().as_str() {
            "PL" => Self::Polish,
            _ => Self::English,
        }
    }
}

/// The letter's first sentence, in the promoter's language (O.6).
///
/// English delegates to `GigPlan::opening_line` — one definition, and the
/// console's English read cannot drift from the English letter. Polish is
/// written here rather than in `gig_plan` so the plan stays about the decision
/// and this file stays about the words a stranger reads.
///
/// Same rule as the bullets: the numbers are stated plainly and nothing is
/// softened in translation.
#[must_use]
pub fn opening_line(plan: &crate::gig_plan::GigPlan, language: LetterLanguage) -> String {
    use crate::gig_plan::Reason;
    if language == LetterLanguage::English {
        return plan.opening_line();
    }
    let city = &plan.city;
    let venue = &plan.venue;
    match plan.reasons.first() {
        Some(Reason::ComparableActsPlayedHere { count, .. }) => {
            if *count == 1 {
                format!("Jeden zespół z naszego gatunku zagrał w {venue} według naszych danych.")
            } else {
                format!(
                    "Zespoły z naszego gatunku zagrały w {venue} {count} razy według naszych \
                     danych."
                )
            }
        }
        Some(Reason::ReachableAudience { reachable }) => format!(
            "{reachable} osób w okolicy {city} poprosiło, żebyśmy dali znać, kiedy gramy w \
             pobliżu."
        ),
        Some(Reason::RoomDraws { typical_draw }) => format!(
            "Biletowane koncerty w {venue} sprzedają średnio {typical_draw} biletów, i to jest \
             skala, w której gramy."
        ),
        Some(Reason::NeverPlayedButHasFans { reachable }) => format!(
            "{reachable} osób w okolicy {city} prosiło, żeby dać im znać, kiedy zagramy w \
             pobliżu, a nigdy nie graliśmy w tym mieście."
        ),
        Some(Reason::OverdueReturn { months, active_30d }) => format!(
            "Ostatni raz graliśmy w {city} {months} miesięcy temu, a {active_30d} osób stamtąd \
             było z nami aktywnych w ostatnim miesiącu."
        ),
        Some(Reason::CoBillAddsAudience {
            act,
            adds_reachable,
        }) => format!(
            "Wspólny line-up z {act} dociera do {adds_reachable} osób w okolicy {city}, do \
             których sami nie docieramy."
        ),
        Some(Reason::WarmPromoter { name }) => {
            format!("{name} — pisaliśmy już ze sobą, a my znów patrzymy na {city}.")
        }
        Some(Reason::RoomIsActive {
            days_since_last_event,
        }) => format!(
            "W {venue} coś się działo {days_since_last_event} dni temu, a my patrzymy na {city}."
        ),
        // Unreachable by construction, for the same reason it is on the
        // English side: `plan_gig` never returns a proposal with no reasons.
        None => format!("Patrzymy na {city}, a {venue} jest tym klubem."),
    }
}

/// One reason as a letter bullet, in the promoter's language (O.6).
///
/// The bullets used to be rendered in `crowdrelay-infra` in English only, so a
/// Polish frame would have wrapped English evidence — the same seam the crew
/// email exists to avoid, moved to a stranger's inbox. Rendering them here is
/// also where they belong: the letter owns the words the promoter reads, and
/// the infra layer owns the rows they are measured from.
///
/// The bullets are evidence, not voice. The numbers are stated plainly in both
/// languages and nothing is softened in translation.
#[must_use]
pub fn reason_line(reason: &crate::gig_plan::Reason, language: LetterLanguage) -> String {
    use crate::gig_plan::Reason;
    match (language, reason) {
        // All-time count over a twelve-month window — the subset phrasing is
        // not guaranteed by the number, so the sentence states the record.
        (LetterLanguage::English, Reason::ComparableActsPlayedHere { count, .. }) => {
            if *count == 1 {
                "one act from our genre has played there on record".to_owned()
            } else {
                format!("{count} acts from our genre have played there on record")
            }
        }
        (LetterLanguage::Polish, Reason::ComparableActsPlayedHere { count, .. }) => {
            if *count == 1 {
                "jeden zespół z naszego gatunku zagrał tam według naszych danych".to_owned()
            } else {
                format!("zespoły z naszego gatunku zagrały tam {count} razy według naszych danych")
            }
        }
        (LetterLanguage::English, Reason::ReachableAudience { reachable }) => {
            format!("{reachable} people nearby asked us to tell them when we play")
        }
        (LetterLanguage::Polish, Reason::ReachableAudience { reachable }) => {
            format!("{reachable} osób w okolicy poprosiło, żebyśmy dali znać, kiedy gramy")
        }
        (LetterLanguage::English, Reason::RoomDraws { typical_draw }) => {
            format!("the room averages {typical_draw} paid tickets per ticketed show")
        }
        (LetterLanguage::Polish, Reason::RoomDraws { typical_draw }) => {
            format!("klub sprzedaje średnio {typical_draw} biletów na biletowany koncert")
        }
        (LetterLanguage::English, Reason::NeverPlayedButHasFans { reachable }) => {
            format!("{reachable} people nearby follow us and we have never played the city")
        }
        (LetterLanguage::Polish, Reason::NeverPlayedButHasFans { reachable }) => {
            format!("{reachable} osób w okolicy nas słucha, a nigdy nie graliśmy w tym mieście")
        }
        (LetterLanguage::English, Reason::OverdueReturn { months, active_30d }) => format!(
            "our last show there was {months} months ago and {active_30d} people there were \
             active with us this month"
        ),
        (LetterLanguage::Polish, Reason::OverdueReturn { months, active_30d }) => format!(
            "ostatni koncert graliśmy tam {months} miesięcy temu, a {active_30d} osób stamtąd \
             było z nami aktywnych w tym miesiącu"
        ),
        (
            LetterLanguage::English,
            Reason::CoBillAddsAudience {
                act,
                adds_reachable,
            },
        ) => format!("a bill with {act} reaches {adds_reachable} people we do not reach alone"),
        (
            LetterLanguage::Polish,
            Reason::CoBillAddsAudience {
                act,
                adds_reachable,
            },
        ) => format!(
            "wspólny line-up z {act} dociera do {adds_reachable} osób, do których sami nie docieramy"
        ),
        (LetterLanguage::English, Reason::WarmPromoter { name }) => {
            format!("{name} has answered us before")
        }
        (LetterLanguage::Polish, Reason::WarmPromoter { name }) => {
            format!("{name} już nam kiedyś odpisał(a)")
        }
        (
            LetterLanguage::English,
            Reason::RoomIsActive {
                days_since_last_event,
            },
        ) => format!("the room had something on {days_since_last_event} days ago"),
        (
            LetterLanguage::Polish,
            Reason::RoomIsActive {
                days_since_last_event,
            },
        ) => format!("w klubie coś się działo {days_since_last_event} dni temu"),
    }
}

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
    /// What the promoter reads it in. Chosen from the room's country, not from
    /// the band's own locale: the letter is for them.
    pub language: LetterLanguage,
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
        greeting(input.language).to_owned(),
        String::new(),
        opening.to_owned(),
        String::new(),
    ];
    body.push(match input.language {
        LetterLanguage::English => format!(
            "{} and we are putting together a night at {venue}. This letter goes to \
             everyone who books the room at once, so nobody hears about it secondhand.",
            introduction(input.sender, act, input.language)
        ),
        LetterLanguage::Polish => format!(
            "{} i planujemy koncert w {venue}. Ten list trafia do wszystkich osób, \
             które bukują ten klub, więc nikt nie dowiaduje się o nim z drugiej ręki.",
            introduction(input.sender, act, input.language)
        ),
    });
    body.push(String::new());
    if !rest.is_empty() {
        body.push(
            match input.language {
                LetterLanguage::English => "Why we think the night works:",
                LetterLanguage::Polish => "Dlaczego uważamy, że ten wieczór ma sens:",
            }
            .to_owned(),
        );
        for reason in rest {
            body.push(format!("- {reason}"));
        }
        body.push(String::new());
    }
    body.push(match input.language {
        LetterLanguage::English => format!(
            "If {venue} has a window in the coming months, tell us what the night \
             needs and we will come back with a concrete offer."
        ),
        LetterLanguage::Polish => format!(
            "Jeśli {venue} ma wolny termin w najbliższych miesiącach, napiszcie \
             czego potrzebuje ten wieczór, a wrócimy z konkretną propozycją."
        ),
    });
    body.push(String::new());
    if let Some(site) = site_line(input.sender, input.language) {
        body.push(site);
        body.push(String::new());
    }
    body.push(sign_off(input.language).to_owned());
    body.push(act.to_owned());

    GigLetter {
        subject: truncate(&match input.language {
            LetterLanguage::English => format!("{act} x {venue} — show proposal"),
            LetterLanguage::Polish => format!("{act} x {venue} — propozycja koncertu"),
        }),
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
        greeting(input.language).to_owned(),
        String::new(),
        opening.to_owned(),
        String::new(),
    ];
    body.push(match input.language {
        LetterLanguage::English => format!(
            "The night at {venue} on {date} is already ours, and the slot you offered \
             is still open. {support} is the labelmate we want to put in it. This \
             letter goes to everyone who books the room at once, so nobody hears \
             about it secondhand."
        ),
        LetterLanguage::Polish => format!(
            "Koncert w {venue} w dniu {date} jest już nasz, a slot, który \
             zaproponowaliście, nadal jest wolny. Chcemy wstawić w niego {support} — \
             zespół z tej samej stajni. Ten list trafia do wszystkich osób, które \
             bukują ten klub, więc nikt nie dowiaduje się o nim z drugiej ręki."
        ),
    });
    body.push(String::new());
    if !rest.is_empty() {
        body.push(match input.language {
            LetterLanguage::English => format!("Why {support} fits the slot:"),
            LetterLanguage::Polish => format!("Dlaczego {support} pasuje do tego slotu:"),
        });
        for reason in rest {
            body.push(format!("- {reason}"));
        }
        body.push(String::new());
    }
    body.push(match input.language {
        LetterLanguage::English => format!(
            "If {support} works for the slot, say so and they are confirmed — and if \
             you would rather hold it, that answer is just as useful."
        ),
        LetterLanguage::Polish => format!(
            "Jeśli {support} pasuje na ten slot, dajcie znać i mamy to potwierdzone — \
             a jeśli wolicie go zatrzymać, ta odpowiedź jest tak samo przydatna."
        ),
    });
    body.push(String::new());
    body.push(sign_off(input.language).to_owned());
    body.push(act.to_owned());

    GigLetter {
        subject: truncate(&match input.language {
            LetterLanguage::English => format!("{support} for the {venue} slot — {date}"),
            LetterLanguage::Polish => format!("{support} na slot w {venue} — {date}"),
        }),
        body: body.join("\n"),
    }
}

/// Polish month names in the genitive — "18 października", the form a date
/// takes inside a sentence. Index-safe: a month outside 1–12 is impossible
/// for `time::Month`, and `get` makes a broken table a compile-time-sized
/// absence rather than a panic.
const POLISH_MONTHS: [&str; 12] = [
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

/// A date the way Polish copy writes it — "18 października 2026". Shared by
/// every composed text that quotes a day: a letter's "on 18 October" line and
/// a campaign mail's date are the same sentence part in the same language.
#[must_use]
pub fn polish_date(at: OffsetDateTime) -> String {
    let month = POLISH_MONTHS
        .get(usize::from(u8::from(at.month())) - 1)
        .copied()
        .unwrap_or("");
    format!("{} {} {}", at.day(), month, at.year())
}

/// A clock time the way Polish copy writes it — "19:30".
#[must_use]
pub fn polish_time(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}", at.hour(), at.minute())
}

/// A date the way English copy writes it — "18 October 2026".
#[must_use]
pub fn english_date(at: OffsetDateTime) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let month = MONTHS
        .get(usize::from(u8::from(at.month())) - 1)
        .copied()
        .unwrap_or("");
    format!("{} {} {}", month, at.day(), at.year())
}

/// The date in the language the letter is written in — a Polish day-name in
/// an English sentence is a machine slipping, not a flourish.
#[must_use]
pub fn letter_date(at: OffsetDateTime, language: LetterLanguage) -> String {
    match language {
        LetterLanguage::Polish => polish_date(at),
        LetterLanguage::English => english_date(at),
    }
}

const fn greeting(language: LetterLanguage) -> &'static str {
    match language {
        LetterLanguage::English => "Hi,",
        LetterLanguage::Polish => "Cześć,",
    }
}

const fn sign_off(language: LetterLanguage) -> &'static str {
    match language {
        LetterLanguage::English => "Best,",
        LetterLanguage::Polish => "Pozdrawiamy,",
    }
}

/// "We are X", plus whatever else the tenant has actually said about itself.
///
/// Every clause is conditional on a stored fact. A band that has not declared a
/// style gets a shorter sentence, not a guessed one — the promoter reading it
/// would rather have four true words than a genre somebody's software picked.
fn introduction(sender: &SenderIdentity, act: &str, language: LetterLanguage) -> String {
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
    match (language, style, city) {
        (LetterLanguage::English, Some(style), Some(city)) => {
            format!("We are {act}, a {style} band from {city},")
        }
        (LetterLanguage::English, Some(style), None) => format!("We are {act}, a {style} band,"),
        (LetterLanguage::English, None, Some(city)) => format!("We are {act}, a band from {city},"),
        (LetterLanguage::English, None, None) => format!("We are {act},"),
        (LetterLanguage::Polish, Some(style), Some(city)) => {
            format!("Jesteśmy {act}, zespół grający {style} z {city},")
        }
        (LetterLanguage::Polish, Some(style), None) => {
            format!("Jesteśmy {act}, zespół grający {style},")
        }
        (LetterLanguage::Polish, None, Some(city)) => format!("Jesteśmy {act}, zespół z {city},"),
        (LetterLanguage::Polish, None, None) => format!("Jesteśmy {act},"),
    }
}

fn site_line(sender: &SenderIdentity, language: LetterLanguage) -> Option<String> {
    sender
        .site_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|site| match language {
            LetterLanguage::English => format!("Music: {site}"),
            LetterLanguage::Polish => format!("Muzyka: {site}"),
        })
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
            language: LetterLanguage::English,
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
            language: LetterLanguage::English,
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
            language: LetterLanguage::English,
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
            language: LetterLanguage::English,
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

    /// O.6: the letter is for the promoter, so it is in their language. Virya
    /// books rooms in Poland and every proposal so far arrived in English —
    /// the first impression a stranger forms of a band, and one nobody chose.
    #[test]
    fn a_polish_room_gets_a_polish_letter() {
        let sender = sender();
        let reasons = reasons();
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            language: LetterLanguage::Polish,
            sender: &sender,
            venue: "Klub X",
            opening_line: "Cztery podobne zespoły zagrały w Klub X w ostatnim roku.",
            reasons: &reasons,
            support_act: None,
            show_date: None,
        })
        .expect("composes");
        assert!(letter.body.starts_with("Cześć,\n"), "{}", letter.body);
        assert!(
            letter
                .body
                .contains("Jesteśmy Virya, zespół grający modern metal z Wrocław,")
        );
        assert!(
            letter
                .body
                .contains("Dlaczego uważamy, że ten wieczór ma sens:")
        );
        assert!(letter.body.contains("Muzyka: https://virya.music/"));
        assert!(letter.body.ends_with("Pozdrawiamy,\nVirya"));
        assert_eq!(letter.subject, "Virya x Klub X — propozycja koncertu");
        // Nothing English leaks through the frame.
        assert!(!letter.body.contains("Hi,"));
        assert!(!letter.body.contains("Best,"));
    }

    /// The language follows the room's country, and a country nothing is
    /// written for falls back to English rather than to a guess.
    #[test]
    fn the_country_picks_the_language_and_the_unknown_one_is_english() {
        assert_eq!(LetterLanguage::for_country("PL"), LetterLanguage::Polish);
        assert_eq!(LetterLanguage::for_country("pl"), LetterLanguage::Polish);
        assert_eq!(LetterLanguage::for_country(" pl "), LetterLanguage::Polish);
        assert_eq!(LetterLanguage::for_country("DE"), LetterLanguage::English);
        assert_eq!(LetterLanguage::for_country(""), LetterLanguage::English);
    }

    /// The ask travels too, and still refuses to announce a night that is
    /// already held.
    #[test]
    fn the_polish_ask_is_about_the_slot_not_a_new_night() {
        let sender = sender();
        let reasons = reasons();
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::SupportSlotAsk,
            language: LetterLanguage::Polish,
            sender: &sender,
            venue: "Klub X",
            opening_line: "Slot, który zaproponowaliście, jest nadal wolny.",
            reasons: &reasons,
            support_act: Some("Second Act"),
            show_date: Some("12 października"),
        })
        .expect("composes");
        assert!(letter.body.contains("jest już nasz"));
        assert!(
            letter
                .body
                .contains("Dlaczego Second Act pasuje do tego slotu:")
        );
        assert_eq!(
            letter.subject,
            "Second Act na slot w Klub X — 12 października"
        );
        assert!(!letter.body.contains("planujemy koncert"));
    }

    /// One reason is a whole proposal: the opening line says it, and the letter
    /// carries no empty bullet list underneath.
    #[test]
    fn a_single_reason_produces_no_bullet_section() {
        let sender = sender();
        let one = vec!["the room books this kind of night".to_owned()];
        let letter = compose_letter(&LetterInput {
            kind: LetterKind::Proposal,
            language: LetterLanguage::English,
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
            language: LetterLanguage::English,
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
