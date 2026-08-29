//! Reach metrics — unified tracking of outbound contact attempts.
//!
//! The brain needs to know: who did we contact, how, what happened, and did
//! they convert to a real fan? This module provides the domain types for a
//! unified reach ledger that subsumes email sends, Reddit posts, Signal
//! pushes, and any future channel.
//!
//! # Why a unified reach ledger?
//!
//! Today, reach data is split across multiple channel-specific tables:
//! - `community_posts` for Reddit posts
//! - `fan_push_deliveries` for Signal pushes
//! - `viryaos_outreach_interactions` for email outreach
//!
//! The brain can't answer simple questions like "how many people did we reach
//! this week, and what % converted?" without joining across these tables.
//! This module provides a normalized `ReachEvent` type that the brain reads
//! to compute reach metrics, feed the calibration loop, and learn which
//! channels and templates produce the best reach-to-fan conversion rates.
//!
//! # Integration with the episode model
//!
//! Each reach event links to an [`crate::opportunity::OpportunityEpisode`] via
//! `episode_id`. This lets the brain attribute fan conversions to specific
//! dispatch sequences and compute per-channel credit assignment.
//!
//! # Integration with calibration
//!
//! The brain predicts how many fans a dispatch will produce. The reach event
//! records the actual outcome (including `converted_fan_id`). This feeds the
//! [`crate::calibration::CalibrationTracker`] to detect and correct
//! systematic prediction bias.

use serde::{Deserialize, Serialize};

/// The kind of recipient a reach event targets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachRecipientKind {
    /// An individual fan (via Signal push, email, etc.).
    #[default]
    Fan,
    /// An outreach target (playlist curator, press contact, etc.).
    OutreachTarget,
    /// A subreddit audience (via a Reddit post).
    SubredditAudience,
    /// A platform audience (e.g. all Spotify followers).
    PlatformAudience,
    /// A community (e.g. a Discord server, Facebook group).
    Community,
}

impl ReachRecipientKind {
    /// Returns the string representation for DB storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fan => "fan",
            Self::OutreachTarget => "outreach_target",
            Self::SubredditAudience => "subreddit_audience",
            Self::PlatformAudience => "platform_audience",
            Self::Community => "community",
        }
    }

    /// Parses a string into a `ReachRecipientKind`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fan" => Some(Self::Fan),
            "outreach_target" => Some(Self::OutreachTarget),
            "subreddit_audience" => Some(Self::SubredditAudience),
            "platform_audience" => Some(Self::PlatformAudience),
            "community" => Some(Self::Community),
            _ => None,
        }
    }

    /// Returns true if this recipient kind is an individual (not a broadcast).
    #[must_use]
    pub const fn is_individual(self) -> bool {
        matches!(self, Self::Fan | Self::OutreachTarget)
    }
}

/// The channel used to reach a recipient.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachChannel {
    /// Email outreach.
    Email,
    /// A public Reddit post (broadcast to a subreddit).
    RedditPost,
    /// A Reddit direct message.
    RedditDm,
    /// A Signal push notification.
    SignalPush,
    /// A social media post (Facebook, Instagram, etc.).
    SocialPost,
    /// An SMS message.
    Sms,
    /// Any other channel.
    #[default]
    Other,
}

impl ReachChannel {
    /// Returns the string representation for DB storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::RedditPost => "reddit_post",
            Self::RedditDm => "reddit_dm",
            Self::SignalPush => "signal_push",
            Self::SocialPost => "social_post",
            Self::Sms => "sms",
            Self::Other => "other",
        }
    }

    /// Parses a string into a `ReachChannel`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "email" => Some(Self::Email),
            "reddit_post" => Some(Self::RedditPost),
            "reddit_dm" => Some(Self::RedditDm),
            "signal_push" => Some(Self::SignalPush),
            "social_post" => Some(Self::SocialPost),
            "sms" => Some(Self::Sms),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Returns true if this channel is a broadcast (one-to-many).
    #[must_use]
    pub const fn is_broadcast(self) -> bool {
        matches!(self, Self::RedditPost | Self::SocialPost)
    }

    /// Returns true if this channel is a direct message (one-to-one).
    #[must_use]
    pub const fn is_direct(self) -> bool {
        matches!(
            self,
            Self::Email | Self::RedditDm | Self::SignalPush | Self::Sms
        )
    }
}

