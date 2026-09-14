//! Whether a community is a music space at all — the topical screen.
//!
//! Member count cannot answer this. The r/metalgearsolid incident: a
//! video-game subreddit was admitted on size alone because nothing read the
//! community's own description, and the band's account posted a fabricated
//! anecdote into a forum that could never hold a fan. This module is the
//! deterministic vocabulary the screen consults — the same signal a human
//! would read: the place's recorded name, notes and genre tags, never the
//! proposing model's say-so.
//!
//! Two vocabularies, two match rules:
//!
//! - **Genre phrases** match inside a separator-free normalization, so
//!   "PostHardcore", "post-hardcore" and "post hardcore" are one signal.
//!   Each phrase is specific enough that a hit means the space is about
//!   music — bare "metal", "rock" and "pop" are absent on purpose.
//! - **Context terms** match on ASCII word boundaries only, so "ska" inside
//!   "Nebraska" and "band" inside "husband" do not count. Words that collide
//!   with other subjects entirely ("jazz" → Utah Jazz, "bass" → fishing,
//!   "country", "folk", "soul") live in neither list.
//!
//! And the honest third state: `Unknown`. A community nobody described —
//! a cryptic single-token name like "GGGOLDDD" and nothing else — is not
//! refused on a signal that does not exist. Absent is not suspicious,
//! matching the rest of the discovery policy.

use serde::Serialize;

/// What the community's own text says about whether it is a music space.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommunityTopicSignal {
    /// A genre phrase or music-domain term appears in the community's own
    /// name, notes or genre tags.
    MusicRelated,
    /// The community describes itself and none of it is music. This is the
    /// `OffTopic` refusal — a 3M-member object-identification forum is not
    /// made a growth target by being large.
    Unrelated,
    /// No name, notes or genre tags were recorded — nothing to screen on.
    /// Absent is not suspicious, matching the rest of this policy: the
    /// community is left to the causal model rather than refused on a
    /// signal nobody has.
    #[default]
    Unknown,
}

/// Genre and scene phrases, normalized to `[a-z0-9]+` and matched as
/// substrings against the same-normalized community text. Each is specific
/// enough that a hit means the space is about music — which is why bare
/// "metal", "rock" and "pop" are absent: r/metalgearsolid is a video-game
/// community, r/metaldetecting is treasure hunting, and both contain
/// "metal". "metal gear" and "metal detecting" contain no genre phrase.
const MUSIC_GENRE_PHRASES: &[&str] = &[
    "heavymetal",
    "blackmetal",
    "deathmetal",
    "thrashmetal",
    "doommetal",
    "numetal",
    "nümetal",
    "powermetal",
    "folkmetal",
    "vikingmetal",
    "progmetal",
    "progressivemetal",
    "postmetal",
    "metalcore",
    "deathcore",
    "grindcore",
    "metalhead",
    "metalheads",
    "metalmusic",
    "metalband",
    "metalbands",
    "metalscene",
    "metalmemes",
    "metaladjacent",
    "metalcomm",
    "posthardcore",
    "postrock",
    "progrock",
    "progressiverock",
    "stonerrock",
    "alternativerock",
    "altrock",
    "indierock",
    "classicrock",
    "punkrock",
    "poppunk",
    "postpunk",
    "hardcorepunk",
    "crustpunk",
    "screamo",
    "djent",
    "shoegaze",
    "rockmusic",
    "rockband",
    "rockbands",
    "indiemusic",
    "newmusic",
    "livemusic",
    "listentothis",
    "drumandbass",
    "drumbass",
    "drumnbass",
    "electronicmusic",
    "housemusic",
    "technomusic",
    "ambientmusic",
    "classicalmusic",
    "jazzmusic",
    "jazzband",
    "bluesmusic",
    "folkmusic",
    "countrymusic",
    "soulmusic",
    "funkmusic",
    "reggaemusic",
    "hiphop",
    "rapmusic",
];

