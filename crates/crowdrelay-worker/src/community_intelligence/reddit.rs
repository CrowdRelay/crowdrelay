//! Reddit source adapter — subreddits, read through the logged-in browser.
//!
//! Production has 28 active Reddit `discovery_places` and, until this adapter,
//! nothing that claimed them. The community-intelligence sweep matched places
//! by `platform = adapter.id()`, the only adapter was `brutalland`, and no
//! place has that platform — so every sweep for months found zero work,
//! recorded a success, and wrote nothing. `community_observations` and
//! `community_entities` were both empty.
//!
//! Reddit cannot be read any other way. The public `.json` endpoints return
//! 403, the API application was rejected, and proxy IPs are blocked. The one
//! path that works is the authenticated browser session the agents service
//! already keeps for posting, so this adapter borrows it over
//! `POST /reddit/observe` — two `.json` GETs, no writes.
//!
//! **`subscribers` is reach, never audience.** r/Metal having ~4M members says
//! nothing about how many people follow this band, and counting it as audience
//! is exactly the mistake that once inflated the number 1,400×. It is recorded
//! here as a property of the place being observed, and the growth-metric
//! vocabulary keeps it out of the audience series.

use std::time::Duration;

use crowdrelay_domain::community_intelligence::{EntityType, normalize_strength};
use serde::Deserialize;
use uuid::Uuid;

use super::adapter::{
    AdapterError, AdapterPlace, ParsedEntity, ParsedObservation, RateLimitPolicy, SourceAdapter,
};

const COLLECTOR_VERSION: &str = "reddit-browser-v1";

/// The browser is slow on the production host: a cold Chromium launch alone
/// measured 10.3s, and a subreddit read is two authenticated navigations on
/// top of that. This is background work, so the budget is generous — a read
/// that takes a minute is worth more than one that gives up and leaves the
/// place unobserved.
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Reddit is observed less often than a forum index. Nothing here changes by
/// the hour, and every read costs a browser navigation on a shared session.
const RECOMMENDED_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// How many hot posts to read per sweep. Enough to see what a community is
/// actually talking about; small enough to stay one page.
const POST_SAMPLE: u32 = 25;

/// Genre words that appear in subreddit names, descriptions and post titles.
///
/// Longest-first: "black metal" must match before "metal", or every genre
/// collapses into one. Kept in sync with the Brutalland adapter's list
/// because both feed the same `community_entities` vocabulary — an entity
/// spelled two ways is two entities to the brain.
const GENRE_KEYWORDS: &[(&str, &str)] = &[
    ("blackened death", "Blackened Death"),
    ("progressive metal", "Progressive Metal"),
    ("technical death", "Technical Death Metal"),
    ("melodic death", "Melodic Death Metal"),
    ("black metal", "Black Metal"),
    ("death metal", "Death Metal"),
    ("power metal", "Power Metal"),
    ("heavy metal", "Heavy Metal"),
    ("doom metal", "Doom Metal"),
    ("folk metal", "Folk Metal"),
    ("viking metal", "Viking Metal"),
    ("industrial metal", "Industrial Metal"),
    ("thrash", "Thrash Metal"),
    ("grindcore", "Grindcore"),
    ("goregrind", "Goregrind"),
    ("metalcore", "Metalcore"),
    ("deathcore", "Deathcore"),
    ("progressive", "Progressive Metal"),
    ("doom", "Doom Metal"),
    ("djent", "Djent"),
    ("slam", "Slam"),
];

/// One subreddit observation, as the agents service reports it.
#[derive(Debug, Deserialize)]
struct ObserveResponse {
    subreddit: String,
    title: Option<String>,
    public_description: Option<String>,
    subscribers: Option<i64>,
    active_user_count: Option<i64>,
    #[serde(default)]
    over18: bool,
    #[serde(default)]
    posts: Vec<ObservedPost>,
}

