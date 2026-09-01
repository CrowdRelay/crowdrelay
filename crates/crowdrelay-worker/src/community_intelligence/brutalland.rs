//! Brutalland source adapter — phpBB forum at https://brutalland.pl/.
//!
//! The simplest of the three planned adapters (Brutalland, Metal Archives,
//! Orbis Metallum). The index page lists sections with post counts and
//! shows online users count. Each section URL is predictable
//! (`viewforum.php?f={id}`).
//!
//! The parser must fail closed when markers disappear. HTML scrapers have
//! no stability guarantee — unknown ≠ zero. If the page structure changes,
//! the adapter errors rather than recording misleading zeros (same rule as
//! growth metric sync).

use std::time::Duration;

use crowdrelay_domain::community_intelligence::{
    ENTITY_STRENGTH_MAX, EntityType, normalize_strength,
};

use super::adapter::{
    AdapterError, AdapterPlace, ParsedEntity, ParsedObservation, RateLimitPolicy, SourceAdapter,
};

const COLLECTOR_VERSION: &str = "brutalland-v1";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const RECOMMENDED_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6h

/// Known genre keywords that appear in Brutalland section names.
/// Used to extract genre entities from the index page.
const GENRE_KEYWORDS: &[(&str, &str)] = &[
    ("black metal", "Black Metal"),
    ("death metal", "Death Metal"),
    ("thrash", "Thrash Metal"),
    ("doom", "Doom Metal"),
    ("power metal", "Power Metal"),
    ("heavy metal", "Heavy Metal"),
    ("grindcore", "Grindcore"),
    ("progressive", "Progressive Metal"),
    ("folk", "Folk Metal"),
    ("viking", "Viking Metal"),
    ("industrial", "Industrial Metal"),
    ("blackened", "Blackened Death"),
    ("goregrind", "Goregrind"),
    ("slam", "Slam"),
    ("djent", "Djent"),
    ("metalcore", "Metalcore"),
    ("deathcore", "Deathcore"),
];

pub struct BrutallandAdapter {
    http_client: reqwest::Client,
}

impl BrutallandAdapter {
    pub fn new() -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent("crowdrelay-community-intel/1")
            .build()
            .unwrap_or_default();
        Self { http_client }
    }
}

impl Default for BrutallandAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SourceAdapter for BrutallandAdapter {
    fn id(&self) -> &str {
        "brutalland"
    }

    fn recommended_interval(&self) -> Duration {
        RECOMMENDED_INTERVAL
    }

    fn rate_limit_policy(&self) -> RateLimitPolicy {
        RateLimitPolicy {
            max_concurrency: 1,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(10 * 60),
        }
    }

