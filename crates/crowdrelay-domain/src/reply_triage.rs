//! Reply triage — first-party classification of inbound outreach and booking
//! replies.
//!
//! The plan (Phase 19) says: "Inbound replies classified and routed, so a
//! human reads the three that need a human rather than forty that do not."
//! Today n8n assigns a disposition (`positive`, `declined`, `do_not_contact`,
//! `received`) but does not classify reliably — some replies are unclassified
//! (`received`), some are misclassified. This module gives the agent a
//! first-party classifier that reads the reply text and assigns a disposition
//! or routes to human review.
//!
//! The classifier is keyword-based, not ML. The plan says "classified and
//! routed", not "classified perfectly". A wrong classification is corrected
//! by the human review path, which is the point of `NeedsHuman`.

use crate::autonomy::Confidence;
use crate::outreach::{OutreachReplyDisposition, OutreachTargetKind};

/// Input to the reply classifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyClassificationInput<'a> {
    /// The free-text body of the reply, typically 1–5 sentences.
    pub reply_text: &'a str,
    /// What kind of target the reply came from — a playlist curator, a venue,
    /// a radio station, etc. Used for context-specific keyword matching.
    pub target_kind: OutreachTargetKind,
    /// The disposition n8n assigned, if any. `None` means no prior
    /// classification; `Some(Received)` means n8n stored it without
    /// classifying; the others are n8n's claim that this classifier
    /// re-examines.
    pub previous_disposition: Option<OutreachReplyDisposition>,
}

/// The classifier's verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplyClassification {
    /// The reply was classified with enough confidence to act on.
    Auto {
        disposition: OutreachReplyDisposition,
        confidence: Confidence,
        /// Which rules matched, for auditability. A human reviewing the
        /// classification later can see why the agent decided what it did.
        matched_rules: Vec<&'static str>,
    },
    /// The reply needs a human to read it before any action is taken.
    NeedsHuman {
        reason: HumanReviewReason,
        confidence: Confidence,
    },
}

/// Why a reply was routed to human review rather than auto-classified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanReviewReason {
    /// Both positive and negative signals are present in the text.
    AmbiguousText,
    /// The text is not detectably Polish or English.
    NotInSupportedLanguage,
    /// The text is too short to classify reliably (likely a typo or empty).
    TooShort,
    /// The target was previously marked do-not-contact. Re-classification
    /// of a DNC target needs a human — the agent must not silently lift a
    /// DNC flag based on keyword matching.
    PreviousDoNotContact,
    /// No keywords matched any category. The reply is in a supported language
    /// and long enough, but says something the rules do not recognise.
    UnmatchedText,
    /// The reply came from a booking-channel counterparty — a promoter, a
    /// venue, or a festival. A negotiation reply is always a human's call: the operator
    /// already filed its disposition with the reply, and the number inside
    /// the text is a proposal to confirm, not a disposition to infer.
    NegotiationReply,
}

impl HumanReviewReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousText => "ambiguous_text",
            Self::NotInSupportedLanguage => "not_in_supported_language",
            Self::TooShort => "too_short",
            Self::PreviousDoNotContact => "previous_do_not_contact",
            Self::UnmatchedText => "unmatched_text",
            Self::NegotiationReply => "negotiation_reply",
        }
    }
}

