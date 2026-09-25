//! Sheet-verdict → disposition mapping for imported outreach answers.
//!
//! The band's outreach workbooks (`VIRYA_MASTER.xlsx` OUTREACH tab,
//! `PROMO.xlsx` OUTREACH tab) carry a result column the operator fills in by
//! hand — `POSITIVE`, `GMAIL_REPLY`, `Odrzucone`, `NEGOTIATING`. The
//! one-off import that produced the `master:`/`promo:` interaction rows
//! stored those verdicts in `metadata` and filed every unmapped row as
//! `received`, so nineteen real yeses were indistinguishable from forty
//! maybes. This map is the vocabulary, in both sheets' spellings, decided
//! deterministically.
//!
//! Two outcomes, nothing between:
//!
//! - [`ImportedVerdict::Terminal`] — the sheet recorded a final answer; the
//!   interaction's disposition is updated to match.
//! - [`ImportedVerdict::NeedsHuman`] — the sheet records that a reply
//!   exists without saying what it means (`GMAIL_REPLY`), or records a
//!   state that is a decision (`NEGOTIATING`, `DECLINED_TEMPORARILY`).
//!   Those mint a triage row with `imported_verdict` — a human reads the
//!   thread before anything acts on it.
//!
//! Unknown codes route to `NeedsHuman`. A guessed disposition is worse than
//! a queue.
//!
//! The raw verdict is always preserved in `metadata` by the caller — this
//! map decides the normalized disposition only, so a mapping corrected
//! later re-reads the same source value.

use crate::outreach::OutreachReplyDisposition;

/// What a sheet verdict means for the reply that carries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedVerdict {
    /// The sheet's answer is terminal — write this disposition onto the
    /// interaction. Nothing reviews it further.
    Terminal(OutreachReplyDisposition),
    /// The verdict does not settle the reply — a human reads it. The
    /// interaction keeps `received` and the reply joins the triage queue
    /// with reason `imported_verdict`.
    NeedsHuman,
}

/// Exact verdict spellings that mean the conversation went well.
///
/// The sheet mixes machine codes and hand-typed prose; both are kept as
/// written. "Positive routing" is the band's own positive family — the
/// answer reached the right person — while bare `FORWARDED` stays with the
/// humans because it records a hand-off, not an answer.
const POSITIVE_EXACT: &[&str] = &[
    "positive",
    "positive_routing",
    "positive_pending_details",
    "patronage_accepted",
    "review_accepted_pending_cd",
    "listing_accepted",
    "accepted / coverage promised",
    "warm / review interest",
    "potwierdzone",
    "zamknięte / sukces",
    "zamkniete / sukces",
];

/// Verdicts whose head is fixed but whose tail carries per-row detail —
/// `Dodano listing 31.08`, `Battle of Bands active 17–21 Aug`.
const POSITIVE_PREFIX: &[&str] = &["dodano listing", "battle of bands active"];

/// `Technophobia aired`, `… aired` — the coverage ran.
const POSITIVE_SUFFIX: &[&str] = &[" aired"];

const DECLINED_EXACT: &[&str] = &["negative", "no", "odrzucone", "odrzucenie"];

const DO_NOT_CONTACT_EXACT: &[&str] = &[
    "do_not_contact",
    "dnc",
    "unsubscribe",
    "wypisz",
    "nie kontaktuj",
];

/// Verdicts that mean "a person answered, the sheet does not say what they
/// decided" — every one of these is a conversation waiting on the band.
const NEEDS_HUMAN_EXACT: &[&str] = &[
    "gmail_reply",
    "decision required",
    "negotiating",
    "declined_temporarily",
    "neutral",
    "forwarded",
];

