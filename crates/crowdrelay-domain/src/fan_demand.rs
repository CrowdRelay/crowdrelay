//! Demand: a stranger, in a room the band already reads, asking for exactly
//! what the band makes.
//!
//! Everything else the growth machinery does with a community is *supply*
//! side: it drafts a post about the band's video and puts it in a room. A post
//! like that is an interruption, and the rooms say so — moderators removed
//! enough of them that Reddit posting is halted. Demand is the opposite: the
//! thread "bands like Gojira?" or "which metal song of 2026 is your favourite"
//! is a person who has asked, today, to be told. An honest, specific answer in
//! that thread is the only kind of mention a community welcomes, and it is
//! the highest-intent exposure a stranger can get.
//!
//! Until this module nothing in the brain looked for it. `room_reading`
//! records what each room is discussing (the community sweep stores every
//! thread title with its date, score and comment count); the drafter was
//! shown those titles only as context for a new post. This module reads the
//! same facts for one purpose: which of them are asking.
//!
//! The classifier is deliberately plain string rules, not a model. It decides
//! *whether a thread is a request*, which is a property of the title; whether
//! the band's music fits what is being asked is a separate judgement that a
//! person (or a later, grounded drafter) makes with the thread open. A rule
//! that is wrong is wrong visibly: the rule's name is returned beside every
//! signal so an operator can see why a thread was surfaced.
//!
//! What this does not do: it does not draft, post or reply. It ranks.

use serde::Serialize;
use time::{Date, OffsetDateTime};

/// What the thread is asking for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandKind {
    /// "Recommend me / bands like X / anything similar" — a direct request for
    /// new music.
    Recommendation,
    /// "Favourite song of 2026 / what are you listening to" — an open invitation
    /// to name music, where naming the band is the thread's whole point.
    OpenFloor,
}

impl DemandKind {
    /// Intent in basis points: how likely a reply naming new music is the
    /// thing the poster wanted. A request beats an invitation; neither is
    /// certain.
    #[must_use]
    pub const fn intent_basis_points(self) -> u16 {
        match self {
            Self::Recommendation => 9_000,
            Self::OpenFloor => 6_000,
        }
    }
}

/// One thread that reads as a request, and why.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DemandSignal {
    pub kind: DemandKind,
    /// The name of the rule that fired — shown to the operator verbatim.
    pub rule: &'static str,
}

/// A thread as the community sweep recorded it.
#[derive(Clone, Copy, Debug)]
pub struct ThreadFacts<'a> {
    pub title: &'a str,
    pub flair: Option<&'a str>,
    pub comments: Option<u32>,
    /// The thread's own date, as the sweep records it — a day, not an instant.
    pub posted_on: Date,
}

/// Threads older than this many days are no longer where the room is looking.
/// The sweep records a thread's date, not its hour, so the unit is the day.
pub const MAX_AGE_DAYS: i64 = 3;

/// A reply under this many comments is read; far past it, it is buried.
pub const BURIED_COMMENTS: u32 = 120;

/// What else a phrase needs to be present before it counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Need {
    /// The phrase is enough on its own ("bands like").
    Nothing,
    /// The phrase is generic ("recommendations") and only means music if the
    /// title also names music. Production proved the need: a VAT-invoicing
    /// software request in a joined Polish room matched "recommend".
    MusicWord,
    /// The phrase is an open question that is only about *now* when the title
    /// says so ("favourite album of 2026"). Without it, "favourite albums from
    /// TesseracT" is a thread about one other band, and a reply naming ours
    /// would be off-topic.
    Recency,
}