/// Classifies an inbound reply.
///
/// The rules are keyword-based, case-insensitive, and cover Polish and
/// English — the two languages the band's curators write in. The classifier
/// is deliberately conservative: when in doubt, route to a human. The cost
/// of a wrong auto-classification (pitching someone who said no, or ignoring
/// someone who said yes) is higher than the cost of a human reading one
/// extra reply.
#[must_use]
pub fn classify_reply(input: &ReplyClassificationInput<'_>) -> ReplyClassification {
    // A previous DNC always needs a human. The agent must not silently lift
    // a do-not-contact flag based on keyword matching — that is a decision
    // a human makes after reading the reply in context.
    if input.previous_disposition == Some(OutreachReplyDisposition::DoNotContact) {
        return ReplyClassification::NeedsHuman {
            reason: HumanReviewReason::PreviousDoNotContact,
            confidence: Confidence::saturating_from_basis_points(10_000),
        };
    }

    let text = input.reply_text.trim();
    if text.len() < 3 {
        return ReplyClassification::NeedsHuman {
            reason: HumanReviewReason::TooShort,
            confidence: Confidence::saturating_from_basis_points(3_000),
        };
    }

    let lower = text.to_lowercase();
    if !is_polish_or_english(&lower) {
        return ReplyClassification::NeedsHuman {
            reason: HumanReviewReason::NotInSupportedLanguage,
            confidence: Confidence::saturating_from_basis_points(3_000),
        };
    }

    let mut positive = positive_matches(&lower);
    let declined = declined_matches(&lower);
    let dnc = dnc_matches(&lower);

    // "not interested" is a decline, but "interested" alone is positive.
    // If the negative form matched, remove the bare positive match to avoid
    // a false ambiguity.
    if declined.contains(&"declined:not_interested") {
        positive.retain(|r| *r != "positive:interested");
    }
    // Same for "nie zainteresowany" / "nie zainteresowana" vs "zainteresowany".
    if declined.contains(&"declined:nie_zainteresowany")
        || declined.contains(&"declined:nie_zainteresowana")
    {
        positive.retain(|r| *r != "positive:zainteresowany" && *r != "positive:zainteresowana");
    }

    // DNC is the strongest signal — "stop contacting me" is a legal request
    // and overrides everything else. If "unsubscribe" and "yes" both appear,
    // the unsubscribe wins.
    if !dnc.is_empty() {
        return ReplyClassification::Auto {
            disposition: OutreachReplyDisposition::DoNotContact,
            confidence: Confidence::saturating_from_basis_points(9_800),
            matched_rules: dnc,
        };
    }

    // Ambiguous: both positive and negative signals present. A human should
    // read this — "yes but not now" is positive in intent and negative in
    // timing, and the agent cannot tell which side wins.
    if !positive.is_empty() && !declined.is_empty() {
        return ReplyClassification::NeedsHuman {
            reason: HumanReviewReason::AmbiguousText,
            confidence: Confidence::saturating_from_basis_points(5_000),
        };
    }

    if !positive.is_empty() {
        return ReplyClassification::Auto {
            disposition: OutreachReplyDisposition::Positive,
            confidence: Confidence::saturating_from_basis_points(9_800),
            matched_rules: positive,
        };
    }

    if !declined.is_empty() {
        return ReplyClassification::Auto {
            disposition: OutreachReplyDisposition::Declined,
            confidence: Confidence::saturating_from_basis_points(9_800),
            matched_rules: declined,
        };
    }

    ReplyClassification::NeedsHuman {
        reason: HumanReviewReason::UnmatchedText,
        confidence: Confidence::saturating_from_basis_points(5_000),
    }
}

/// Heuristic language detection: Polish and English share the Latin alphabet,
/// so the check is for Polish-specific characters and common words. A text
/// with no Polish diacritics and no recognisable English or Polish words is
/// treated as unsupported.
fn is_polish_or_english(lower: &str) -> bool {
    // Polish diacritics are a strong signal.
    if lower
        .chars()
        .any(|c| matches!(c, 'ą' | 'ć' | 'ę' | 'ł' | 'ń' | 'ó' | 'ś' | 'ź' | 'ż'))
    {
        return true;
    }
    // Common English/Polish function words. If none appear, the text is
    // likely not in a supported language.
    let common_words = [
        "the ",
        "yes",
        "no ",
        "not ",
        "and ",
        "for ",
        "but ",
        "thanks",
        "thank",
        "tak",
        "nie ",
        "nie,",
        "dzięk",
        "proszę",
        "oczywi",
        "interes",
        "hello",
        "hi ",
        "cześć",
        "witaj",
        "please",
        "me ",
        "from",
        "list",
        "stop",
        "remove",
        "unsubscribe",
        "wypisz",
        "kontakt",
    ];
    common_words.iter().any(|word| lower.contains(word))
}