    async fn fetch(&self, place: &AdapterPlace) -> Result<ParsedObservation, AdapterError> {
        let url = &place.url;
        let response = self
            .http_client
            .get(url)
            .send()
            .await
            .map_err(|e| AdapterError::HttpFetch(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(AdapterError::HttpStatus {
                status: status.as_u16(),
                url: url.to_owned(),
            });
        }

        let html = response
            .text()
            .await
            .map_err(|e| AdapterError::HttpFetch(e.to_string()))?;

        let parsed = parse_brutalland_index(&html, url)?;

        Ok(ParsedObservation {
            source: "brutalland".to_owned(),
            source_url: url.to_owned(),
            collector_version: COLLECTOR_VERSION.to_owned(),
            raw_activity_metrics: parsed.metrics,
            observation_quality: parsed.quality,
            entities: parsed.entities,
        })
    }
}

/// Parsed result from the Brutalland index page.
struct ParsedIndex {
    metrics: serde_json::Value,
    quality: i32,
    entities: Vec<ParsedEntity>,
}

/// A parsed forum section.
struct ParsedSection {
    name: String,
    /// `None` when the section was found but its post count was not.
    /// Recording 0 there would be a measurement, and a wrong one: the value
    /// lands in an append-only series where a fabricated zero reads as the
    /// section being wiped.
    post_count: Option<i64>,
}

/// Parses the Brutalland phpBB index page.
///
/// phpBB index pages have:
/// - A "Who is online" section with online users count
/// - Forum sections with post counts (e.g. "33504 Posty")
///
/// The parser extracts:
/// - `online_users`: count of currently online users
/// - `sections`: list of {name, post_count} for each forum section
/// - Genre entities from section names
///
/// If the page structure changes, the parser fails closed
/// (returns `AdapterError::StructureChanged`).
fn parse_brutalland_index(html: &str, _url: &str) -> Result<ParsedIndex, AdapterError> {
    let online_users = extract_online_users(html);
    let sections = extract_sections(html);

    // Nothing recognisable on the page: fail closed rather than record zeros.
    if sections.is_empty() && online_users.is_none() {
        return Err(AdapterError::StructureChanged);
    }

    let entities = extract_genre_entities(&sections);

    // Quality is the share of expected readings actually extracted, so partial
    // breakage is visible instead of silent. The previous version returned
    // 10000 whenever *anything* parsed, which meant the most likely failure —
    // phpBB keeps the section links but changes the post-count markup — was
    // recorded as a full-confidence observation of a forum where every section
    // had zero posts.
    let expected = sections.len() + 1; // every section, plus the online count
    let extracted = sections
        .iter()
        .filter(|section| section.post_count.is_some())
        .count()
        + usize::from(online_users.is_some());
    let quality = i32::try_from(extracted.saturating_mul(10_000) / expected.max(1))
        .unwrap_or(10_000)
        .clamp(0, 10_000);
    // Sections present but not one post count readable: the page still parses
    // as HTML but carries no measurement worth storing.
    if quality == 0 {
        return Err(AdapterError::StructureChanged);
    }

    let sections_json: Vec<serde_json::Value> = sections
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "post_count": s.post_count,
                // null when unread — see ParsedSection::post_count.
            })
        })
        .collect();

    let metrics = serde_json::json!({
        "online_users": online_users,
        "sections": sections_json,
        "total_sections": sections.len(),
    });

    Ok(ParsedIndex {
        metrics,
        quality,
        entities,
    })
}

/// Extracts the online users count from the page.
/// Looks for common phpBB patterns in both Polish and English.
fn extract_online_users(html: &str) -> Option<i64> {
    // `użytkowni` is the stem shared by every Polish inflection phpBB renders
    // — użytkownik, użytkownicy, użytkowników. Matching only the singular
    // missed the plural the index page actually uses ("Zarejestrowani
    // użytkownicy: 59"), so this count was never read from the live forum.
    extract_number_after_keyword(html, &["użytkowni", "registered users", "online users"])
}

/// Extracts forum sections with post counts from the index page.
/// phpBB index pages have links to viewforum.php with post counts nearby.
fn extract_sections(html: &str) -> Vec<ParsedSection> {
    let mut sections = Vec::new();
    let mut search_pos = 0;

    while let Some(rel_start) = html.get(search_pos..).and_then(|s| s.find("viewforum.php")) {
        let abs_start = search_pos + rel_start;

        // Find the <a href="..."> tag that contains this viewforum link.
        // Walk backwards to find the opening <a tag.
        let before = html.get(..abs_start).unwrap_or("");
        let a_tag_start = before.rfind("<a ").or_else(|| before.rfind("<a\n"));

        if let Some(tag_start) = a_tag_start {
            // Find the closing </a> tag.
            let after_tag = html.get(abs_start..).unwrap_or("");
            if let Some(close_tag) = after_tag.find('>') {
                let content_start = abs_start + close_tag + 1;
                let content_area = html.get(content_start..).unwrap_or("");
                if let Some(end_a) = content_area.find("</a>") {
                    let link_text = content_area
                        .get(..end_a)
                        .unwrap_or("")
                        .trim()
                        .trim_matches('"')
                        .to_owned();

                    if !link_text.is_empty() && link_text.len() <= 200 {
                        // Search the surrounding area for post counts.
                        // Look in a window around the link.
                        let window_start = tag_start.saturating_sub(200);
                        let window_end = (content_start + end_a + 200).min(html.len());
                        let window = html.get(window_start..window_end).unwrap_or("");

                        let post_count = extract_post_count(window);
                        sections.push(ParsedSection {
                            name: strip_html_tags(&link_text),
                            post_count,
                        });
                    }
                }
            }
        }

        search_pos = abs_start + "viewforum.php".len();
    }

    sections
}

