//! Peer observation sweep (sprint 3.5b.1).
//!
//! Deterministic, no LLM: confirmed peers in `viryaos_peers` carry `handles`
//! like `{"youtube": "@handle" | "UC...", "rss": "https://..."}`. Each sweep
//! resolves the handle to a feed, fetches it, and records one dated fact per
//! entry — `viryaos_peer_observations` rows where `fact` is the entry title
//! and `observed_at` is the entry's own publish date, so the dedup index
//! makes repeated sweeps idempotent even while view counts grow underneath.
//!
//! Anything that cannot be fetched or parsed is dropped for this sweep and
//! retried on the next interval — an observation is only as good as its
//! source, and a failed fetch must never invent one.

use std::time::Duration;

use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, timeout};

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::content_engine::{
    ContentEngineError, NewPeerObservation, PostgresContentEngineRepository,
};

/// Bound on how much of a peer page/feed we will ever read. Channel pages
/// are ~1 MB and feeds ~100 KB; anything larger is not a feed we want.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// A single sweep must finish inside this window or the ticker skips it.
/// Sized from the fetch budget: MAX_PEERS handles × MAX_FETCHES_PER_HANDLE
/// spread over FETCH_CONCURRENCY lanes at the per-request timeout (~15s by
/// default, 60s max) — the worst case stays under ten minutes.
const SWEEP_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// In-flight handle fetches. Sequential sweeps let a few tarpitted hosts
/// starve the tail of the watch list forever; a small window bounds the
/// damage while staying polite to the sources.
const FETCH_CONCURRENCY: usize = 8;
/// Entries per handle per sweep — feeds rarely carry more than ~15 anyway.
const MAX_ENTRIES_PER_HANDLE: usize = 25;
/// Peers per sweep. A band's watch list is tens, not thousands.
const MAX_PEERS_PER_SWEEP: usize = 64;
/// Entries older than this are history, not a trend input.
const MAX_OBSERVATION_AGE_DAYS: i64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum PeerObservationError {
    #[error("peer observation repository operation failed")]
    Repository(#[from] ContentEngineError),
    #[error("peer observation HTTP operation failed")]
    Network(#[from] reqwest::Error),
    /// A peer page/feed over the read cap is dropped rather than truncated —
    /// half an XML document parses as nothing anyway.
    #[error("response body exceeds read cap")]
    Oversized,
    /// Handle values are operator data; the obvious self-targeting hosts
    /// are not feeds worth fetching.
    #[error("refusing to fetch a local host")]
    LocalHost,
}

/// One dated entry parsed out of a feed.
#[derive(Clone, Debug)]
pub struct FeedEntry {
    pub title: String,
    pub published: Date,
    pub url: Option<String>,
    pub views: Option<u64>,
}

pub struct PeerObservationWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    client: reqwest::Client,
    poll_interval: Duration,
}

impl PeerObservationWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(operation_timeout)
            .user_agent("crowdrelay-peer-observation/1.0")
            .build()?;
        Ok(Self::with_client(pool, workspace_id, poll_interval, client))
    }

    /// Tests inject their own client to resolve a fake hostname at the
    /// canned feed — the local-target guard stays live even in tests.
    pub fn with_client(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        client: reqwest::Client,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            client,
            poll_interval,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    match timeout(SWEEP_WATCHDOG_TIMEOUT, self.sweep()).await {
                        Ok(Ok(recorded)) if recorded > 0 => {
                            tracing::info!(recorded, "peer observation sweep recorded new facts");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = %error, "peer observation sweep failed");
                        }
                        Err(_) => tracing::warn!("peer observation sweep timed out"),
                    }
                }
            }
        }
    }

    /// One pass over the confirmed peers of this workspace. `pub` so the
    /// postgres integration test can drive a single sweep against a canned
    /// feed instead of waiting out the ticker.
    pub async fn sweep(&self) -> Result<usize, PeerObservationError> {
        let repository = PostgresContentEngineRepository::new(self.pool.clone());
        let peers = repository
            .list_peers(
                self.workspace_id,
                Some(crowdrelay_domain::content_engine::PeerStatus::Confirmed),
            )
            .await?;
        // Flatten peers into (peer, platform, handle) work items, fetch them
        // on a bounded concurrency window, then insert sequentially — the
        // dedup index makes the write order irrelevant.
        struct Work {
            peer_id: crowdrelay_domain::PeerId,
            peer_name: String,
            platform: String,
            handle: String,
        }
        let mut work = Vec::new();
        for peer in peers.into_iter().take(MAX_PEERS_PER_SWEEP) {
            for (platform, handle) in peer_handles(&peer.handles) {
                work.push(Work {
                    peer_id: peer.id,
                    peer_name: peer.name.clone(),
                    platform,
                    handle,
                });
            }
        }
        use futures_util::StreamExt as _;
        let fetched: Vec<_> = futures_util::stream::iter(work.into_iter().map(|item| async move {
            let entries = match self.fetch_platform(&item.platform, &item.handle).await {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::debug!(
                        peer = %item.peer_name,
                        platform = %item.platform,
                        handle = %item.handle,
                        error = %error,
                        "peer observation fetch dropped"
                    );
                    Vec::new()
                }
            };
            (
                item.peer_id,
                item.peer_name,
                item.platform,
                item.handle,
                entries,
            )
        }))
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect()
        .await;
        let mut recorded = 0_usize;
        let today = OffsetDateTime::now_utc().date();
        for (peer_id, peer_name, platform, _handle, entries) in fetched {
            for entry in entries.into_iter().take(MAX_ENTRIES_PER_HANDLE) {
                // An undated entry was already dropped by the parser; a
                // future-dated one is a fact that has not happened yet, and
                // an ancient one is history, not a trend input.
                let age = today - entry.published;
                if age > time::Duration::days(MAX_OBSERVATION_AGE_DAYS)
                    || age < time::Duration::ZERO
                {
                    continue;
                }
                let observation = NewPeerObservation {
                    peer_id,
                    observed_at: entry.published,
                    platform: platform.clone(),
                    kind: observation_kind(&platform).to_owned(),
                    fact: entry.title,
                    url: entry.url,
                    metrics: match entry.views {
                        Some(views) => serde_json::json!({ "views": views }),
                        None => serde_json::json!({}),
                    },
                };
                match repository
                    .record_observation(self.workspace_id, &observation)
                    .await
                {
                    Ok(Some(_)) => recorded += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            peer = %peer_name,
                            error = %error,
                            "peer observation insert failed"
                        );
                    }
                }
            }
        }
        Ok(recorded)
    }

    /// Fetches one platform handle and returns the dated entries found.
    /// Unsupported platforms return an empty list rather than an error —
    /// the handles map is operator data and may name platforms this build
    /// does not read yet.
    async fn fetch_platform(
        &self,
        platform: &str,
        handle: &str,
    ) -> Result<Vec<FeedEntry>, PeerObservationError> {
        match platform {
            "youtube" => self.fetch_youtube(handle).await,
            "rss" => {
                let body = self.fetch_text(handle).await?;
                Ok(parse_feed(&body))
            }
            _ => Ok(Vec::new()),
        }
    }

    /// YouTube exposes a public Atom feed per channel. The operator may
    /// store a raw `UC...` channel id, an `@handle`, or a channel URL — all
    /// three resolve to the same feed URL.
    async fn fetch_youtube(&self, handle: &str) -> Result<Vec<FeedEntry>, PeerObservationError> {
        let channel_id = if is_channel_id(handle) {
            handle.to_owned()
        } else {
            let page_url = if handle.starts_with('@') {
                format!("https://www.youtube.com/{handle}")
            } else if handle.starts_with("http") {
                handle.to_owned()
            } else {
                format!("https://www.youtube.com/@{handle}")
            };
            let page = self.fetch_text(&page_url).await?;
            match extract_channel_id(&page) {
                Some(id) => id,
                None => {
                    tracing::debug!(handle, "no channelId in youtube page; peer dropped");
                    return Ok(Vec::new());
                }
            }
        };
        let feed_url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
        let body = self.fetch_text(&feed_url).await?;
        Ok(parse_feed(&body))
    }

    async fn fetch_text(&self, url: &str) -> Result<String, PeerObservationError> {
        if is_local_host(url) {
            return Err(PeerObservationError::LocalHost);
        }
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            return Err(PeerObservationError::Oversized);
        }
        // Chunked read bounded by the cap: `bytes()` would materialize the
        // whole body first, so a huge endpoint could force the allocation
        // the cap is meant to prevent.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(PeerObservationError::Oversized);
            }
            body.extend_from_slice(&chunk);
        }
        // Feeds and channel pages are XML/HTML; lossy keeps scanning alive
        // over stray bytes instead of failing the whole peer.
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// Handles are operator-entered data; loopback, link-local and private
/// targets are not feeds worth fetching (metadata endpoints included).
fn is_local_host(url: &str) -> bool {
    use std::net::IpAddr;
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_unspecified() || v4.is_link_local() || v4.is_private()
        }
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || v6.is_unspecified(),
        Err(_) => false,
    }
}

