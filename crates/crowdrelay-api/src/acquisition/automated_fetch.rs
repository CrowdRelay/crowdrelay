//! Which requests to a tracked link are not a person.
//!
//! A click is the learning signal for every channel: the brain ranks channels
//! and messages by what the clicks did next. Until now every GET to a smart
//! link was recorded as one — including the unfurlers that fetch a URL the
//! moment it is pasted into Discord, Telegram, Facebook, Slack or WhatsApp,
//! search crawlers, HEAD probes and browser prefetches. Production on
//! 2026-10-02 showed single "clicks" on every Discord and Telegram link within
//! seconds of posting, and a link clicked two to four times at regular
//! intervals all day. A channel whose "audience" is its own preview bot looks
//! like a channel that works.
//!
//! The redirect is still served to all of them — a preview needs the
//! destination's metadata — but nothing is recorded and no attribution cookie
//! is set. Recognition is by declared identity only (method, `Purpose` headers,
//! user-agent tokens). A bot that lies about being a browser is
//! indistinguishable here; this removes the honest majority, not all noise.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::{HeaderMap, Method, header::USER_AGENT};

/// Why a tracked-link fetch was not counted as a click. The reason is the
/// metric label: a sudden rise in `declared_bot` after a post is the preview
/// bot, a rise in `missing_agent` is a script, and a falling total of real
/// clicks beside a flat total of fetches is the signal this module exists for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomatedFetch {
    NotGet,
    Prefetch,
    MissingAgent,
    DeclaredBot,
}

impl AutomatedFetch {
    const ALL: [Self; 4] = [
        Self::NotGet,
        Self::Prefetch,
        Self::MissingAgent,
        Self::DeclaredBot,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::NotGet => "not_get",
            Self::Prefetch => "prefetch",
            Self::MissingAgent => "missing_agent",
            Self::DeclaredBot => "declared_bot",
        }
    }

    fn counter(self) -> &'static AtomicU64 {
        static NOT_GET: AtomicU64 = AtomicU64::new(0);
        static PREFETCH: AtomicU64 = AtomicU64::new(0);
        static MISSING_AGENT: AtomicU64 = AtomicU64::new(0);
        static DECLARED_BOT: AtomicU64 = AtomicU64::new(0);
        match self {
            Self::NotGet => &NOT_GET,
            Self::Prefetch => &PREFETCH,
            Self::MissingAgent => &MISSING_AGENT,
            Self::DeclaredBot => &DECLARED_BOT,
        }
    }
}

pub(crate) fn record_dropped(reason: AutomatedFetch) {
    reason.counter().fetch_add(1, Ordering::Relaxed);
}

/// Prometheus text for the per-reason counters.
pub(crate) fn dropped_prometheus() -> String {
    let mut out = String::from(
        "# HELP crowdrelay_tracked_link_fetches_not_clicked_total Tracked-link fetches served the redirect but not recorded as a click, by reason.\n\
# TYPE crowdrelay_tracked_link_fetches_not_clicked_total counter\n",
    );
    for reason in AutomatedFetch::ALL {
        out.push_str(&format!(
            "crowdrelay_tracked_link_fetches_not_clicked_total{{reason=\"{}\"}} {}\n",
            reason.label(),
            reason.counter().load(Ordering::Relaxed)
        ));
    }
    out
}

/// Substrings (lowercase) in a user-agent that declare an automated fetcher.
/// `bot` covers Discordbot, TelegramBot, Googlebot, bingbot, Twitterbot,
/// Slackbot-LinkExpanding, LinkedInBot, Applebot, redditbot, ….
const AUTOMATED_AGENT_TOKENS: &[&str] = &[
    "bot",
    "crawler",
    "spider",
    "slurp",
    "preview",
    "facebookexternalhit",
    "facebot",
    "whatsapp",
    "embedly",
    "iframely",
    "vkshare",
    "pinterest",
    "mastodon",
    "headlesschrome",
    "curl/",
    "wget/",
    "python-requests",
    "python-urllib",
    "go-http-client",
    "node-fetch",
    "axios/",
    "java/",
    "libwww",
    "okhttp-unfurl",
];

/// Phone makers whose name contains `bot` — a real handset, not a fetcher.
const HANDSET_EXCEPTIONS: &[&str] = &["cubot"];