/// Strips basic HTML tags from a string.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(c);
        }
    }
    result.trim().to_owned()
}

/// Extracts a number that appears after one of the given keywords.
fn extract_number_after_keyword(text: &str, keywords: &[&str]) -> Option<i64> {
    let lower = text.to_lowercase();
    for keyword in keywords {
        if let Some(pos) = lower.find(keyword) {
            // Search the next 50 characters for a number.
            let end_bound = pos.saturating_add(50).min(text.len());
            let rest = text.get(pos..end_bound).unwrap_or("");
            let rest_lower = rest.to_lowercase();
            let kw_end = rest_lower.len().min(keyword.len() + 30);
            let after_kw = rest_lower.get(keyword.len()..kw_end).unwrap_or("");
            // Find first digit run.
            let mut start = None;
            let mut end = 0;
            for (i, c) in after_kw.char_indices() {
                if c.is_ascii_digit() {
                    if start.is_none() {
                        start = Some(i);
                    }
                    end = i + c.len_utf8();
                } else if start.is_some() {
                    break;
                }
            }
            if let Some(s) = start {
                let num_str = after_kw.get(s..end).unwrap_or("");
                if let Ok(n) = num_str.parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extracts a post count from text containing "Posty" (Polish) or "Posts" (English).
fn extract_post_count(text: &str) -> Option<i64> {
    let lower = text.to_lowercase();
    for keyword in &["posty", "posts"] {
        if let Some(pos) = lower.find(keyword) {
            // Search backwards from the keyword for a number.
            let before = text.get(..pos).unwrap_or("");
            // Find the last number before the keyword.
            let mut num_end = None;
            let mut num_start = 0;
            for (i, c) in before.char_indices().rev() {
                if c.is_ascii_digit() {
                    if num_end.is_none() {
                        num_end = Some(i + c.len_utf8());
                    }
                    num_start = i;
                } else if num_end.is_some() {
                    break;
                }
            }
            if let Some(ne) = num_end {
                let num_str = before.get(num_start..ne).unwrap_or("");
                if let Ok(n) = num_str.parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extracts genre entities from section names.
fn extract_genre_entities(sections: &[ParsedSection]) -> Vec<ParsedEntity> {
    let mut entities = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for section in sections {
        let lower = section.name.to_lowercase();
        for (keyword, label) in GENRE_KEYWORDS {
            if lower.contains(keyword) && seen.insert(*label) {
                // Strength is observed prominence: more posts, more of the
                // forum's attention. An unread post count falls back to the
                // floor rather than to zero — the section exists, we simply
                // could not size it.
                let strength = match section.post_count {
                    Some(count) if count > 0 => {
                        normalize_strength(u32::try_from(count).unwrap_or(u32::MAX))
                    }
                    _ => ENTITY_STRENGTH_MAX / 10,
                };
                entities.push(ParsedEntity {
                    entity_type: EntityType::Genre,
                    entity_ref: (*label).to_owned(),
                    strength,
                });
            }
        }
    }

    entities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_post_count_finds_polish_pattern() {
        let text = "Black Metal 33504 Posty";
        assert_eq!(extract_post_count(text), Some(33504));
    }

    #[test]
    fn extract_post_count_finds_english_pattern() {
        let text = "Death Metal 12345 Posts";
        assert_eq!(extract_post_count(text), Some(12345));
    }

    #[test]
    fn extract_post_count_returns_none_when_no_number() {
        let text = "No posts here";
        assert_eq!(extract_post_count(text), None);
    }

    #[test]
    fn extract_genre_entities_finds_known_genres() {
        let sections = vec![
            ParsedSection {
                name: "Black Metal".to_owned(),
                post_count: Some(33504),
            },
            ParsedSection {
                name: "Death Metal".to_owned(),
                post_count: Some(12000),
            },
            ParsedSection {
                name: "General Discussion".to_owned(),
                post_count: Some(5000),
            },
        ];
        let entities = extract_genre_entities(&sections);
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].entity_ref, "Black Metal");
        assert_eq!(entities[1].entity_ref, "Death Metal");
    }

    #[test]
    fn extract_genre_entities_deduplicates() {
        let sections = vec![
            ParsedSection {
                name: "Black Metal Discussion".to_owned(),
                post_count: Some(100),
            },
            ParsedSection {
                name: "Black Metal Reviews".to_owned(),
                post_count: Some(200),
            },
        ];
        let entities = extract_genre_entities(&sections);
        assert_eq!(entities.len(), 1); // Only one "Black Metal" entity
    }

    #[test]
    fn a_section_whose_post_count_is_unreadable_is_null_not_zero() {
        // The realistic breakage: phpBB keeps the forum links and changes the
        // post-count markup. Recording 0 would land a fabricated level in an
        // append-only series and read as the section being wiped.
        let html = r#"<html><body>
            <div class="forabg">
                <a href="viewforum.php?f=1">Black Metal</a>
                <dd>no count here any more</dd>
            </div>
            <div class="stat-block">Zarejestrowani użytkownicy: 59</div>
        </body></html>"#;
        let parsed =
            parse_brutalland_index(html, "https://brutalland.pl/").expect("online count survives");
        let sections = parsed.metrics["sections"].as_array().expect("sections");
        assert!(
            sections.iter().all(|s| s["post_count"].is_null()),
            "an unreadable post count must be null, never 0"
        );
        // One of two readings survived, and the observation says so instead of
        // claiming full confidence.
        assert!(
            parsed.quality < 10_000,
            "partial extraction must lower observation quality, got {}",
            parsed.quality
        );
    }

    #[test]
    fn a_page_with_links_but_no_readable_numbers_fails_closed() {
        let html = r#"<html><body>
            <div class="forabg"><a href="viewforum.php?f=1">Black Metal</a></div>
        </body></html>"#;
        assert!(
            matches!(
                parse_brutalland_index(html, "https://brutalland.pl/"),
                Err(AdapterError::StructureChanged)
            ),
            "a page yielding no measurement at all must not be recorded"
        );
    }

    #[test]
    fn parse_brutalland_index_fails_closed_on_empty_page() {
        let html = "<html><body></body></html>";
        let result = parse_brutalland_index(html, "https://brutalland.pl/");
        assert!(result.is_err());
        match result {
            Err(AdapterError::StructureChanged) => {}
            _ => panic!("expected StructureChanged error"),
        }
    }

    #[test]
    fn parse_brutalland_index_extracts_sections() {
        let html = r#"<html><body>
            <div class="forabg">
                <a href="viewforum.php?f=1">Black Metal</a>
                <dd>33504 Posty</dd>
                <a href="viewforum.php?f=2">Death Metal</a>
                <dd>12000 Posty</dd>
            </div>
            <div class="stat-block">Zarejestrowani użytkownicy: 59</div>
        </body></html>"#;
        let result = parse_brutalland_index(html, "https://brutalland.pl/");
        assert!(result.is_ok());
        let parsed = result.unwrap();
        let metrics = parsed.metrics.as_object().unwrap();
        assert!(metrics.contains_key("sections"));
        assert!(metrics.contains_key("online_users"));
        let sections = metrics["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 2, "both forum links must be found");
        // Every reading present in the fixture must be read: two post counts
        // and the online-user count. A quality below 10000 here means the
        // parser silently dropped one of them.
        for section in sections {
            assert!(
                !section["post_count"].is_null(),
                "post count not read for {}",
                section["name"]
            );
        }
        assert!(!metrics["online_users"].is_null());
        assert_eq!(parsed.quality, 10_000);
    }

    #[test]
    fn strip_html_tags_removes_tags() {
        assert_eq!(strip_html_tags("<b>Bold</b>"), "Bold");
        assert_eq!(strip_html_tags("No tags"), "No tags");
        assert_eq!(strip_html_tags("<a href='x'>Link</a>"), "Link");
    }
}