/// Maps one sheet verdict to its disposition outcome.
///
/// Matching is case-insensitive over the trimmed verdict with interior
/// whitespace collapsed — the sheets are typed by hand and `Decision
/// required` must equal `Decision  Required`.
#[must_use]
pub fn map_sheet_verdict(raw: &str) -> ImportedVerdict {
    let verdict = normalize(raw);
    if verdict.is_empty() {
        return ImportedVerdict::NeedsHuman;
    }
    if POSITIVE_EXACT.contains(&verdict.as_str())
        || POSITIVE_PREFIX
            .iter()
            .any(|prefix| verdict.starts_with(prefix))
        || POSITIVE_SUFFIX.iter().any(|suffix| {
            let word = suffix.trim_start();
            verdict == word
                || verdict
                    .strip_suffix(word)
                    .is_some_and(|head| head.ends_with(' '))
        })
    {
        return ImportedVerdict::Terminal(OutreachReplyDisposition::Positive);
    }
    if DECLINED_EXACT.contains(&verdict.as_str()) {
        return ImportedVerdict::Terminal(OutreachReplyDisposition::Declined);
    }
    if DO_NOT_CONTACT_EXACT.contains(&verdict.as_str()) {
        return ImportedVerdict::Terminal(OutreachReplyDisposition::DoNotContact);
    }
    if NEEDS_HUMAN_EXACT.contains(&verdict.as_str()) {
        return ImportedVerdict::NeedsHuman;
    }
    ImportedVerdict::NeedsHuman
}

/// Lowercase, trim, collapse interior whitespace runs to one space.
fn normalize(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maps_to(raw: &str, expected: ImportedVerdict) {
        assert_eq!(map_sheet_verdict(raw), expected, "verdict {raw:?}");
    }

    #[test]
    fn every_positive_code_seen_in_production_maps_positive() {
        for code in [
            "POSITIVE",
            "POSITIVE_ROUTING",
            "POSITIVE_PENDING_DETAILS",
            "PATRONAGE_ACCEPTED",
            "REVIEW_ACCEPTED_PENDING_CD",
            "LISTING_ACCEPTED",
            "Accepted / coverage promised",
            "Warm / review interest",
            "Potwierdzone",
            "Zamknięte / sukces",
            "Dodano listing 31.08",
            "Technophobia aired",
            "Battle of Bands active 17–21 Aug",
        ] {
            maps_to(
                code,
                ImportedVerdict::Terminal(OutreachReplyDisposition::Positive),
            );
        }
    }

    #[test]
    fn every_declined_code_seen_in_production_maps_declined() {
        for code in ["NEGATIVE", "NO", "Odrzucone"] {
            maps_to(
                code,
                ImportedVerdict::Terminal(OutreachReplyDisposition::Declined),
            );
        }
    }

    #[test]
    fn ambiguous_and_progress_codes_need_a_human() {
        for code in [
            "GMAIL_REPLY",
            "Decision required",
            "NEGOTIATING",
            "DECLINED_TEMPORARILY",
            "NEUTRAL",
            "FORWARDED",
        ] {
            maps_to(code, ImportedVerdict::NeedsHuman);
        }
    }

    #[test]
    fn unknown_and_empty_codes_fail_toward_a_human() {
        for code in ["", "   ", "SOMETHING_NEW", "Bounce", "auto-reply"] {
            maps_to(code, ImportedVerdict::NeedsHuman);
        }
    }

    #[test]
    fn matching_is_case_and_whitespace_insensitive() {
        maps_to(
            "positive",
            ImportedVerdict::Terminal(OutreachReplyDisposition::Positive),
        );
        maps_to(
            "  Accepted   /   coverage   promised ",
            ImportedVerdict::Terminal(OutreachReplyDisposition::Positive),
        );
        maps_to(
            "ODRZUCONE",
            ImportedVerdict::Terminal(OutreachReplyDisposition::Declined),
        );
    }

    #[test]
    fn explicit_do_not_contact_codes_map_to_dnc() {
        for code in ["DO_NOT_CONTACT", "dnc", "unsubscribe"] {
            maps_to(
                code,
                ImportedVerdict::Terminal(OutreachReplyDisposition::DoNotContact),
            );
        }
    }
}
