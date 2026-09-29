//! Curator handle sweep.
//!
//! Most tracked Telegram places are broadcast channels: nobody can post but
//! the admin, so the join-and-post machinery has nothing to do with them.
//! Their admins are still a route — the public `t.me` preview publishes the
//! channel's description, its subscriber count, and usually a contact
//! handle. This sweep reads that page for each tracked place, files every
//! published handle as an `outreach_candidates` row (screened by the same
//! `screen_candidate` every other source goes through), and marks a channel
//! `not_a_fit` with the admin contact in its `membership_note` so the
//! posting lanes stop drafting for it.
//!
//! Nothing here sends anything. The queue the operator actually works is
//! `GET /v1/control-plane/content/videos/{source_id}/curator-queue`.

use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, timeout};

use crowdrelay_application::autopilot::{
    AutopilotTargetDiscoveryRepository, IngestOutreachCandidate, OutreachSweepReport,
};
use crowdrelay_application::ports::IdempotencyKey;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::outreach::OutreachTargetKind;
use crowdrelay_domain::target_discovery::{CandidateSource, RouteKind};
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;

/// Places one sweep will read. A tenant's tracked Telegram list is tens.
const MAX_PLACES_PER_SWEEP: usize = 64;
/// A t.me preview page is ~60 KB; anything far larger is not the page we asked for.
const MAX_BODY_BYTES: usize = 512 * 1024;
/// One place fetch may not stall the sweep.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// The whole sweep is bounded so a wedged ticker cannot stack passes.
const SWEEP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Fit is unscored for handle candidates — no genre-overlap helper produces
/// basis points, so they sit exactly at the admission floor and the evidence
/// says so. See `TargetDiscoveryPolicy::default().minimum_fit_basis_points`.
const UNSCORED_FIT_BASIS_POINTS: u16 = 6_000;

/// What one public `t.me` page told us.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TelegramPageInfo {
    /// `og:description`, decoded — the channel's own text.
    pub description: Option<String>,
    /// "N subscribers" — a broadcast channel. Nobody posts but the admin.
    pub subscribers: Option<u64>,
    /// "N members[, M online]" — a real group.
    pub members: Option<u64>,
    /// `@handles` the page publishes, without the `@`, own handle excluded.
    pub handles: Vec<String>,
}

pub struct CuratorHandleWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
    client: reqwest::Client,
}