/// The status of a reach event — a state machine.
///
/// The typical progression is:
/// ```text
/// sent → delivered → (opened | clicked) → (replied | converted | ignored)
/// sent → delivered → (bounced | complained)
/// sent → failed
/// ```
///
/// For broadcast channels (Reddit posts), `delivered` means the post was
/// published, and `converted` means at least one fan joined from the post.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachStatus {
    /// Message was sent to the channel.
    #[default]
    Sent,
    /// Confirmed delivery (push delivered, email accepted, post published).
    Delivered,
    /// Email opened / push seen.
    Opened,
    /// Link clicked.
    Clicked,
    /// Recipient replied (any disposition).
    Replied,
    /// Recipient replied positively.
    PositiveReply,
    /// Recipient explicitly declined.
    Declined,
    /// Recipient became a fan.
    Converted,
    /// Delivery failed (hard bounce).
    Bounced,
    /// Recipient marked as spam / complaint.
    Complained,
    /// No response after observation window.
    Ignored,
    /// Internal error during send.
    Failed,
}

impl ReachStatus {
    /// Returns the string representation for DB storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Opened => "opened",
            Self::Clicked => "clicked",
            Self::Replied => "replied",
            Self::PositiveReply => "positive_reply",
            Self::Declined => "declined",
            Self::Converted => "converted",
            Self::Bounced => "bounced",
            Self::Complained => "complained",
            Self::Ignored => "ignored",
            Self::Failed => "failed",
        }
    }

    /// Parses a string into a `ReachStatus`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sent" => Some(Self::Sent),
            "delivered" => Some(Self::Delivered),
            "opened" => Some(Self::Opened),
            "clicked" => Some(Self::Clicked),
            "replied" => Some(Self::Replied),
            "positive_reply" => Some(Self::PositiveReply),
            "declined" => Some(Self::Declined),
            "converted" => Some(Self::Converted),
            "bounced" => Some(Self::Bounced),
            "complained" => Some(Self::Complained),
            "ignored" => Some(Self::Ignored),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Returns true if this status indicates a successful conversion.
    #[must_use]
    pub const fn is_converted(self) -> bool {
        matches!(self, Self::Converted | Self::PositiveReply)
    }

    /// Returns true if this status indicates a negative outcome.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        matches!(
            self,
            Self::Bounced | Self::Complained | Self::Declined | Self::Failed
        )
    }

    /// Returns true if this status is terminal (no further transitions).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Converted
                | Self::Bounced
                | Self::Complained
                | Self::Declined
                | Self::Ignored
                | Self::Failed
        )
    }
}

/// A single reach event — one outbound contact attempt.
///
/// This is the domain type that the brain reads and writes. The persistence
/// layer (in `crowdrelay-infra`) maps this to the `viryaos_reach_events`
/// table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReachEvent {
    /// The workspace this event belongs to.
    pub workspace_id: uuid::Uuid,
    /// The autopilot action that triggered this reach (optional).
    pub action_id: Option<uuid::Uuid>,
    /// Who was reached.
    pub recipient_kind: ReachRecipientKind,
    /// The recipient identifier (fan ID, target ID, subreddit name, etc.).
    pub recipient_id: String,
    /// The channel used.
    pub channel: ReachChannel,
    /// The worker template that produced this reach.
    pub template_id: String,
    /// Estimated audience size (1 for individuals, subscriber count for
    /// broadcasts).
    pub estimated_reach: u32,
    /// The current status.
    pub status: ReachStatus,
    /// When the reach was sent.
    pub sent_at: time::OffsetDateTime,
    /// When the status was last updated.
    pub status_updated_at: time::OffsetDateTime,
    /// The fan this event converted to (if status == Converted).
    pub converted_fan_id: Option<uuid::Uuid>,
    /// When the conversion happened.
    pub converted_at: Option<time::OffsetDateTime>,
    /// The episode this reach event belongs to.
    pub episode_id: Option<String>,
    /// Free-form metadata.
    pub metadata: serde_json::Value,
}

