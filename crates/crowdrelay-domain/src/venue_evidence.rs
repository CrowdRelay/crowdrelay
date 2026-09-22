//! The venue answer as an evidence row and one sentence (plan §12-1).
//!
//! A ranking always produces a number one — asked "which room is best", it
//! answers even when it knows nothing, because sorting an empty list still
//! returns a first element. A band then writes to a room that never hosted
//! its genre, hears nothing, and concludes the tool does not work. This
//! module is the other shape: everything the graph knows about one room
//! arrives as [`VenueEvidence`], and [`assess`] turns it into one of two
//! sentences in the tenant's own language.
//!
//! Three rules keep the sentence worth reading:
//!
//! **Never a zero for an unknown.** A clause exists only for a fact that
//! exists — a room with no capacity on record produces no capacity clause,
//! which is the sentence-level form of "capacity — not known".
//!
//! **A fact with no source is not a fact.** Every [`EvidenceFact`] carries
//! its provenance and when it was observed; the caller builds them only
//! from resolved fact triples, so a claim that cannot name where it came
//! from never reaches a sentence.
//!
//! **The graph may refuse.** Below the evidence floor — no usable clause at
//! all — the answer is [`VenueAssessment::InsufficientEvidence`], an honest
//! refusal rather than a low score. The same discipline as
//! `listing::ListingRefusal`: the refusal is what makes the confident
//! answers worth something.
//!
//! The sentence itself is "Klub X jest warte kontaktu, ponieważ …" — the
//! *ponieważ* lists the two or three strongest facts rather than narrating
//! a weight vector. Clauses stay typed ([`EvidenceClause`]) so a test
//! asserts which fact was chosen, not which words came out.

use time::OffsetDateTime;

/// How fresh a tenant's own booking contact must be to count as evidence.
/// A booking address goes stale silently; past the window it stops being a
/// reason rather than becoming a longer claim.
const BOOKING_CONTACT_FRESH_DAYS: i64 = 60;

/// How recent a played show must be for "the room is alive" to be a fact.
const RECENTLY_PLAYED_DAYS: i64 = 90;

/// A sentence lists the two or three strongest facts — never more.
const MAX_CLAUSES: usize = 3;

/// One resolved fact line — the (value, provenance, observed_at) triple a
/// `place_venue_facts` resolution produces. A claim that cannot name all
/// three is not a fact and does not get built.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceFact {
    /// What is claimed: `capacity`, `genres`, `booking_email`, …
    pub attribute: String,
    /// The claim itself, as the winning source stated it.
    pub value: String,
    /// Where it came from — `played`, `researched`, `event_evidence`, …
    pub provenance: String,
    /// When the claim was last seen to be true.
    pub observed_at: OffsetDateTime,
}

/// What the graph knows about one room: the resolved facts plus the
/// mark-derived aggregates the `city_venues` read already computes.
///
/// `facts` are global claims (capacity, genres, website, address, status),
/// resolved in provenance trust order out of `workspace_id IS NULL` rows.
/// `private_facts` are the caller's own — a booking email, a fit judgement —
/// usable in the sentence because the reader is the tenant who owns them,
/// and never part of the shared row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VenueEvidence {
    pub display_name: String,
    pub city_name: String,
    /// Global facts, resolved — one per attribute at most.
    pub facts: Vec<EvidenceFact>,
    /// The tenant's own facts — never shared, never shown to another tenant.
    pub private_facts: Vec<EvidenceFact>,
    /// All-time shows the room has already seen — past-tense marks.
    pub shows_played: i64,
    /// Published nights still ahead — a future booking is not a played show.
    pub shows_booked: i64,
    /// Buyers who paid for at least two shows here.
    pub repeat_attenders: i64,
    /// Acts on this room's bills whose genres intersect the tenant's own —
    /// counted from bills, not asserted. A floor, not a guess: a name-only
    /// peer act with no genre claims honestly counts as nothing.
    pub comparable_acts: i64,
    /// Mean paid orders over the room's ticketed shows. `None` means no
    /// marked show sold tickets through us — unmeasured, not zero.
    pub typical_draw: Option<f64>,
    pub last_played_at: Option<OffsetDateTime>,
    pub next_show_at: Option<OffsetDateTime>,
}

/// The languages a venue sentence can be read in. `En` is the source
/// language and is always complete; `Pl` is an overlay.
///
/// This is the domain's own copy of the convention — `BriefingLocale` lives
/// in the application layer and domain may not depend on application.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EvidenceLocale {
    #[default]
    En,
    Pl,
}

impl EvidenceLocale {
    /// Resolves a BCP-47 tag from the tenant's `crew_locale` setting.
    ///
    /// Matches on the language subtag only: `pl`, `pl-PL` and `pl_PL` are one
    /// language as far as this copy is concerned, and a region we have no
    /// wording for is the source language rather than an error.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        let language = tag
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match language.as_str() {
            "pl" => Self::Pl,
            _ => Self::En,
        }
    }
}