impl CuratorHandleWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(operation_timeout)
            .user_agent("crowdrelay-curator-handles/1.0")
            .build()?;
        Ok(Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
            client,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    match timeout(SWEEP_TIMEOUT, self.sweep_once()).await {
                        Ok(Ok(report)) => {
                            if report.places_read > 0 {
                                tracing::info!(
                                    places = report.places_read,
                                    candidates = report.candidates,
                                    channels_marked = report.channels_marked,
                                    "curator handle sweep complete",
                                );
                            }
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "curator handle sweep failed");
                        }
                        Err(_) => {
                            tracing::warn!("curator handle sweep timed out");
                        }
                    }
                }
            }
        }
    }

    async fn sweep_once(&self) -> Result<SweepReport, CuratorHandleError> {
        let places = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            r#"
            SELECT id, name, url
            FROM discovery_places
            WHERE workspace_id = $1
              AND place_kind = 'telegram'
              AND status = 'active'
            ORDER BY member_count DESC NULLS LAST, id
            LIMIT $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(i64::try_from(MAX_PLACES_PER_SWEEP).unwrap_or(64))
        .fetch_all(&self.pool)
        .await
        .map_err(CuratorHandleError::Database)?;

        let mut report = SweepReport::default();
        let mut candidates = Vec::new();
        for (place_id, name, url) in places {
            report.places_read += 1;
            let Some(page) = self.fetch_page(&url).await else {
                continue;
            };
            if page.subscribers.is_some() {
                self.mark_broadcast(place_id, &name, &page).await?;
                report.channels_marked += 1;
            }
            let own = telegram_handle(&url);
            for handle in &page.handles {
                if own.as_deref() == Some(handle.as_str()) {
                    continue;
                }
                candidates.push(candidate_for(&url, handle, &page));
            }
        }
        report.candidates = u32::try_from(candidates.len()).unwrap_or(u32::MAX);

        if report.places_read > 0 {
            let repository = PostgresAutopilotRepository::new_with_timeouts(
                self.pool.clone(),
                self.operation_timeout,
            );
            // One operation per calendar day per chunk: a same-day retry
            // replays rather than double-recording the sweep, and ingestion's
            // own 100-row batch bound is kept by chunking.
            let day = time::OffsetDateTime::now_utc().date();
            for (chunk_index, chunk) in candidates.chunks(100).enumerate() {
                let idempotency_key =
                    IdempotencyKey::parse(format!("curator-handles:{day}:{chunk_index}"))
                        .map_err(|_| CuratorHandleError::Key)?;
                repository
                    .ingest_outreach_candidates(
                        self.workspace_id,
                        chunk.to_vec(),
                        Some(OutreachSweepReport {
                            sources_read: report.places_read,
                            items_seen: report.candidates,
                        }),
                        &idempotency_key,
                        None,
                    )
                    .await
                    .map_err(|error| {
                        tracing::warn!(%error, "curator candidate ingestion failed");
                        CuratorHandleError::Ingest
                    })?;
            }
        }
        Ok(report)
    }

    /// A fetch or parse failure drops the place for this sweep only — the
    /// next interval retries, and a dead place must not poison the batch.
    async fn fetch_page(&self, url: &str) -> Option<TelegramPageInfo> {
        let response = match timeout(FETCH_TIMEOUT, self.client.get(url).send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(%error, %url, "curator handle fetch failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(%url, "curator handle fetch timed out");
                return None;
            }
        };
        let bytes = match response.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_BODY_BYTES => bytes,
            Ok(_) => {
                tracing::warn!(%url, "curator handle page over read cap");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, %url, "curator handle page unreadable");
                return None;
            }
        };
        Some(parse_telegram_page(&String::from_utf8_lossy(&bytes)))
    }

    /// A channel cannot be posted to, so the community lanes must stop
    /// drafting for it; the admin handle it publishes is the way in instead.
    async fn mark_broadcast(
        &self,
        place_id: uuid::Uuid,
        name: &str,
        page: &TelegramPageInfo,
    ) -> Result<(), CuratorHandleError> {
        let admin = page
            .handles
            .first()
            .map(|handle| format!("; admin @{handle}"))
            .unwrap_or_default();
        sqlx::query(
            r#"
            UPDATE discovery_places
            SET membership_state = 'not_a_fit',
                membership_note = $3,
                membership_changed_at = now(),
                membership_changed_by = 'curator_handles'
            WHERE workspace_id = $1
              AND id = $2
              AND membership_state <> 'not_a_fit'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(place_id)
        .bind(format!("telegram broadcast channel: {name}{admin}"))
        .execute(&self.pool)
        .await
        .map_err(CuratorHandleError::Database)?;
        Ok(())
    }
}

