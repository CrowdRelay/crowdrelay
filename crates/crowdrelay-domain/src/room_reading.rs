//! What the band has read in a room before it speaks in it.
//!
//! The rule this serves is the same one `contact_research` serves for people:
//! nobody is addressed as a stranger. A community is a room full of people with
//! its own current conversation. Before the band posts there, it has looked at
//! what is being discussed *now*, and the post says which of those threads it
//! fits. A post written from the community's description and its rules — which
//! is all the drafter had until now — is a post written about a room nobody in
//! the band has been into.
//!
//! The reading is not produced by a model. The community sweep already fetches
//! the room's current threads on every pass (`fan_observations`, kind `post`,
//! with the thread's own date and permalink). This module only decides what
//! counts as having read enough, and what citing a thread means: the URL a
//! draft names must be one of the threads the sweep recorded for that room.
//!
//! What it does not do: it does not judge whether the post *does* fit the
//! thread it names. A human approves every community post and sees the thread
//! next to the draft; this gate establishes that the band looked, not that the
//! band understood.
//!
//! Only rooms the sweep can read have a reading. Today that is Reddit. Every
//! other platform has no reader, so the gate holds them: "not read" is the
//! honest state of a room nobody has looked into, and it is fail-closed.

use time::{Date, Duration};

// A date on the wire is `YYYY-MM-DD`, the form Postgres and the prompt use. The
// `time` default is a (year, ordinal) pair, which no SQL expression produces.
time::serde::format_description!(day, Date, "[year]-[month]-[day]");

/// A thread older than this is not "what the room is talking about".
pub const READ_MAX_AGE_DAYS: i64 = 14;

/// Fewer threads than this is not a reading: two titles cannot tell a lively
/// room's topics from a quiet one's last hour.
pub const MIN_THREADS: usize = 3;

/// The most threads shown to the drafter. Enough to see the shape of the
/// room, few enough to stay a paragraph.
pub const PROMPT_THREADS: usize = 6;

/// One thread the sweep read in a room.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RoomThread {
    pub title: String,
    pub url: String,
    #[serde(with = "day")]
    pub posted_on: Date,
}

/// The oldest thread date that still counts, for a given day.
#[must_use]
pub fn oldest_counted(today: Date) -> Date {
    today - Duration::days(READ_MAX_AGE_DAYS)
}

/// The threads that still count on `today`: recorded within the window and not
/// dated in the future. The loaders already bound the query to the same window;
/// the evaluator applies it again because it owns the clock.
#[must_use]
pub fn counted(threads: &[RoomThread], today: Date) -> Vec<RoomThread> {
    let oldest = oldest_counted(today);
    threads
        .iter()
        .filter(|thread| thread.posted_on >= oldest && thread.posted_on <= today)
        .cloned()
        .collect()
}

/// Whether the threads amount to having read the room.
#[must_use]
pub fn is_read(threads: &[RoomThread]) -> bool {
    threads.len() >= MIN_THREADS
}

fn comparable(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .replace("://old.reddit.com", "://www.reddit.com")
        .replace("://reddit.com", "://www.reddit.com")
}

/// Whether `url` names one of the threads that were read.
#[must_use]
pub fn cites_a_thread(url: &str, threads: &[RoomThread]) -> bool {
    let wanted = comparable(url);
    !wanted.is_empty()
        && threads
            .iter()
            .any(|thread| comparable(&thread.url) == wanted)
}

/// The paragraph that tells the drafter what the room is discussing, and what
/// it must do with that.
///
/// The URL is printed beside each title because the draft must hand one back
/// (`fits_thread_url`) and the worker checks it against the same list.
#[must_use]
pub fn room_paragraph(threads: &[RoomThread]) -> String {
    let mut text = String::from(
        "\n\nWHAT THIS ROOM IS DISCUSSING NOW (read by the system, newest first, with the date each thread was posted):",
    );
    for thread in threads.iter().take(PROMPT_THREADS) {
        text.push_str(&format!(
            "\n- {} — {} ({})",
            thread.title.trim(),
            thread.posted_on,
            thread.url
        ));
    }
    text.push_str(
        "\nThe post must fit this room as it is today. Put the URL of ONE thread above that your post sits next to \
         in the item's \"fits_thread_url\" field, copied exactly. If nothing above is near what the post is about, \
         emit no post for this community and say so in the rationale. Never quote or imitate a thread's author.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn thread(n: u8) -> RoomThread {
        RoomThread {
            title: format!("thread {n}"),
            url: format!("https://www.reddit.com/comments/abc{n}"),
            posted_on: date!(2026 - 10 - 01),
        }
    }

    #[test]
    fn two_threads_are_not_a_reading() {
        assert!(!is_read(&[thread(1), thread(2)]));
        assert!(is_read(&[thread(1), thread(2), thread(3)]));
        assert!(!is_read(&[]));
    }

    #[test]
    fn only_a_thread_that_was_read_can_be_cited() {
        let read = [thread(1), thread(2), thread(3)];
        assert!(cites_a_thread(
            "https://www.reddit.com/comments/abc2",
            &read
        ));
        // The usual spellings of the same permalink are the same thread.
        assert!(cites_a_thread(
            "https://old.reddit.com/comments/ABC2/",
            &read
        ));
        assert!(cites_a_thread(
            "  https://reddit.com/comments/abc2  ",
            &read
        ));
        // A thread the system never showed is not citable, nor is nothing.
        assert!(!cites_a_thread(
            "https://www.reddit.com/comments/zzz9",
            &read
        ));
        assert!(!cites_a_thread("", &read));
        assert!(!cites_a_thread("   ", &read));
    }

    #[test]
    fn a_thread_round_trips_as_the_json_postgres_builds() {
        let from_sql = serde_json::json!({
            "title": "thread 1",
            "url": "https://www.reddit.com/comments/abc1",
            "posted_on": "2026-10-01",
        });
        let thread: RoomThread = serde_json::from_value(from_sql.clone()).expect("parses");
        assert_eq!(thread, self::tests::thread(1));
        assert_eq!(serde_json::to_value(&thread).expect("serializes"), from_sql);
    }

    #[test]
    fn only_threads_inside_the_window_count() {
        let today = date!(2026 - 10 - 15);
        let mut old = thread(1);
        old.posted_on = date!(2026 - 09 - 30);
        let mut future = thread(2);
        future.posted_on = date!(2026 - 10 - 16);
        let mut edge = thread(3);
        edge.posted_on = date!(2026 - 10 - 01);
        let kept = counted(&[old, future, edge.clone(), thread(4)], today);
        assert_eq!(kept, vec![edge, thread(4)]);
    }

    #[test]
    fn the_window_is_two_weeks() {
        assert_eq!(oldest_counted(date!(2026 - 10 - 15)), date!(2026 - 10 - 01));
    }

    #[test]
    fn the_paragraph_shows_every_url_the_draft_may_cite() {
        let read: Vec<_> = (1..=8).map(thread).collect();
        let text = room_paragraph(&read);
        for shown in &read[..PROMPT_THREADS] {
            assert!(text.contains(&shown.url), "{text}");
        }
        // Only what is shown can be cited by the prompt's own instruction.
        assert!(!text.contains(&read[PROMPT_THREADS].url));
        assert!(text.contains("fits_thread_url"));
    }
}
