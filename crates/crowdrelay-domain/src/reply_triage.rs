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
}