/// One because-clause, typed so tests assert the *fact chosen*, not a string.
///
/// The ordering [`assess`] applies is the "strongest facts" rule, strongest
/// first: shows the room has played, fans who keep coming back, comparable
/// acts the room's bills have already hosted, a show in the last 90 days, a
/// night already booked forward, a booking contact checked inside 60 days,
/// then the registry facts — capacity, genres — and last the measured draw.
/// A sentence lists at most the first three, so the tail exists only as
/// tie-break depth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceClause {
    /// All-time shows already played in the room.
    PlayedShows { count: i64 },
    /// Buyers who paid for two or more shows here.
    RepeatAttenders { count: i64 },
    /// Acts on the room's bills whose genres intersect the tenant's — the
    /// comparable-acts edge §12-1 calls the one that matters.
    ComparableActs { count: i64 },
    /// Mean paid orders over ticketed shows, rounded.
    TypicalDraw { draw: i64 },
    /// The room's capacity as the winning source states it.
    Capacity { value: String },
    /// The room's genres as the winning source states them.
    Genres { tags: String },
    /// Days since the tenant's own booking contact was observed.
    BookingContactFresh { days: i64 },
    /// Days since the room last saw a show.
    RecentlyPlayed { days_ago: i64 },
    /// A published night is already booked forward.
    NextShowBooked,
}

/// What the evidence supports saying about one room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VenueAssessment {
    /// Enough evidence — `sentence` is "… is worth contacting, because …"
    /// (or the Polish equivalent) over up to three clauses, strongest first.
    WorthContact {
        sentence: String,
        clauses: Vec<EvidenceClause>,
    },
    /// Exists-only — the honest refusal, itself a full sentence.
    InsufficientEvidence { sentence: String },
    /// The room is on record as shut. A known state, not a missing one: no
    /// amount of remaining evidence may talk a dead room back into "worth
    /// contacting" — pitching it is the confident wrong answer the resolved
    /// `status` fact exists to prevent.
    Closed { sentence: String },
}

/// Turns what the graph knows about one room into the venue answer.
///
/// Pure: the caller resolves the facts and computes the aggregates; the
/// assessment only reads them. Zero usable clauses is the evidence floor —
/// below it the answer is the refusal, because a confident sentence built
/// on nothing is the failure this type exists to prevent.
#[must_use]
pub fn assess(
    evidence: &VenueEvidence,
    now: OffsetDateTime,
    locale: EvidenceLocale,
) -> VenueAssessment {
    // A reported closure outranks every clause — but only the *winning*
    // status claim counts. The caller resolves each fact attribute per
    // scope, so 'closed' and 'active' arrive as separate rows rather than
    // one resolved value; a tenant's fresh private 'active' must lift a
    // stale global 'closed' the same way the proposal reads resolve it,
    // or the assessment and the proposals would disagree about one room.
    if resolved_status(evidence).is_some_and(|status| status.eq_ignore_ascii_case("closed")) {
        return VenueAssessment::Closed {
            sentence: closed_sentence(&evidence.display_name, locale),
        };
    }
    let clauses = clauses_in_strength_order(evidence, now);
    if clauses.is_empty() {
        return VenueAssessment::InsufficientEvidence {
            sentence: refusal_sentence(&evidence.display_name, locale),
        };
    }
    let chosen: Vec<EvidenceClause> = clauses.into_iter().take(MAX_CLAUSES).collect();
    VenueAssessment::WorthContact {
        sentence: worth_sentence(&evidence.display_name, &chosen, locale),
        clauses: chosen,
    }
}

/// The winning `status` claim across the global and private fact sets, in
/// the registry's trust order — provenance first, then the newest
/// observation. `None` means nobody has claimed anything about the room's
/// liveness, which is the common case.
fn resolved_status(evidence: &VenueEvidence) -> Option<&str> {
    fn rank(provenance: &str) -> u8 {
        match provenance {
            "played" => 0,
            "researched" => 1,
            "event_evidence" => 2,
            "open_directory" => 3,
            _ => 4,
        }
    }
    evidence
        .facts
        .iter()
        .chain(evidence.private_facts.iter())
        .filter(|fact| fact.attribute == "status" && !fact.value.trim().is_empty())
        .min_by(|a, b| {
            rank(&a.provenance)
                .cmp(&rank(&b.provenance))
                .then(b.observed_at.cmp(&a.observed_at))
        })
        .map(|fact| fact.value.trim())
}

