//! The drop surge: a fresh video or release fans out to every owned lane at
//! once instead of waiting on the generic posting cadence.
//!
//! Split out of `content_supply.rs` for the source-size ratchet; items are
//! re-exported there so paths stay `content_supply::…`.

use serde::Serialize;
use time::{Duration, OffsetDateTime};

use super::{
    ContentSourceKind, ContentSupplyPolicy, ContentSupplySnapshot, retry::MAX_ARTIFACT_ATTEMPTS,
};

/// The lanes a fresh video or release fans out to when it drops — one
/// idempotent action each, so a failed lane retries alone instead of
/// re-sending the ones that already went out. The names are the idempotency
/// keys; `community` is the engager dispatch, the rest are owned channels.
pub const DROP_SURGE_LANES: &[&str] = &[
    "signal_push",
    "email",
    "telegram",
    "discord",
    "instagram",
    "facebook",
    "x",
    "community",
];

/// Total attempts per surge lane, the first included — the same bound the
/// artifact chain holds, so a provider outage retries rather than stalling,
/// and a lane that keeps failing stops instead of spamming the channel.
pub const DROP_SURGE_MAX_ATTEMPTS: u32 = MAX_ARTIFACT_ATTEMPTS;

/// A surge lane whose earlier sends failed, for one source.
///
/// Same repair the artifact chain got: a failed lane is neither sent nor in
/// flight, and re-raising it under the same key dedupes onto the dead action.
/// The failure count rides in the next request's key so the retry is a new
/// action, and [`DROP_SURGE_MAX_ATTEMPTS`] caps the count.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DropSurgeLaneFailure {
    /// An owned lane name, or `community:{target_uuid}` for one community.
    /// The target is part of the retry identity: a failed draft must not
    /// re-key successful siblings or exhaust another community's budget.
    pub lane: String,
    pub failures: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_failed_at: OffsetDateTime,
}

impl DropSurgeLaneFailure {
    /// The same growing delay as the artifact chain: thirty minutes after
    /// the first failure, sixty after the second. A provider outage must not
    /// consume the entire attempt budget in consecutive autopilot cycles.
    #[must_use]
    pub fn retry_due(&self) -> OffsetDateTime {
        let doublings = self.failures.saturating_sub(1).min(4);
        self.last_failed_at + Duration::minutes(30 * (1_i64 << doublings))
    }
}

/// The smart-link slug a surge lane points at: `drop-{key}-{lane}`.
///
/// Same sanitization shape as the release link's — a source key is free text
/// and the slug column is not. `youtube:iijgBMteL9I` sanitizes lossy
/// (lowercased, colon to dash), so whenever anything was replaced the slug
/// carries a digest of the original key and two videos cannot silently share
/// a link. `None` when nothing usable survives — a lane with no link gets no
/// tracked post.
#[must_use]
pub fn drop_surge_link_slug(source_key: &str, lane: &str) -> Option<String> {
    let cleaned: String = source_key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    // The whole slug stays under 100 chars with room for the lane suffix —
    // the slug column is the narrowest funnel every tracked route passes.
    let lane_suffix = format!("-{lane}");
    let budget = 100_usize.saturating_sub("drop-".len() + lane_suffix.len() + 9);
    let bounded: String = trimmed.chars().take(budget).collect();
    let bounded = bounded.trim_end_matches('-');
    if bounded.is_empty() || !bounded.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    let digest = if bounded == source_key.to_ascii_lowercase() {
        String::new()
    } else {
        format!("-{:08x}", drop_surge_key_digest(source_key))
    };
    Some(format!("drop-{bounded}{digest}-{lane}"))
}

