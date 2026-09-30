//! The `/watch` capture seam: which redirect destinations become a landing
//! page instead of a bare 302 into YouTube.
//!
//! A tracked click that lands on `virya.music/watch/{id}` sees the video and
//! a one-step join form — the click is attributed either way, so sending a
//! capturable audience through the capture page costs nothing and turns a
//! view into a fan. Not every lane is capturable: a Reddit community whose
//! rules refuse off-site links must still get the bare YouTube redirect, so
//! the decision stays per-link.

/// Channels whose audiences can be asked to join without violating anything —
/// CrowdRelay's own inboxes and feeds, and the social lanes where a landing
/// page is an ordinary link. Reddit is absent on purpose: each community's
/// rules decide, and `landing_for` checks them per post.
pub const CAPTURE_CHANNELS: &[&str] = &[
    "telegram",
    "discord",
    "email",
    "push",
    "signal",
    "drop_surge",
    "facebook",
    "instagram",
    "x",
    "bluesky",
    "tiktok",
];

/// Rule words that forbid an off-site landing page. A community whose
/// verified rules summary names any of these — as a substring, so "links"
/// catches "link" — gets the plain redirect.
const OFFSITE_RULE_WORDS: &[&str] = &[
    "youtube", "direct", "link", "website", "platform", "spotify", "bandcamp", "blog", "site",
];

