//! Reach metrics — unified tracking of outbound contact attempts.
//!
//! The brain needs to know: who did we contact, how, what happened, and did
//! they convert to a real fan? This module provides the domain types for a
//! unified reach ledger that subsumes email sends, Reddit posts, Signal
//! pushes, and any future channel.
//!
//! `ReachMetrics` is loaded by the API growth metrics endpoint and the
//! autopilot evaluator. `ReachChannel` is used by the evidence recording
//! path to tag each dispatch with its delivery channel.

use serde::{Deserialize, Serialize};

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

    /// Returns true if this channel is a broadcast (one-to-many). Broadcast
    /// channels use a Beta-Binomial model: one action exposes N people, K of
    /// whom convert. The conversion model updates α += K, β += (N - K).
    #[must_use]
    pub const fn is_broadcast(self) -> bool {
        matches!(self, Self::RedditPost | Self::SocialPost | Self::SignalPush)
    }

    /// Returns true if this channel is a direct message (one-to-one). Direct
    /// channels use a Beta-Bernoulli model: one action exposes 1 person, who
    /// either converts or not. The conversion model updates α += 1 or β += 1.
    #[must_use]
    pub const fn is_direct(self) -> bool {
        matches!(self, Self::Email | Self::RedditDm | Self::Sms)
    }
}

/// Aggregated reach metrics for a workspace, channel, or template.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReachMetrics {
    /// Total reach events.
    pub total_events: u64,
    /// Total estimated people reached (sum of `estimated_reach`). Gross —
    /// same person may be counted multiple times. Use `unique_reach` for
    /// deduplicated reach.
    pub total_reach: u64,
    /// Unique recipients reached (distinct `recipient_id` values).
    pub unique_reach: u64,
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
    /// Total fan conversions from the conversions table (true count for broadcasts).
    pub total_conversions: u64,
    /// Incremental conversions (wouldn't have happened without the action).
    pub incremental_conversions: u64,
    /// Durable conversions (fan still active after 30 days).
    pub durable_conversions: u64,
}

impl ReachMetrics {
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

    #[test]
    fn reach_channel_round_trip() {
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
    fn reach_channel_broadcast_classification() {
        assert!(ReachChannel::RedditPost.is_broadcast());
        assert!(ReachChannel::SocialPost.is_broadcast());
        assert!(ReachChannel::SignalPush.is_broadcast());
        assert!(!ReachChannel::Email.is_broadcast());
        assert!(!ReachChannel::RedditDm.is_broadcast());
    }

    #[test]
    fn reach_channel_direct_classification() {
        assert!(ReachChannel::Email.is_direct());
        assert!(ReachChannel::RedditDm.is_direct());
        assert!(ReachChannel::Sms.is_direct());
        assert!(!ReachChannel::RedditPost.is_direct());
        assert!(!ReachChannel::SocialPost.is_direct());
    }

    #[test]
    fn reach_metrics_rates_handle_zero() {
        let metrics = ReachMetrics::default();
        assert!((metrics.delivery_rate() - 0.0).abs() < 1e-9);
        assert!((metrics.conversion_rate() - 0.0).abs() < 1e-9);
        assert!((metrics.reach_to_fan_rate() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn reach_metrics_rates_compute() {
        let metrics = ReachMetrics {
            total_events: 100,
            delivered: 80,
            converted: 5,
            total_reach: 1000,
            ..Default::default()
        };
        assert!((metrics.delivery_rate() - 0.8).abs() < 1e-9);
        assert!((metrics.conversion_rate() - 0.05).abs() < 1e-9);
        assert!((metrics.reach_to_fan_rate() - 0.005).abs() < 1e-9);
    }
}