fn positive_matches(lower: &str) -> Vec<&'static str> {
    let rules: &[(&str, &str)] = &[
        ("yes", "positive:yes"),
        ("tak", "positive:tak"),
        ("sure", "positive:sure"),
        ("of course", "positive:of_course"),
        ("oczywiście", "positive:oczywiscie"),
        // "interested" alone is positive, but "not interested" is negative.
        // Check the negative form first in declined_matches; if it matched,
        // we skip the bare positive here. The caller handles ambiguity.
        ("interested", "positive:interested"),
        ("zainteresowany", "positive:zainteresowany"),
        ("zainteresowana", "positive:zainteresowana"),
        ("let's do it", "positive:lets_do_it"),
        ("zróbmy to", "positive:zrobmy_to"),
        ("sounds good", "positive:sounds_good"),
        ("brzmi dobrze", "positive:brzmi_dobrze"),
        ("great", "positive:great"),
        ("świetnie", "positive:swietnie"),
        ("love it", "positive:love_it"),
        ("super", "positive:super"),
        ("chętnie", "positive:chelnie"),
        ("jak najbardziej", "positive:jak_najbardziej"),
        ("with pleasure", "positive:with_pleasure"),
        ("z przyjemnością", "positive:z_przyjemnoscia"),
    ];
    rules
        .iter()
        .filter(|(keyword, _)| lower.contains(keyword))
        .map(|(_, rule)| *rule)
        .collect()
}

fn declined_matches(lower: &str) -> Vec<&'static str> {
    let rules: &[(&str, &str)] = &[
        ("no thanks", "declined:no_thanks"),
        ("nie dziękuję", "declined:nie_dziekuje"),
        ("nie, dziękuję", "declined:nie_dziekuje"),
        ("nie dziękuje", "declined:nie_dziekuje"),
        ("not interested", "declined:not_interested"),
        ("nie zainteresowany", "declined:nie_zainteresowany"),
        ("nie zainteresowana", "declined:nie_zainteresowana"),
        // "pass" alone is ambiguous — "password", "compass", "passage".
        // Match "pass" only as a standalone reply, not as a substring.
        ("i'll pass", "declined:pass"),
        ("i will pass", "declined:pass"),
        ("pass on this", "declined:pass"),
        ("pass on it", "declined:pass"),
        ("maybe later", "declined:maybe_later"),
        ("może później", "declined:moze_pozniej"),
        ("może następnym", "declined:moze_nastepnym"),
        ("not now", "declined:not_now"),
        ("nie teraz", "declined:nie_teraz"),
        ("nope", "declined:nope"),
        ("unfortunately not", "declined:unfortunately_not"),
        ("niestety nie", "declined:niestety_nie"),
        ("can't", "declined:cant"),
        ("nie mogę", "declined:nie_moge"),
        ("nie możemy", "declined:nie_mozemy"),
        ("regretfully", "declined:regretfully"),
    ];
    rules
        .iter()
        .filter(|(keyword, _)| lower.contains(keyword))
        .map(|(_, rule)| *rule)
        .collect()
}