// Phrases that make a title a request for music. Lowercase; matched against the
// lowercased title. English and Polish, because the band's rooms are both.
// Statements are deliberately absent: "such an underrated album" and "thanks to
// who recommended this" are not requests, and neither is "can't recommend".
const RECOMMENDATION_PHRASES: &[(&str, &str, Need)] = &[
    ("recommend me", "asks_for_recommendations", Need::MusicWord),
    (
        "recommend some",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    ("recommend a ", "asks_for_recommendations", Need::MusicWord),
    (
        "recommend any ",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "anyone recommend",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "can you recommend",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "could you recommend",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "please recommend",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "would you recommend",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "recommendations",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    (
        "recommendation",
        "asks_for_recommendations",
        Need::MusicWord,
    ),
    ("suggestions for", "asks_for_suggestions", Need::MusicWord),
    ("suggest me", "asks_for_suggestions", Need::MusicWord),
    ("bands like", "bands_like", Need::Nothing),
    ("artists like", "bands_like", Need::Nothing),
    ("band like", "bands_like", Need::Nothing),
    ("bands similar", "similar_to", Need::Nothing),
    ("albums similar", "similar_to", Need::Nothing),
    ("similar to", "similar_to", Need::MusicWord),
    ("sounds like", "sounds_like", Need::MusicWord),
    ("anything like", "anything_like", Need::MusicWord),
    ("more like", "more_like", Need::MusicWord),
    ("new bands", "new_bands", Need::Nothing),
    ("what should i listen", "what_to_listen", Need::Nothing),
    ("what to listen", "what_to_listen", Need::Nothing),
    ("looking for new music", "looking_for_music", Need::Nothing),
    ("looking for new metal", "looking_for_music", Need::Nothing),
    ("looking for new bands", "looking_for_music", Need::Nothing),
    ("looking for some", "looking_for_music", Need::MusicWord),
    ("polec", "pl_polecenia", Need::MusicWord),
    ("poleć", "pl_polecenia", Need::MusicWord),
    ("szukam zespo", "pl_szukam", Need::Nothing),
    ("szukam muzyki", "pl_szukam", Need::Nothing),
    ("podobn", "pl_podobne", Need::MusicWord),
    ("co posłuchać", "pl_co_posluchac", Need::Nothing),
    ("co posluchac", "pl_co_posluchac", Need::Nothing),
    ("jakie zespoły", "pl_jakie_zespoly", Need::Nothing),
];

const OPEN_FLOOR_PHRASES: &[(&str, &str, Need)] = &[
    ("what are you listening", "listening_to", Need::Nothing),
    ("what's everyone listening", "listening_to", Need::Nothing),
    ("what is everyone listening", "listening_to", Need::Nothing),
    ("what are you spinning", "listening_to", Need::Nothing),
    ("current rotation", "listening_to", Need::Nothing),
    ("favourite song", "favourite_song", Need::Recency),
    ("favorite song", "favourite_song", Need::Recency),
    ("favourite album", "favourite_song", Need::Recency),
    ("favorite album", "favourite_song", Need::Recency),
    ("favourite metal", "favourite_song", Need::Recency),
    ("favorite metal", "favourite_song", Need::Recency),
    ("favourite band", "favourite_song", Need::Recency),
    ("favorite band", "favourite_song", Need::Recency),
    ("best song of", "best_of", Need::Nothing),
    ("best album of", "best_of", Need::Nothing),
    ("czego słuchacie", "pl_listening_to", Need::Nothing),
    ("czego sluchacie", "pl_listening_to", Need::Nothing),
    ("co teraz słuchacie", "pl_listening_to", Need::Nothing),
];

const MUSIC_WORDS: &[&str] = &[
    "band", "album", "song", "music", "metal", "core", "djent", "prog", "riff", "listen", "artist",
    "track", "playlist", "genre", "ep ", "zespo", "muzyk", "płyt", "plyt", "utwór", "utwor",
    "kawałk", "kawalk", "słuch", "posłuch", "posluch",
];

const RECENCY_WORDS: &[&str] = &[
    "2026",
    "this year",
    "released",
    "lately",
    "this week",
    "this month",
    "right now",
];

fn satisfied(need: Need, title: &str) -> bool {
    match need {
        Need::Nothing => true,
        Need::MusicWord => MUSIC_WORDS.iter().any(|word| title.contains(word)),
        // A question, not a share: "One of my favourite albums of 2026" is
        // somebody showing a record, not asking for one.
        Need::Recency => {
            title.contains('?') && RECENCY_WORDS.iter().any(|word| title.contains(word))
        }
    }
}

// A title that matches a request phrase but is about something other than
// finding music to hear: hiring a player, buying gear, a meme.
const NOT_A_LISTENER: &[&str] = &[
    "bassist",
    "drummer",
    "guitarist",
    "vocalist",
    "singer for",
    "band member",
    "lineup",
    "line-up",
    "looking for a band",
    "basista",
    "perkusist",
    "gitarzyst",
    "wokalist",
    "pedal",
    "amp ",
    "amplifier",
    "strings",
    "interface",
    "plugin",
    " vst",
    " daw",
    "preamp",
    "pickup",
    "tuning ",
    "cab ir",
    "for sale",
    "wts",
    "wtb",
    "tab for",
    "tabs for",
    "tutorial",
    "lesson",
];

const NOT_A_LISTENER_FLAIRS: &[&str] = &[
    "meme",
    "shitpost",
    "gear",
    "advice",
    "help",
    "question: gear",
];

/// Decides whether a thread title is a request for music. `None` is the
/// default: most threads in any room are not.
#[must_use]
pub fn classify(thread: &ThreadFacts<'_>) -> Option<DemandSignal> {
    let title = thread.title.to_lowercase();
    // A title is short; padding with a space lets word-boundary-ish phrases
    // like " vst" and "amp " match at the edges.
    let padded = format!(" {title} ");
    if NOT_A_LISTENER.iter().any(|word| padded.contains(word)) {
        return None;
    }
    if let Some(flair) = thread.flair {
        let flair = flair.to_lowercase();
        if NOT_A_LISTENER_FLAIRS
            .iter()
            .any(|blocked| flair.contains(blocked))
        {
            return None;
        }
    }
    if let Some((_, rule, _)) = RECOMMENDATION_PHRASES
        .iter()
        .find(|(phrase, _, need)| title.contains(phrase) && satisfied(*need, &padded))
    {
        return Some(DemandSignal {
            kind: DemandKind::Recommendation,
            rule,
        });
    }
    OPEN_FLOOR_PHRASES
        .iter()
        .find(|(phrase, _, need)| title.contains(phrase) && satisfied(*need, &padded))
        .map(|(_, rule, _)| DemandSignal {
            kind: DemandKind::OpenFloor,
            rule,
        })
}

/// How reachable a reply in this thread still is, in basis points: posted
/// today and not yet crowded is full value; a thread older than
/// [`MAX_AGE_DAYS`] is worth nothing, and one already past
/// [`BURIED_COMMENTS`] comments is read by few.
#[must_use]
pub fn reach_basis_points(thread: &ThreadFacts<'_>, now: OffsetDateTime) -> u16 {
    let age_days = (now.date() - thread.posted_on).whole_days();
    if !(0..=MAX_AGE_DAYS).contains(&age_days) {
        return 0;
    }
    // Linear freshness: 100% today, 25% at the age limit.
    let freshness = 10_000_u32.saturating_sub(
        u32::try_from(age_days).unwrap_or(0) * (7_500 / u32::try_from(MAX_AGE_DAYS).unwrap_or(1)),
    );
    let crowding = match thread.comments {
        // Unknown is not zero: a thread with no count is scored as middling
        // rather than as empty.
        None => 7_000,
        Some(comments) if comments >= BURIED_COMMENTS => 2_000,
        Some(comments) => 10_000 - comments.saturating_mul(6_000) / BURIED_COMMENTS,
    };
    u16::try_from(freshness.saturating_mul(crowding) / 10_000).unwrap_or(u16::MAX)
}

/// The ranking score: intent × reach, in basis points. Zero when the thread
/// is not a request or is out of reach.
#[must_use]
pub fn score_basis_points(thread: &ThreadFacts<'_>, now: OffsetDateTime) -> u16 {
    let Some(signal) = classify(thread) else {
        return 0;
    };
    let intent = u32::from(signal.kind.intent_basis_points());
    let reach = u32::from(reach_basis_points(thread, now));
    u16::try_from(intent * reach / 10_000).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Duration, macros::datetime};

    const NOW: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

    fn thread<'a>(title: &'a str, days_old: i64, comments: Option<u32>) -> ThreadFacts<'a> {
        ThreadFacts {
            title,
            flair: None,
            comments,
            posted_on: NOW.date() - Duration::days(days_old),
        }
    }

    #[test]
    fn a_request_for_music_is_demand_and_names_its_rule() {
        for (title, kind, rule) in [
            (
                "Bands like Gojira but more melodic?",
                DemandKind::Recommendation,
                "bands_like",
            ),
            (
                "Recommend me some modern metalcore",
                DemandKind::Recommendation,
                "asks_for_recommendations",
            ),
            (
                "Looking for new bands in the vein of Architects",
                DemandKind::Recommendation,
                "new_bands",
            ),
            (
                "Polećcie zespoły podobne do Behemoth",
                DemandKind::Recommendation,
                "pl_polecenia",
            ),
            (
                "Which is your favourite metal song released in 2026?",
                DemandKind::OpenFloor,
                "favourite_song",
            ),
            (
                "Looking for some melodic / melancholic metal recommendations",
                DemandKind::Recommendation,
                "asks_for_recommendations",
            ),
            (
                "What are you listening to this week?",
                DemandKind::OpenFloor,
                "listening_to",
            ),
        ] {
            let signal = classify(&thread(title, 0, Some(5))).unwrap_or_else(|| panic!("{title}"));
            assert_eq!((signal.kind, signal.rule), (kind, rule), "{title}");
        }
    }

    #[test]
    fn hiring_gear_and_memes_are_not_listeners() {
        for title in [
            "Looking for a bassist for a shoegaze band",
            "Recommend a good overdrive pedal for djent",
            "Best amp sim plugin for metal rhythm?",
            "WTS Ibanez RG, similar to a Jackson",
            "Szukam gitarzysty do zespołu",
            "Calvin and thrash metal",
            "Any self-hosted software you can recommend for VAT invoicing clients?",
            "Thrice - “Beggars” is such an underrated album",
            "After today’s news, I can’t recommend anything else other than Venom",
            "I'm addicted. Thanks to who recommended this here",
            "What are your favorite albums/songs from TesseracT?",
            "One of my Favourite Albums of 2026",
            "Miss May I - No Place For Me [Album Discussion]",
        ] {
            assert!(classify(&thread(title, 0, Some(5))).is_none(), "{title}");
        }
        let mut meme = thread("Recommend me a worse take than this", 0, Some(5));
        meme.flair = Some("Meme/Shitpost");
        assert!(classify(&meme).is_none());
    }

    #[test]
    fn a_direct_request_outranks_an_open_invitation() {
        let ask = score_basis_points(&thread("Bands like Gojira?", 0, Some(10)), NOW);
        let invite = score_basis_points(&thread("What are you listening to?", 0, Some(10)), NOW);
        assert!(ask > invite && invite > 0, "{ask} vs {invite}");
    }

    #[test]
    fn a_stale_or_buried_thread_is_worth_less_and_an_expired_one_nothing() {
        let fresh = score_basis_points(&thread("Bands like Gojira?", 0, Some(3)), NOW);
        let day_old = score_basis_points(&thread("Bands like Gojira?", 2, Some(3)), NOW);
        let buried = score_basis_points(&thread("Bands like Gojira?", 0, Some(400)), NOW);
        assert!(
            fresh > day_old && fresh > buried,
            "{fresh} {day_old} {buried}"
        );
        assert_eq!(
            score_basis_points(&thread("Bands like Gojira?", 4, Some(3)), NOW),
            0
        );
    }

    #[test]
    fn an_unknown_comment_count_is_not_scored_as_an_empty_thread() {
        let unknown = reach_basis_points(&thread("Bands like Gojira?", 0, None), NOW);
        let empty = reach_basis_points(&thread("Bands like Gojira?", 0, Some(0)), NOW);
        assert!(unknown < empty, "{unknown} < {empty}");
    }

    #[test]
    fn a_non_request_scores_zero_however_fresh() {
        assert_eq!(
            score_basis_points(&thread("Saw Noxis last night...", 0, Some(0)), NOW),
            0
        );
    }
}