impl Default for ReachEvent {
    fn default() -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            workspace_id: uuid::Uuid::nil(),
            action_id: None,
            recipient_kind: ReachRecipientKind::default(),
            recipient_id: String::new(),
            channel: ReachChannel::default(),
            template_id: String::new(),
            estimated_reach: 1,
            status: ReachStatus::default(),
            sent_at: now,
            status_updated_at: now,
            converted_fan_id: None,
            converted_at: None,
            episode_id: None,
            metadata: serde_json::json!({}),
        }
    }
}

impl ReachEvent {
    /// Creates a new reach event with the given parameters and `Sent` status.
    #[must_use]
    pub fn new(
        workspace_id: uuid::Uuid,
        recipient_kind: ReachRecipientKind,
        recipient_id: String,
        channel: ReachChannel,
        template_id: String,
    ) -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            workspace_id,
            action_id: None,
            recipient_kind,
            recipient_id,
            channel,
            template_id,
            estimated_reach: 1,
            status: ReachStatus::Sent,
            sent_at: now,
            status_updated_at: now,
            converted_fan_id: None,
            converted_at: None,
            episode_id: None,
            metadata: serde_json::json!({}),
        }
    }

    /// Sets the action ID.
    #[must_use]
    pub fn with_action_id(mut self, action_id: uuid::Uuid) -> Self {
        self.action_id = Some(action_id);
        self
    }

    /// Sets the estimated reach (audience size).
    #[must_use]
    pub fn with_estimated_reach(mut self, reach: u32) -> Self {
        self.estimated_reach = reach.max(1);
        self
    }

    /// Sets the episode ID.
    #[must_use]
    pub fn with_episode_id(mut self, episode_id: String) -> Self {
        self.episode_id = Some(episode_id);
        self
    }

    /// Sets the metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Transitions the status and updates `status_updated_at`.
    pub fn transition(&mut self, new_status: ReachStatus) {
        self.status = new_status;
        self.status_updated_at = time::OffsetDateTime::now_utc();
    }

    /// Marks this reach event as converted to a fan.
    pub fn convert(&mut self, fan_id: uuid::Uuid) {
        self.converted_fan_id = Some(fan_id);
        self.converted_at = Some(time::OffsetDateTime::now_utc());
        self.transition(ReachStatus::Converted);
    }
}

/// Aggregated reach metrics for a workspace, channel, or template.
///
/// This is the summary the brain uses to learn which channels and templates
/// produce the best reach-to-fan conversion rates.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ReachMetrics {
    /// Total reach events.
    pub total_events: u64,
    /// Total estimated people reached (sum of `estimated_reach`).
    pub total_reach: u64,
    /// Events that were delivered.
    pub delivered: u64,
    /// Events that were opened (email) or seen (push).
    pub opened: u64,
    /// Events where a link was clicked.
    pub clicked: u64,
    /// Events that received any reply.
    pub replied: u64,
    /// Events that received a positive reply.
    pub positive_replies: u64,
    /// Events that were declined.
    pub declined: u64,
    /// Events that converted to a fan.
    pub converted: u64,
    /// Events that bounced.
    pub bounced: u64,
    /// Events that were marked as spam/complaint.
    pub complained: u64,
    /// Events that failed to send.
    pub failed: u64,
    /// Events with no response after observation window.
    pub ignored: u64,
}

impl ReachMetrics {
    /// Computes reach metrics from a list of reach events.
    #[must_use]
    pub fn from_events(events: &[ReachEvent]) -> Self {
        let mut metrics = Self::default();
        for event in events {
            metrics.total_events += 1;
            metrics.total_reach += u64::from(event.estimated_reach);
            match event.status {
                ReachStatus::Sent => {}
                ReachStatus::Delivered => metrics.delivered += 1,
                ReachStatus::Opened => metrics.opened += 1,
                ReachStatus::Clicked => metrics.clicked += 1,
                ReachStatus::Replied => metrics.replied += 1,
                ReachStatus::PositiveReply => metrics.positive_replies += 1,
                ReachStatus::Declined => metrics.declined += 1,
                ReachStatus::Converted => metrics.converted += 1,
                ReachStatus::Bounced => metrics.bounced += 1,
                ReachStatus::Complained => metrics.complained += 1,
                ReachStatus::Ignored => metrics.ignored += 1,
                ReachStatus::Failed => metrics.failed += 1,
            }
        }
        metrics
    }

