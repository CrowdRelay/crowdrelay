//! What the band knows about a person before it writes to them.
//!
//! # The rule
//!
//! Nobody is written to as a stranger. Before a letter goes to a promoter, a
//! journalist, a presenter or a photographer, the band has looked at what they
//! have done lately — an episode, a review, a project — and the letter opens
//! with that. A letter that shows its sender read the work is answered far more
//! often than one that could have gone to anybody, and it is the difference
//! between a colleague writing and a mailshot.
//!
//! This is a gate, not a guideline. The composers take a [`PersonalHook`] by
//! reference and the eligibility rule holds anybody without a recent one, so a
//! letter to somebody the band has not read is unrepresentable rather than
//! merely discouraged.
//!
//! # What a hook is
//!
//! One dated, sourced thing a person did: *what* (a sentence the letter can
//! quote), *when* it happened, and *where* the sender found it. The source is
//! mandatory because the person who approves the letter must be able to check
//! it in ten seconds, and because a fact that turns out wrong has to be
//! revocable. The letter itself never prints the source URL: every URL in an
//! outward letter is a tracked link, and a research link is not one.
//!
//! An optional `praise` is one specific sentence of appreciation that refers to
//! the fact. It is the part a model is most tempted to inflate, so it is held to
//! the band's register: no exclamation marks, no hashtags, no links, short.
//!
//! # Who writes hooks
//!
//! The research agent (a premium agent run with web access, the same one the
//! event scout uses) or a person. Both go through [`PersonalHook::new`], so the
//! checks below apply to either.

use crate::gig_letter::LetterLanguage;
use time::{Date, Duration};

/// How old the researched thing may be. A review from last spring is not
/// "lately", and a letter that cites it reads as a lookup rather than as
/// somebody paying attention.
pub const HOOK_MAX_AGE_DAYS: i64 = 120;

/// Shorter than this is a label, not a fact ("a show", "new episode").
pub const FACT_MIN_CHARS: usize = 20;
pub const FACT_MAX_CHARS: usize = 400;
/// The wire field is still called `praise` in v1, but for outward mail it is
/// the human opening sentence. Too short reads generic; too long reads generated.
pub const PRAISE_MIN_CHARS: usize = 40;
pub const PRAISE_MAX_CHARS: usize = 260;

/// Research attention is spent only on a real, rested relationship. These
/// thresholds deliberately describe the professional relationship, not fan or
/// marketing consent: somebody may be a fan, or may have opted out of
/// marketing, while still remaining a journalist/promoter/creator the act
/// legitimately works with.
pub const RELATIONSHIP_RESEARCH_MIN_SCORE: i32 = 60;
pub const RELATIONSHIP_RESEARCH_QUIET_DAYS: i64 = 21;

/// Whether this relationship is worth spending deep-research budget on now.
///
/// This grants no contact authority. It only prevents expensive AI research
/// from being spent on cold directory rows, recently contacted people, refused
/// routes, or somebody we already researched recently.
#[must_use]
pub fn relationship_is_worth_researching(
    has_replied: bool,
    relationship_score: i32,
    accepts_outreach: bool,
    do_not_contact: bool,
    days_since_last_contact: Option<i64>,
    has_recent_research: bool,
) -> bool {
    if do_not_contact || !accepts_outreach || has_recent_research {
        return false;
    }
    let known = has_replied || relationship_score >= RELATIONSHIP_RESEARCH_MIN_SCORE;
    let rested =
        days_since_last_contact.is_some_and(|days| days >= RELATIONSHIP_RESEARCH_QUIET_DAYS);
    known && rested
}

/// One thing a person did, with where it was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersonalHook {
    /// A phrase the letter can quote after an em dash, with no sentence-final
    /// full stop: `recenzja płyty „Szum” w audycji „Metalowy Wieczór”`.
    pub fact: String,
    /// One specific sentence of appreciation, if the research found something
    /// true to say. `None` is fine: the fact alone already shows the work was
    /// read.
    pub praise: Option<String>,
    /// Where the fact was found. Shown to the operator, never printed in the
    /// letter.
    pub source_url: String,
    /// When the thing happened or was published — not when it was found.
    pub observed_on: Date,
}