fn dnc_matches(lower: &str) -> Vec<&'static str> {
    let rules: &[(&str, &str)] = &[
        ("unsubscribe", "dnc:unsubscribe"),
        // "stop" alone is ambiguous — "bus stop", "don't stop", "stopwatch".
        // Removed the bare "stop" rule; "stop emailing" and "stop contacting"
        // are specific enough. A bare "stop" routes to NeedsHuman via
        // UnmatchedText, which is the safe fallback.
        ("don't contact", "dnc:dont_contact"),
        ("do not contact", "dnc:do_not_contact"),
        ("nie kontaktuj", "dnc:nie_kontaktuj"),
        ("remove me", "dnc:remove_me"),
        ("usuń mnie", "dnc:usun_mnie"),
        ("stop emailing", "dnc:stop_emailing"),
        ("przestań pisać", "dnc:przestan_pisac"),
        ("nie pisz", "dnc:nie_pisz"),
        ("take me off", "dnc:take_me_off"),
        ("wypisz mnie", "dnc:wypisz_mnie"),
    ];
    rules
        .iter()
        .filter(|(keyword, _)| lower.contains(keyword))
        .map(|(_, rule)| *rule)
        .collect()
}

/// What the reader found in a negotiation reply, when the text carried
/// exactly one confident money figure.
///
/// This is a proposal, never a write: the columns it fills tell the human
/// what the machine read, and the human still certifies the number through
/// the terms route. The extractor is deliberately blind to anything short
/// of a figure glued to a currency — two different amounts, a range, or a
/// bare number all yield `None`, because a guessed fee is worse than none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProposedTerms {
    /// The figure in the currency's minor unit.
    pub fee_minor: i64,
    /// ISO-4217 code the figure was quoted in.
    pub currency: &'static str,
}

/// Currency markers the reader recognises, longest spellings first so
/// `złoty` never collides with `zł`. `comma_decimal` is the convention the
/// currency is quoted in: continental currencies read `400,50` as four
/// hundred and a half, Anglo ones read it as forty thousand and fifty.
const CURRENCY_MARKERS: &[(&[&str], &str, bool)] = &[
    (&["euros", "euro", "eur", "€"], "EUR", true),
    (&["złotych", "złote", "złoty", "pln", "zł"], "PLN", true),
    (&["dollars", "dollar", "usd", "$"], "USD", false),
    (&["pounds", "pound", "gbp", "£"], "GBP", false),
];

/// Reads the one money figure a reply text carries, if exactly one exists.
///
/// A figure only counts when a currency marker touches it — "capacity 400"
/// is not an offer, "€400" is. Two different figures, or a figure the
/// currency's own convention cannot parse, yield `None` and the reply stays
/// entirely the human's.
#[must_use]
pub fn extract_offer_terms(text: &str) -> Option<ProposedTerms> {
    // Case-fold once and scan the folded text: everything the reader needs —
    // digits, separators, markers — is identical in both, and folding can
    // change byte lengths, so positions must never index the original.
    let lowered = text.to_lowercase();
    let mut found: Option<ProposedTerms> = None;
    for (markers, currency, comma_decimal) in CURRENCY_MARKERS {
        for marker in *markers {
            let mut search_from = 0;
            while let Some(at) = lowered
                .get(search_from..)
                .and_then(|tail| tail.find(marker))
            {
                let marker_at = search_from + at;
                search_from = marker_at + marker.len();
                // An alphabetic marker must sit on a word boundary — "eur"
                // inside "amateur" is not a currency.
                if marker.starts_with(|c: char| c.is_alphanumeric()) {
                    let before = lowered
                        .get(..marker_at)
                        .and_then(|head| head.chars().next_back());
                    let after = lowered
                        .get(marker_at + marker.len()..)
                        .and_then(|tail| tail.chars().next());
                    if before.is_some_and(|c| c.is_alphanumeric())
                        || after.is_some_and(|c| c.is_alphanumeric())
                    {
                        continue;
                    }
                }
                for span in [
                    number_before(&lowered, marker_at),
                    number_after(&lowered, marker_at + marker.len()),
                ]
                .into_iter()
                .flatten()
                {
                    let Some(minor) = lowered
                        .get(span.0..span.1)
                        .and_then(|raw| parse_amount(raw, *comma_decimal))
                    else {
                        continue;
                    };
                    // A figure glued to another figure through a range sign
                    // is one end of an offer, not the offer.
                    if touches_range(&lowered, span.0, span.1) {
                        continue;
                    }
                    let candidate = ProposedTerms {
                        fee_minor: minor,
                        currency,
                    };
                    match found {
                        None => found = Some(candidate),
                        Some(existing) if existing == candidate => {}
                        Some(_) => return None,
                    }
                }
            }
        }
    }
    found
}