#[derive(Default)]
struct SweepReport {
    places_read: u32,
    candidates: u32,
    channels_marked: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CuratorHandleError {
    #[error("database operation failed")]
    Database(#[source] sqlx::Error),
    #[error("candidate ingestion failed")]
    Ingest,
    #[error("could not build a sweep idempotency key")]
    Key,
}

/// The place's own handle from its canonical `https://t.me/{name}` URL.
fn telegram_handle(url: &str) -> Option<String> {
    let tail = url.trim_end_matches('/').rsplit('/').next()?;
    let tail = tail.strip_prefix('@').unwrap_or(tail);
    (!tail.is_empty() && tail.bytes().all(valid_handle_byte)).then(|| tail.to_owned())
}

fn valid_handle_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Reads one public `t.me` preview page: the description, whether it is a
/// broadcast channel ("subscribers") or a group ("members"), and every
/// `@handle` or `t.me/name` contact the text publishes.
pub fn parse_telegram_page(html: &str) -> TelegramPageInfo {
    let description = meta_content(html, "og:description").map(decode_entities);
    let extra = div_text(html, "tgme_page_extra").map(decode_entities);
    let (subscribers, members) = match &extra {
        Some(extra) if extra.to_lowercase().contains("subscriber") => (count_prefix(extra), None),
        Some(extra) if extra.to_lowercase().contains("member") => (None, count_prefix(extra)),
        _ => (None, None),
    };
    let mut handles = Vec::new();
    if let Some(description) = &description {
        collect_handles(description, &mut handles);
    }
    if let Some(extra) = &extra {
        collect_handles(extra, &mut handles);
    }
    // `t.me/name` links in the description resolve to the same contacts.
    let mut rest = html;
    while let Some(at) = rest.find("t.me/") {
        rest = rest.get(at + 5..).unwrap_or_default();
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii() && valid_handle_byte(*c as u8))
            .collect();
        if is_contact_name(&name) && !handles.contains(&name) {
            handles.push(name);
        }
    }
    TelegramPageInfo {
        description,
        subscribers,
        members,
        handles,
    }
}

/// `content="…"` on the meta tag named `property="og:{property}"`.
fn meta_content<'a>(html: &'a str, property: &str) -> Option<&'a str> {
    let at = html.find(property)?;
    let tag_end = html.get(at..)?.find('>')? + at;
    let tag = html.get(at..tag_end)?;
    let content_at = tag.find("content=\"")? + 9;
    let content = tag.get(content_at..)?;
    let end = content.find('"')?;
    content.get(..end)
}

/// Inner text of the first `div` carrying `class`, e.g. `tgme_page_extra`.
fn div_text<'a>(html: &'a str, class: &str) -> Option<&'a str> {
    let at = html.find(class)?;
    let open_end = html.get(at..)?.find('>')? + at;
    let body = html.get(open_end + 1..)?;
    let end = body.find('<')?;
    body.get(..end).map(str::trim).filter(|s| !s.is_empty())
}

/// The leading count in "26 181 subscribers" / "1 234 members, 56 online" —
/// Telegram pads thousands with spaces or non-breaking spaces.
fn count_prefix(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || c.is_whitespace() || *c == '\u{a0}')
        .filter(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// `@handles` inside a text: `@` followed by 5–32 `[a-zA-Z0-9_]` characters,
/// which is Telegram's own handle grammar. The `@` must start a token — a
/// letter or digit right before it means an email address, whose domain
/// would otherwise mint a handle that does not exist.
fn collect_handles(text: &str, into: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(at) = text
        .get(index..)
        .and_then(|s| s.find('@'))
        .map(|a| a + index)
    {
        index = at + 1;
        // `press@metalzine.com` has a word byte before the `@`; a mention's
        // `@` follows whitespace, punctuation, or the start of the text —
        // otherwise an email's domain would mint a handle that does not exist.
        if at > 0
            && bytes
                .get(at - 1)
                .is_some_and(|before| valid_handle_byte(*before))
        {
            continue;
        }
        let start = at + 1;
        let end = bytes
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .take_while(|b| valid_handle_byte(**b))
            .count()
            + start;
        index = index.max(end);
        let Some(name) = text.get(start..end) else {
            continue;
        };
        if name.len() >= 5
            && name.len() <= 32
            && is_contact_name(name)
            && !into.iter().any(|seen| seen == name)
        {
            into.push(name.to_owned());
        }
    }
}

/// Excludes service paths a `t.me/` link can carry that are never contacts.
fn is_contact_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('+')
        && !matches!(
            name,
            "s" | "joinchat"
                | "telegram"
                | "addstickers"
                | "proxy"
                | "share"
                | "iv"
                | "confirmphone"
        )
}