/// The candidate clauses, strongest first — the ordering documented on
/// [`EvidenceClause`]. A clause is pushed only when its fact exists: a
/// missing input produces no clause rather than a weak one.
fn clauses_in_strength_order(evidence: &VenueEvidence, now: OffsetDateTime) -> Vec<EvidenceClause> {
    let mut clauses = Vec::new();
    if evidence.shows_played > 0 {
        clauses.push(EvidenceClause::PlayedShows {
            count: evidence.shows_played,
        });
    }
    if evidence.repeat_attenders > 0 {
        clauses.push(EvidenceClause::RepeatAttenders {
            count: evidence.repeat_attenders,
        });
    }
    if evidence.comparable_acts > 0 {
        clauses.push(EvidenceClause::ComparableActs {
            count: evidence.comparable_acts,
        });
    }
    if let Some(last_played_at) = evidence.last_played_at {
        let days_ago = (now - last_played_at).whole_days();
        // A mark dated ahead of now is not "recently played" — it is the
        // booked clause's job to say so.
        if (0..=RECENTLY_PLAYED_DAYS).contains(&days_ago) {
            clauses.push(EvidenceClause::RecentlyPlayed { days_ago });
        }
    }
    if evidence.next_show_at.is_some() {
        clauses.push(EvidenceClause::NextShowBooked);
    }
    if let Some(fact) = evidence
        .private_facts
        .iter()
        .find(|fact| fact.attribute == "booking_email")
    {
        let days = (now - fact.observed_at).whole_days();
        if (0..=BOOKING_CONTACT_FRESH_DAYS).contains(&days) {
            clauses.push(EvidenceClause::BookingContactFresh { days });
        }
    }
    if let Some(fact) = evidence
        .facts
        .iter()
        .find(|fact| fact.attribute == "capacity" && !fact.value.trim().is_empty())
    {
        clauses.push(EvidenceClause::Capacity {
            value: fact.value.clone(),
        });
    }
    if let Some(fact) = evidence
        .facts
        .iter()
        .find(|fact| fact.attribute == "genres" && !fact.value.trim().is_empty())
    {
        clauses.push(EvidenceClause::Genres {
            tags: fact.value.clone(),
        });
    }
    if let Some(typical_draw) = evidence.typical_draw
        && typical_draw > 0.0
    {
        clauses.push(EvidenceClause::TypicalDraw {
            draw: typical_draw.round() as i64,
        });
    }
    clauses
}

/// Renders one clause in `locale`.
///
/// Every text stays honest about what the number means: `shows_played` is
/// all-time, `repeat_attenders` paid twice or more, `typical_draw` averages
/// only nights that sold tickets through us. No invented windows — the read
/// carries no "last 12 months" bound, so the clause does not claim one.
fn clause_text(clause: &EvidenceClause, locale: EvidenceLocale) -> String {
    match (clause, locale) {
        (EvidenceClause::PlayedShows { count }, EvidenceLocale::En) => format!(
            "{count} {} already played here",
            english_plural(*count, "show", "shows")
        ),
        (EvidenceClause::PlayedShows { count }, EvidenceLocale::Pl) => format!(
            "zagrano tu już {count} {}",
            polish_count(*count, "koncert", "koncerty", "koncertów")
        ),
        (EvidenceClause::RepeatAttenders { count }, EvidenceLocale::En) => format!(
            "{count} {} bought tickets for at least two shows here",
            english_plural(*count, "fan", "fans")
        ),
        (EvidenceClause::RepeatAttenders { count }, EvidenceLocale::Pl) if *count == 1 => {
            "1 fan kupił tu bilety na co najmniej dwa koncerty".to_owned()
        }
        (EvidenceClause::RepeatAttenders { count }, EvidenceLocale::Pl) => {
            format!("{count} fanów kupiło tu bilety na co najmniej dwa koncerty")
        }
        (EvidenceClause::ComparableActs { count }, EvidenceLocale::En) if *count == 1 => {
            "1 comparable act has played here".to_owned()
        }
        (EvidenceClause::ComparableActs { count }, EvidenceLocale::En) => {
            format!("{count} comparable acts have played here")
        }
        (EvidenceClause::ComparableActs { count }, EvidenceLocale::Pl) => format!(
            "{} tu już {count} {}",
            polish_played(*count),
            polish_count(
                *count,
                "pokrewny zespół",
                "pokrewne zespoły",
                "pokrewnych zespołów"
            )
        ),
        (EvidenceClause::RecentlyPlayed { days_ago }, EvidenceLocale::En) => {
            format!("the last show there was {}", age_english(*days_ago))
        }
        (EvidenceClause::RecentlyPlayed { days_ago }, EvidenceLocale::Pl) => {
            format!("ostatni koncert był {}", age_polish(*days_ago))
        }
        (EvidenceClause::NextShowBooked, EvidenceLocale::En) => {
            "another show is already booked there".to_owned()
        }
        (EvidenceClause::NextShowBooked, EvidenceLocale::Pl) => {
            "następny koncert jest już tam umówiony".to_owned()
        }
        (EvidenceClause::BookingContactFresh { days }, EvidenceLocale::En) => {
            format!("we checked the booking contact {}", age_english(*days))
        }
        (EvidenceClause::BookingContactFresh { days }, EvidenceLocale::Pl) => {
            format!("kontakt bookingowy sprawdziliśmy {}", age_polish(*days))
        }
        (EvidenceClause::Capacity { value }, EvidenceLocale::En) => {
            format!("the listed capacity is {value}")
        }
        (EvidenceClause::Capacity { value }, EvidenceLocale::Pl) => {
            format!("deklarowana pojemność to {value}")
        }
        (EvidenceClause::Genres { tags }, EvidenceLocale::En) => {
            format!("the room programmes {tags}")
        }
        (EvidenceClause::Genres { tags }, EvidenceLocale::Pl) => {
            format!("w repertuarze ma {tags}")
        }
        (EvidenceClause::TypicalDraw { draw }, EvidenceLocale::En) => format!(
            "a typical ticketed night there draws about {draw} paid {}",
            english_plural(*draw, "order", "orders")
        ),
        (EvidenceClause::TypicalDraw { draw }, EvidenceLocale::Pl) => format!(
            "typowy biletowany koncert przynosi tam ok. {draw} {}",
            polish_count(
                *draw,
                "płatne zamówienie",
                "płatne zamówienia",
                "płatnych zamówień"
            )
        ),
    }
}

