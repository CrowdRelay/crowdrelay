//! Whether a community draft is written in the community's language.
//!
//! # Why this exists
//!
//! The repost worker is told the community's language and asked to write in
//! it; when the language is not recorded it is asked to infer it. That is a
//! request to a model, and on 2026-09-26 the model answered it wrong: the
//! band's Polish caption went to r/melodicdeathmetal in Polish, word for
//! word, and a moderator removed it. The relay batch had been approved once,
//! on an English sample for r/metalcore; every later draft queued under that
//! approval without a person reading it. This check is the part that reads
//! every draft.
//!
//! # What it decides, and what it does not
//!
//! Polish against English only — the two languages the tenants write in. It
//! counts words that only one of the two languages uses; a draft too short or
//! too mixed to call is `Undetermined` and passes, because refusing what we
//! cannot read would refuse every one-line title.
//!
//! A community with no language on record is read as English. Every
//! community admitted on Reddit so far is English-language (44 of 44 on
//! 2026-09-27, 41 of them with no language recorded); a Polish community gets
//! `pl` recorded when it is admitted, and a draft for it is then held to that.

/// `last_error_kind` for a community draft stopped because it is not in
/// its community's language — withdrawn from the approval queue, or refused
/// at dispatch if it was already approved.
pub const COMMUNITY_LANGUAGE_MISMATCH: &str = "community_language_mismatch";

/// The language a draft reads as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftLanguage {
    Polish,
    English,
    Undetermined,
}

impl DraftLanguage {
    #[must_use]
    pub const fn code(self) -> Option<&'static str> {
        match self {
            Self::Polish => Some("pl"),
            Self::English => Some("en"),
            Self::Undetermined => None,
        }
    }
}

/// Words only Polish uses among the two — function words that carry no
/// band or place name. Single-letter Polish words ("i", "w", "z") are left
/// out: "i" is English too. So are "do" and "was", which both languages
/// spell alike — neither list carries them.
const POLISH_WORDS: &[&str] = &[
    "się",
    "nie",
    "jest",
    "że",
    "jak",
    "dla",
    "oraz",
    "czy",
    "ale",
    "już",
    "tylko",
    "przez",
    "będzie",
    "jesteśmy",
    "nas",
    "wam",
    "gramy",
    "koncert",
    "koncertu",
    "dzięki",
    "na",
    "od",
    "po",
    "za",
    "tak",
    "też",
    "bardzo",
    "wszystkim",
    "zobaczenia",
];

/// Words only English uses among the two.
const ENGLISH_WORDS: &[&str] = &[
    "the", "and", "of", "is", "are", "in", "for", "with", "we", "our", "you", "your", "this",
    "that", "on", "at", "see", "thanks", "from", "it", "be",
];

fn has_polish_letter(word: &str) -> bool {
    word.chars()
        .any(|c| matches!(c, 'ą' | 'ć' | 'ę' | 'ł' | 'ń' | 'ó' | 'ś' | 'ź' | 'ż'))
}

/// Polish spells four sounds with digraphs English words almost never
/// contain. Slang drops the diacritics and the function words both: "Wariacie
/// wpadasz na gigusa?" — queued on 2026-09-26 for r/deathmetal and
/// r/metalcore — has one listed word ("na") and no Polish letter, so it read
/// as undetermined and passed. "wpadasz" is the second vote.
fn has_polish_digraph(word: &str) -> bool {
    ["sz", "cz", "rz", "dz"]
        .iter()
        .any(|digraph| word.contains(digraph))
}

/// Reads a draft's language from the words only one of the two languages
/// uses. A word with a Polish letter counts for Polish — but a place name
/// in an English sentence ("we play Gorzów") is outvoted by the English
/// around it, so the call is the larger count, and needs at least two.
#[must_use]
pub fn detect_draft_language(text: &str) -> DraftLanguage {
    let lower = text.to_lowercase();
    let mut polish = 0_usize;
    let mut english = 0_usize;
    for word in lower
        .split(|c: char| !c.is_alphabetic() && c != '\'')
        .filter(|word| !word.is_empty())
    {
        if has_polish_letter(word) || has_polish_digraph(word) || POLISH_WORDS.contains(&word) {
            polish += 1;
        } else if ENGLISH_WORDS.contains(&word) {
            english += 1;
        }
    }
    if polish >= 2 && polish > english {
        DraftLanguage::Polish
    } else if english >= 2 && english > polish {
        DraftLanguage::English
    } else {
        DraftLanguage::Undetermined
    }
}