/// Music-domain words matched on ASCII word boundaries — safe alone, too
/// ambiguous as substrings ("ska" inside "Nebraska", "band" inside
/// "husband"). Bare genre words that collide with other subjects live in
/// neither list: "jazz" (Utah Jazz), "bass" (fishing), "blues" (St. Louis
/// Blues), "country", "folk" (folklore), "house" (real estate), "soul",
/// "pop", "grind", "tour" (travel) — they count only inside a genre phrase.
const MUSIC_CONTEXT_TERMS: &[&str] = &[
    "music",
    "musical",
    "musician",
    "musicians",
    "band",
    "bands",
    "album",
    "albums",
    "song",
    "songs",
    "songwriter",
    "songwriting",
    "concert",
    "concerts",
    "festival",
    "festivals",
    "vinyl",
    "guitar",
    "guitars",
    "guitarist",
    "guitarists",
    "drummer",
    "drummers",
    "drums",
    "bassist",
    "bassists",
    "vocalist",
    "vocalists",
    "singer-songwriter",
    "riff",
    "riffs",
    "metalhead",
    "metalheads",
    "playlist",
    "playlists",
    "gigs",
    "ska",
    "emo",
    "rap",
    "techno",
    "edm",
    "dnb",
    "funk",
    "shoegaze",
    "djent",
    "metalcore",
    "deathcore",
    "grindcore",
    "hardcore",
    "synthwave",
    "vaporwave",
    "bluegrass",
    "punk",
];

/// Reads the topical signal off a community's own description.
///
/// `genres` are the audience graph's recorded tags: any tag at all means a
/// discovery pass already classified the space as music, so the text check
/// is skipped. Otherwise the community's `name` and `notes` are screened
/// against the two vocabularies above. Empty everything is [`Unknown`] — a
/// place nobody described yet is not refused, matching the rest of this
/// policy's absent-is-not-suspicious rule.
#[must_use]
pub fn community_topic_signal(
    name: &str,
    notes: Option<&str>,
    genres: &[String],
) -> CommunityTopicSignal {
    if genres.iter().any(|genre| !genre.trim().is_empty()) {
        return CommunityTopicSignal::MusicRelated;
    }
    let text = format!("{name} {}", notes.unwrap_or_default());
    if text.trim().is_empty() {
        return CommunityTopicSignal::Unknown;
    }
    if notes.is_none_or(|n| n.trim().is_empty()) {
        // Only the name is known. A name that reads as a phrase — "For the
        // identification of mysterious objects" — is a description and can
        // be refused on; a bare token like "GGGOLDDD" or "Ningen" is not a
        // description, and refusing it would reject cryptically-named band
        // communities on no evidence at all.
        let name_words = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .count();
        if name_words < 2 {
            return CommunityTopicSignal::Unknown;
        }
    }
    // Genre phrases are matched on a separator-free normalization so that
    // "PostHardcore", "post-hardcore" and "post hardcore" are one signal.
    let squashed: String = text
        .chars()
        .filter_map(|c| c.to_lowercase().next())
        .filter(|c| c.is_ascii_alphanumeric() || *c == 'ü')
        .collect();
    if MUSIC_GENRE_PHRASES
        .iter()
        .any(|phrase| squashed.contains(phrase))
    {
        return CommunityTopicSignal::MusicRelated;
    }
    // Context terms are matched on word boundaries so that "Nebraska" does
    // not count as ska and "husband" does not count as a band.
    let lowered = text.to_lowercase();
    if MUSIC_CONTEXT_TERMS
        .iter()
        .any(|term| contains_word(&lowered, term))
    {
        return CommunityTopicSignal::MusicRelated;
    }
    CommunityTopicSignal::Unrelated
}

