//! Split out of `content_supply.rs` for the source-size ratchet — the
//! relay pacing unit tests, unchanged. `use super::*` resolves to the
//! parent module, so every name these tests touch stays in scope.

use super::*;

fn at(hours_ago: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid") - Duration::hours(hours_ago)
}

fn push(hours_ago: i64, title: &str, body: &str) -> RecentRelayPush {
    RecentRelayPush {
        at: at(hours_ago),
        title: title.to_owned(),
        body: body.to_owned(),
    }
}

#[test]
fn the_same_words_cross_posted_are_not_pushed_twice() {
    let title = "Terapia grupowa, spowiedź szaleńca, mental metal.";
    let recent = [push(
        30,
        title,
        "Terapia grupowa. Łapcie mordeczki\n\nhttps://www.facebook.com/1069/posts/1",
    )];
    assert_eq!(
        relay_push_verdict(
            title,
            "Terapia grupowa. Łapcie mordeczki\n\nhttps://www.instagram.com/p/Dd1/",
            &recent,
            at(0)
        ),
        RelayPushVerdict::AlreadyRelayed
    );
}

#[test]
fn a_second_post_waits_out_the_gap() {
    let recent = [push(2, "Gramy w Gorzowie", "17.10")];
    assert_eq!(
        relay_push_verdict("Nowy klip", "Już jest", &recent, at(0)),
        RelayPushVerdict::TooSoon
    );
    let older = [push(RELAY_PUSH_MIN_GAP_HOURS, "Gramy w Gorzowie", "17.10")];
    assert_eq!(
        relay_push_verdict("Nowy klip", "Już jest", &older, at(0)),
        RelayPushVerdict::Send
    );
}

#[test]
fn words_older_than_the_memory_may_be_relayed_again() {
    let recent = [push(RELAY_PUSH_DEDUPE_DAYS * 24 + 1, "Gramy", "17.10")];
    assert_eq!(
        relay_push_verdict("Gramy", "17.10", &recent, at(0)),
        RelayPushVerdict::Send
    );
}