#[derive(Debug, Deserialize)]
struct ObservedPost {
    title: String,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    num_comments: Option<i64>,
    #[serde(default)]
    link_flair_text: Option<String>,
}

pub struct RedditAdapter {
    http_client: reqwest::Client,
    agent_service_url: String,
    agent_service_auth_key: String,
    workspace_id: Uuid,
}

impl RedditAdapter {
    /// Returns `None` when the agents service has no auth key configured.
    ///
    /// Without it every fetch would 401, and a source that fails every sweep
    /// is worse than one that never registers: it burns backoff and fills the
    /// log with an error whose cause is a missing environment variable.
    pub fn new(
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
        workspace_id: Uuid,
    ) -> Option<Self> {
        let agent_service_auth_key = agent_service_auth_key?;
        let http_client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent("crowdrelay-community-intel/1")
            .build()
            .ok()?;
        Some(Self {
            http_client,
            agent_service_url: agent_service_url.trim_end_matches('/').to_owned(),
            agent_service_auth_key,
            workspace_id,
        })
    }
}

#[async_trait::async_trait]
impl SourceAdapter for RedditAdapter {
    fn id(&self) -> &str {
        "reddit"
    }

    fn recommended_interval(&self) -> Duration {
        RECOMMENDED_INTERVAL
    }

    fn rate_limit_policy(&self) -> RateLimitPolicy {
        RateLimitPolicy {
            // One at a time, always. Every read drives the same single browser
            // session; two concurrent navigations fight over one page.
            max_concurrency: 1,
            backoff_base: Duration::from_secs(60),
            backoff_max: Duration::from_secs(30 * 60),
        }
    }

    async fn fetch(&self, place: &AdapterPlace) -> Result<ParsedObservation, AdapterError> {
        let subreddit = subreddit_from_url(&place.url).ok_or_else(|| {
            AdapterError::Parse(format!("no subreddit in place url: {}", place.url))
        })?;

        let token =
            crate::discovery::derive_agent_token(&self.agent_service_auth_key, self.workspace_id);
        let response = self
            .http_client
            .post(format!("{}/reddit/observe", self.agent_service_url))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({ "subreddit": subreddit, "limit": POST_SAMPLE }))
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    AdapterError::Timeout(FETCH_TIMEOUT)
                } else {
                    AdapterError::HttpFetch(error.to_string())
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(AdapterError::HttpStatus {
                status: status.as_u16(),
                url: format!("{}/reddit/observe", self.agent_service_url),
            });
        }

        let observed: ObserveResponse = response
            .json()
            .await
            .map_err(|error| AdapterError::Parse(error.to_string()))?;

        parse_observation(&observed, &place.url)
    }
}

/// Extracts the subreddit name from a place URL.
///
/// Places are stored as `https://reddit.com/r/Metal`, but a trailing slash, a
/// `www.`/`old.` host or a deeper path are all things an operator will type.
/// Returning `None` rather than guessing keeps a malformed URL from becoming
/// an arbitrary authenticated navigation.
fn subreddit_from_url(url: &str) -> Option<String> {
    let after = url.split("/r/").nth(1)?;
    let name = after.split(['/', '?', '#']).next()?.trim();
    if name.is_empty() || name.len() > 21 {
        return None;
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_owned())
}