/// Why a candidate hook was refused. Each is a sentence the operator can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookRefusal {
    FactTooShort,
    FactTooLong,
    PraiseTooShort,
    PraiseTooLong,
    /// Describing the research process is the voice of software, not a person.
    MetaResearchVoice,
    /// Empty flattery can be pasted into anybody's inbox.
    GenericPraise,
    /// The opener shares no concrete term with the sourced fact and could be
    /// pasted into an unrelated email.
    UngroundedOpening,
    /// An exclamation mark or a hashtag: not the band's register.
    NotOurRegister,
    /// A link in the text: the letter's links are tracked ones only.
    LinkInText,
    SourceNotHttps,
    FromTheFuture,
    TooOld,
}

impl HookRefusal {
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::FactTooShort => format!(
                "the fact is too short to be a fact — say what they did, in at least \
                 {FACT_MIN_CHARS} characters"
            ),
            Self::FactTooLong => format!("the fact is over {FACT_MAX_CHARS} characters"),
            Self::PraiseTooShort => format!(
                "the opening sentence is under {PRAISE_MIN_CHARS} characters — name one concrete detail instead of a generic compliment"
            ),
            Self::PraiseTooLong => {
                format!("the opening sentence is over {PRAISE_MAX_CHARS} characters")
            }
            Self::MetaResearchVoice => "do not tell the recipient that software researched them — open on the concrete thing itself".to_owned(),
            Self::GenericPraise => "generic praise is not personalization — the opening must contain a concrete observation that could not fit everyone".to_owned(),
            Self::UngroundedOpening => "the opening does not name any concrete detail from the sourced fact — it could be pasted into somebody else's email".to_owned(),
            Self::NotOurRegister => "no exclamation marks or hashtags — a colleague writing, not \
                 a campaign"
                .to_owned(),
            Self::LinkInText => "no links in the text: the source goes in source_url, and the \
                 letter prints only tracked links"
                .to_owned(),
            Self::SourceNotHttps => "the source must be an https address anybody can open and \
                 check"
                .to_owned(),
            Self::FromTheFuture => "the date is in the future".to_owned(),
            Self::TooOld => format!(
                "older than {HOOK_MAX_AGE_DAYS} days — that is not 'lately', find something \
                 recent"
            ),
        }
    }
}

/// Whether a thing observed on `observed_on` is still recent on `today`.
#[must_use]
pub fn is_recent(observed_on: Date, today: Date) -> bool {
    observed_on <= today && today - observed_on <= Duration::days(HOOK_MAX_AGE_DAYS)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_link(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("http://") || lower.contains("https://") || lower.contains("www.")
}

fn grounded_opening(opening: &str, fact: &str) -> bool {
    let words = |text: &str| {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| word.chars().count() >= 4)
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    };
    let fact_words = words(fact);
    words(opening).iter().any(|word| fact_words.contains(word))
}

