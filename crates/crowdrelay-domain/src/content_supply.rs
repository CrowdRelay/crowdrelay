//! Deterministic content supply-chain bounded context.
//!
//! The domain schedules provider-neutral artifact requests from trusted source
//! facts. It does not generate prose, choose a social provider, or depend on an
//! LLM. Existing approved templates remain the executable language surface.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    ContentSourceId, OutreachTargetId, autonomy::Confidence, release_autopilot::ReleaseTier,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSourceKind {
    Event,
    Release,
    ShowCompleted,
    /// A published video (e.g. a music video on YouTube) — the artifact the
    /// community-engagement loop shares. Trusted facts only: title, link,
    /// published timestamp; the story around it is never invented.
    Video,
    /// A first-person account the tenant entered through the control plane —
    /// the only first-person material an agent may narrate.
    Story,
    /// A post the band itself published on an owned social account (a synced
    /// fact — title, link, timestamp — never a paraphrase). It exists so the
    /// watcher sees the band alive and the relay path has something real to
    /// carry; what it owes is decided by the relay work, not assumed here.
    SocialPost,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentArtifactKind {
    SignalPush,
    NewsletterBlock,
    SocialFeed,
    SocialStory,
    LiveListing,
    PressHook,
    PostShowRecap,
}

impl ContentArtifactKind {
    /// The operator-facing name — briefings and panels read this, not the
    /// serde key or the Debug form.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SignalPush => "Signal push",
            Self::NewsletterBlock => "Newsletter block",
            Self::SocialFeed => "Social feed",
            Self::SocialStory => "Social story",
            Self::LiveListing => "Live listing",
            Self::PressHook => "Press hook",
            Self::PostShowRecap => "Post-show recap",
        }
    }

    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::SignalPush => "content.signal_push.v1",
            Self::NewsletterBlock => "content.newsletter_block.v1",
            Self::SocialFeed => "content.social_feed.v1",
            Self::SocialStory => "content.social_story.v1",
            Self::LiveListing => "content.live_listing.v1",
            Self::PressHook => "content.press_hook.v1",
            Self::PostShowRecap => "content.post_show_recap.v1",
        }
    }
}

/// Facts about a synced band post, projected for the relay path: the post's
/// own first line, its permalink, the platform account it came from and the
/// caption itself. The relay carries these verbatim — it shares what the band
/// published, it does not draft a post about a post.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SocialPostFact {
    /// The post's own first line — the caption's head, never a rewrite.
    pub title: String,
    /// The platform permalink, when the API gave one.
    pub url: Option<String>,
    /// Which owned account it came from — `facebook`, `instagram`, `x`.
    pub platform: String,
    /// The caption itself, as truncated by the sync.
    pub body: Option<String>,
    /// The post's media as the platform reported it — IG `media_url` (jpeg
    /// for a photo, mp4 for a video), a carousel's first image, FB
    /// `full_picture`. A signed CDN URL that expires; `media_id` re-mints it.
    pub media_url: Option<String>,
    /// The Graph object the media URL belongs to — the post itself, or the
    /// chosen carousel child. The executor re-mints a fresh URL through
    /// `/{media_id}?fields=media_url` (or `thumbnail_url` for a video).
    pub media_id: Option<String>,
    /// IG `media_type` (`IMAGE`/`VIDEO`/`CAROUSEL_ALBUM`); `None` on FB.
    pub media_type: Option<String>,
    /// The still a video post shows — what an image post can actually carry
    /// when `media_type` is VIDEO.
    pub thumbnail_url: Option<String>,
    /// How the post landed with the band's own followers, against the band's
    /// other recent posts on the same platform. `None` until the sync has
    /// read its engagement.
    pub resonance: Option<PostResonance>,
}

/// A synced post's engagement against the account's own recent posts.
///
/// Weighted count — likes, plus comments ×3, plus shares ×5: a comment or a
/// share is a person doing something, a like is a thumb. Compared with the
/// median of the same account's earlier posts, so the bar is the band's own
/// normal, not a number from somebody else's audience.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PostResonance {
    pub engagement: i64,
    /// Median engagement of the account's earlier posts on this platform,
    /// rounded up — the bar errs high.
    pub peer_median: Option<i64>,
    /// How many earlier posts the median is over.
    pub peers: u32,
    /// Engagement per thousand people reached, when the platform reported
    /// reach (Instagram insights). Fair to a post shown to fewer people.
    pub rate_per_mille: Option<i64>,
    /// The same rate's median over earlier posts that reported reach.
    pub peer_rate_median: Option<i64>,
    /// How many earlier posts the rate median is over.
    pub rate_peers: u32,
    /// Average watch time in milliseconds (a reel) — what a hook moves.
    pub watch_ms: Option<i64>,
    /// Median average watch time of earlier reels.
    pub peer_watch_median: Option<i64>,
}

