//! A lifecycle message answers one real moment, not each evaluation cycle.

use crate::audience_lifecycle::{FanLifecycleSnapshot, LifecycleTemplate};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEpisode {
    pub key: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub since: Option<OffsetDateTime>,
    pub ticket_count: Option<u32>,
    pub event_slug: Option<String>,
}

impl LifecycleEpisode {
    /// Scoped by fan and template by the caller. An unrelated message, policy
    /// edit or event interest cannot re-arm a thank-you for the same milestone.
    #[must_use]
    pub fn for_message(
        snapshot: &FanLifecycleSnapshot,
        template: LifecycleTemplate,
    ) -> Option<Self> {
        let mut episode = Self {
            key: "once".to_owned(),
            since: None,
            ticket_count: None,
            event_slug: None,
        };
        match template {
            LifecycleTemplate::Welcome
            | LifecycleTemplate::FirstTicketThankYou
            | LifecycleTemplate::ReferralInvite
            | LifecycleTemplate::SignalInstallAsk => {}
            LifecycleTemplate::ReturningFanThankYou => {
                episode.ticket_count = Some(snapshot.paid_ticket_count);
                episode.key = format!("shows-{}", snapshot.paid_ticket_count);
            }
            LifecycleTemplate::ReferralThankYou => {
                episode.since = Some(snapshot.last_qualified_referral_at?)
            }
            LifecycleTemplate::SynesthesiaFollowUp => {
                episode.since = Some(snapshot.synesthesia_completed_at?)
            }
            LifecycleTemplate::DormantReactivation => {
                episode.since = Some(
                    snapshot
                        .latest_engagement_at()
                        .unwrap_or(snapshot.created_at),
                );
            }
            LifecycleTemplate::ShowRecall => {
                let checkin = snapshot.recent_checkin.as_ref()?;
                episode.since = Some(checkin.checked_in_at);
                episode.event_slug = Some(checkin.event_slug.clone());
            }
        }
        if let Some(at) = episode.since {
            episode.key = format!(
                "{}:{}",
                episode.event_slug.as_deref().unwrap_or("at"),
                at.unix_timestamp_nanos()
            );
        }
        Some(episode)
    }
}