impl PersonalHook {
    /// Validates and normalises a candidate. This is the only constructor, so
    /// everything that reaches a letter has passed it.
    ///
    /// # Errors
    ///
    /// The first rule the candidate breaks.
    pub fn new(
        fact: &str,
        praise: Option<&str>,
        source_url: &str,
        observed_on: Date,
        today: Date,
    ) -> Result<Self, HookRefusal> {
        let fact = collapse(fact).trim_end_matches(['.', ' ']).to_owned();
        if fact.chars().count() < FACT_MIN_CHARS {
            return Err(HookRefusal::FactTooShort);
        }
        if fact.chars().count() > FACT_MAX_CHARS {
            return Err(HookRefusal::FactTooLong);
        }
        let praise = praise
            .map(collapse)
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        if let Some(opening) = praise.as_deref() {
            let len = opening.chars().count();
            if len < PRAISE_MIN_CHARS {
                return Err(HookRefusal::PraiseTooShort);
            }
            if len > PRAISE_MAX_CHARS {
                return Err(HookRefusal::PraiseTooLong);
            }
            let lower = opening.to_lowercase();
            const META: [&str; 8] = [
                "before writing", "we looked at", "we researched", "we checked",
                "zanim napisaliśmy", "zajrzeliśmy", "sprawdziliśmy", "przejrzeliśmy",
            ];
            if META.iter().any(|needle| lower.contains(needle)) {
                return Err(HookRefusal::MetaResearchVoice);
            }
            const GENERIC: [&str; 6] = [
                "świetna robota", "great work", "love what you do",
                "thanks for supporting the scene", "dzięki za wspieranie sceny",
                "dziękujemy za wspieranie sceny",
            ];
            if GENERIC.iter().any(|needle| lower.contains(needle)) {
                return Err(HookRefusal::GenericPraise);
            }
            if !grounded_opening(opening, &fact) {
                return Err(HookRefusal::UngroundedOpening);
            }
        }
        for text in std::iter::once(fact.as_str()).chain(praise.as_deref()) {
            if text.contains('!') || text.contains('#') {
                return Err(HookRefusal::NotOurRegister);
            }
            if has_link(text) {
                return Err(HookRefusal::LinkInText);
            }
        }
        let source_url = source_url.trim().to_owned();
        let host_ok = source_url
            .strip_prefix("https://")
            .is_some_and(|rest| rest.contains('.') && !rest.starts_with('.'));
        if !host_ok || source_url.chars().any(char::is_whitespace) || source_url.len() > 2048 {
            return Err(HookRefusal::SourceNotHttps);
        }
        if observed_on > today {
            return Err(HookRefusal::FromTheFuture);
        }
        if !is_recent(observed_on, today) {
            return Err(HookRefusal::TooOld);
        }
        Ok(Self {
            fact,
            praise,
            source_url,
            observed_on,
        })
    }

    /// The fact as a sentence ending, for the letter: terminal full stop added
    /// once, here, so a composer never has to remember whether the fact has one.
    #[must_use]
    pub fn praise_sentence(&self) -> Option<String> {
        self.praise.as_deref().map(|text| {
            if text.ends_with(['.', '?']) {
                text.to_owned()
            } else {
                format!("{text}.")
            }
        })
    }
}

/// How the letter addresses its reader, which decides how the opening speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Register {
    /// One working person writing to another they already know: "you".
    Colleague,
    /// A pitch to an outlet, a programme or a desk: no assumption about who is
    /// reading, so no "you".
    Outlet,
}

/// The paragraph a letter opens with: what the band read, before anything it
/// wants. The source is deliberately not in it — every URL in a letter is a
/// tracked link, and a research link is not one.
#[must_use]
pub fn known_paragraph(
    hook: &PersonalHook,
    _language: LetterLanguage,
    _register: Register,
) -> String {
    // The old implementation literally announced the research process
    // ("Before writing we looked at..." / "Zanim napisaliśmy..."). That is
    // exactly how an AI assistant explains itself and exactly how a real
    // curator spots automation. The research worker now supplies one grounded
    // human sentence in `praise`; render that sentence, not the machinery.
    hook.praise_sentence().unwrap_or_else(|| {
        if hook.fact.ends_with(['.', '?']) {
            hook.fact.clone()
        } else {
            format!("{}.", hook.fact)
        }
    })
}