/// `(platform, handle)` pairs out of the peer's handles object, with the
/// platform key normalized — `{"YouTube": ...}` must not silently become a
/// peer that is never observed. Non-string and empty values are ignored.
fn peer_handles(handles: &serde_json::Value) -> Vec<(String, String)> {
    handles
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(platform, value)| {
                    value.as_str().and_then(|handle| {
                        let platform = platform.trim().to_lowercase();
                        let handle = handle.trim();
                        (!platform.is_empty() && !handle.is_empty())
                            .then(|| (platform, handle.to_owned()))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The observation `kind` a platform's entries get. Kept coarse — classing
/// an entry as release/post/teaser is the trend detector's job (3.5b.3),
/// not the sweep's.
fn observation_kind(platform: &str) -> &'static str {
    match platform {
        "youtube" => "video",
        _ => "post",
    }
}

/// `UC` + 22 word chars is the fixed channel-id shape.
fn is_channel_id(value: &str) -> bool {
    value.len() == 24
        && value.starts_with("UC")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Pulls the channel id out of a channel page. Bare `"channelId"` is NOT
/// used: a page embeds a half-dozen of them for recommended/featured
/// channels, and the first is usually not the page owner's. The
/// authoritative markers are `externalId` (channel metadata), the canonical
/// link, and the `itemprop=identifier` meta — all three name the owner.
pub fn extract_channel_id(page: &str) -> Option<String> {
    for key in [
        "\"externalId\":\"",
        "rel=\"canonical\" href=\"https://www.youtube.com/channel/",
        "itemprop=\"identifier\" content=\"",
    ] {
        // Every occurrence is tried, not just the first: embedded JSON can
        // hold several, and the owner's marker is whichever holds a real id.
        for (start, _) in page.match_indices(key) {
            let Some(rest) = page.get(start + key.len()..) else {
                continue;
            };
            let end = rest.find('"').unwrap_or(rest.len());
            if let Some(candidate) = rest.get(..end)
                && is_channel_id(candidate)
            {
                return Some(candidate.to_owned());
            }
        }
    }
    None
}

/// Parses an Atom or RSS feed into dated entries. Entries without a
/// publish date are dropped here — an undated "fact" cannot anchor a trend.
pub fn parse_feed(body: &str) -> Vec<FeedEntry> {
    let Ok(document) = roxmltree::Document::parse(body) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for node in document.descendants() {
        if !node.is_element() {
            continue;
        }
        let tag = node.tag_name().name();
        if tag != "entry" && tag != "item" {
            continue;
        }
        let mut title = None;
        let mut url = None;
        // `published`/`pubDate` is the fact's date; `updated`/`dc:date` is
        // only a fallback — an entry edit must not shift the dedup key.
        let mut published = None;
        let mut updated = None;
        let mut views = None;
        for child in node.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                "title" => title = child.text().map(str::trim).map(str::to_owned),
                "link" => {
                    // Atom: href attribute (rel="alternate" or absent — an
                    // enclosure/self link is not the entry's public page);
                    // RSS: element text.
                    let rel = child.attribute("rel");
                    if url.is_none() && rel.is_none_or(|r| r == "alternate") {
                        url = child
                            .attribute("href")
                            .map(str::to_owned)
                            .or_else(|| child.text().map(str::trim).map(str::to_owned));
                    }
                }
                "published" | "pubDate" => {
                    if published.is_none() {
                        published = child.text().and_then(parse_feed_date);
                    }
                }
                "updated" | "date" => {
                    if updated.is_none() {
                        updated = child.text().and_then(parse_feed_date);
                    }
                }
                "statistics" => {
                    views = child.attribute("views").and_then(|v| v.parse::<u64>().ok());
                }
                "community" | "group" => {
                    // media:community/media:group wraps media:statistics.
                    for grandchild in child.descendants().filter(|n| n.is_element()) {
                        if grandchild.tag_name().name() == "statistics" && views.is_none() {
                            views = grandchild
                                .attribute("views")
                                .and_then(|v| v.parse::<u64>().ok());
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some(title), Some(date)) = (title, published.or(updated))
            && !title.is_empty()
        {
            entries.push(FeedEntry {
                title,
                published: date,
                url,
                views,
            });
        }
    }
    entries
}

/// Feed dates are RFC3339 (Atom) or RFC2822 (RSS).
fn parse_feed_date(text: &str) -> Option<Date> {
    let trimmed = text.trim();
    if let Ok(t) = OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc3339) {
        return Some(t.date());
    }
    if let Ok(t) = OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc2822) {
        return Some(t.date());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_atom_feed_yields_dated_entries() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015"
      xmlns:media="http://search.yahoo.com/mrss/"
      xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <yt:videoId>abc123def45</yt:videoId>
    <title>New single out now</title>
    <link rel="alternate" href="https://www.youtube.com/watch?v=abc123def45"/>
    <published>2026-09-10T18:00:00+00:00</published>
    <updated>2026-09-11T02:00:00+00:00</updated>
    <media:group>
      <media:community>
        <media:statistics views="1200000"/>
      </media:community>
    </media:group>
  </entry>
</feed>"#;
        let entries = parse_feed(feed);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "New single out now");
        assert_eq!(entries[0].views, Some(1_200_000));
        assert_eq!(
            entries[0].url.as_deref(),
            Some("https://www.youtube.com/watch?v=abc123def45")
        );
    }

    #[test]
    fn rss_items_parse_pubdate() {
        let feed = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item>
    <title>Tour dates announced</title>
    <link>https://example.test/news/1</link>
    <pubDate>Thu, 10 Sep 2026 18:00:00 GMT</pubDate>
  </item>
</channel></rss>"#;
        let entries = parse_feed(feed);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].published.year(), 2026);
    }

    #[test]
    fn undated_entries_are_dropped() {
        let feed = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>no date</title></entry>
</feed>"#;
        assert!(parse_feed(feed).is_empty());
    }

    #[test]
    fn channel_id_extraction_uses_owner_markers_only() {
        // A bare channelId is somebody else's channel in the page chrome;
        // it must not be mistaken for the owner.
        let chrome_only = r#"..."channelId":"UCabc123_def-456XYZ01234"..."#;
        assert!(extract_channel_id(chrome_only).is_none());
        let external = r#"..."externalId":"UCxyz789-abc01234DEF_567"..."#;
        assert_eq!(
            extract_channel_id(external).as_deref(),
            Some("UCxyz789-abc01234DEF_567")
        );
        let canonical = r#"<link rel="canonical" href="https://www.youtube.com/channel/UCabc123_def-456XYZ01234">"#;
        assert_eq!(
            extract_channel_id(canonical).as_deref(),
            Some("UCabc123_def-456XYZ01234")
        );
        assert!(extract_channel_id("{\"nothing\":1}").is_none());
    }

    #[test]
    fn channel_id_shape_is_strict() {
        assert!(is_channel_id("UCabc123_def-456XYZ01234"));
        assert!(!is_channel_id("@somehandle"));
        assert!(!is_channel_id("UCshort"));
    }

    #[test]
    fn published_wins_over_updated_regardless_of_order() {
        // Atom does not fix element order; an <updated>-first entry must
        // still anchor on its publish date or an edit moves the dedup key.
        let feed = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title>Edited later</title>
    <updated>2026-09-14T00:00:00Z</updated>
    <published>2026-09-01T00:00:00Z</published>
  </entry>
</feed>"#;
        let entries = parse_feed(feed);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].published.day(), 1);
    }

    #[test]
    fn local_hosts_are_not_feeds() {
        assert!(is_local_host("http://localhost:8080/feed.xml"));
        assert!(is_local_host("http://169.254.169.254/latest/meta-data"));
        assert!(is_local_host("http://192.168.1.1/rss"));
        assert!(is_local_host("http://127.0.0.1:5432/"));
        assert!(!is_local_host("https://example.com/feed.xml"));
    }

    #[test]
    fn handles_ignore_non_string_values() {
        let handles = serde_json::json!({"youtube": "@band", "misc": 42, "rss": null});
        let pairs = peer_handles(&handles);
        assert_eq!(pairs, vec![("youtube".to_owned(), "@band".to_owned())]);
    }

    #[test]
    fn handles_normalize_platform_keys() {
        let handles = serde_json::json!({"YouTube ": "@band", "  ": "x", "rss": " "});
        let pairs = peer_handles(&handles);
        assert_eq!(pairs, vec![("youtube".to_owned(), "@band".to_owned())]);
    }
}
