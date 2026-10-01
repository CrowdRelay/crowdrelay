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

use time::{Date, Duration};

/// How old the researched thing may be. A review from last spring is not
/// "lately", and a letter that cites it reads as a lookup rather than as
/// somebody paying attention.
pub const HOOK_MAX_AGE_DAYS: i64 = 120;

/// Shorter than this is a label, not a fact ("a show", "new episode").
pub const FACT_MIN_CHARS: usize = 20;
pub const FACT_MAX_CHARS: usize = 400;
pub const PRAISE_MAX_CHARS: usize = 300;

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
    PraiseTooLong,
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
            Self::PraiseTooLong => {
                format!("the appreciation is over {PRAISE_MAX_CHARS} characters")
            }
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
        if praise
            .as_deref()
            .is_some_and(|text| text.chars().count() > PRAISE_MAX_CHARS)
        {
            return Err(HookRefusal::PraiseTooLong);
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    const TODAY: Date = date!(2026 - 10 - 02);

    fn ok() -> PersonalHook {
        PersonalHook::new(
            "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            Some("Dobrze, że ktoś mówi o tej płycie tak konkretnie."),
            "https://example.test/audycje/metalowy-wieczor",
            date!(2026 - 09 - 20),
            TODAY,
        )
        .expect("a recent, sourced fact")
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
            Some("Dobrze, że ktoś mówi o tej płycie tak konkretnie.")
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
            ("Świetna robota!", HookRefusal::NotOurRegister),
            ("Super #metal", HookRefusal::NotOurRegister),
            ("Zobacz https://e.test/x", HookRefusal::LinkInText),
            ("Zajrzyj na www.e.test", HookRefusal::LinkInText),
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
            HookRefusal::PraiseTooLong,
            HookRefusal::NotOurRegister,
            HookRefusal::LinkInText,
            HookRefusal::SourceNotHttps,
            HookRefusal::FromTheFuture,
            HookRefusal::TooOld,
        ] {
            assert!(refusal.message().len() > 20, "{refusal:?}");
        }
    }
}