/// FNV-1a over the original key. Not security-relevant — it exists only to
/// keep two keys that sanitise to the same letters from sharing one link.
fn drop_surge_key_digest(source_key: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in source_key.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Whether this source is a drop the surge should fan out right now.
///
/// Videos and releases only: a new video is the strongest fan-growth asset
/// the band publishes, and the generic social cadence — a flat two-day
/// clock measured in production on 2026-09-28, where a premiere got zero
/// owned-channel posts on day one — does not react to it. The surge is the
/// event-driven path: the moment a fresh video or release lands in the
/// supply, every owned lane gets its own idempotent action.
///
/// The release plan's own communication switch still wins — `Some(false)`
/// means the plan's owner turned fan-facing communication off, and a surge
/// would be exactly the broadcast they declined. A source with no link has
/// nothing to point fans at; it is not promotable, it is a draft.
#[must_use]
pub fn drop_surge_eligible(
    snapshot: &ContentSupplySnapshot,
    policy: &ContentSupplyPolicy,
    now: OffsetDateTime,
) -> bool {
    if !matches!(
        snapshot.source_kind,
        ContentSourceKind::Video | ContentSourceKind::Release
    ) {
        return false;
    }
    if snapshot.source_version <= 0
        || snapshot.occurred_at > now
        || snapshot.expires_at <= snapshot.occurred_at
        || snapshot.expires_at <= now
    {
        return false;
    }
    // Freshness: the drop window runs from `occurred_at`, and an operator's
    // explicit promote re-opens it for twenty-four hours — long enough for
    // every lane to retry, short enough that a week-old video is not surging
    // forever on one click.
    let fresh = now - snapshot.occurred_at
        <= Duration::hours(i64::from(policy.drop_surge_hours.max(1)))
        || snapshot
            .surge_requested_at
            .is_some_and(|requested| requested <= now && now - requested <= Duration::hours(24));
    if !fresh {
        return false;
    }
    if snapshot.communication_enabled == Some(false) {
        return false;
    }
    snapshot
        .source_url
        .as_deref()
        .is_some_and(|url| url.starts_with("http"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_source_key_slugs_verbatim() {
        assert_eq!(
            drop_surge_link_slug("release-technophobia", "telegram"),
            Some("drop-release-technophobia-telegram".to_owned())
        );
    }

    #[test]
    fn a_lossy_key_carries_its_digest_so_two_videos_never_share_a_link() {
        // `youtube:ABC` and `youtube/ABC` sanitize to the same letters; the
        // digest keeps their links distinct.
        let colon = drop_surge_link_slug("youtube:iijgBMteL9I", "x").expect("a slug");
        let slash = drop_surge_link_slug("youtube/iijgBMteL9I", "x").expect("a slug");
        assert_ne!(colon, slash);
        assert!(colon.starts_with("drop-youtube-iijgbmtel9i-"));
        assert!(colon.ends_with("-x"));
        assert!(colon.len() <= 100);
    }

    #[test]
    fn a_key_with_nothing_usable_gets_no_slug() {
        assert_eq!(drop_surge_link_slug(":::", "email"), None);
        assert_eq!(drop_surge_link_slug("", "email"), None);
    }

    #[test]
    fn a_long_key_bounds_the_slug_at_the_column_limit() {
        let slug = drop_surge_link_slug(&"a".repeat(500), "instagram").expect("a slug");
        assert!(slug.len() <= 100, "{slug}");
    }

    #[test]
    fn eligibility_needs_a_fresh_linked_video() {
        let now = OffsetDateTime::now_utc();
        let snapshot = |occurred_at, url: Option<&str>| ContentSupplySnapshot {
            promotion_excluded_platforms: Vec::new(),
            source_id: crate::ContentSourceId::new(),
            source_kind: ContentSourceKind::Video,
            source_version: 1,
            source_key: "youtube:x".to_owned(),
            title: "t".to_owned(),
            source_url: url.map(str::to_owned),
            source_body: None,
            source_thumbnail_url: None,
            site_origin: None,
            drop_surge_failures: Vec::new(),
            surge_requested_at: None,
            occurred_at,
            expires_at: now + Duration::days(30),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            lane_outages: Vec::new(),
            social_post: None,
        };
        let policy = ContentSupplyPolicy::default();
        assert!(drop_surge_eligible(
            &snapshot(now - Duration::minutes(20), Some("https://youtu.be/x")),
            &policy,
            now
        ));
        // Old, unlinked, not-yet-happened and expired sources never surge.
        assert!(!drop_surge_eligible(
            &snapshot(now - Duration::days(30), Some("https://youtu.be/x")),
            &policy,
            now
        ));
        assert!(!drop_surge_eligible(
            &snapshot(now - Duration::minutes(20), None),
            &policy,
            now
        ));
        assert!(!drop_surge_eligible(
            &snapshot(now + Duration::hours(2), Some("https://youtu.be/x")),
            &policy,
            now
        ));
        // An event is not a drop — it belongs to the artifact chain.
        let mut event = snapshot(now - Duration::minutes(20), Some("https://x.test"));
        event.source_kind = ContentSourceKind::Event;
        assert!(!drop_surge_eligible(&event, &policy, now));
    }
}
