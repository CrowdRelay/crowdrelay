//! Relay freshness: when a synced post's words have stopped being true.
//!
//! Split from `content_supply`, which holds the pacing verdict itself; the
//! title-dedupe tests below exercise that verdict with the cross-post that
//! reached fans twice on 2026-09-26.

use time::{Duration, OffsetDateTime};

/// How long a post that says "today" or "tonight" stays true.
///
/// On 2026-09-26 fans were pushed "Tymczasem dzisiaj wieczorne słuchowisko"
/// — "tonight's listening session" — from a post made two days earlier. A
/// relay can be raised up to 72 hours after the post, and a held one later
/// still; a sentence about today is only news on the day it was written.
pub const RELATIVE_DAY_FRESH_HOURS: i64 = 12;

const RELATIVE_DAY_WORDS: &[&str] = &[
    "dzisiaj",
    "dziś",
    "dzis",
    "dzisiejszy",
    "dzisiejsza",
    "dzisiejsze",
    "wieczorem",
    "jutro",
    "today",
    "tonight",
    "tomorrow",
];

/// Whether a post's words tie it to the day it was written, and that day has
/// passed: `text` names today, tonight or tomorrow and was posted more than
/// [`RELATIVE_DAY_FRESH_HOURS`] ago.
#[must_use]
pub fn relative_day_has_passed(
    text: &str,
    occurred_at: OffsetDateTime,
    now: OffsetDateTime,
) -> bool {
    if now - occurred_at <= Duration::hours(RELATIVE_DAY_FRESH_HOURS) {
        return false;
    }
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| RELATIVE_DAY_WORDS.contains(&word))
}

#[cfg(test)]
mod relative_day_tests {
    use super::*;
    use time::Duration;

    #[test]
    fn tonight_two_days_later_has_passed() {
        let posted = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let text = "Tymczasem dzisiaj wieczorne słuchowisko ;)";
        assert!(relative_day_has_passed(
            text,
            posted,
            posted + Duration::days(2)
        ));
        assert!(!relative_day_has_passed(
            text,
            posted,
            posted + Duration::hours(3)
        ));
        assert!(!relative_day_has_passed(
            "Nowy klip już jest",
            posted,
            posted + Duration::days(2)
        ));
    }
}

#[cfg(test)]
mod relay_title_dedupe_tests {
    use crate::content_supply::{RecentRelayPush, RelayPushVerdict, relay_push_verdict};
    use time::{Duration, OffsetDateTime};

    /// The pair that reached fans twice on 2026-09-26: one reel, posted to
    /// Instagram with a thanks line and to Facebook without it.
    #[test]
    fn a_cross_post_with_a_different_body_is_already_relayed() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let title = "Terapia grupowa, spowiedź szaleńca, mental metal.";
        let recent = [RecentRelayPush {
            at: now - Duration::days(1),
            title: title.to_owned(),
            body: "Terapia grupowa, spowiedź szaleńca, mental metal. Łapcie mordeczki i do zobaczenia na terapii. Dzięki bardzo za te ujęcia\n\nhttps://www.instagram.com/reel/DdrZDybITVB/".to_owned(),
        }];
        assert_eq!(
            relay_push_verdict(
                title,
                "Terapia grupowa, spowiedź szaleńca, mental metal. Łapcie mordeczki i do zobaczenia na terapii.\n\nhttps://www.facebook.com/reel/4645372902451686/",
                &recent,
                now,
            ),
            RelayPushVerdict::AlreadyRelayed
        );
    }

    /// A short, generic title is not an identity: two different posts both
    /// titled "Gramy" are two posts.
    #[test]
    fn a_short_title_alone_is_not_a_cross_post() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let recent = [RecentRelayPush {
            at: now - Duration::days(1),
            title: "Gramy".to_owned(),
            body: "Wrocław 11.09".to_owned(),
        }];
        assert_eq!(
            relay_push_verdict("Gramy", "Gorzów 17.10", &recent, now),
            RelayPushVerdict::Send
        );
    }
}