/// The language a community is held to: its recorded code, or English when
/// none is recorded (see the module docs).
#[must_use]
pub fn expected_community_language(recorded: Option<&str>) -> String {
    recorded
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .map_or_else(|| "en".to_owned(), str::to_ascii_lowercase)
}

/// `Some((expected, found))` when the draft reads as a language other than
/// the community's; `None` when it matches or cannot be called.
#[must_use]
pub fn community_language_mismatch(
    text: &str,
    recorded: Option<&str>,
) -> Option<(String, &'static str)> {
    let expected = expected_community_language(recorded);
    let found = detect_draft_language(text).code()?;
    // Only the two languages this check can read are enforced; a community
    // recorded as anything else is left to the operator.
    if !matches!(expected.as_str(), "pl" | "en") || expected == found {
        return None;
    }
    Some((expected, found))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slang with no diacritics and one function word, queued for two
    /// English-language communities before the ingest gate existed.
    #[test]
    fn diacritic_free_slang_still_reads_polish() {
        let title_only = "Wariacie wpadasz na gigusa?\n#modernmetal";
        assert_eq!(detect_draft_language(title_only), DraftLanguage::Polish);
        assert_eq!(
            community_language_mismatch(title_only, None),
            Some(("en".to_owned(), "pl"))
        );
        // Its English sibling, and a hashtag-only body, stay English or
        // undetermined — never Polish.
        assert_ne!(
            detect_draft_language("Crazy, you drop into the gig?\n#modernmetal"),
            DraftLanguage::Polish
        );
        assert_eq!(
            detect_draft_language("Catch the faces. See you at therapy."),
            DraftLanguage::English
        );
    }

    /// The post a moderator removed on 2026-09-26, and the English drafts of
    /// the same caption that were written for the other two communities.
    #[test]
    fn the_removed_relay_reads_polish_and_its_siblings_english() {
        let removed = "Terapia grupowa, spowiedź szaleńca, mental metal.\n\
                       Łapcie mordeczki i do zobaczenia na terapii.";
        assert_eq!(detect_draft_language(removed), DraftLanguage::Polish);
        assert_eq!(
            community_language_mismatch(removed, None),
            Some(("en".to_owned(), "pl"))
        );
        let english = "Group therapy, madman's confession, mental metal.\n\
                       Grab the bros and see you at therapy. Thanks for the footage.";
        assert_eq!(detect_draft_language(english), DraftLanguage::English);
        assert_eq!(community_language_mismatch(english, None), None);
    }

    /// A Polish place name inside an English sentence is not a Polish post.
    #[test]
    fn a_place_name_does_not_make_an_english_post_polish() {
        let text = "We play Gorzów Wielkopolski on 17 October, see you at the front.";
        assert_eq!(detect_draft_language(text), DraftLanguage::English);
    }

    #[test]
    fn a_polish_community_holds_drafts_to_polish() {
        let english = "New video is out, thanks for the support and see you at the show.";
        assert_eq!(
            community_language_mismatch(english, Some("pl")),
            Some(("pl".to_owned(), "en"))
        );
        let polish = "Nowy klip już jest, dzięki za wsparcie i do zobaczenia na koncercie.";
        assert_eq!(community_language_mismatch(polish, Some("PL")), None);
    }

    /// Too short to call is not a verdict: a bare title passes.
    #[test]
    fn a_draft_too_short_to_read_passes() {
        assert_eq!(
            detect_draft_language("VIRYA — Echoes"),
            DraftLanguage::Undetermined
        );
        assert_eq!(community_language_mismatch("Mental metal.", None), None);
    }

    #[test]
    fn a_community_in_another_language_is_left_to_the_operator() {
        let polish = "Nowy klip już jest, dzięki za wsparcie i do zobaczenia na koncercie.";
        assert_eq!(community_language_mismatch(polish, Some("de")), None);
    }
}