/// Joins clause texts into the because-list — "a, b and c" / "a, b i c".
fn join_clauses(texts: &[String], locale: EvidenceLocale) -> String {
    let conjunction = match locale {
        EvidenceLocale::En => " and ",
        EvidenceLocale::Pl => " i ",
    };
    match texts.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}{}{}", rest.join(", "), conjunction, last),
        None => String::new(),
    }
}

fn worth_sentence(name: &str, clauses: &[EvidenceClause], locale: EvidenceLocale) -> String {
    let texts: Vec<String> = clauses
        .iter()
        .map(|clause| clause_text(clause, locale))
        .collect();
    let because = join_clauses(&texts, locale);
    match locale {
        EvidenceLocale::En => format!("{name} is worth contacting, because {because}."),
        EvidenceLocale::Pl => format!("{name} jest warte kontaktu, ponieważ {because}."),
    }
}

/// The booking outreach first line (§12-6) — one honest sentence composed
/// directly from a [`crate::booking::BookingVenueEvidence`] row.
///
/// The lean evidence row is what the target snapshot carries, so this is the
/// venue sentence's smaller sibling rather than [`assess`]: no refusal, just
/// `None` when there is nothing true to say — the outreach then ships with no
/// first line rather than a fabricated one. Clauses come strongest first:
/// the room's last-year shows, comparable acts on its bills, then at most one
/// resolved global fact (genres or capacity), so the line stays a fact and
/// not a paragraph.
#[must_use]
pub fn booking_evidence_line(
    evidence: &crate::booking::BookingVenueEvidence,
    locale: EvidenceLocale,
) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    if evidence.shows_last_12m > 0 {
        let count = evidence.shows_last_12m;
        clauses.push(match locale {
            EvidenceLocale::En => format!(
                "{count} {} in the last year",
                english_plural(count, "show", "shows")
            ),
            EvidenceLocale::Pl => format!(
                "{count} {} w ostatnim roku",
                polish_count(count, "koncert", "koncerty", "koncertów")
            ),
        });
    }
    if evidence.comparable_acts > 0 {
        let count = evidence.comparable_acts;
        clauses.push(match locale {
            EvidenceLocale::En if count == 1 => "1 comparable act on its bills".to_owned(),
            EvidenceLocale::En => format!("{count} comparable acts on its bills"),
            EvidenceLocale::Pl => format!(
                "{} na jego afiszu {count} {}",
                polish_played(count),
                polish_count(
                    count,
                    "pokrewny zespół",
                    "pokrewne zespoły",
                    "pokrewnych zespołów"
                )
            ),
        });
    }
    // One resolved fact at most — genres say who the room books, capacity
    // says how big a night it runs; both together is a list, not a line.
    if let Some(genres) = evidence
        .genres
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        clauses.push(match locale {
            EvidenceLocale::En => format!("programmes {genres}"),
            EvidenceLocale::Pl => format!("ma w repertuarze {genres}"),
        });
    } else if let Some(capacity) = evidence
        .capacity
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        clauses.push(match locale {
            EvidenceLocale::En => format!("listed capacity {capacity}"),
            EvidenceLocale::Pl => format!("deklarowana pojemność {capacity}"),
        });
    }
    if clauses.is_empty() {
        return None;
    }
    let mut line = clauses
        .into_iter()
        .take(MAX_CLAUSES)
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(first) = line.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    line.push('.');
    Some(line)
}