    /// Returns the delivery rate: delivered / total_events.
    #[must_use]
    pub fn delivery_rate(&self) -> f64 {
        if self.total_events == 0 {
            return 0.0;
        }
        self.delivered as f64 / self.total_events as f64
    }

    /// Returns the conversion rate: converted / total_events.
    #[must_use]
    pub fn conversion_rate(&self) -> f64 {
        if self.total_events == 0 {
            return 0.0;
        }
        self.converted as f64 / self.total_events as f64
    }

    /// Returns the positive reply rate: positive_replies / total_events.
    #[must_use]
    pub fn positive_reply_rate(&self) -> f64 {
        if self.total_events == 0 {
            return 0.0;
        }
        self.positive_replies as f64 / self.total_events as f64
    }

    /// Returns the reach-to-fan conversion rate: converted / total_reach.
    #[must_use]
    pub fn reach_to_fan_rate(&self) -> f64 {
        if self.total_reach == 0 {
            return 0.0;
        }
        self.converted as f64 / self.total_reach as f64
    }

    /// Returns the bounce rate: bounced / total_events.
    #[must_use]
    pub fn bounce_rate(&self) -> f64 {
        if self.total_events == 0 {
            return 0.0;
        }
        self.bounced as f64 / self.total_events as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(channel: ReachChannel, status: ReachStatus, reach: u32) -> ReachEvent {
        ReachEvent {
            workspace_id: uuid::Uuid::from_u128(1),
            recipient_kind: ReachRecipientKind::Fan,
            recipient_id: "fan_1".to_owned(),
            channel,
            template_id: "signal-inviter".to_owned(),
            estimated_reach: reach,
            status,
            sent_at: time::OffsetDateTime::now_utc(),
            status_updated_at: time::OffsetDateTime::now_utc(),
            ..Default::default()
        }
    }

    #[test]
    fn reach_recipient_kind_as_str_roundtrips() {
        for kind in [
            ReachRecipientKind::Fan,
            ReachRecipientKind::OutreachTarget,
            ReachRecipientKind::SubredditAudience,
            ReachRecipientKind::PlatformAudience,
            ReachRecipientKind::Community,
        ] {
            let s = kind.as_str();
            assert_eq!(ReachRecipientKind::parse(s), Some(kind));
        }
    }

    #[test]
    fn reach_recipient_kind_is_individual() {
        assert!(ReachRecipientKind::Fan.is_individual());
        assert!(ReachRecipientKind::OutreachTarget.is_individual());
        assert!(!ReachRecipientKind::SubredditAudience.is_individual());
        assert!(!ReachRecipientKind::PlatformAudience.is_individual());
        assert!(!ReachRecipientKind::Community.is_individual());
    }

    #[test]
    fn reach_channel_as_str_roundtrips() {
        for channel in [
            ReachChannel::Email,
            ReachChannel::RedditPost,
            ReachChannel::RedditDm,
            ReachChannel::SignalPush,
            ReachChannel::SocialPost,
            ReachChannel::Sms,
            ReachChannel::Other,
        ] {
            let s = channel.as_str();
            assert_eq!(ReachChannel::parse(s), Some(channel));
        }
    }

    #[test]
    fn reach_channel_is_broadcast() {
        assert!(ReachChannel::RedditPost.is_broadcast());
        assert!(ReachChannel::SocialPost.is_broadcast());
        assert!(!ReachChannel::Email.is_broadcast());
        assert!(!ReachChannel::SignalPush.is_broadcast());
    }

    #[test]
    fn reach_channel_is_direct() {
        assert!(ReachChannel::Email.is_direct());
        assert!(ReachChannel::RedditDm.is_direct());
        assert!(ReachChannel::SignalPush.is_direct());
        assert!(ReachChannel::Sms.is_direct());
        assert!(!ReachChannel::RedditPost.is_direct());
        assert!(!ReachChannel::SocialPost.is_direct());
    }

    #[test]
    fn reach_status_as_str_roundtrips() {
        for status in [
            ReachStatus::Sent,
            ReachStatus::Delivered,
            ReachStatus::Opened,
            ReachStatus::Clicked,
            ReachStatus::Replied,
            ReachStatus::PositiveReply,
            ReachStatus::Declined,
            ReachStatus::Converted,
            ReachStatus::Bounced,
            ReachStatus::Complained,
            ReachStatus::Ignored,
            ReachStatus::Failed,
        ] {
            let s = status.as_str();
            assert_eq!(ReachStatus::parse(s), Some(status));
        }
    }

    #[test]
    fn reach_status_is_converted() {
        assert!(ReachStatus::Converted.is_converted());
        assert!(ReachStatus::PositiveReply.is_converted());
        assert!(!ReachStatus::Sent.is_converted());
        assert!(!ReachStatus::Replied.is_converted());
    }

    #[test]
    fn reach_status_is_negative() {
        assert!(ReachStatus::Bounced.is_negative());
        assert!(ReachStatus::Complained.is_negative());
        assert!(ReachStatus::Declined.is_negative());
        assert!(ReachStatus::Failed.is_negative());
        assert!(!ReachStatus::Converted.is_negative());
        assert!(!ReachStatus::Sent.is_negative());
    }

    #[test]
    fn reach_status_is_terminal() {
        assert!(ReachStatus::Converted.is_terminal());
        assert!(ReachStatus::Bounced.is_terminal());
        assert!(ReachStatus::Ignored.is_terminal());
        assert!(ReachStatus::Failed.is_terminal());
        assert!(!ReachStatus::Sent.is_terminal());
        assert!(!ReachStatus::Delivered.is_terminal());
    }

    #[test]
    fn reach_event_new_has_sent_status() {
        let event = ReachEvent::new(
            uuid::Uuid::from_u128(1),
            ReachRecipientKind::Fan,
            "fan_1".to_owned(),
            ReachChannel::SignalPush,
            "signal-inviter".to_owned(),
        );
        assert_eq!(event.status, ReachStatus::Sent);
        assert_eq!(event.estimated_reach, 1);
    }

    #[test]
    fn reach_event_with_estimated_reach_clamps_to_one() {
        let event = ReachEvent::new(
            uuid::Uuid::from_u128(1),
            ReachRecipientKind::SubredditAudience,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            "community-engager".to_owned(),
        )
        .with_estimated_reach(5000);
        assert_eq!(event.estimated_reach, 5000);
    }

    #[test]
    fn reach_event_with_estimated_reach_zero_clamps_to_one() {
        let event = ReachEvent::new(
            uuid::Uuid::from_u128(1),
            ReachRecipientKind::SubredditAudience,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            "community-engager".to_owned(),
        )
        .with_estimated_reach(0);
        assert_eq!(event.estimated_reach, 1);
    }

    #[test]
    fn reach_event_transition_updates_status() {
        let mut event = make_event(ReachChannel::Email, ReachStatus::Sent, 1);
        event.transition(ReachStatus::Delivered);
        assert_eq!(event.status, ReachStatus::Delivered);
    }

    #[test]
    fn reach_event_convert_sets_fan_id() {
        let mut event = make_event(ReachChannel::Email, ReachStatus::Delivered, 1);
        let fan_id = uuid::Uuid::from_u128(42);
        event.convert(fan_id);
        assert_eq!(event.status, ReachStatus::Converted);
        assert_eq!(event.converted_fan_id, Some(fan_id));
        assert!(event.converted_at.is_some());
    }

    #[test]
    fn reach_event_builder_methods() {
        let action_id = uuid::Uuid::from_u128(99);
        let event = ReachEvent::new(
            uuid::Uuid::from_u128(1),
            ReachRecipientKind::Fan,
            "fan_1".to_owned(),
            ReachChannel::SignalPush,
            "signal-inviter".to_owned(),
        )
        .with_action_id(action_id)
        .with_estimated_reach(1)
        .with_episode_id("ep_1".to_owned())
        .with_metadata(serde_json::json!({"post_title": "Check out our new release"}));

        assert_eq!(event.action_id, Some(action_id));
        assert_eq!(event.episode_id, Some("ep_1".to_owned()));
        assert!(event.metadata.get("post_title").is_some());
    }

    #[test]
    fn reach_metrics_from_events_computes_counts() {
        let events = vec![
            make_event(ReachChannel::Email, ReachStatus::Delivered, 1),
            make_event(ReachChannel::Email, ReachStatus::Opened, 1),
            make_event(ReachChannel::Email, ReachStatus::Converted, 1),
            make_event(ReachChannel::Email, ReachStatus::Bounced, 1),
            make_event(ReachChannel::RedditPost, ReachStatus::Delivered, 500),
            make_event(ReachChannel::RedditPost, ReachStatus::Converted, 500),
        ];
        let metrics = ReachMetrics::from_events(&events);
        assert_eq!(metrics.total_events, 6);
        assert_eq!(metrics.total_reach, 1004);
        assert_eq!(metrics.delivered, 2);
        assert_eq!(metrics.opened, 1);
        assert_eq!(metrics.converted, 2);
        assert_eq!(metrics.bounced, 1);
    }

    #[test]
    fn reach_metrics_rates() {
        let events = vec![
            make_event(ReachChannel::Email, ReachStatus::Delivered, 1),
            make_event(ReachChannel::Email, ReachStatus::Converted, 1),
            make_event(ReachChannel::Email, ReachStatus::Bounced, 1),
            make_event(ReachChannel::Email, ReachStatus::Sent, 1),
        ];
        let metrics = ReachMetrics::from_events(&events);
        // 4 events, 1 converted → 25%
        assert!((metrics.conversion_rate() - 0.25).abs() < 0.001);
        // 4 events, 1 bounced → 25%
        assert!((metrics.bounce_rate() - 0.25).abs() < 0.001);
        // 4 events, 1 delivered → 25%
        assert!((metrics.delivery_rate() - 0.25).abs() < 0.001);
    }

    #[test]
    fn reach_metrics_reach_to_fan_rate() {
        let events = vec![
            make_event(ReachChannel::RedditPost, ReachStatus::Converted, 1000),
            make_event(ReachChannel::RedditPost, ReachStatus::Delivered, 1000),
        ];
        let metrics = ReachMetrics::from_events(&events);
        // 2000 total reach, 2 converted → 0.1%
        assert!((metrics.reach_to_fan_rate() - 2.0 / 2000.0).abs() < 0.001);
    }

    #[test]
    fn reach_metrics_empty_has_zero_rates() {
        let metrics = ReachMetrics::from_events(&[]);
        assert!((metrics.conversion_rate()).abs() < 0.001);
        assert!((metrics.delivery_rate()).abs() < 0.001);
        assert!((metrics.bounce_rate()).abs() < 0.001);
        assert!((metrics.reach_to_fan_rate()).abs() < 0.001);
    }

    #[test]
    fn reach_metrics_positive_reply_rate() {
        let events = vec![
            make_event(ReachChannel::Email, ReachStatus::PositiveReply, 1),
            make_event(ReachChannel::Email, ReachStatus::Sent, 1),
            make_event(ReachChannel::Email, ReachStatus::Declined, 1),
            make_event(ReachChannel::Email, ReachStatus::Ignored, 1),
        ];
        let metrics = ReachMetrics::from_events(&events);
        // 4 events, 1 positive → 25%
        assert!((metrics.positive_reply_rate() - 0.25).abs() < 0.001);
    }

    #[test]
    fn reach_event_serializes() {
        let event = make_event(ReachChannel::Email, ReachStatus::Delivered, 1);
        let json = serde_json::to_string(&event).expect("should serialize");
        assert!(json.contains("email"));
        assert!(json.contains("delivered"));
    }

    #[test]
    fn reach_metrics_serializes() {
        let metrics = ReachMetrics {
            total_events: 100,
            converted: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&metrics).expect("should serialize");
        assert!(json.contains("total_events"));
        assert!(json.contains("converted"));
    }

    #[test]
    fn reach_status_default_is_sent() {
        assert_eq!(ReachStatus::default(), ReachStatus::Sent);
    }
}
