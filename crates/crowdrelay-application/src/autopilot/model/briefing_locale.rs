// Briefings in the language the crew reads.
//
// `briefing()` authors every action in English, exhaustively — the compiler
// refuses a new payload variant until somebody writes its briefing, and that
// guarantee is worth keeping exactly as it is. This module is the layer that
// puts the result in front of a person.
//
// # Why this is not a translation of the file next door
//
// Virya is Polish, and before this module the briefings were English prose
// inside a Polish email frame: a crew member read *"Dlaczego to ważne:
// Changing the ticket price affects both revenue and attendance."* A handful
// of labels had drifted into Polish too, so neither language was coherent.
//
// The obvious fix — write the briefings in Polish — would bake one tenant's
// language into a crate every tenant shares. So the briefing stays English at
// source and is localised here, per tenant, from the locale the control plane
// already collects in the regional profile.
//
// # Partial by design
//
// A locale covers what has been written for it and nothing else. Anything
// absent falls back to the English source rather than failing to compile or
// rendering an empty string — the same rule the console applies to north-star
// wording, where an unknown value keeps the server's own label. A briefing
// that is half translated is worse than one that is honestly English, so the
// tables below are organised by action so a whole briefing moves at once.

use crate::autopilot::control::ActionBriefing;

/// The languages a briefing can be read in. `En` is the source language and is
/// always complete; every other locale is an overlay that may be partial.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BriefingLocale {
    #[default]
    En,
    Pl,
}