/// The honest refusal — itself a full sentence, in the tenant's language.
/// A room the graph knows nothing about gets told as known-nothing, so the
/// next fact that lands changes the answer rather than confirming a guess.
/// What a room reads as when the tenant's own facts could not be read.
///
/// Distinct from the refusal above, which says the graph knows nothing. This
/// one says *we could not check*: a failed read of the caller's own contacts
/// can turn a room the tenant has a fresh booking address for into "no
/// evidence", and a confident wrong refusal is worse than an admitted gap.
#[must_use]
pub fn unchecked_sentence(name: &str, locale: EvidenceLocale) -> String {
    match locale {
        EvidenceLocale::En => format!(
            "{name} — we could not read your own notes on this room just now, so this is \
             not an answer yet. What the registry knows on its own was not enough to \
             decide; try again in a moment."
        ),
        EvidenceLocale::Pl => format!(
            "{name} — nie udało się teraz odczytać Waszych własnych notatek o tym miejscu, \
             więc to jeszcze nie jest odpowiedź. To, co wie sam rejestr, nie wystarczyło do \
             oceny; spróbujcie za chwilę."
        ),
    }
}

/// A room on record as shut — stated as a claim, because what the registry
/// holds is a source's report, not a door somebody checked today.
fn closed_sentence(name: &str, locale: EvidenceLocale) -> String {
    match locale {
        EvidenceLocale::En => format!(
            "{name} is reported closed. It stays on the record so nobody pitches \
             a dead room again — a closed venue is not a booking lead."
        ),
        EvidenceLocale::Pl => format!(
            "{name} — zgłoszone jako zamknięte. Zostaje w rejestrze, żeby nikt \
             więcej nie pisał do martwego miejsca — zamknięty klub to nie lead."
        ),
    }
}

fn refusal_sentence(name: &str, locale: EvidenceLocale) -> String {
    match locale {
        EvidenceLocale::En => format!(
            "{name} — we know it exists and nothing more. There is no evidence to base \
             contact on yet; it gets checked when the first sign that anyone plays there \
             arrives."
        ),
        EvidenceLocale::Pl => format!(
            "{name} — wiemy, że istnieje, i nic poza tym. Nie ma jeszcze na czym oprzeć \
             kontaktu. Sprawdzimy go, kiedy pojawi się pierwszy dowód, że ktoś tam grał."
        ),
    }
}

/// "today" / "yesterday" / "N days ago" — the age half of an English clause.
fn age_english(days: i64) -> String {
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        n => format!("{n} days ago"),
    }
}

/// "dziś" / "wczoraj" / "N dni temu" — the age half of a Polish clause.
/// `dni` is the only counted form `dzień` has above one, so no table.
fn age_polish(days: i64) -> String {
    match days {
        0 => "dziś".to_owned(),
        1 => "wczoraj".to_owned(),
        n => format!("{n} dni temu"),
    }
}

fn english_plural<'a>(count: i64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// The Polish count class: one, the 2–4 few, or many — with the teen
/// exception (12–14 take `many`) and the pattern repeating on the tens
/// (22–24 → `few`, 25+ → `many`). Shared by noun and verb so the two halves
/// of a clause can never disagree.
#[derive(Clone, Copy)]
enum PolishForm {
    One,
    Few,
    Many,
}

fn polish_form(count: i64) -> PolishForm {
    let count = count.unsigned_abs();
    if count == 1 {
        return PolishForm::One;
    }
    let rem100 = count % 100;
    if (12..=14).contains(&rem100) {
        return PolishForm::Many;
    }
    if (2..=4).contains(&(count % 10)) {
        return PolishForm::Few;
    }
    PolishForm::Many
}

/// Polish count agreement: 1 koncert, 2–4 koncerty, 5+ koncertów.
fn polish_count<'a>(count: i64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    match polish_form(count) {
        PolishForm::One => one,
        PolishForm::Few => few,
        PolishForm::Many => many,
    }
}

/// The verb half of the comparable-acts clause — "grał" / "grały" /
/// "grało" — inflected on the same one/few/many split as the noun it leads
/// into.
fn polish_played(count: i64) -> &'static str {
    match polish_form(count) {
        PolishForm::One => "grał",
        PolishForm::Few => "grały",
        PolishForm::Many => "grało",
    }
}

#[cfg(test)]
mod tests {

    /// The unchecked sentence is distinct from the refusal in both languages,
    /// and says the thing that separates them: nobody decided.
    #[test]
    fn an_unchecked_room_does_not_read_as_a_refusal() {
        for locale in [EvidenceLocale::En, EvidenceLocale::Pl] {
            let unchecked = unchecked_sentence("Klub X", locale);
            let refused = refusal_sentence("Klub X", locale);
            assert_ne!(unchecked, refused);
            assert!(unchecked.contains("Klub X"));
            assert!(
                unchecked.len() > 60,
                "the sentence has to explain itself: {unchecked}"
            );
        }
    }

    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn fact(attribute: &str, value: &str, days_ago: i64) -> EvidenceFact {
        EvidenceFact {
            attribute: attribute.to_owned(),
            value: value.to_owned(),
            provenance: "researched".to_owned(),
            observed_at: now() - Duration::days(days_ago),
        }
    }