/// Turns one subreddit read into an observation.
///
/// Fails closed when the community cannot be sized. `subscribers: null` means
/// the read did not work — an unreadable community is not a community of zero,
/// and recording it as one would teach the brain that a dead subreddit is a
/// real observation. Same rule the growth metric sync follows.
fn parse_observation(
    observed: &ObserveResponse,
    source_url: &str,
) -> Result<ParsedObservation, AdapterError> {
    let Some(subscribers) = observed.subscribers else {
        return Err(AdapterError::StructureChanged);
    };

    let post_count = observed.posts.len() as i64;
    let total_score: i64 = observed.posts.iter().filter_map(|post| post.score).sum();
    let total_comments: i64 = observed
        .posts
        .iter()
        .filter_map(|post| post.num_comments)
        .sum();

    // Quality falls when the sample is thin. A subreddit that returned two
    // posts is a weaker observation than one that returned twenty-five, and
    // the brain should be able to tell them apart rather than treating both
    // as equally solid evidence.
    let quality = match post_count {
        0 => 40,
        1..=4 => 60,
        5..=14 => 80,
        _ => 95,
    };

    let metrics = serde_json::json!({
        // Named "community_size", not "followers": this is how many people are
        // in the room, not how many follow the band. The distinction is the
        // whole reason the audience series excludes it.
        "community_size": subscribers,
        "active_user_count": observed.active_user_count,
        "sampled_posts": post_count,
        "sampled_score_total": total_score,
        "sampled_comment_total": total_comments,
        "over18": observed.over18,
        "title": observed.title,
    });

    let entities = extract_entities(observed);

    Ok(ParsedObservation {
        source: "reddit".to_owned(),
        source_url: source_url.to_owned(),
        collector_version: COLLECTOR_VERSION.to_owned(),
        raw_activity_metrics: metrics,
        observation_quality: quality,
        entities,
    })
}

