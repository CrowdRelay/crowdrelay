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

// Split submodules — the retry vocabulary and the drop surge each carry
// enough commentary to stand alone, and the ratchet caps this file at 1200.
// Everything is re-exported, so paths stay `content_supply::…`.
mod drop_surge;
mod retry;

pub use drop_surge::{
    DROP_SURGE_LANES, DROP_SURGE_MAX_ATTEMPTS, DropSurgeLaneFailure, drop_surge_eligible,
    drop_surge_link_slug,
};
pub use retry::{FailedArtifact, MAX_ARTIFACT_ATTEMPTS, RelayLaneFailure, artifact_retry_due};

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
    /// Distinct fans whose conversion provenance points at a relay/posting
    /// action carrying this source. Unlike likes or reach this is already the
    /// product outcome: somebody crossed from audience into the fan graph.
    pub acquired_fans: u32,
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
/// get posts with one of two pieces of evidence after the settle window:
///
/// * the band's own followers engaged at or above the account's normal; or
/// * the source already produced at least one first-party attributed fan.
///
/// Fan acquisition is the stronger signal. A post that quietly converted a
/// listener must not be suppressed because its like count looked ordinary.
/// With neither conversions nor readable engagement, fail closed.
#[must_use]
pub fn resonates_for_communities(
    post: &SocialPostFact,
    occurred_at: OffsetDateTime,
    now: OffsetDateTime,
) -> bool {
    if now - occurred_at < Duration::hours(RESONANCE_SETTLE_HOURS) {
        return false;
    }
    if post.acquired_fans > 0 {
        return true;
    }
    let Some(resonance) = post.resonance else {
        return false;
    };
    if resonance.engagement <= 0 {
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
    pub platform: String,
    pub community_url: Option<String>,
    /// The clean subreddit name (no `r/`), as stored on the admitted target.
    pub subreddit: String,
    /// The language the community posts in — BCP-47-ish short code (`pl`,
    /// `en`), declared by the discovery outcome that proposed the target.
    /// `None` means nobody recorded it; the drafting worker then infers the
    /// language from the subreddit's own description.
    pub language: Option<String>,
    /// Relay dispatches into this community that failed, one entry per
    /// source they carried. The evaluator re-emits a retry under an
    /// `:attempt{n}` key — without it a dead dispatch dedupes forever while
    /// the community keeps winning the rotation slot.
    pub relay_failures: Vec<RelayLaneFailure>,
    /// What the room has been discussing lately, as the community sweep read it
    /// (see `room_reading`). Empty means nobody has looked, and the drop surge
    /// does not post into a room it has not read.
    pub recent_threads: Vec<crate::room_reading::RoomThread>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContentSupplySnapshot {
    pub source_id: ContentSourceId,
    pub source_kind: ContentSourceKind,
    pub source_version: i64,
    /// Source-owned campaign exclusions; rechecked before publishing.
    #[serde(default)]
    pub promotion_excluded_platforms: Vec<String>,
    /// The source's own key — `youtube:{video_id}`, `release:{plan}` — the
    /// stable name a drop surge's tracked links are minted from.
    #[serde(default)]
    pub source_key: String,
    /// The source's own title — the band's words, so the surge copy can be
    /// composed without a model call.
    #[serde(default)]
    pub title: String,
    /// Where the source lives publicly — the video's YouTube URL, the
    /// release's listen link. `None` means there is nothing to point fans
    /// at, and a surge that cannot be clicked is not a surge.
    #[serde(default)]
    pub source_url: Option<String>,
    /// The source's own description/body — the band's voice, reusable
    /// verbatim in surge copy rather than model-paraphrased.
    #[serde(default)]
    pub source_body: Option<String>,
    /// The video's thumbnail, when the source carries or derives one — the
    /// picture a channel post attaches. `None` posts without art.
    #[serde(default)]
    pub source_thumbnail_url: Option<String>,
    /// The tenant's public site origin (`tenant_settings.member_site_base_url`,
    /// trimmed). The email lane's copy needs the absolute tracked URL;
    /// `None` means the surge skips email rather than printing a link that
    /// cannot resolve.
    #[serde(default)]
    pub site_origin: Option<String>,
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
    /// Surge lanes that already failed for this source, newest failure kept.
    /// The count is what a retry's key carries so it lands as a new action
    /// instead of deduping onto the dead one.
    #[serde(default)]
    pub drop_surge_failures: Vec<DropSurgeLaneFailure>,
    /// When an operator last asked for this source's surge explicitly
    /// (`POST …/promote`). A promote re-arms the fan-out for a source whose
    /// own `occurred_at` has aged out of the drop window — the lanes that
    /// already delivered still dedupe, so the ask retries only what never
    /// landed.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub surge_requested_at: Option<OffsetDateTime>,
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
    /// How long a fresh video or release counts as a drop — the window where
    /// the surge fans it out to every owned channel at once instead of
    /// waiting on the generic posting cadence. The first day is where a new
    /// video earns its reach; a drop older than this keeps getting ordinary
    /// artifacts but the surge has already had its say.
    pub drop_surge_hours: u32,
}

impl Default for ContentSupplyPolicy {
    fn default() -> Self {
        Self {
            maximum_source_age_days: 45,
            post_show_harvest_hours: 72,
            social_post_relay_hours: 72,
            drop_surge_hours: 72,
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
        let relay_hours = if snapshot.social_post.as_ref().is_some_and(|post| {
            post.acquired_fans > 0 || post.resonance.as_ref().is_some_and(is_outlier)
        }) {
            // A content item that already converted somebody is at least as
            // valuable to keep in the relay window as an engagement outlier.
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
        if !artifact_owed(snapshot, *artifact)
            || !crate::video_promotion::artifact_allowed(
                *artifact,
                &snapshot.promotion_excluded_platforms,
            )
        {
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

/// The shortest title, in letters and digits, that identifies a post on its
/// own for cross-post dedupe.
const RELAY_TITLE_MIN_CHARS: usize = 20;

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
    // The title alone also identifies a cross-post. On 2026-09-26 the band's
    // Instagram and Facebook copies of one reel carried the same title and
    // bodies that differed by one thanks line, so the whole-text comparison
    // read them as two posts and fans got both. A title too short to be
    // distinctive ("Gramy", "Nowy klip") is not used on its own.
    let title_words = relay_words(title, "");
    let distinctive_title = title_words.chars().count() >= RELAY_TITLE_MIN_CHARS;
    let remembered = now - Duration::days(RELAY_PUSH_DEDUPE_DAYS);
    if !words.is_empty()
        && recent.iter().any(|push| {
            push.at >= remembered
                && (relay_words(&push.title, &push.body) == words
                    || (distinctive_title && relay_words(&push.title, "") == title_words))
        })
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
mod relay_pacing_tests;

#[cfg(test)]
mod tests;