pub(crate) fn automated_fetch(method: &Method, headers: &HeaderMap) -> Option<AutomatedFetch> {
    if method != Method::GET {
        return Some(AutomatedFetch::NotGet);
    }
    for name in ["purpose", "sec-purpose", "x-moz", "x-purpose"] {
        if headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("prefetch") || value.contains("preview")
            })
        {
            return Some(AutomatedFetch::Prefetch);
        }
    }
    // Every browser sends a user-agent; a request without one is a script.
    let Some(agent) = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
    else {
        return Some(AutomatedFetch::MissingAgent);
    };
    let agent = agent.trim().to_ascii_lowercase();
    if agent.is_empty() {
        return Some(AutomatedFetch::MissingAgent);
    }
    let agent_without_handsets = HANDSET_EXCEPTIONS
        .iter()
        .fold(agent, |agent, handset| agent.replace(handset, ""));
    AUTOMATED_AGENT_TOKENS
        .iter()
        .any(|token| agent_without_handsets.contains(token))
        .then_some(AutomatedFetch::DeclaredBot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(agent: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(USER_AGENT, HeaderValue::from_str(agent).unwrap());
        map
    }

    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
        (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36";
    const SAFARI_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
        AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
    const ANDROID_FB_APP: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
        (KHTML, like Gecko) Version/4.0 Chrome/129.0.0.0 Mobile Safari/537.36 \
        [FB_IAB/FB4A;FBAV/480.0.0.0]";
    const CUBOT_PHONE: &str = "Mozilla/5.0 (Linux; Android 13; CUBOT P80) AppleWebKit/537.36 \
        (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36";

    #[test]
    fn people_in_real_browsers_are_clicks() {
        for agent in [CHROME, SAFARI_IOS, ANDROID_FB_APP, CUBOT_PHONE] {
            assert!(
                automated_fetch(&Method::GET, &headers(agent)).is_none(),
                "{agent} is a person"
            );
        }
    }

    #[test]
    fn link_unfurlers_and_crawlers_are_not() {
        for agent in [
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
            "TelegramBot (like TwitterBot)",
            "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)",
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
            "WhatsApp/2.23.20.0 A",
            "Twitterbot/1.0",
            "LinkedInBot/1.0 (compatible; Mozilla/5.0)",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 (Applebot/0.1)",
            "curl/8.4.0",
            "python-requests/2.31.0",
            "Go-http-client/2.0",
            "Mozilla/5.0 HeadlessChrome/129.0.0.0 Safari/537.36",
            "Mozilla/5.0 (compatible; SkypeUriPreview Preview/0.5)",
        ] {
            assert!(
                automated_fetch(&Method::GET, &headers(agent)) == Some(AutomatedFetch::DeclaredBot),
                "{agent} is a fetcher"
            );
        }
    }

    #[test]
    fn a_missing_or_blank_agent_is_a_script() {
        let missing = Some(AutomatedFetch::MissingAgent);
        assert_eq!(automated_fetch(&Method::GET, &HeaderMap::new()), missing);
        assert_eq!(automated_fetch(&Method::GET, &headers("  ")), missing);
    }

    #[test]
    fn head_and_prefetch_are_not_clicks() {
        assert_eq!(
            automated_fetch(&Method::HEAD, &headers(CHROME)),
            Some(AutomatedFetch::NotGet)
        );
        for (name, value) in [
            ("purpose", "prefetch"),
            ("sec-purpose", "prefetch;prerender"),
            ("x-moz", "prefetch"),
        ] {
            let mut map = headers(CHROME);
            map.insert(name, HeaderValue::from_static(value));
            assert_eq!(
                automated_fetch(&Method::GET, &map),
                Some(AutomatedFetch::Prefetch),
                "{name}: {value}"
            );
        }
    }

    #[test]
    fn every_reason_is_exported_with_its_own_label() {
        record_dropped(AutomatedFetch::DeclaredBot);
        let text = dropped_prometheus();
        for reason in AutomatedFetch::ALL {
            assert!(
                text.contains(&format!("reason=\"{}\"", reason.label())),
                "{reason:?} missing"
            );
        }
        assert!(!text.contains("declared_bot\"} 0"), "the increment shows");
    }
}