/// The YouTube video id a destination points at, when it is one of the
/// forms a smart-link carries: `youtube.com/watch?v=ID` (with any extra
/// query noise trimmed), `youtu.be/ID`, or `youtube.com/shorts/ID`, on the
/// bare, `www.`, or `m.` hosts. An id that is not exactly eleven
/// `[A-Za-z0-9_-]` characters answers None.
#[must_use]
pub fn owned_youtube_id(destination: &str) -> Option<&str> {
    let rest = destination
        .trim()
        .strip_prefix("https://")
        .or_else(|| destination.trim().strip_prefix("http://"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest.get(..authority_end)?.to_lowercase();
    let host = host.split(':').next().unwrap_or(host.as_str());
    let path_and_query = rest.get(authority_end..).unwrap_or("");

    let candidate = match host {
        "youtu.be" | "www.youtu.be" => path_and_query
            .strip_prefix('/')
            .and_then(|rest| rest.split(['/', '?', '#']).next()),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
            let path_lower = path_and_query
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .to_lowercase();
            if path_lower == "/watch" {
                let query = path_and_query
                    .split_once('?')
                    .map(|(_, q)| q.split('#').next().unwrap_or(q))
                    .unwrap_or("");
                query.split('&').find_map(|pair| pair.strip_prefix("v="))
            } else if path_lower.starts_with("/shorts/") {
                path_and_query
                    .get("/shorts/".len()..)
                    .and_then(|rest| rest.split(['/', '?', '#']).next())
            } else {
                None
            }
        }
        _ => return None,
    }?;
    (candidate.len() == 11
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some(candidate)
}

/// Whether a community's verified rules permit an off-site landing page.
/// The summary is a person's prose; a lowercase substring scan for the
/// words rules use to forbid links errs toward the bare redirect, which is
/// never wrong.
#[must_use]
pub fn reddit_rules_allow_offsite(rules_summary: &str) -> bool {
    let summary = rules_summary.to_lowercase();
    !OFFSITE_RULE_WORDS.iter().any(|word| summary.contains(word))
}

/// What a redirect decision needs from the resolved link.
pub struct LandingInputs<'a> {
    /// The link's destination URL.
    pub destination: &'a str,
    /// The channel the link was minted for (`smart_links.channel_source`).
    pub channel: Option<&'a str>,
    /// The community the link was minted for (`smart_links.channel_community`).
    pub community: Option<&'a str>,
}

/// The `/watch` URL a capturable click should land on, or None when the
/// redirect should go to the destination exactly as today.
///
/// Landing happens only when all of these hold: the destination parses as a
/// YouTube video, the id is one of ours (`is_owned`), and the lane allows
/// it — a capture channel outright, or `reddit` in a community whose rules
/// pass `reddit_offsite_ok`. `origin` carries no trailing slash; the URL
/// keeps UTM parameters so the join form can still read the lane.
#[must_use]
pub fn landing_for(
    inputs: LandingInputs<'_>,
    is_owned: impl Fn(&str) -> bool,
    reddit_offsite_ok: impl Fn(&str) -> bool,
    origin: &str,
) -> Option<String> {
    let id = owned_youtube_id(inputs.destination)?;
    if !is_owned(id) {
        return None;
    }
    let channel = inputs.channel?.trim().to_lowercase();
    let allowed = CAPTURE_CHANNELS.contains(&channel.as_str())
        || (channel == "reddit"
            && inputs
                .community
                .is_some_and(|community| reddit_offsite_ok(&normalize_community(community))));
    allowed.then(|| format!("{origin}/watch/{id}/?utm_source={channel}&utm_medium=watch"))
}

/// `r/Foo`, `FOO`, and ` foo ` are one community — the redirect snapshot
/// stores this normalization, so `landing_for` applies it before asking.
#[must_use]
pub fn normalize_community(community: &str) -> String {
    community
        .trim()
        .strip_prefix("r/")
        .or_else(|| community.trim().strip_prefix("R/"))
        .unwrap_or_else(|| community.trim())
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://virya.music";

    fn owned(id: &str) -> bool {
        id == "abc123DEF_-"
    }

    fn offsite_ok(community: &str) -> bool {
        community == "melodicdeathmetal"
    }

    #[test]
    fn a_telegram_click_on_an_owned_video_lands_on_watch() {
        let url = landing_for(
            LandingInputs {
                destination: "https://www.youtube.com/watch?v=abc123DEF_-",
                channel: Some("telegram"),
                community: None,
            },
            owned,
            offsite_ok,
            ORIGIN,
        );
        assert_eq!(
            url.as_deref(),
            Some("https://virya.music/watch/abc123DEF_-/?utm_source=telegram&utm_medium=watch")
        );
    }

    #[test]
    fn facebook_and_instagram_are_capture_channels_too() {
        for channel in ["facebook", "instagram", "FACEBOOK"] {
            assert!(
                landing_for(
                    LandingInputs {
                        destination: "https://youtu.be/abc123DEF_-",
                        channel: Some(channel),
                        community: None,
                    },
                    owned,
                    offsite_ok,
                    ORIGIN,
                )
                .is_some(),
                "{channel} should capture"
            );
        }
    }

    #[test]
    fn reddit_lands_only_when_the_community_rules_allow_it() {
        let destination = "https://youtu.be/abc123DEF_-";
        // A community with no verified offsite-ok rules gets the redirect.
        assert_eq!(
            landing_for(
                LandingInputs {
                    destination,
                    channel: Some("reddit"),
                    community: Some("metalcore"),
                },
                owned,
                offsite_ok,
                ORIGIN,
            ),
            None
        );
        // A community whose rules allow it captures — and `r/Foo` is `foo`.
        assert!(
            landing_for(
                LandingInputs {
                    destination,
                    channel: Some("reddit"),
                    community: Some("r/MelodicDeathMetal"),
                },
                owned,
                offsite_ok,
                ORIGIN,
            )
            .is_some()
        );
        assert!(
            landing_for(
                LandingInputs {
                    destination,
                    channel: Some("reddit"),
                    community: Some(" melodicdeathmetal "),
                },
                owned,
                offsite_ok,
                ORIGIN,
            )
            .is_some()
        );
    }

    #[test]
    fn a_link_without_a_channel_never_lands() {
        assert_eq!(
            landing_for(
                LandingInputs {
                    destination: "https://youtu.be/abc123DEF_-",
                    channel: None,
                    community: None,
                },
                owned,
                offsite_ok,
                ORIGIN,
            ),
            None
        );
    }

    #[test]
    fn a_video_we_do_not_own_redirects_straight_to_youtube() {
        assert_eq!(
            landing_for(
                LandingInputs {
                    destination: "https://youtu.be/xyz987XYZ_-",
                    channel: Some("telegram"),
                    community: None,
                },
                owned,
                offsite_ok,
                ORIGIN,
            ),
            None
        );
    }

    #[test]
    fn the_youtube_channel_is_not_a_capture_lane() {
        assert_eq!(
            landing_for(
                LandingInputs {
                    destination: "https://youtu.be/abc123DEF_-",
                    channel: Some("youtube"),
                    community: None,
                },
                owned,
                offsite_ok,
                ORIGIN,
            ),
            None
        );
    }

    #[test]
    fn every_owned_video_url_shape_parses() {
        assert_eq!(
            owned_youtube_id("https://www.youtube.com/watch?v=abc123DEF_-"),
            Some("abc123DEF_-")
        );
        assert_eq!(
            owned_youtube_id("https://youtu.be/abc123DEF_-?si=share"),
            Some("abc123DEF_-")
        );
        assert_eq!(
            owned_youtube_id("https://www.youtube.com/shorts/abc123DEF_-"),
            Some("abc123DEF_-")
        );
        assert_eq!(
            owned_youtube_id("https://m.youtube.com/watch?v=abc123DEF_-"),
            Some("abc123DEF_-")
        );
        assert_eq!(
            owned_youtube_id("https://youtube.com/watch?v=abc123DEF_-"),
            Some("abc123DEF_-")
        );
    }

    #[test]
    fn extra_query_parameters_do_not_reach_the_watch_url() {
        // `&t=30s` belongs to the watch player, not our landing page — the
        // id stops at the next `&`.
        let url = landing_for(
            LandingInputs {
                destination: "https://www.youtube.com/watch?v=abc123DEF_-&t=30s",
                channel: Some("discord"),
                community: None,
            },
            owned,
            offsite_ok,
            ORIGIN,
        )
        .expect("an owned discord link lands");
        assert!(!url.contains("&t=30s"));
        assert!(url.contains("/watch/abc123DEF_-"));
    }

    #[test]
    fn malformed_and_foreign_urls_never_lands() {
        for destination in [
            "https://www.youtube.com/watch?v=tooshort",
            "https://www.youtube.com/watch?v=toolong1234567",
            "https://www.youtube.com/watch?v=abc!@#DEF_-",
            "https://evil-youtube.com/watch?v=abc123DEF_-",
            "https://open.spotify.com/track/abc123DEF_-",
            "not a url",
            "https://youtu.be/",
        ] {
            assert_eq!(owned_youtube_id(destination), None, "{destination}");
        }
    }

    /// The real `rules_summary` strings production carries, both directions.
    #[test]
    fn reddit_rules_read_the_community_summary() {
        for rules in [
            "No youtube links/promoting other sites or platforms, or advertising in any way.",
            "Please put the artist and song name in the title || No direct image links || No mobile links",
            "No X Links || Reposting",
        ] {
            assert!(!reddit_rules_allow_offsite(rules), "{rules}");
        }
        for rules in [
            "Standardize titles for song posts || Songs should be melodic death metal || No too soon reposts",
            "No memes or parodical media || No full releases || Band blacklist",
        ] {
            assert!(reddit_rules_allow_offsite(rules), "{rules}");
        }
    }
}