/// Whole-word ASCII match: `term` appears in `text` bounded by non-word
/// characters on both sides. Multi-word terms work unchanged — a two-word
/// term needs its internal space anyway.
fn contains_word(text: &str, term: &str) -> bool {
    let word_char = |i: usize| {
        text.as_bytes()
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric())
    };
    text.match_indices(term)
        .any(|(index, _)| (index == 0 || !word_char(index - 1)) && !word_char(index + term.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target_discovery::{
        CommunityCandidateSnapshot, RefusalReason, ScreeningVerdict, TargetDiscoveryPolicy,
        screen_community_candidate,
    };

    fn signal(name: &str, notes: &str) -> CommunityTopicSignal {
        community_topic_signal(name, Some(notes), &[])
    }

    fn community(topic_signal: CommunityTopicSignal) -> CommunityCandidateSnapshot {
        CommunityCandidateSnapshot {
            has_evidence: true,
            member_count: Some(40_000),
            activity_basis_points: Some(5_000),
            self_promo_ratio_percent: Some(10),
            sells_placement: false,
            refused_by_us_or_them: false,
            topic_signal,
        }
    }

    fn community_refusal(verdict: ScreeningVerdict) -> Option<RefusalReason> {
        match verdict {
            ScreeningVerdict::Refuse(reason) => Some(reason),
            ScreeningVerdict::Admit { .. } => None,
        }
    }

    // These cases are the production list that produced the r/metalgearsolid
    // incident: communities admitted on member count alone because nothing
    // read their own description.

    #[test]
    fn a_video_game_subreddit_is_off_topic_despite_metal_in_the_name() {
        assert_eq!(
            signal(
                "METAL GEAR SOLID | Tactical Subreddit Operations",
                "The home for everything Metal Gear on reddit"
            ),
            CommunityTopicSignal::Unrelated
        );
    }

    #[test]
    fn an_object_identification_forum_is_off_topic() {
        assert_eq!(
            signal(
                "For the identification of mysterious objects",
                "For the identification of mysterious objects"
            ),
            CommunityTopicSignal::Unrelated
        );
    }

    #[test]
    fn metal_detecting_is_treasure_hunting_not_music() {
        assert_eq!(
            signal(
                "metal detecting: treasure hunting",
                "Welcome to r/metaldetecting, a place to discuss all things metal detecting!"
            ),
            CommunityTopicSignal::Unrelated
        );
    }

    #[test]
    fn a_general_national_subreddit_is_off_topic() {
        assert_eq!(
            signal(
                "The Polish reddit",
                "The official English language subreddit for Poland and Polish news."
            ),
            CommunityTopicSignal::Unrelated
        );
    }

    #[test]
    fn a_real_metal_community_passes_on_its_description() {
        assert_eq!(
            signal(
                "Death Metal - news, reviews, videos & discussion.",
                "Death metal is a subgenre of heavy metal music."
            ),
            CommunityTopicSignal::MusicRelated
        );
        assert_eq!(
            signal(
                "MetalForTheMasses",
                "A community for discussion of metal and metal-adjacent music."
            ),
            CommunityTopicSignal::MusicRelated
        );
        assert_eq!(
            signal("Black Metal", ""),
            CommunityTopicSignal::MusicRelated
        );
    }

    #[test]
    fn adjacent_music_spaces_pass() {
        assert_eq!(
            signal(
                "All Rig... No Gig",
                "Are you serious about guitar? Do you spend three hours a day practicing?"
            ),
            CommunityTopicSignal::MusicRelated
        );
        assert_eq!(
            signal("Listen To This", "The musical community of reddit"),
            CommunityTopicSignal::MusicRelated
        );
        assert_eq!(
            signal("Buffalo and WNY Music Scene", "Local shows and bands."),
            CommunityTopicSignal::MusicRelated
        );
    }

    #[test]
    fn recorded_genre_tags_admit_without_reading_the_text() {
        assert_eq!(
            community_topic_signal("GGGOLDDD", None, &["post-metal".to_owned()],),
            CommunityTopicSignal::MusicRelated
        );
    }

    #[test]
    fn an_undescribed_community_is_unknown_not_unrelated() {
        assert_eq!(
            community_topic_signal("GGGOLDDD", None, &[]),
            CommunityTopicSignal::Unknown
        );
    }

    #[test]
    fn unrelated_communities_are_refused_whatever_their_size() {
        let snapshot = CommunityCandidateSnapshot {
            member_count: Some(3_500_000),
            ..community(CommunityTopicSignal::Unrelated)
        };
        assert_eq!(
            community_refusal(screen_community_candidate(
                &snapshot,
                TargetDiscoveryPolicy::default()
            )),
            Some(RefusalReason::OffTopic)
        );
    }

    #[test]
    fn an_unknown_topic_is_not_refused_on_topic_alone() {
        let snapshot = CommunityCandidateSnapshot {
            member_count: Some(3_000),
            ..community(CommunityTopicSignal::Unknown)
        };
        assert!(matches!(
            screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()),
            ScreeningVerdict::Admit { .. }
        ));
    }
}