/// The handful of entities Telegram emits in `og:description`; anything else
/// is left alone rather than mangled.
fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn candidate_for(
    place_url: &str,
    handle: &str,
    page: &TelegramPageInfo,
) -> IngestOutreachCandidate {
    IngestOutreachCandidate {
        target_kind: OutreachTargetKind::Creator,
        display_name: format!("@{handle}"),
        source: CandidateSource::CuratorSite,
        source_reference: place_url.to_owned(),
        evidence: page.description.as_ref().map(|description| {
            let mut evidence = description.chars().take(3_800).collect::<String>();
            evidence.push_str("\n\n(fit unscored)");
            evidence
        }),
        route_kind: RouteKind::Handle,
        route_value: format!("@{handle}"),
        route_is_published: true,
        channel_slug: None,
        fit_basis_points: UNSCORED_FIT_BASIS_POINTS,
        follower_count: page
            .subscribers
            .or(page.members)
            .and_then(|count| u32::try_from(count).ok()),
        engagement_count: None,
        sells_placement: false,
        churns_indiscriminately: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_telegram_page, telegram_handle};

    const CHANNEL_PAGE: &str = r#"
        <html><head>
        <meta property="og:description" content="Daily metal videos. Feedback &amp; submissions: @Metal_Admin or t.me/metal_helper"/>
        </head><body>
        <div class="tgme_page_extra">26 181 subscribers</div>
        </body></html>"#;

    const GROUP_PAGE: &str = r#"
        <html><head>
        <meta property="og:description" content="Prog talk. Contact @Prog_Snobs_Admin"/>
        </head><body>
        <div class="tgme_page_extra">124 members, 3 online</div>
        </body></html>"#;

    #[test]
    fn a_channel_page_yields_subscribers_and_contact_handles() {
        let page = parse_telegram_page(CHANNEL_PAGE);
        assert_eq!(page.subscribers, Some(26_181));
        assert_eq!(page.members, None);
        assert!(
            page.handles.contains(&"Metal_Admin".to_owned())
                && page.handles.contains(&"metal_helper".to_owned()),
            "handles: {:?}",
            page.handles
        );
        assert!(
            page.description
                .as_deref()
                .is_some_and(|d| d.contains("Feedback & submissions"))
        );
    }

    #[test]
    fn a_group_page_yields_members() {
        let page = parse_telegram_page(GROUP_PAGE);
        assert_eq!(page.members, Some(124));
        assert_eq!(page.subscribers, None);
        assert_eq!(page.handles, vec!["Prog_Snobs_Admin".to_owned()]);
    }

    #[test]
    fn a_page_without_handles_yields_none() {
        let page = parse_telegram_page(
            r#"<meta property="og:description" content="Music daily"/>
               <div class="tgme_page_extra">1 009 subscribers</div>"#,
        );
        assert_eq!(page.subscribers, Some(1_009));
        assert!(page.handles.is_empty());
    }

    #[test]
    fn short_and_service_names_are_not_handles() {
        let page = parse_telegram_page(
            r#"<meta property="og:description" content="see @ab and t.me/joinchat/x"/>
               <a href="https://t.me/somechannel">x</a>"#,
        );
        assert_eq!(page.handles, vec!["somechannel".to_owned()]);
    }

    #[test]
    fn an_email_address_is_not_a_handle() {
        let page = parse_telegram_page(
            r#"<meta property="og:description" content="mail press@metalzine.com or dm @Curator_Admin"/>"#,
        );
        assert_eq!(page.handles, vec!["Curator_Admin".to_owned()]);
    }

    #[test]
    fn the_place_url_gives_back_its_handle() {
        assert_eq!(
            telegram_handle("https://t.me/metalworld"),
            Some("metalworld".to_owned())
        );
        assert_eq!(
            telegram_handle("https://t.me/@someone/"),
            Some("someone".to_owned())
        );
        assert_eq!(telegram_handle("https://example.com/"), None);
    }
}