    fn exists_only_room() -> VenueEvidence {
        VenueEvidence {
            display_name: "Klub Y".to_owned(),
            city_name: "Gdańsk".to_owned(),
            ..VenueEvidence::default()
        }
    }

    /// A room the graph actually knows: played, repeated, recent, booked
    /// forward, fresh contact, catalogued — every clause on the table.
    fn rich_room() -> VenueEvidence {
        VenueEvidence {
            display_name: "Klub X".to_owned(),
            city_name: "Wrocław".to_owned(),
            facts: vec![
                fact("capacity", "300", 40),
                fact("genres", "metal, rock", 40),
            ],
            private_facts: vec![fact("booking_email", "booking@klub-x.example", 18)],
            shows_played: 14,
            shows_booked: 1,
            repeat_attenders: 9,
            comparable_acts: 0,
            typical_draw: Some(87.4),
            last_played_at: Some(now() - Duration::days(11)),
            next_show_at: Some(now() + Duration::days(20)),
        }
    }

    #[test]
    fn a_rich_room_lists_the_three_strongest_clauses_in_order() {
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&rich_room(), now(), EvidenceLocale::En)
        else {
            panic!("a rich room must be worth contacting")
        };
        assert_eq!(
            clauses,
            vec![
                EvidenceClause::PlayedShows { count: 14 },
                EvidenceClause::RepeatAttenders { count: 9 },
                EvidenceClause::RecentlyPlayed { days_ago: 11 },
            ]
        );
        assert_eq!(
            sentence,
            "Klub X is worth contacting, because 14 shows already played here, \
             9 fans bought tickets for at least two shows here and the last show \
             there was 11 days ago."
        );
    }

    #[test]
    fn the_rich_room_sentence_reads_polish_for_a_polish_crew() {
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&rich_room(), now(), EvidenceLocale::Pl)
        else {
            panic!("a rich room must be worth contacting")
        };
        assert_eq!(clauses.len(), 3);
        assert_eq!(
            sentence,
            "Klub X jest warte kontaktu, ponieważ zagrano tu już 14 koncertów, \
             9 fanów kupiło tu bilety na co najmniej dwa koncerty i ostatni \
             koncert był 11 dni temu."
        );
    }

    #[test]
    fn an_exists_only_room_refuses_in_both_languages() {
        let room = exists_only_room();
        let VenueAssessment::InsufficientEvidence { sentence } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("an empty room must refuse")
        };
        assert_eq!(
            sentence,
            "Klub Y — we know it exists and nothing more. There is no evidence \
             to base contact on yet; it gets checked when the first sign that \
             anyone plays there arrives."
        );
        let VenueAssessment::InsufficientEvidence { sentence } =
            assess(&room, now(), EvidenceLocale::Pl)
        else {
            panic!("an empty room must refuse")
        };
        assert_eq!(
            sentence,
            "Klub Y — wiemy, że istnieje, i nic poza tym. Nie ma jeszcze na \
             czym oprzeć kontaktu. Sprawdzimy go, kiedy pojawi się pierwszy \
             dowód, że ktoś tam grał."
        );
    }

    #[test]
    fn a_reported_closure_outranks_every_other_clause() {
        // The Łykend case: rich researched evidence (genres, fit, fresh
        // research date) on a room that announced it is not reopening.
        let mut room = rich_room();
        room.facts.push(fact("status", "closed", 3));
        let VenueAssessment::Closed { sentence } = assess(&room, now(), EvidenceLocale::En) else {
            panic!("a closed room must never read as worth contacting")
        };
        assert_eq!(
            sentence,
            "Klub X is reported closed. It stays on the record so nobody pitches \
             a dead room again — a closed venue is not a booking lead."
        );
    }

    #[test]
    fn closure_reads_polish_for_a_polish_crew() {
        let mut room = rich_room();
        room.facts.push(fact("status", "closed", 3));
        let VenueAssessment::Closed { sentence } = assess(&room, now(), EvidenceLocale::Pl) else {
            panic!("a closed room must never read as worth contacting")
        };
        assert!(sentence.starts_with("Klub X — zgłoszone jako zamknięte"));
    }

    #[test]
    fn only_closed_disqualifies_other_statuses_do_not() {
        for status in ["active", "reopened", "temporarily_closed"] {
            let mut room = rich_room();
            room.facts.push(fact("status", status, 3));
            assert!(
                matches!(
                    assess(&room, now(), EvidenceLocale::En),
                    VenueAssessment::WorthContact { .. }
                ),
                "status {status:?} must not read as closed"
            );
        }
        // A bare exists-only room with a closed claim is closed, not
        // "insufficient evidence" — the registry knows something decisive.
        let mut room = exists_only_room();
        room.facts.push(fact("status", "Closed", 3));
        assert!(matches!(
            assess(&room, now(), EvidenceLocale::En),
            VenueAssessment::Closed { .. }
        ));
    }

    #[test]
    fn a_fresh_booking_contact_is_evidence_of_its_own() {
        let room = VenueEvidence {
            facts: vec![fact("capacity", "300", 40)],
            private_facts: vec![fact("booking_email", "booking@klub-y.example", 18)],
            ..exists_only_room()
        };
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a fresh contact plus capacity is above the floor")
        };
        assert_eq!(
            clauses,
            vec![
                EvidenceClause::BookingContactFresh { days: 18 },
                EvidenceClause::Capacity {
                    value: "300".to_owned()
                },
            ]
        );
        assert!(sentence.contains("we checked the booking contact 18 days ago"));
    }

    #[test]
    fn a_stale_booking_contact_is_not_evidence() {
        let room = VenueEvidence {
            private_facts: vec![fact("booking_email", "booking@klub-y.example", 61)],
            ..exists_only_room()
        };
        assert_eq!(
            assess(&room, now(), EvidenceLocale::En),
            VenueAssessment::InsufficientEvidence {
                sentence: refusal_sentence("Klub Y", EvidenceLocale::En),
            }
        );
    }

    #[test]
    fn a_stale_contact_drops_out_of_an_otherwise_rich_sentence() {
        let mut room = rich_room();
        room.private_facts = vec![fact("booking_email", "booking@klub-x.example", 90)];
        let VenueAssessment::WorthContact { clauses, .. } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a rich room must be worth contacting")
        };
        assert!(
            clauses
                .iter()
                .all(|clause| !matches!(clause, EvidenceClause::BookingContactFresh { .. })),
            "a 90-day-old contact must not appear: {clauses:?}"
        );
    }

    #[test]
    fn an_unmeasured_draw_produces_no_draw_clause() {
        let mut room = rich_room();
        room.typical_draw = None;
        let VenueAssessment::WorthContact { clauses, .. } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a rich room must be worth contacting")
        };
        assert!(
            clauses
                .iter()
                .all(|clause| !matches!(clause, EvidenceClause::TypicalDraw { .. })),
            "an unmeasured draw is absent, never zero: {clauses:?}"
        );
    }

    /// A measured draw alone clears the floor — and rounds to people.
    #[test]
    fn a_measured_draw_can_carry_a_sentence() {
        let room = VenueEvidence {
            typical_draw: Some(119.6),
            ..exists_only_room()
        };
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a measured draw is evidence")
        };
        assert_eq!(clauses, vec![EvidenceClause::TypicalDraw { draw: 120 }]);
        assert_eq!(
            sentence,
            "Klub Y is worth contacting, because a typical ticketed night there \
             draws about 120 paid orders."
        );
    }

    /// A show 100 days out is not "the room is alive".
    #[test]
    fn an_old_last_show_drops_the_recent_clause() {
        let mut room = rich_room();
        room.last_played_at = Some(now() - Duration::days(100));
        room.next_show_at = None;
        let VenueAssessment::WorthContact { clauses, .. } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a rich room must be worth contacting")
        };
        assert!(
            clauses
                .iter()
                .all(|clause| !matches!(clause, EvidenceClause::RecentlyPlayed { .. })),
            "a 100-day-old show is not recent: {clauses:?}"
        );
    }

    /// Comparable acts sit between repeat attenders and the recency clause —
    /// the edge §12-1 calls the one that matters, outranked only by the
    /// room's own played history and its regulars.
    #[test]
    fn comparable_acts_rank_after_repeat_attenders_and_before_recency() {
        let room = VenueEvidence {
            repeat_attenders: 4,
            comparable_acts: 3,
            last_played_at: Some(now() - Duration::days(11)),
            ..exists_only_room()
        };
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("comparable bills plus regulars are above the floor")
        };
        assert_eq!(
            clauses,
            vec![
                EvidenceClause::RepeatAttenders { count: 4 },
                EvidenceClause::ComparableActs { count: 3 },
                EvidenceClause::RecentlyPlayed { days_ago: 11 },
            ]
        );
        assert_eq!(
            sentence,
            "Klub Y is worth contacting, because 4 fans bought tickets for at \
             least two shows here, 3 comparable acts have played here and the \
             last show there was 11 days ago."
        );
    }

    /// A comparable-acts count on its own is above the floor — bills are
    /// evidence, and the singular form agrees with its verb.
    #[test]
    fn one_comparable_act_carries_a_sentence_in_english() {
        let room = VenueEvidence {
            comparable_acts: 1,
            ..exists_only_room()
        };
        let VenueAssessment::WorthContact { sentence, clauses } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("a comparable act is evidence")
        };
        assert_eq!(clauses, vec![EvidenceClause::ComparableActs { count: 1 }]);
        assert_eq!(
            sentence,
            "Klub Y is worth contacting, because 1 comparable act has played here."
        );
    }

    /// Polish needs the verb to agree — grał / grały / grało — on the same
    /// one/few/many split the noun takes.
    #[test]
    fn comparable_acts_read_polish_with_the_right_verb_form() {
        let clause = |count: i64| {
            clause_text(
                &EvidenceClause::ComparableActs { count },
                EvidenceLocale::Pl,
            )
        };
        assert_eq!(clause(1), "grał tu już 1 pokrewny zespół");
        assert_eq!(clause(3), "grały tu już 3 pokrewne zespoły");
        assert_eq!(clause(5), "grało tu już 5 pokrewnych zespołów");
        // The teen exception takes the many form on both words.
        assert_eq!(clause(12), "grało tu już 12 pokrewnych zespołów");
        assert_eq!(clause(23), "grały tu już 23 pokrewne zespoły");
    }

    /// Zero comparable acts is an absent clause, never a weak one — a
    /// name-only bill contributes nothing the sentence may cite.
    #[test]
    fn no_comparable_acts_produces_no_clause() {
        let room = VenueEvidence {
            shows_played: 6,
            comparable_acts: 0,
            ..exists_only_room()
        };
        let VenueAssessment::WorthContact { clauses, .. } =
            assess(&room, now(), EvidenceLocale::En)
        else {
            panic!("played shows alone are above the floor")
        };
        assert!(
            clauses
                .iter()
                .all(|clause| !matches!(clause, EvidenceClause::ComparableActs { .. })),
            "a zero count must not appear: {clauses:?}"
        );
    }

    #[test]
    fn polish_counts_take_the_right_noun_form() {
        assert_eq!(
            polish_count(1, "koncert", "koncerty", "koncertów"),
            "koncert"
        );
        assert_eq!(
            polish_count(2, "koncert", "koncerty", "koncertów"),
            "koncerty"
        );
        assert_eq!(
            polish_count(5, "koncert", "koncerty", "koncertów"),
            "koncertów"
        );
        // Teens take the many form even though they end in 2–4.
        assert_eq!(
            polish_count(12, "koncert", "koncerty", "koncertów"),
            "koncertów"
        );
        assert_eq!(
            polish_count(22, "koncert", "koncerty", "koncertów"),
            "koncerty"
        );
        assert_eq!(
            polish_count(25, "koncert", "koncerty", "koncertów"),
            "koncertów"
        );
    }

    #[test]
    fn locale_resolution_matches_the_briefing_convention() {
        assert_eq!(EvidenceLocale::from_tag("pl-PL"), EvidenceLocale::Pl);
        assert_eq!(EvidenceLocale::from_tag("pl_PL"), EvidenceLocale::Pl);
        assert_eq!(EvidenceLocale::from_tag("PL"), EvidenceLocale::Pl);
        assert_eq!(EvidenceLocale::from_tag("en-IE"), EvidenceLocale::En);
        assert_eq!(EvidenceLocale::from_tag(""), EvidenceLocale::En);
        assert_eq!(EvidenceLocale::default(), EvidenceLocale::En);
    }

    /// The booking first line says only what the row actually knows.
    #[test]
    fn booking_line_says_the_strongest_facts_and_nothing_else() {
        use crate::booking::BookingVenueEvidence;
        let evidence = BookingVenueEvidence {
            shows_last_12m: 9,
            comparable_acts: 3,
            genres: Some("metal, rock".to_owned()),
            capacity: Some("300".to_owned()),
            days_since_last_event: Some(11),
            booking_contact_days: Some(18),
        };
        assert_eq!(
            booking_evidence_line(&evidence, EvidenceLocale::En),
            Some(
                "9 shows in the last year, 3 comparable acts on its bills, programmes metal, rock."
                    .to_owned()
            )
        );
        assert_eq!(
            booking_evidence_line(&evidence, EvidenceLocale::Pl),
            Some(
                "9 koncertów w ostatnim roku, grały na jego afiszu 3 pokrewne zespoły, \
                 ma w repertuarze metal, rock."
                    .to_owned()
            )
        );
    }

    /// An evidence row of unknowns produces no line — never a zero claim.
    #[test]
    fn booking_line_refuses_to_fabricate_when_nothing_is_known() {
        use crate::booking::BookingVenueEvidence;
        let evidence = BookingVenueEvidence {
            shows_last_12m: 0,
            comparable_acts: 0,
            genres: None,
            capacity: None,
            days_since_last_event: None,
            booking_contact_days: None,
        };
        assert_eq!(booking_evidence_line(&evidence, EvidenceLocale::En), None);
        assert_eq!(booking_evidence_line(&evidence, EvidenceLocale::Pl), None);
    }

    /// One and two clauses join without the comma the three-clause form has.
    #[test]
    fn the_because_list_joins_honestly_at_any_length() {
        assert_eq!(join_clauses(&["one".to_owned()], EvidenceLocale::En), "one");
        assert_eq!(
            join_clauses(&["one".to_owned(), "two".to_owned()], EvidenceLocale::Pl),
            "one i two"
        );
        assert_eq!(
            join_clauses(
                &["one".to_owned(), "two".to_owned(), "three".to_owned()],
                EvidenceLocale::En
            ),
            "one, two and three"
        );
    }
}