/// How long an outlier stays relayable. A post that did twice the account's
/// usual stays worth carrying for a week, not the normal three days: a
/// community admitted later, or one the daily cap held back, still gets it.
/// It never goes to the same community twice — the relay keys see to that.
pub const OUTLIER_RELAY_HOURS: u32 = 168;

/// Whether a post did at least twice the account's usual — by rate when the
/// platform reported reach, by raw engagement otherwise.
#[must_use]
pub fn is_outlier(resonance: &PostResonance) -> bool {
    if let (Some(rate), Some(peer_rate)) = (resonance.rate_per_mille, resonance.peer_rate_median)
        && resonance.rate_peers >= RESONANCE_MIN_PEERS
        && peer_rate > 0
    {
        return rate >= peer_rate.saturating_mul(2);
    }
    matches!(
        resonance.peer_median,
        Some(median) if resonance.peers >= RESONANCE_MIN_PEERS
            && median > 0
            && resonance.engagement >= median.saturating_mul(2)
    )
}

/// Posts younger than this have not had time to show how they landed.
pub const RESONANCE_SETTLE_HOURS: i64 = 36;
/// Below this many earlier posts a median says nothing; any real engagement
/// is enough.
const RESONANCE_MIN_PEERS: u32 = 3;

/// Whether a synced post has earned a place in other people's communities.
///
/// The owned audience gets every post — they followed the band. Communities
/// get the ones the band's own followers answered: at or above the account's
/// usual engagement, once it has had time to show. A post nobody engaged
/// with at home is not one strangers will; carrying only what resonated is
/// both what spreads and what reads as a band sharing its best, not a bot
/// mirroring a feed. Fail closed: no engagement read, no community relay.
#[must_use]
pub fn resonates_for_communities(
    post: &SocialPostFact,
    occurred_at: OffsetDateTime,
    now: OffsetDateTime,
) -> bool {
    let Some(resonance) = post.resonance else {
        return false;
    };
    if now - occurred_at < Duration::hours(RESONANCE_SETTLE_HOURS) || resonance.engagement <= 0 {
        return false;
    }
    // With reach on both sides, the rate decides: engagement per thousand
    // reached against the account's usual rate. A reel below that rate still
    // earns its place when people watched it clearly longer than usual — a
    // hook that held attention is the signal, even when nobody tapped like.
    if let (Some(rate), Some(peer_rate)) = (resonance.rate_per_mille, resonance.peer_rate_median)
        && resonance.rate_peers >= RESONANCE_MIN_PEERS
    {
        let held_attention = matches!(
            (resonance.watch_ms, resonance.peer_watch_median),
            (Some(watch), Some(peer)) if peer > 0 && watch.saturating_mul(4) >= peer.saturating_mul(5)
        );
        return rate >= peer_rate || held_attention;
    }
    match resonance.peer_median {
        Some(median) if resonance.peers >= RESONANCE_MIN_PEERS => resonance.engagement >= median,
        _ => true,
    }
}

/// A community the workspace may post in: an outreach target the screening
/// pipeline admitted and an operator-visible promotion carried to `promoted`.
/// The relay reads this list — a community not on it is not reachable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommunityRelayTarget {
    pub target_id: OutreachTargetId,
    /// The clean subreddit name (no `r/`), as stored on the admitted target.
    pub subreddit: String,
    /// The language the community posts in — BCP-47-ish short code (`pl`,
    /// `en`), declared by the discovery outcome that proposed the target.
    /// `None` means nobody recorded it; the drafting worker then infers the
    /// language from the subreddit's own description.
    pub language: Option<String>,
}

/// How many fans a Signal push would reach right now, measured with the
/// send path's own eligibility (active fan, newest marketing consent
/// granted, the segment's predicates, at least one live push endpoint).
/// `reached` is `eligible` clamped by the workspace's per-step recipient
/// bound — the bound clamps the fan set, not just the report, so an
/// approval screen should print `reached`, never `eligible`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SignalPushAudience {
    pub eligible: u32,
    pub reached: u32,
}