/// The number token ending just before `pos`, skipping whitespace. A space
/// inside the token continues it only when another digit follows — "1 200"
/// is one figure, "sold 400 tickets" is not.
fn number_before(text: &str, pos: usize) -> Option<(usize, usize)> {
    let head = text.get(..pos)?.trim_end();
    let mut start = head.len();
    let mut saw_digit = false;
    while start > 0 {
        let c = head.get(..start)?.chars().next_back()?;
        if c.is_ascii_digit() {
            saw_digit = true;
            start -= 1;
        } else if saw_digit && matches!(c, '.' | ',' | '\'' | '\u{00A0}' | '\u{202F}') {
            start -= c.len_utf8();
        } else if saw_digit && c == ' ' {
            let before_space = head.get(..start).and_then(|h| h.chars().next_back());
            if before_space.is_some_and(|b| b.is_ascii_digit()) {
                // Peek further back: the space continues the figure only if
                // the digit before it begins a 3-digit group.
                start -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    saw_digit.then_some((start, head.len()))
}

/// The number token beginning just after `pos`, same rule mirrored.
fn number_after(text: &str, pos: usize) -> Option<(usize, usize)> {
    let tail = text.get(pos..)?;
    let leading_ws = tail.len() - tail.trim_start().len();
    let body = tail.get(leading_ws..)?;
    let mut end = 0;
    let mut chars = body.char_indices().peekable();
    let mut saw_digit = false;
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_digit() {
            saw_digit = true;
            end = i + 1;
        } else if saw_digit && matches!(c, '.' | ',' | '\'' | '\u{00A0}' | '\u{202F}') {
            end = i + c.len_utf8();
        } else if saw_digit && c == ' ' {
            if chars.peek().is_some_and(|(_, n)| n.is_ascii_digit()) {
                end = i + 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    // Trailing separators are not part of the figure.
    let raw = body.get(..end)?;
    let trimmed = raw.trim_end_matches(['.', ',', '\'', ' ', '\u{00A0}', '\u{202F}']);
    (!trimmed.is_empty()).then_some((pos + leading_ws, pos + leading_ws + trimmed.len()))
}

/// True when the figure at `span` is welded to a neighbour by a range sign —
/// "400-500" and "from 400 to 500" are a range, not a proposal.
fn touches_range(text: &str, start: usize, end: usize) -> bool {
    const RANGE_SIGNS: &[char] = &[
        '-', '\u{2013}', '\u{2014}', '\u{2212}', '\u{2011}', '/', '~',
    ];
    let before = text.get(..start).map(str::trim_end).unwrap_or_default();
    if before
        .chars()
        .next_back()
        .is_some_and(|prev| RANGE_SIGNS.contains(&prev))
    {
        return true;
    }
    // A range word immediately before the figure makes it the far end of
    // one — "from 400 to 500", "either 400 or 500", "400 and 500". "up to
    // €400" is refused too: a ceiling is not a committed offer.
    let word_before = before
        .rsplit(|c: char| !c.is_alphabetic())
        .find(|word| !word.is_empty())
        .unwrap_or_default();
    if matches!(word_before, "to" | "or" | "and" | "from" | "between") {
        return true;
    }
    let after = text.get(end..).map(str::trim_start).unwrap_or_default();
    if let Some(next) = after.chars().next() {
        if RANGE_SIGNS.contains(&next)
            && after
                .get(next.len_utf8()..)
                .map(str::trim_start)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        {
            return true;
        }
        let first_word: String = after.chars().take_while(|c| c.is_alphabetic()).collect();
        // A currency marker may sit between the range word and the figure
        // ("or €450"), so the question is whether the next alphanumeric
        // character is a digit, not whether a digit is literally next.
        if matches!(first_word.as_str(), "to" | "or" | "and")
            && after
                .get(first_word.len()..)
                .and_then(|rest| rest.chars().find(|c| c.is_alphanumeric()))
                .is_some_and(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Parses a figure under the currency's own decimal convention. Returns
/// `None` for anything the convention cannot read unambiguously — a second
/// decimal separator, a broken group, an empty figure.
fn parse_amount(raw: &str, comma_decimal: bool) -> Option<i64> {
    let decimal_sep = if comma_decimal { ',' } else { '.' };
    let thousands_sep = if comma_decimal { '.' } else { ',' };
    let (int_raw, frac_minor) = match raw.rfind(decimal_sep) {
        Some(at) => {
            let tail = raw.get(at + 1..).unwrap_or_default();
            // One or two digits after the separator read as the fraction;
            // three read as grouping ("1,200" under a comma convention is
            // ambiguous, so it is refused rather than guessed).
            if !tail.is_empty() && tail.len() <= 2 && tail.bytes().all(|b| b.is_ascii_digit()) {
                let mut frac = tail.parse::<i64>().ok()?;
                if tail.len() == 1 {
                    frac *= 10;
                }
                (raw.get(..at).unwrap_or_default(), frac)
            } else {
                (raw, 0)
            }
        }
        None => (raw, 0),
    };
    // A decimal separator left in the integer part means two of them —
    // not a figure either convention will read.
    if int_raw.contains(decimal_sep) {
        return None;
    }
    let mut digits = String::with_capacity(int_raw.len());
    let mut groups = int_raw
        .split([thousands_sep, '\'', ' ', '\u{00A0}', '\u{202F}'])
        .peekable();
    let head = groups.next()?;
    // The ≤3-digit head rule only binds a grouped figure: "1.200" groups
    // as twelve hundred, but ungrouped "1200" is twelve hundred too.
    if head.is_empty()
        || (head.len() > 3 && groups.peek().is_some())
        || !head.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.push_str(head);
    for group in groups {
        if group.len() != 3 || !group.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.push_str(group);
    }
    let int: i64 = digits.parse().ok()?;
    let minor = int.checked_mul(100)?.checked_add(frac_minor)?;
    (minor > 0).then_some(minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> ReplyClassificationInput<'_> {
        ReplyClassificationInput {
            reply_text: text,
            target_kind: OutreachTargetKind::Playlist,
            previous_disposition: None,
        }
    }

    fn input_with_previous(
        text: &str,
        prev: OutreachReplyDisposition,
    ) -> ReplyClassificationInput<'_> {
        ReplyClassificationInput {
            reply_text: text,
            target_kind: OutreachTargetKind::Playlist,
            previous_disposition: Some(prev),
        }
    }

    #[test]
    fn classifies_positive_english() {
        let result = classify_reply(&input("Yes, sure! Sounds great."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::Positive,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"positive:yes"));
                assert!(matched_rules.contains(&"positive:sure"));
            }
            other => panic!("expected Auto Positive, got {other:?}"),
        }
    }

    #[test]
    fn classifies_positive_polish() {
        let result = classify_reply(&input("Tak, oczywiście! Chętnie."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::Positive,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"positive:tak"));
                assert!(matched_rules.contains(&"positive:oczywiscie"));
                assert!(matched_rules.contains(&"positive:chelnie"));
            }
            other => panic!("expected Auto Positive, got {other:?}"),
        }
    }

    #[test]
    fn classifies_declined_english() {
        let result = classify_reply(&input("No thanks, not interested."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::Declined,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"declined:no_thanks"));
                assert!(matched_rules.contains(&"declined:not_interested"));
            }
            other => panic!("expected Auto Declined, got {other:?}"),
        }
    }

    #[test]
    fn classifies_declined_polish() {
        let result = classify_reply(&input("Nie dziękuję, nie zainteresowany."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::Declined,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"declined:nie_dziekuje"));
                assert!(matched_rules.contains(&"declined:nie_zainteresowany"));
            }
            other => panic!("expected Auto Declined, got {other:?}"),
        }
    }

    #[test]
    fn classifies_do_not_contact_english() {
        let result = classify_reply(&input("Please unsubscribe me from your list."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::DoNotContact,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"dnc:unsubscribe"));
            }
            other => panic!("expected Auto DNC, got {other:?}"),
        }
    }

    #[test]
    fn classifies_do_not_contact_polish() {
        let result = classify_reply(&input("Proszę wypisz mnie z listy."));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::DoNotContact,
                confidence,
                matched_rules,
            } => {
                assert_eq!(confidence, Confidence::saturating_from_basis_points(9_800));
                assert!(matched_rules.contains(&"dnc:wypisz_mnie"));
            }
            other => panic!("expected Auto DNC, got {other:?}"),
        }
    }

    #[test]
    fn dnc_overrides_positive() {
        // "yes" and "stop" both present — stop wins.
        let result = classify_reply(&input("Yes I read it but please stop emailing me."));
        match result {
            ReplyClassification::Auto { disposition, .. } => {
                assert_eq!(disposition, OutreachReplyDisposition::DoNotContact)
            }
            other => panic!("expected Auto DNC, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_text_needs_human() {
        let result = classify_reply(&input("Yes sure but no thanks not interested."));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::AmbiguousText,
                ..
            } => {}
            other => panic!("expected NeedsHuman AmbiguousText, got {other:?}"),
        }
    }

    #[test]
    fn too_short_needs_human() {
        let result = classify_reply(&input("ok"));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::TooShort,
                ..
            } => {}
            other => panic!("expected NeedsHuman TooShort, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_needs_human() {
        let result = classify_reply(&input(""));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::TooShort,
                ..
            } => {}
            other => panic!("expected NeedsHuman TooShort, got {other:?}"),
        }
    }

    #[test]
    fn previous_dnc_needs_human() {
        let result = classify_reply(&input_with_previous(
            "Yes I changed my mind, interested!",
            OutreachReplyDisposition::DoNotContact,
        ));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::PreviousDoNotContact,
                ..
            } => {}
            other => panic!("expected NeedsHuman PreviousDoNotContact, got {other:?}"),
        }
    }

    #[test]
    fn unmatched_text_needs_human() {
        let result = classify_reply(&input("Thanks for reaching out, I will check the track."));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::UnmatchedText,
                ..
            } => {}
            other => panic!("expected NeedsHuman UnmatchedText, got {other:?}"),
        }
    }

    #[test]
    fn polish_diacritics_detected() {
        // Polish text with diacritics but no matching keywords — should
        // be UnmatchedText, not NotInSupportedLanguage.
        let result = classify_reply(&input("Dziękuję za przesłanie materiału."));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::UnmatchedText,
                ..
            } => {}
            other => panic!("expected NeedsHuman UnmatchedText, got {other:?}"),
        }
    }

    #[test]
    fn non_supported_language_needs_human() {
        // German text — no Polish diacritics, no English/Polish common words.
        let result = classify_reply(&input("Vielen Dank für Ihre Nachricht."));
        match result {
            ReplyClassification::NeedsHuman {
                reason: HumanReviewReason::NotInSupportedLanguage,
                ..
            } => {}
            other => panic!("expected NeedsHuman NotInSupportedLanguage, got {other:?}"),
        }
    }

    #[test]
    fn previous_positive_can_be_reclassified() {
        // A previous positive does not block reclassification.
        let result = classify_reply(&input_with_previous(
            "No thanks, not interested anymore.",
            OutreachReplyDisposition::Positive,
        ));
        match result {
            ReplyClassification::Auto {
                disposition: OutreachReplyDisposition::Declined,
                ..
            } => {}
            other => panic!("expected Auto Declined, got {other:?}"),
        }
    }

    #[test]
    fn a_figure_with_a_currency_is_the_offer() {
        assert_eq!(
            extract_offer_terms("We can offer €400 for the night."),
            Some(ProposedTerms {
                fee_minor: 40_000,
                currency: "EUR",
            })
        );
        assert_eq!(
            extract_offer_terms("Możemy zaproponować 400 zł za występ."),
            Some(ProposedTerms {
                fee_minor: 40_000,
                currency: "PLN",
            })
        );
        assert_eq!(
            extract_offer_terms("Our best is 1,200 USD all-in."),
            Some(ProposedTerms {
                fee_minor: 120_000,
                currency: "USD",
            })
        );
        assert_eq!(
            extract_offer_terms("Budget is 1.200,50 EUR this time."),
            Some(ProposedTerms {
                fee_minor: 120_050,
                currency: "EUR",
            })
        );
        assert_eq!(
            extract_offer_terms("How about £350 plus the door?"),
            Some(ProposedTerms {
                fee_minor: 35_000,
                currency: "GBP",
            })
        );
    }

    #[test]
    fn a_repeated_figure_is_still_one_offer() {
        // The same number quoted twice is one proposal, not two.
        assert_eq!(
            extract_offer_terms("We could do €400 — yes, 400 euros total."),
            Some(ProposedTerms {
                fee_minor: 40_000,
                currency: "EUR",
            })
        );
    }

    #[test]
    fn two_figures_or_a_range_propose_nothing() {
        assert_eq!(
            extract_offer_terms("Either €400 or €450 depending on the date."),
            None
        );
        assert_eq!(extract_offer_terms("Somewhere between €400-500."), None);
        assert_eq!(extract_offer_terms("Somewhere between 400-500 EUR."), None);
        // No currency marker: a bare number is never a proposal.
        assert_eq!(extract_offer_terms("The room holds 400 people."), None);
        assert_eq!(extract_offer_terms("Sure, see you then."), None);
        // A marker without a figure proposes nothing either.
        assert_eq!(extract_offer_terms("Payment in EUR is fine."), None);
        // A figure the convention cannot read is refused, not guessed.
        assert_eq!(extract_offer_terms("Could do 1,2,3 EUR maybe."), None);
    }

    #[test]
    fn ungrouped_figures_read_but_word_ranges_and_zero_do_not() {
        // The common magnitudes, written without separators.
        assert_eq!(
            extract_offer_terms("We can offer 1200 EUR for the night."),
            Some(ProposedTerms {
                fee_minor: 120_000,
                currency: "EUR"
            })
        );
        assert_eq!(
            extract_offer_terms("1500 zł i dobrze."),
            Some(ProposedTerms {
                fee_minor: 150_000,
                currency: "PLN"
            })
        );
        assert_eq!(
            extract_offer_terms("$5000 is our ceiling."),
            Some(ProposedTerms {
                fee_minor: 500_000,
                currency: "USD"
            })
        );
        // Zero is not an offer — and would violate the proposed-fee CHECK.
        assert_eq!(extract_offer_terms("We can do €0 this time."), None);
        assert_eq!(extract_offer_terms("0 zł, niestety."), None);
        // Word ranges propose nothing: the marked figure is one end.
        assert_eq!(extract_offer_terms("From 400 to 500 EUR."), None);
        assert_eq!(extract_offer_terms("Either 400 or 500 EUR."), None);
        assert_eq!(extract_offer_terms("€400—500 depending."), None);
        // A ceiling is not a committed figure either.
        assert_eq!(extract_offer_terms("Up to €400, no more."), None);
    }
}
