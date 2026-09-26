//! Whether a community post reads like a person or like a bot.
//!
//! `publish_guard` refuses what is mechanically wrong — an unapproved link, an
//! unfilled template, shouting. A post can pass all of that and still read as
//! exactly what moderators remove on sight: a hashtag trail, an emoji burst, a
//! call to action, the stock phrases generated text reaches for, or last
//! week's post reworded. None of those is a hallucination; each is a register
//! a community recognises as marketing, and removal is how it answers.
//!
//! Community channel only. The band's own Telegram or Instagram may carry
//! hashtags and a "stream now" — its audience opted in. Someone else's
//! subreddit did not.
//!
//! Same contract as the publish guard: deterministic, and a hold for a person
//! rather than a discard. A false positive costs a post going out by hand; a
//! false negative costs a removal, and removals cost the account.

use std::collections::BTreeSet;

/// Why a community draft was held. The sentence is what the operator reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterHold {
    Hashtags,
    EmojiFlood,
    CallToAction,
    GeneratedPhrasing,
    NearDuplicate,
}

impl RegisterHold {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hashtags => "held: hashtags — on Reddit they read as cross-posted marketing",
            Self::EmojiFlood => "held: more emoji than a person would use in one sentence",
            Self::CallToAction => {
                "held: a sales line (\"check out\", \"stream now\", \"link in bio\") — \
                 communities remove posts that ask instead of share"
            }
            Self::GeneratedPhrasing => {
                "held: stock phrasing generated text reaches for (\"delve\", \"sonic journey\") \
                 — rewrite it in the band's own words"
            }
            Self::NearDuplicate => {
                "held: nearly the same words as a post we published in the last 30 days"
            }
        }
    }
}

/// Emoji allowed in one community post before it reads as decoration.
const MAX_EMOJI: usize = 2;
/// Word-trigram overlap at or above which two posts are the same post.
const NEAR_DUPLICATE_SIMILARITY: f64 = 0.6;

/// Calls to action and hype, English and Polish. Lowercased substrings.
const CALL_TO_ACTION: &[&str] = &[
    "check out our",
    "check it out",
    "don't miss",
    "dont miss",
    "smash that",
    "like and subscribe",
    "hit the like",
    "link in bio",
    "follow us",
    "support us",
    "please share",
    "stream now",
    "go stream",
    "out now on all",
    "available on all platforms",
    "you won't believe",
    "must-listen",
    "must listen",
    "new banger",
    "nie przegap",
    "obserwuj nas",
    "zasubskrybuj",
    "udostępnij",
    "link w bio",
    "wspierajcie nas",
];

/// Phrases generated text reaches for and people rarely write. Lowercased.
const GENERATED_PHRASING: &[&str] = &[
    "delve",
    "tapestry",
    "sonic journey",
    "sonic landscape",
    "sonic tapestry",
    "immerse yourself",
    "embark on",
    "unleash",
    "elevate your",
    "whether you're a fan",
    "look no further",
    "in today's",
    "a testament to",
    "we are thrilled",
    "we're thrilled",
    "we are excited to",
    "we're excited to",
];

/// Reviews a community draft's register against the recent posts it must not
/// repeat. `None` means it reads like a person; publish-guard checks still apply.
#[must_use]
pub fn review_community_register(body: &str, recent_bodies: &[String]) -> Option<RegisterHold> {
    let lowered = body.to_lowercase();
    if has_hashtag(body) {
        return Some(RegisterHold::Hashtags);
    }
    if body.chars().filter(|c| is_emoji(*c)).count() > MAX_EMOJI {
        return Some(RegisterHold::EmojiFlood);
    }
    if CALL_TO_ACTION.iter().any(|phrase| lowered.contains(phrase)) {
        return Some(RegisterHold::CallToAction);
    }
    if GENERATED_PHRASING
        .iter()
        .any(|phrase| lowered.contains(phrase))
    {
        return Some(RegisterHold::GeneratedPhrasing);
    }
    let draft = trigrams(body);
    if !draft.is_empty()
        && recent_bodies
            .iter()
            .any(|recent| similarity(&draft, &trigrams(recent)) >= NEAR_DUPLICATE_SIMILARITY)
    {
        return Some(RegisterHold::NearDuplicate);
    }
    None
}

/// A `#` followed by a letter at the start of a word. `#1` and `C#` are not
/// hashtags; a URL fragment is not either, because URLs are stripped first.
fn has_hashtag(body: &str) -> bool {
    body.split_whitespace()
        .filter(|token| !token.contains("://"))
        .any(|token| {
            let mut chars = token.chars();
            chars.next() == Some('#') && chars.next().is_some_and(char::is_alphabetic)
        })
}

/// Pictographic emoji. Deliberately the common blocks, not every symbol: a
/// `—` or a `♪` is punctuation a person uses.
fn is_emoji(c: char) -> bool {
    matches!(u32::from(c), 0x1F300..=0x1FAFF | 0x2600..=0x26FF | 0x2700..=0x27BF)
}

/// Word trigrams of the text with links removed, lowercased.
fn trigrams(body: &str) -> BTreeSet<String> {
    let words: Vec<String> = body
        .split_whitespace()
        .filter(|token| !token.contains("://") && !token.starts_with("www."))
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    words.windows(3).map(|window| window.join(" ")).collect()
}

fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count();
    let union = a.union(b).count();
    #[expect(
        clippy::cast_precision_loss,
        reason = "trigram counts of one post are tiny; f64 is exact"
    )]
    let ratio = shared as f64 / union as f64;
    ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(body: &str) -> Option<RegisterHold> {
        review_community_register(body, &[])
    }

    #[test]
    fn a_plain_sentence_in_the_bands_voice_passes() {
        assert_eq!(
            review(
                "New video for Ashes, the slowest song we have ever recorded. https://virya.music/l/ashes"
            ),
            None
        );
        assert_eq!(
            review(
                "Nowy klip do Popiołów, najwolniejszy numer jaki nagraliśmy. https://virya.music/l/ashes"
            ),
            None
        );
    }

    #[test]
    fn marketing_registers_are_held() {
        assert_eq!(
            review("New single is here #doom #metal #newmusic"),
            Some(RegisterHold::Hashtags)
        );
        assert_eq!(
            review("New video out 🔥🔥🤘 go watch"),
            Some(RegisterHold::EmojiFlood)
        );
        assert_eq!(
            review("Check out our new video, link below!"),
            Some(RegisterHold::CallToAction)
        );
        assert_eq!(
            review("Nie przegap naszego nowego klipu"),
            Some(RegisterHold::CallToAction)
        );
        assert_eq!(
            review("Immerse yourself in a crushing sonic journey through grief"),
            Some(RegisterHold::GeneratedPhrasing)
        );
    }

    #[test]
    fn music_notation_and_urls_are_not_hashtags() {
        assert_eq!(review("Tuned to C# standard, track #1 on the record"), None);
        assert_eq!(
            review("The live cut from Warsaw is up https://virya.music/l/x#live"),
            None
        );
    }

    #[test]
    fn a_reworded_repeat_is_held_and_a_new_post_is_not() {
        let recent = vec![
            "New video for Ashes, the slowest song we have ever recorded. https://virya.music/l/a"
                .to_owned(),
        ];
        assert_eq!(
            review_community_register(
                "New video for Ashes, the slowest song we have ever recorded! https://virya.music/l/b",
                &recent
            ),
            Some(RegisterHold::NearDuplicate)
        );
        assert_eq!(
            review_community_register(
                "We filmed the Embers video in one take in a flooded quarry. https://virya.music/l/c",
                &recent
            ),
            None
        );
    }
}