impl BriefingLocale {
    /// Resolves a BCP-47 locale tag from the tenant's regional profile.
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

impl ActionBriefing {
    /// Returns this briefing in `locale`, leaving anything untranslated in the
    /// source language.
    #[must_use]
    pub fn localized(mut self, locale: BriefingLocale) -> Self {
        let BriefingLocale::Pl = locale else {
            return self;
        };
        self.summary = translate_pl(&self.summary);
        self.why_it_matters = translate_pl(&self.why_it_matters);
        for step in &mut self.steps {
            step.what_to_do = translate_pl(&step.what_to_do);
            step.why_it_matters = translate_pl(&step.why_it_matters);
        }
        for field in &mut self.content {
            field.label = translate_pl(&field.label);
        }
        self
    }
}

/// Looks a phrase up, falling back to the source string.
///
/// Matching is exact on the whole string. Substring or fuzzy matching over
/// prose produces sentences that are half one language and read worse than
/// either — the failure this module exists to remove.
fn translate_pl(source: &str) -> String {
    PL.iter()
        .find(|(en, _)| *en == source)
        .map_or_else(|| source.to_owned(), |(_, pl)| (*pl).to_owned())
}

/// English source phrase, then the Polish a band member reads.
///
/// Field labels first — they are a small closed set, they repeat across almost
/// every action, and they are what the panel renders in its content block.
/// Step text second: the steps repeat far more than the summaries do, because
/// most actions end in the same two instructions.
const PL: &[(&str, &str)] = &[
    // ── Field labels ────────────────────────────────────────────────────
    ("Event", "Wydarzenie"),
    ("Opportunity", "Okazja"),
    ("Task", "Zadanie"),
    ("Task title", "Nazwa zadania"),
    ("Ticket type", "Typ biletu"),
    ("Target", "Cel"),
    ("Targets", "Cele"),
    ("Target type", "Typ celu"),
    ("Play type", "Typ działania"),
    ("Recommended action", "Zalecane działanie"),
    ("Outcome", "Wynik"),
    ("Platform", "Platforma"),
    ("Campaign", "Kampania"),
    ("Experiment", "Eksperyment"),
    ("Reason", "Powód"),
    ("Body", "Treść"),
    ("Subject", "Temat"),
    ("Title", "Tytuł"),
    ("Name", "Nazwa"),
    ("Description", "Opis"),
    ("Recipient", "Adresat"),
    ("City", "Miasto"),
    ("Amount", "Kwota"),
    ("Currency", "Waluta"),
    ("Deadline", "Termin"),
    ("Previous price", "Poprzednia cena"),
    ("New price", "Nowa cena"),
    ("Variant", "Wariant"),
    ("Winning variant", "Zwycięski wariant"),
    ("Product", "Produkt"),
    ("Source", "Źródło"),
    ("Template", "Szablon"),
    ("Phase", "Etap"),
    ("Tier", "Poziom"),
    ("Type", "Typ"),
    ("Track ID", "Numer utworu"),
    ("Smart link", "Link"),
    ("Fan", "Fan"),
    ("Beacon", "Beacon"),
    ("Signal", "Signal"),
    ("Subreddit", "Subreddit"),
    // ── Step instructions ───────────────────────────────────────────────
    (
        "Click APPROVE to apply it",
        "Kliknij ZATWIERDŹ, aby to wykonać",
    ),
    (
        "Check the new price and the ticket type",
        "Sprawdź nową cenę i typ biletu",
    ),
    (
        "Once approved the price is live immediately",
        "Po zatwierdzeniu cena obowiązuje od razu",
    ),
    (
        "Make sure the change is deliberate",
        "Upewnij się, że zmiana jest zamierzona",
    ),
    (
        "Make sure the venue can hold it",
        "Upewnij się, że miejsce pomieści tyle osób",
    ),
    (
        "Once approved the capacity is live",
        "Po zatwierdzeniu limit obowiązuje od razu",
    ),
    (
        "Make sure the tone fits",
        "Sprawdź, czy ton pasuje do zespołu",
    ),
    (
        "The message is delivered to the fan",
        "Wiadomość trafia do fana",
    ),
    (
        "Confirm the stock is genuinely short",
        "Potwierdź, że zapas naprawdę się kończy",
    ),
    (
        "Once approved the order is placed",
        "Po zatwierdzeniu zamówienie zostaje złożone",
    ),
    // ── Why-it-matters, for the actions that reach the crew most ────────
    (
        "Changing the ticket price affects both revenue and attendance. Someone may already have paid the old price, and this cannot be undone.",
        "Zmiana ceny biletu wpływa na przychód i na frekwencję. Ktoś mógł już zapłacić starą cenę, a tego nie da się cofnąć.",
    ),
    (
        "Capacity decides how many tickets can be sold. Lowering it can invalidate tickets already sold.",
        "Limit decyduje, ile biletów można sprzedać. Obniżenie go może unieważnić bilety już sprzedane.",
    ),
    (
        "This goes to a fan who consented to be contacted. A sent message cannot be unsent.",
        "To trafia do fana, który zgodził się na kontakt. Wysłanej wiadomości nie da się cofnąć.",
    ),
    (
        "A merch order costs money and takes time to arrive. Confirm the stock is genuinely short before ordering.",
        "Zamówienie merchu kosztuje i trwa. Potwierdź, że zapas naprawdę się kończy, zanim zamówisz.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_subtag_decides_the_locale() {
        assert_eq!(BriefingLocale::from_tag("pl-PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("pl_PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("en-IE"), BriefingLocale::En);
    }

    /// A locale we have no wording for must read as the source language, not
    /// as an empty briefing.
    #[test]
    fn an_unknown_locale_is_the_source_language() {
        assert_eq!(BriefingLocale::from_tag("de-DE"), BriefingLocale::En);
        assert_eq!(BriefingLocale::from_tag(""), BriefingLocale::En);
    }

    #[test]
    fn an_untranslated_phrase_keeps_its_source_text() {
        let source = "A sentence nobody has written Polish for yet";
        assert_eq!(translate_pl(source), source);
    }

    #[test]
    fn a_translated_phrase_is_replaced_whole() {
        assert_eq!(translate_pl("Event"), "Wydarzenie");
    }

    /// Prose must match end to end. A substring rule would translate "Event"
    /// inside "Event sold out" and leave the rest English, which is the mixed
    /// sentence this module removes.
    #[test]
    fn matching_is_whole_string_not_substring() {
        assert_eq!(translate_pl("Event sold out"), "Event sold out");
    }

    #[test]
    fn the_source_locale_changes_nothing() {
        let briefing = ActionBriefing {
            summary: "Event".into(),
            why_it_matters: "Event".into(),
            steps: vec![],
            content: vec![],
            deadline_note: String::new(),
        };
        let unchanged = briefing.clone().localized(BriefingLocale::En);
        assert_eq!(unchanged.summary, "Event");
        let polish = briefing.localized(BriefingLocale::Pl);
        assert_eq!(polish.summary, "Wydarzenie");
    }

    /// Two English phrases must never map to one Polish string by accident:
    /// the panel shows labels side by side and duplicates read as a bug.
    #[test]
    fn the_table_has_no_duplicate_source_phrases() {
        let mut seen = std::collections::HashSet::new();
        for (en, _) in PL {
            assert!(seen.insert(*en), "duplicate source phrase: {en}");
        }
    }
}
