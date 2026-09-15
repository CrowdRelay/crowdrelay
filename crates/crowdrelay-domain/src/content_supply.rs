//! Deterministic content supply-chain bounded context.
//!
//! The domain schedules provider-neutral artifact requests from trusted source
//! facts. It does not generate prose, choose a social provider, or depend on an
//! LLM. Existing approved templates remain the executable language surface.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{ContentSourceId, autonomy::Confidence, release_autopilot::ReleaseTier};

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
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ContentSupplyPolicy {
    pub maximum_source_age_days: u32,
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

    for artifact in required_artifacts(snapshot.source_kind) {
        if !artifact_owed(snapshot, *artifact) {
            continue;
        }
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
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
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
            communication_enabled: Some(true),
            press_enabled: Some(true),
            release_tier: Some(ReleaseTier::Single),
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
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
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
}