/// Pulls genre entities out of the subreddit name, description and post text.
///
/// Strength is mention count, normalised the same way every other adapter
/// does it, so a genre seen once in a description does not outrank one named
/// in fifteen post titles.
fn extract_entities(observed: &ObserveResponse) -> Vec<ParsedEntity> {
    let mut haystack = String::new();
    haystack.push_str(&observed.subreddit.to_lowercase());
    haystack.push(' ');
    if let Some(title) = &observed.title {
        haystack.push_str(&title.to_lowercase());
        haystack.push(' ');
    }
    if let Some(description) = &observed.public_description {
        haystack.push_str(&description.to_lowercase());
        haystack.push(' ');
    }
    for post in &observed.posts {
        haystack.push_str(&post.title.to_lowercase());
        haystack.push(' ');
        if let Some(flair) = &post.link_flair_text {
            haystack.push_str(&flair.to_lowercase());
            haystack.push(' ');
        }
    }

    let mut entities: Vec<ParsedEntity> = Vec::new();
    // Consume matches longest-first so "black metal" is not also counted as
    // "metal"; each match is blanked out of the haystack before the next
    // keyword runs.
    let mut remaining = haystack;
    for (needle, label) in GENRE_KEYWORDS {
        let mut count = 0u32;
        while let Some(at) = remaining.find(needle) {
            count += 1;
            remaining.replace_range(at..at + needle.len(), &" ".repeat(needle.len()));
        }
        if count > 0 {
            entities.push(ParsedEntity {
                entity_type: EntityType::Genre,
                entity_ref: (*label).to_owned(),
                strength: normalize_strength(count),
            });
        }
    }
    entities
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(subscribers: Option<i64>, posts: Vec<ObservedPost>) -> ObserveResponse {
        ObserveResponse {
            subreddit: "Metal".to_owned(),
            title: Some("Metal".to_owned()),
            public_description: Some("For all things heavy metal".to_owned()),
            subscribers,
            active_user_count: Some(1_200),
            over18: false,
            posts,
        }
    }

    fn post(title: &str) -> ObservedPost {
        ObservedPost {
            title: title.to_owned(),
            score: Some(10),
            num_comments: Some(3),
            link_flair_text: None,
        }
    }

    #[test]
    fn a_subreddit_url_yields_its_name() {
        assert_eq!(
            subreddit_from_url("https://reddit.com/r/Metal").as_deref(),
            Some("Metal"),
        );
        assert_eq!(
            subreddit_from_url("https://www.reddit.com/r/progmetal/").as_deref(),
            Some("progmetal"),
        );
        assert_eq!(
            subreddit_from_url("https://old.reddit.com/r/doom/hot.json?limit=5").as_deref(),
            Some("doom"),
        );
    }

    #[test]
    fn a_url_that_names_no_subreddit_is_refused() {
        // Guessing here would turn a malformed place row into an arbitrary
        // authenticated navigation through the band's own logged-in session.
        assert_eq!(subreddit_from_url("https://reddit.com/"), None);
        assert_eq!(subreddit_from_url("https://example.com/r/"), None);
        assert_eq!(subreddit_from_url("https://reddit.com/r/../../etc"), None);
        assert_eq!(
            subreddit_from_url("https://reddit.com/r/waaaaaaaaaaaaaaaaaaaaaaay_too_long"),
            None,
        );
    }

    #[test]
    fn an_unreadable_community_is_not_a_community_of_zero() {
        // `subscribers: null` means the read failed. Recording it as 0 would
        // teach the brain that a thriving subreddit is empty, and the brain
        // has no way to tell that apart from a real collapse.
        let result = parse_observation(&observation(None, vec![post("anything")]), "u");
        assert!(matches!(result, Err(AdapterError::StructureChanged)));
    }

    #[test]
    fn community_size_is_reported_as_reach_not_followers() {
        // The key name is load-bearing. Counting r/Metal's members as the
        // band's audience is what once inflated the number 1,400×.
        let parsed = parse_observation(&observation(Some(3_972_686), vec![post("x")]), "u")
            .expect("should parse");
        let metrics = parsed.raw_activity_metrics;
        assert_eq!(metrics["community_size"], 3_972_686);
        assert!(
            metrics.get("followers").is_none(),
            "community size must never be published under a follower key",
        );
    }

    #[test]
    fn a_thin_sample_scores_lower_than_a_full_one() {
        let thin = parse_observation(&observation(Some(100), vec![post("a")]), "u").unwrap();
        let full = parse_observation(
            &observation(Some(100), (0..20).map(|_| post("a")).collect()),
            "u",
        )
        .unwrap();
        assert!(
            thin.observation_quality < full.observation_quality,
            "two posts should not be as trustworthy as twenty",
        );
    }

    #[test]
    fn a_longer_genre_wins_over_the_shorter_one_inside_it() {
        // "black metal" contains "metal"; counting both makes one mention look
        // like two different genres and doubles the evidence for neither.
        let observed = observation(
            Some(100),
            vec![post("New black metal record"), post("doom metal rules")],
        );
        let parsed = parse_observation(&observed, "u").unwrap();
        let refs: Vec<&str> = parsed
            .entities
            .iter()
            .map(|entity| entity.entity_ref.as_str())
            .collect();
        assert!(refs.contains(&"Black Metal"), "got {refs:?}");
        assert!(refs.contains(&"Doom Metal"), "got {refs:?}");
    }

    #[test]
    fn genre_strength_follows_mention_count() {
        // By ref, not by index: the fixture's description says "heavy metal",
        // so entity 0 is Heavy Metal in both cases and indexing would compare
        // that against itself and pass no matter what strength did.
        let strength_of = |observed: &ObserveResponse, label: &str| {
            parse_observation(observed, "u")
                .unwrap()
                .entities
                .into_iter()
                .find(|entity| entity.entity_ref == label)
                .unwrap_or_else(|| panic!("{label} should have been extracted"))
                .strength
        };
        let many = observation(Some(100), (0..8).map(|_| post("thrash night")).collect());
        let once = observation(Some(100), vec![post("thrash night")]);
        let many_strength = strength_of(&many, "Thrash Metal");
        let once_strength = strength_of(&once, "Thrash Metal");
        assert!(
            many_strength > once_strength,
            "eight mentions ({many_strength}) should outweigh one ({once_strength})",
        );
    }

    #[test]
    fn no_auth_key_means_no_adapter() {
        // A source that 401s on every sweep is worse than one that never
        // registers: it burns backoff and logs an error whose real cause is an
        // unset environment variable.
        assert!(
            RedditAdapter::new("http://agent-service:8095".to_owned(), None, Uuid::nil()).is_none(),
        );
    }
}
