//! Deterministic content supply-chain bounded context.
//!
//! The domain schedules provider-neutral artifact requests from trusted source
//! facts. It does not generate prose, choose a social provider, or depend on an
//! LLM. Existing approved templates remain the executable language surface.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{ContentSourceId, autonomy::Confidence};

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContentSupplySnapshot {
    pub source_id: ContentSourceId,
    pub source_kind: ContentSourceKind,
    pub source_version: i64,
    pub occurred_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub completed_artifacts: Vec<ContentArtifactKind>,
    pub in_flight_artifacts: Vec<ContentArtifactKind>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentSupplyPolicy {
    pub maximum_source_age_days: u32,
}

impl Default for ContentSupplyPolicy {
    fn default() -> Self {
        Self {
            maximum_source_age_days: 45,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSupplyDecision {
    Hold(ContentSupplyHoldReason),
    Request {
        artifact: ContentArtifactKind,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSupplyHoldReason {
    InvalidSnapshot,
    StaleSource,
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

    for artifact in required_artifacts(snapshot.source_kind) {
        let already_done = snapshot.completed_artifacts.contains(artifact);
        let in_flight = snapshot.in_flight_artifacts.contains(artifact);
        if !already_done && !in_flight {
            return ContentSupplyDecision::Request {
                artifact: *artifact,
                confidence: Confidence::saturating_from_basis_points(9_500),
            };
        }
    }

    ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete)
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
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::LiveListing,
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
            completed_artifacts: vec![ContentArtifactKind::LiveListing],
            in_flight_artifacts: Vec::new(),
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::PressHook,
                confidence: Confidence::saturating_from_basis_points(9_500),
            }
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
            completed_artifacts: vec![ContentArtifactKind::SignalPush],
            in_flight_artifacts: vec![ContentArtifactKind::SocialFeed],
        };

        assert_eq!(
            evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
            ContentSupplyDecision::Request {
                artifact: ContentArtifactKind::SocialStory,
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
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
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
}