/// An artifact whose earlier requests failed, for one source version.
///
/// A failed request is neither done nor in flight, so the evaluator asks for
/// the same artifact again next cycle — under the same idempotency key, which
/// dedupes onto the failed action and writes nothing. Before this existed one
/// failure froze the source's whole chain for good: on 2026-09-25 two
/// `live_listing` requests hit Discord's rate limit (HTTP 429, five requests
/// in one second to one webhook), and neither source got another artifact.
/// A retry carries its attempt number in its key, waits out a growing delay,
/// and stops after [`MAX_ARTIFACT_ATTEMPTS`]; the chain skips an artifact in
/// either state rather than waiting on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FailedArtifact {
    pub artifact: ContentArtifactKind,
    pub failures: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_failed_at: OffsetDateTime,
}

/// Requests per artifact and source version, the first included.
pub const MAX_ARTIFACT_ATTEMPTS: u32 = 3;

/// How long after its latest failure an artifact may be asked for again: 30
/// minutes after the first failure, an hour after the second.
#[must_use]
pub fn artifact_retry_due(failed: &FailedArtifact) -> OffsetDateTime {
    let doublings = failed.failures.saturating_sub(1).min(4);
    failed.last_failed_at + Duration::minutes(30 * (1_i64 << doublings))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContentSupplySnapshot {
    pub source_id: ContentSourceId,
    pub source_kind: ContentSourceKind,
    pub source_version: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// The plan's own communication switch, projected as fact for a release
    /// source. `None` means the kind carries no such switch — events, videos,
    /// stories and harvests communicate freely — while `Some(false)` means
    /// the plan's owner turned communication off and the chain owes it no
    /// fan-facing artifact, the same hold the milestone ladder applies.
    pub communication_enabled: Option<bool>,
    /// The plan's press switch, same projection. `Some(false)` means the
    /// chain owes no press hook even while the social artifacts still run.
    pub press_enabled: Option<bool>,
    /// A release plan's tier, projected for the artifact gate: a filler
    /// release is posted, never pitched, so it owes no press hook.
    pub release_tier: Option<ReleaseTier>,
    pub completed_artifacts: Vec<ContentArtifactKind>,
    pub in_flight_artifacts: Vec<ContentArtifactKind>,
    /// Artifacts whose requests for this source version failed at the
    /// executor. See [`FailedArtifact`].
    pub failed_artifacts: Vec<FailedArtifact>,
    /// The synced post's own facts — `Some` only when `source_kind` is
    /// `SocialPost`. The relay needs them to carry the post; every other
    /// kind leaves it `None`.
    pub social_post: Option<SocialPostFact>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ContentSupplyPolicy {
    pub maximum_source_age_days: u32,
    /// How fresh a synced band post has to be for the relay to carry it —
    /// a post past the window is news that already cooled, and relaying it
    /// reads as a channel that cannot tell now from then.
    pub social_post_relay_hours: u32,
    /// How long a finished show's material gets to arrive before the harvest
    /// starts drafting from it: the capture plan needs the night plus a
    /// collection window, so `show_completed` sources stay pending until
    /// `occurred_at + post_show_harvest_hours`. Zero means no delay.
    pub post_show_harvest_hours: u32,
}

impl Default for ContentSupplyPolicy {
    fn default() -> Self {
        Self {
            maximum_source_age_days: 45,
            post_show_harvest_hours: 72,
            social_post_relay_hours: 72,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSupplyDecision {
    Hold(ContentSupplyHoldReason),
    Request {
        artifact: ContentArtifactKind,
        /// Zero for the first request; a retry after `attempt` failures
        /// otherwise. It goes into the idempotency key, so a retry is a new
        /// action rather than a replay of the failed one.
        attempt: u32,
        confidence: Confidence,
    },
    /// The band already made the post — the machine's job is to carry it to
    /// the owned audience (verbatim, with the original media) and to admitted
    /// communities (drafted per community in that community's language by the
    /// repost worker — a raw caption dump is what made forum reposts read as
    /// spam). It is not a new broadcast.
    Relay {
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSupplyHoldReason {
    InvalidSnapshot,
    StaleSource,
    /// The show ended but its harvest window is still open — drafts would
    /// race the photographer. The source becomes live when the window closes.
    HarvestPending,
    Complete,
}

#[must_use]
pub fn evaluate_content_supply(
    snapshot: &ContentSupplySnapshot,
    policy: ContentSupplyPolicy,
    now: OffsetDateTime,
) -> ContentSupplyDecision {
    if snapshot.source_version <= 0
        || snapshot.occurred_at > now
        || snapshot.expires_at <= snapshot.occurred_at
    {
        return ContentSupplyDecision::Hold(ContentSupplyHoldReason::InvalidSnapshot);
    }

    // Time-anchored kinds go stale by age: an event or release stops being
    // news after the policy window. Videos and stories are evergreen
    // material — a year-old video is still a video, and a story the tenant
    // entered is live until its writer-set expiry. For them `expires_at`
    // is the only freshness bound; aging them out by `occurred_at` would
    // make the channel's whole back catalog unshareable on arrival.
    let maximum_age = Duration::days(i64::from(policy.maximum_source_age_days.max(1)));
    let age_bounded = matches!(
        snapshot.source_kind,
        ContentSourceKind::Event | ContentSourceKind::Release | ContentSourceKind::ShowCompleted
    );
    if snapshot.expires_at <= now || (age_bounded && now - snapshot.occurred_at > maximum_age) {
        return ContentSupplyDecision::Hold(ContentSupplyHoldReason::StaleSource);
    }

    // A finished show is harvestable only after its material window closes:
    // the capture plan's shots need the night plus collection time before a
    // recap or social artifact can honestly render from them.
    if snapshot.source_kind == ContentSourceKind::ShowCompleted
        && now - snapshot.occurred_at < Duration::hours(i64::from(policy.post_show_harvest_hours))
    {
        return ContentSupplyDecision::Hold(ContentSupplyHoldReason::HarvestPending);
    }

    // A synced band post owes no artifact — relaying it is the work (2.11).
    // Inside the freshness window the post is carryable news; past it the
    // moment already cooled and the honest answer is Complete, not a late
    // relay that reads like a channel that cannot tell now from then.
    if snapshot.source_kind == ContentSourceKind::SocialPost {
        // `social_post.is_some()` is part of the gate: a source row without
        // the post's own facts has nothing to carry, and a relay that would
        // have to invent the share is exactly what this path exists to
        // prevent.
        let relay_hours = if snapshot
            .social_post
            .as_ref()
            .and_then(|post| post.resonance.as_ref())
            .is_some_and(is_outlier)
        {
            policy.social_post_relay_hours.max(OUTLIER_RELAY_HOURS)
        } else {
            policy.social_post_relay_hours
        };
        let fresh = now - snapshot.occurred_at <= Duration::hours(i64::from(relay_hours.max(1)));
        return if fresh && snapshot.social_post.is_some() {
            ContentSupplyDecision::Relay {
                confidence: Confidence::saturating_from_basis_points(9_000),
            }
        } else {
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete)
        };
    }

    for artifact in required_artifacts(snapshot.source_kind) {
        if !artifact_owed(snapshot, *artifact) {
            continue;
        }
        let already_done = snapshot.completed_artifacts.contains(artifact);
        let in_flight = snapshot.in_flight_artifacts.contains(artifact);
        if already_done || in_flight {
            continue;
        }
        let failed = snapshot
            .failed_artifacts
            .iter()
            .find(|failed| failed.artifact == *artifact);
        if let Some(failed) = failed
            && (failed.failures >= MAX_ARTIFACT_ATTEMPTS || now < artifact_retry_due(failed))
        {
            // Given up, or not yet due: the rest of the chain does not wait.
            continue;
        }
        return ContentSupplyDecision::Request {
            artifact: *artifact,
            attempt: failed.map_or(0, |failed| failed.failures),
            confidence: Confidence::saturating_from_basis_points(9_500),
        };
    }

    ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete)
}

/// Whether a source's own switches owe this artifact at all, evaluated on the
/// bare flags so the evaluator and the execution-time recheck share one rule.
/// A missing flag (`None`) means the kind carries no switch — the artifact is
/// owed. Fan-facing artifacts hold on `communication_enabled`, the press hook
/// on `press_enabled`, and a filler release owes no press hook because it is
/// posted, not pitched. `LiveListing` is a fact surface on the band's own
/// pages, not communication, so no switch reaches it.
#[must_use]
pub fn content_artifact_owed(
    artifact: ContentArtifactKind,
    communication_enabled: Option<bool>,
    press_enabled: Option<bool>,
    release_tier: Option<ReleaseTier>,
) -> bool {
    match artifact {
        ContentArtifactKind::PressHook => {
            press_enabled != Some(false) && release_tier != Some(ReleaseTier::Filler)
        }
        ContentArtifactKind::SignalPush
        | ContentArtifactKind::NewsletterBlock
        | ContentArtifactKind::SocialFeed
        | ContentArtifactKind::SocialStory
        | ContentArtifactKind::PostShowRecap => communication_enabled != Some(false),
        ContentArtifactKind::LiveListing => true,
    }
}

fn artifact_owed(snapshot: &ContentSupplySnapshot, artifact: ContentArtifactKind) -> bool {
    content_artifact_owed(
        artifact,
        snapshot.communication_enabled,
        snapshot.press_enabled,
        snapshot.release_tier,
    )
}

fn required_artifacts(kind: ContentSourceKind) -> &'static [ContentArtifactKind] {
    const EVENT: &[ContentArtifactKind] = &[
        ContentArtifactKind::LiveListing,
        // Every published show should also produce a media-ready local hook.
        // Beacons then receive a concrete story/interview angle instead of a
        // generic EPK blast. The artifact is provider-neutral and fact-only.
        ContentArtifactKind::PressHook,
        ContentArtifactKind::SignalPush,
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
        ContentArtifactKind::NewsletterBlock,
    ];
    const RELEASE: &[ContentArtifactKind] = &[
        ContentArtifactKind::SignalPush,
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
        ContentArtifactKind::NewsletterBlock,
        ContentArtifactKind::PressHook,
    ];
    const POST: &[ContentArtifactKind] = &[
        ContentArtifactKind::PostShowRecap,
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
    ];
    const SOCIAL: &[ContentArtifactKind] = &[
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
    ];

    match kind {
        ContentSourceKind::Event => EVENT,
        ContentSourceKind::Release | ContentSourceKind::Video => RELEASE,
        ContentSourceKind::ShowCompleted => POST,
        // A story is share material, not an announcement: feed artifacts only.
        ContentSourceKind::Story => SOCIAL,
        // A synced social post owes no artifact yet — relaying it is the
        // amplification work (2.11), and drafting a post *about* a post is
        // the shapeless echo the artifact list exists to prevent.
        ContentSourceKind::SocialPost => &[],
    }
}

/// The shortest gap between two relay pushes to the same fans.
///
/// A relay carries one band post to the phones of fans who opted in. On
/// 2026-09-26 the first four relays after a blocked week went out in the
/// same tenth of a second, one of them twice: the band posts the same words
/// to Facebook and Instagram, and each copy was its own relay. Four
/// notifications at once from a band is how an app gets muted. Twelve hours
/// keeps a band that posts daily at one push a day and drops the backlog
/// that piles up while the relay is held; within the 72-hour freshness
/// window a held post still gets its turn.
pub const RELAY_PUSH_MIN_GAP_HOURS: i64 = 12;

/// How long a relayed post's words are remembered, so the same words
/// cross-posted to another platform are not pushed a second time.
pub const RELAY_PUSH_DEDUPE_DAYS: i64 = 7;

/// A relay push already raised, as the pacing reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecentRelayPush {
    pub at: OffsetDateTime,
    pub title: String,
    pub body: String,
}

/// Whether one more relay push may be raised now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayPushVerdict {
    Send,
    /// The same words already reached these fans — a cross-post.
    AlreadyRelayed,
    /// Another relay went out less than [`RELAY_PUSH_MIN_GAP_HOURS`] ago.
    TooSoon,
}

/// The words a relay carries, without links and case: a cross-post differs
/// from the original only in its permalink.
fn relay_words(title: &str, body: &str) -> String {
    format!("{title} {body}")
        .split_whitespace()
        .filter(|word| !word.starts_with("http://") && !word.starts_with("https://"))
        .flat_map(|word| word.chars().filter(|c| c.is_alphanumeric()))
        .flat_map(char::to_lowercase)
        .collect()
}

#[must_use]
pub fn relay_push_verdict(
    title: &str,
    body: &str,
    recent: &[RecentRelayPush],
    now: OffsetDateTime,
) -> RelayPushVerdict {
    let words = relay_words(title, body);
    let remembered = now - Duration::days(RELAY_PUSH_DEDUPE_DAYS);
    if !words.is_empty()
        && recent
            .iter()
            .any(|push| push.at >= remembered && relay_words(&push.title, &push.body) == words)
    {
        return RelayPushVerdict::AlreadyRelayed;
    }
    let gap = Duration::hours(RELAY_PUSH_MIN_GAP_HOURS);
    if recent.iter().any(|push| now - push.at < gap) {
        return RelayPushVerdict::TooSoon;
    }
    RelayPushVerdict::Send
}

#[cfg(test)]
mod relay_pacing_tests {
    use super::*;

    fn at(hours_ago: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid")
            - Duration::hours(hours_ago)
    }

    fn push(hours_ago: i64, title: &str, body: &str) -> RecentRelayPush {
        RecentRelayPush {
            at: at(hours_ago),
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn the_same_words_cross_posted_are_not_pushed_twice() {
        let title = "Terapia grupowa, spowiedź szaleńca, mental metal.";
        let recent = [push(
            30,
            title,
            "Terapia grupowa. Łapcie mordeczki\n\nhttps://www.facebook.com/1069/posts/1",
        )];
        assert_eq!(
            relay_push_verdict(
                title,
                "Terapia grupowa. Łapcie mordeczki\n\nhttps://www.instagram.com/p/Dd1/",
                &recent,
                at(0)
            ),
            RelayPushVerdict::AlreadyRelayed
        );
    }

    #[test]
    fn a_second_post_waits_out_the_gap() {
        let recent = [push(2, "Gramy w Gorzowie", "17.10")];
        assert_eq!(
            relay_push_verdict("Nowy klip", "Już jest", &recent, at(0)),
            RelayPushVerdict::TooSoon
        );
        let older = [push(RELAY_PUSH_MIN_GAP_HOURS, "Gramy w Gorzowie", "17.10")];
        assert_eq!(
            relay_push_verdict("Nowy klip", "Już jest", &older, at(0)),
            RelayPushVerdict::Send
        );
    }

    #[test]
    fn words_older_than_the_memory_may_be_relayed_again() {
        let recent = [push(RELAY_PUSH_DEDUPE_DAYS * 24 + 1, "Gramy", "17.10")];
        assert_eq!(
            relay_push_verdict("Gramy", "17.10", &recent, at(0)),
            RelayPushVerdict::Send
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    #[test]
    fn event_requests_live_listing_before_channel_specific_artifacts() {
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Event,
            source_version: 1,
            occurred_at: now() - Duration::days(1),
            expires_at: now() + Duration::days(10),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::LiveListing,
                attempt: 0,
                confidence: Confidence::saturating_from_basis_points(9_500),
            }
        );
    }

    #[test]
    fn event_builds_press_hook_after_canonical_listing() {
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Event,
            source_version: 1,
            occurred_at: now() - Duration::days(1),
            expires_at: now() + Duration::days(10),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            social_post: None,
            completed_artifacts: vec![ContentArtifactKind::LiveListing],
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::PressHook,
                attempt: 0,
                confidence: Confidence::saturating_from_basis_points(9_500),
            }
        );
    }

    fn event_with_failed_listing(failures: u32, minutes_ago: i64) -> ContentSupplySnapshot {
        ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Event,
            source_version: 1,
            occurred_at: now() - Duration::days(1),
            expires_at: now() + Duration::days(10),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            social_post: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: vec![FailedArtifact {
                artifact: ContentArtifactKind::LiveListing,
                failures,
                last_failed_at: now() - Duration::minutes(minutes_ago),
            }],
        }
    }

    fn requested(snapshot: &ContentSupplySnapshot) -> Option<(ContentArtifactKind, u32)> {
        match evaluate_content_supply(snapshot, ContentSupplyPolicy::default(), now()) {
            ContentSupplyDecision::Request {
                artifact, attempt, ..
            } => Some((artifact, attempt)),
            _ => None,
        }
    }

    #[test]
    fn a_failed_artifact_is_retried_once_due_under_a_new_attempt() {
        // Production, 2026-09-25: a live listing refused with HTTP 429 was
        // asked for again under its old key every cycle, deduped onto the
        // failed action, and the source never got another artifact.
        assert_eq!(
            requested(&event_with_failed_listing(1, 31)),
            Some((ContentArtifactKind::LiveListing, 1))
        );
        assert_eq!(
            requested(&event_with_failed_listing(2, 61)),
            Some((ContentArtifactKind::LiveListing, 2))
        );
    }

    #[test]
    fn a_retry_not_yet_due_does_not_hold_the_chain() {
        assert_eq!(
            requested(&event_with_failed_listing(1, 10)),
            Some((ContentArtifactKind::PressHook, 0))
        );
        // The second retry waits an hour, not thirty minutes.
        assert_eq!(
            requested(&event_with_failed_listing(2, 45)),
            Some((ContentArtifactKind::PressHook, 0))
        );
    }

    #[test]
    fn an_artifact_that_failed_every_attempt_is_skipped() {
        assert_eq!(
            requested(&event_with_failed_listing(MAX_ARTIFACT_ATTEMPTS, 10_000)),
            Some((ContentArtifactKind::PressHook, 0))
        );
    }

    #[test]
    fn release_requests_artifacts_one_at_a_time_and_respects_inflight() {
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Release,
            source_version: 1,
            occurred_at: now() - Duration::days(1),
            expires_at: now() + Duration::days(10),
            communication_enabled: Some(true),
            press_enabled: Some(true),
            release_tier: Some(ReleaseTier::Single),
            social_post: None,
            completed_artifacts: vec![ContentArtifactKind::SignalPush],
            in_flight_artifacts: vec![ContentArtifactKind::SocialFeed],
            failed_artifacts: Vec::new(),
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::SocialStory,
                attempt: 0,
                confidence: Confidence::saturating_from_basis_points(9_500),
            }
        );
    }

    #[test]
    fn a_year_old_event_is_stale_but_a_year_old_video_is_share_material() {
        let old_event = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Event,
            source_version: 1,
            occurred_at: now() - Duration::days(365),
            expires_at: now() + Duration::days(10),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        };
        let old_video = ContentSupplySnapshot {
            source_kind: ContentSourceKind::Video,
            ..old_event.clone()
        };
        let old_story = ContentSupplySnapshot {
            source_kind: ContentSourceKind::Story,
            ..old_event.clone()
        };

        assert_eq!(
            evaluate_content_supply(&old_event, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::StaleSource),
        );
        // Video and story are evergreen: only expires_at bounds them.
        assert!(matches!(
            evaluate_content_supply(&old_video, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request { .. }
        ));
        assert!(matches!(
            evaluate_content_supply(&old_story, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request { .. }
        ));
    }

    #[test]
    fn a_finished_show_waits_out_its_material_window_before_harvesting() {
        let show = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::ShowCompleted,
            source_version: 1,
            occurred_at: now() - Duration::hours(20),
            expires_at: now() + Duration::days(30),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        };

        // Twenty hours in, the night is over but the capture plan's material
        // is still being collected: nothing drafts from it yet.
        assert_eq!(
            evaluate_content_supply(&show, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::HarvestPending),
        );

        // Once the window closes the recap artifact is demanded first — the
        // night's own record before the social reuse of it.
        let mut collected = show.clone();
        collected.occurred_at = now() - Duration::hours(80);
        assert!(matches!(
            evaluate_content_supply(&collected, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::PostShowRecap,
                ..
            }
        ));

        // The gate is kind-scoped: events and releases still draft the moment
        // they land, with no collection window to wait out.
        let mut event = show;
        event.source_kind = ContentSourceKind::Event;
        assert!(matches!(
            evaluate_content_supply(&event, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request { .. }
        ));
    }

    fn release_snapshot() -> ContentSupplySnapshot {
        ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Release,
            source_version: 1,
            occurred_at: now() - Duration::days(1),
            expires_at: now() + Duration::days(10),
            communication_enabled: Some(true),
            press_enabled: Some(true),
            release_tier: Some(ReleaseTier::Single),
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        }
    }

    #[test]
    fn a_release_with_communication_off_owes_no_fan_facing_artifact() {
        let mut snapshot = release_snapshot();
        snapshot.communication_enabled = Some(false);

        // The operator's own switch holds the whole fan-facing chain; the
        // press hook still stands because press is a different switch and it
        // reaches journalists, not fans.
        assert!(matches!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::PressHook,
                ..
            }
        ));

        snapshot.completed_artifacts = vec![ContentArtifactKind::PressHook];
        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
        );
    }

    #[test]
    fn a_release_with_press_off_never_owes_a_press_hook() {
        let mut snapshot = release_snapshot();
        snapshot.press_enabled = Some(false);

        // Signal still comes first — the switch only mutes the press side.
        assert!(matches!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::SignalPush,
                ..
            }
        ));

        snapshot.completed_artifacts = vec![
            ContentArtifactKind::SignalPush,
            ContentArtifactKind::SocialFeed,
            ContentArtifactKind::SocialStory,
            ContentArtifactKind::NewsletterBlock,
        ];
        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
            "press off means the chain completes without the hook"
        );
    }

    #[test]
    fn a_filler_release_is_posted_but_never_pitched() {
        let mut snapshot = release_snapshot();
        snapshot.release_tier = Some(ReleaseTier::Filler);

        // The owned-channel chain still runs — posting the demo is the point
        // of the tier — but a demo owes no press hook.
        snapshot.completed_artifacts = vec![
            ContentArtifactKind::SignalPush,
            ContentArtifactKind::SocialFeed,
            ContentArtifactKind::SocialStory,
            ContentArtifactKind::NewsletterBlock,
        ];
        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
        );
    }

    #[test]
    fn a_synced_social_post_without_facts_cannot_relay() {
        // A source row with no social_post facts has nothing to carry — the
        // relay cannot invent a title or link, so it holds rather than send
        // an empty share.
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 1,
            occurred_at: now() - Duration::hours(6),
            expires_at: now() + Duration::days(44),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
        );
    }

    fn fact(resonance: Option<PostResonance>) -> SocialPostFact {
        SocialPostFact {
            title: "Gramy 17.10 w Gorzowie.".to_owned(),
            url: None,
            platform: "instagram".to_owned(),
            body: None,
            media_url: None,
            media_id: None,
            media_type: None,
            thumbnail_url: None,
            resonance,
        }
    }

    #[test]
    fn only_posts_that_landed_at_home_go_to_communities() {
        let posted = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let settled = posted + Duration::hours(40);
        let above = PostResonance {
            engagement: 80,
            peer_median: Some(50),
            peers: 10,
            ..Default::default()
        };
        let below = PostResonance {
            engagement: 30,
            peer_median: Some(50),
            peers: 10,
            ..Default::default()
        };
        assert!(resonates_for_communities(
            &fact(Some(above)),
            posted,
            settled
        ));
        assert!(!resonates_for_communities(
            &fact(Some(below)),
            posted,
            settled
        ));
        // Too early to tell, however well it is doing.
        assert!(!resonates_for_communities(
            &fact(Some(above)),
            posted,
            posted + Duration::hours(6)
        ));
        // Never read: fail closed.
        assert!(!resonates_for_communities(&fact(None), posted, settled));
        // A new account with no history: any real engagement is enough, none is not.
        let first = PostResonance {
            engagement: 4,
            peer_median: None,
            peers: 0,
            ..Default::default()
        };
        assert!(resonates_for_communities(
            &fact(Some(first)),
            posted,
            settled
        ));
        let silent = PostResonance {
            engagement: 0,
            peer_median: None,
            peers: 0,
            ..Default::default()
        };
        assert!(!resonates_for_communities(
            &fact(Some(silent)),
            posted,
            settled
        ));
    }

    #[test]
    fn reach_and_watch_time_decide_when_the_platform_reports_them() {
        let posted = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let settled = posted + Duration::hours(40);
        let with = |rate: i64, watch: Option<i64>| PostResonance {
            engagement: 10,
            peer_median: Some(500), // raw engagement alone would refuse it
            peers: 10,
            rate_per_mille: Some(rate),
            peer_rate_median: Some(40),
            rate_peers: 8,
            watch_ms: watch,
            peer_watch_median: Some(4_000),
        };
        // Shown to few people, but those people engaged at a high rate.
        assert!(resonates_for_communities(
            &fact(Some(with(55, None))),
            posted,
            settled
        ));
        // Low rate, and watched no longer than usual: not spread.
        assert!(!resonates_for_communities(
            &fact(Some(with(20, Some(4_100)))),
            posted,
            settled
        ));
        // Low rate, but held attention 25%+ longer than usual: the hook worked.
        assert!(resonates_for_communities(
            &fact(Some(with(20, Some(5_000)))),
            posted,
            settled
        ));
    }

    #[test]
    fn an_outlier_stays_relayable_for_a_week_and_an_ordinary_post_does_not() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let snapshot = |resonance: PostResonance| ContentSupplySnapshot {
            source_id: crate::ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 1,
            occurred_at: now - Duration::days(5),
            expires_at: now + Duration::days(30),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: Some(fact(Some(resonance))),
        };
        let outlier = PostResonance {
            engagement: 120,
            peer_median: Some(50),
            peers: 10,
            ..Default::default()
        };
        let ordinary = PostResonance {
            engagement: 60,
            ..outlier
        };
        assert!(is_outlier(&outlier));
        assert!(!is_outlier(&ordinary));
        let policy = ContentSupplyPolicy::default();
        assert!(matches!(
            evaluate_content_supply(&snapshot(outlier), policy, now),
            ContentSupplyDecision::Relay { .. }
        ));
        assert_eq!(
            evaluate_content_supply(&snapshot(ordinary), policy, now),
            ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete)
        );
    }
}