/// Inserts the opening after a letter's greeting line.
///
/// Letters here are a greeting, a blank line, then the body; the opening goes
/// between them, so it is the first thing the reader meets after their name.
/// A body with no blank line gets the opening in front.
#[must_use]
pub fn open_with_known(
    body: &str,
    hook: &PersonalHook,
    language: LetterLanguage,
    register: Register,
) -> String {
    let paragraph = known_paragraph(hook, language, register);
    match body.split_once("\n\n") {
        Some((greeting, rest)) => format!("{greeting}\n\n{paragraph}\n\n{rest}"),
        None => format!("{paragraph}\n\n{body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    const TODAY: Date = date!(2026 - 10 - 02);

    fn ok() -> PersonalHook {
        PersonalHook::new(
            "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            Some("W recenzji „Szum” zwróciło nam uwagę, że weszliście w aranżację, a nie tylko brzmienie."),
            "https://example.test/audycje/metalowy-wieczor",
            date!(2026 - 09 - 20),
            TODAY,
        )
        .expect("a recent, sourced fact")
    }

    #[test]
    fn deep_research_is_for_warm_rested_relationships_only() {
        assert!(relationship_is_worth_researching(
            true,
            20,
            true,
            false,
            Some(40),
            false
        ));
        assert!(relationship_is_worth_researching(
            false,
            RELATIONSHIP_RESEARCH_MIN_SCORE,
            true,
            false,
            Some(40),
            false
        ));
        assert!(!relationship_is_worth_researching(
            false,
            50,
            true,
            false,
            Some(40),
            false
        ));
        assert!(!relationship_is_worth_researching(
            true,
            90,
            true,
            false,
            Some(3),
            false
        ));
        assert!(!relationship_is_worth_researching(
            true,
            90,
            false,
            false,
            Some(40),
            false
        ));
        assert!(!relationship_is_worth_researching(
            true,
            90,
            true,
            true,
            Some(40),
            false
        ));
        assert!(!relationship_is_worth_researching(
            true,
            90,
            true,
            false,
            Some(40),
            true
        ));
    }

    #[test]
    fn a_recent_sourced_fact_passes_and_is_normalised() {
        let hook = PersonalHook::new(
            "  recenzja   płyty „Szum”  w audycji „Metalowy Wieczór”. ",
            None,
            " https://example.test/a ",
            date!(2026 - 09 - 20),
            TODAY,
        )
        .expect("valid");
        assert_eq!(
            hook.fact,
            "recenzja płyty „Szum” w audycji „Metalowy Wieczór”"
        );
        assert_eq!(hook.source_url, "https://example.test/a");
        assert!(hook.praise.is_none());
        assert_eq!(
            ok().praise_sentence().as_deref(),
            Some("W recenzji „Szum” zwróciło nam uwagę, że weszliście w aranżację, a nie tylko brzmienie.")
        );
    }

    #[test]
    fn a_label_is_not_a_fact() {
        let refused = PersonalHook::new(
            "nowy odcinek",
            None,
            "https://e.test/x",
            date!(2026 - 09 - 20),
            TODAY,
        );
        assert_eq!(refused, Err(HookRefusal::FactTooShort));
    }

    #[test]
    fn it_must_be_recent_and_not_from_the_future() {
        let fact = "recenzja płyty „Szum” w audycji „Metalowy Wieczór”";
        let old = date!(2026 - 10 - 02) - Duration::days(HOOK_MAX_AGE_DAYS + 1);
        assert_eq!(
            PersonalHook::new(fact, None, "https://e.test/x", old, TODAY),
            Err(HookRefusal::TooOld)
        );
        let edge = date!(2026 - 10 - 02) - Duration::days(HOOK_MAX_AGE_DAYS);
        assert!(PersonalHook::new(fact, None, "https://e.test/x", edge, TODAY).is_ok());
        assert_eq!(
            PersonalHook::new(fact, None, "https://e.test/x", date!(2026 - 10 - 03), TODAY),
            Err(HookRefusal::FromTheFuture)
        );
        assert!(is_recent(TODAY, TODAY));
        assert!(!is_recent(date!(2026 - 10 - 03), TODAY));
    }

    #[test]
    fn an_email_opener_must_sound_like_a_person_not_research_software() {
        let fact = "recenzja płyty „Szum” w audycji „Metalowy Wieczór”";
        let day = date!(2026 - 09 - 20);
        assert_eq!(
            PersonalHook::new(
                fact,
                Some("Zanim napisaliśmy, zajrzeliśmy do tego, co ostatnio u Was"),
                "https://e.test/x",
                day,
                TODAY
            ),
            Err(HookRefusal::MetaResearchVoice)
        );
        assert_eq!(
            PersonalHook::new(
                fact,
                Some("Świetna robota i dzięki za wspieranie sceny od tylu lat"),
                "https://e.test/x",
                day,
                TODAY
            ),
            Err(HookRefusal::GenericPraise)
        );
        assert_eq!(
            PersonalHook::new(
                fact,
                Some("Fajny materiał."),
                "https://e.test/x",
                day,
                TODAY
            ),
            Err(HookRefusal::PraiseTooShort)
        );
        assert_eq!(
            PersonalHook::new(
                fact,
                Some("To bardzo konkretny materiał i naprawdę dobrze się go czyta od początku do końca"),
                "https://e.test/x",
                day,
                TODAY
            ),
            Err(HookRefusal::UngroundedOpening)
        );
    }

    #[test]
    fn the_source_must_be_a_checkable_https_address() {
        let fact = "recenzja płyty „Szum” w audycji „Metalowy Wieczór”";
        for bad in [
            "",
            "http://e.test/x",
            "https://",
            "https://nodot",
            "ftp://e.test/x",
            "https://e.test/a b",
            "e.test/x",
        ] {
            assert_eq!(
                PersonalHook::new(fact, None, bad, date!(2026 - 09 - 20), TODAY),
                Err(HookRefusal::SourceNotHttps),
                "{bad:?}"
            );
        }
    }

    /// A model asked to be warm will reach for hype, tags and links. None of it
    /// is the band's voice and a link would trip the tracked-link gate.
    #[test]
    fn the_text_keeps_the_bands_register() {
        let fact = "recenzja płyty „Szum” w audycji „Metalowy Wieczór”";
        let day = date!(2026 - 09 - 20);
        for (praise, expected) in [
            ("W recenzji „Szum” podoba nam się konkret, ale świetna robota!", HookRefusal::NotOurRegister),
            ("W recenzji „Szum” jest konkretny detal, ale Super #metal", HookRefusal::NotOurRegister),
            ("W recenzji „Szum” jest konkretny detal — zobacz https://e.test/x", HookRefusal::LinkInText),
            ("W recenzji „Szum” jest konkretny detal — zajrzyj na www.e.test", HookRefusal::LinkInText),
        ] {
            assert_eq!(
                PersonalHook::new(fact, Some(praise), "https://e.test/x", day, TODAY),
                Err(expected),
                "{praise}"
            );
        }
        assert_eq!(
            PersonalHook::new(
                "recenzja płyty „Szum” (zobacz https://e.test)",
                None,
                "https://e.test/x",
                day,
                TODAY
            ),
            Err(HookRefusal::LinkInText)
        );
        let long = "a".repeat(PRAISE_MAX_CHARS + 1);
        assert_eq!(
            PersonalHook::new(fact, Some(&long), "https://e.test/x", day, TODAY),
            Err(HookRefusal::PraiseTooLong)
        );
    }

    #[test]
    fn every_refusal_says_what_to_do() {
        for refusal in [
            HookRefusal::FactTooShort,
            HookRefusal::FactTooLong,
            HookRefusal::PraiseTooShort,
            HookRefusal::PraiseTooLong,
            HookRefusal::MetaResearchVoice,
            HookRefusal::GenericPraise,
            HookRefusal::UngroundedOpening,
            HookRefusal::NotOurRegister,
            HookRefusal::LinkInText,
            HookRefusal::SourceNotHttps,
            HookRefusal::FromTheFuture,
            HookRefusal::TooOld,
        ] {
            assert!(refusal.message().len() > 20, "{refusal:?}");
        }
    }

    #[test]
    fn the_opening_goes_between_the_greeting_and_the_body() {
        let hook = ok();
        let letter = open_with_known(
            "Dzień dobry, Radio Lokalne,\n\nPiszemy w sprawie płyty.\n\nPozdrawiamy",
            &hook,
            LetterLanguage::Polish,
            Register::Outlet,
        );
        let greeting = letter.find("Dzień dobry").unwrap();
        let read = letter.find("recenzja płyty „Szum”").unwrap();
        let ask = letter.find("Piszemy w sprawie").unwrap();
        assert!(greeting < read && read < ask, "{letter}");
        assert!(
            letter.contains("W recenzji „Szum” zwróciło nam uwagę"),
            "grounded human opener: {letter}"
        );
        assert!(!letter.contains("Zanim napisaliśmy"), "{letter}");
        assert!(
            !letter.contains("example.test"),
            "the source stays out of the letter"
        );
        let colleague = known_paragraph(&hook, LetterLanguage::Polish, Register::Colleague);
        assert!(colleague.contains("W recenzji „Szum”"), "{colleague}");
        let english = known_paragraph(&hook, LetterLanguage::English, Register::Outlet);
        assert!(!english.contains("Before writing"), "{english}");
        let no_break = open_with_known(
            "no greeting break",
            &hook,
            LetterLanguage::English,
            Register::Outlet
        );
        assert!(no_break.starts_with("W recenzji „Szum”"), "{no_break}");
        assert!(!no_break.contains("Before writing"), "{no_break}");
    }
}
