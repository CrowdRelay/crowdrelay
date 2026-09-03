//! The worker templates the brain may dispatch, as one closed vocabulary.
//!
//! These ids were a `&[&str]` in the infra snapshot loader and bare string
//! literals in every match arm that had to say something about a template.
//! Adding one meant remembering every list, and the lists were in three
//! crates. `discord-poster` reached production present in the loader and in
//! the evaluator's dispatch rules, and missing from two places that only ever
//! named strings:
//!
//! - `key_window_for_template` fell through to a 24-hour default while the
//!   policy set the cooldown to 48, so the idempotency key would have rotated
//!   twice inside one cooldown and let the same dispatch be raised again
//!   while it was still meant to be resting.
//! - the portfolio's workspace-wide audience list, so a discord post would
//!   have counted as reaching a different audience than the telegram and
//!   social posts going to the same band's own channels — no overlap penalty
//!   between three posts to the same people on the same day.
//!
//! Neither had bitten, because the template had never been selected. Both
//! would have, silently, on its first dispatch.
//!
//! An enum closes that class. A new variant does not compile until every
//! match arm answers for it, which is the only mechanism that survives
//! somebody adding a template in a hurry.

use serde::{Deserialize, Serialize};

/// A worker template the growth-intelligence brain can dispatch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerTemplate {
    RedditScanner,
    TelegramScanner,
    MetalArchivesScanner,
    BandcampScanner,
    PressPitch,
    SocialPost,
    TelegramPoster,
    DiscordPoster,
    CommunityEngager,
    SignalInviter,
    GrowthStrategist,
}

/// What audience a template's dispatch reaches, which is what decides whether
/// two dispatches compete for the same attention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateAudience {
    /// One specific community. Two dispatches to different communities reach
    /// different people and do not overlap.
    Community,
    /// The band's own audience — its channels, its press list, its fans.
    /// Every workspace-wide dispatch competes with every other one for the
    /// same attention, which is what the portfolio's overlap penalty is for.
    Workspace,
}

impl WorkerTemplate {
    /// Every template, in the order the evaluator checks them.
    pub const ALL: [Self; 11] = [
        Self::RedditScanner,
        Self::TelegramScanner,
        Self::MetalArchivesScanner,
        Self::BandcampScanner,
        Self::PressPitch,
        Self::SocialPost,
        Self::TelegramPoster,
        Self::DiscordPoster,
        Self::CommunityEngager,
        Self::SignalInviter,
        Self::GrowthStrategist,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RedditScanner => "reddit-scanner",
            Self::TelegramScanner => "telegram-scanner",
            Self::MetalArchivesScanner => "metal-archives-scanner",
            Self::BandcampScanner => "bandcamp-scanner",
            Self::PressPitch => "press-pitch",
            Self::SocialPost => "social-post",
            Self::TelegramPoster => "telegram-poster",
            Self::DiscordPoster => "discord-poster",
            Self::CommunityEngager => "community-engager",
            Self::SignalInviter => "signal-inviter",
            Self::GrowthStrategist => "growth-strategist",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == value)
    }

    /// Whether this template's dispatch is aimed at one community or at the
    /// band's own audience.
    ///
    /// Exhaustive on purpose: a template whose audience nobody decided is a
    /// template the portfolio cannot reason about, and defaulting it to
    /// "its own unique target" is what silently removes it from overlap
    /// accounting.
    #[must_use]
    pub const fn audience(self) -> TemplateAudience {
        match self {
            Self::CommunityEngager => TemplateAudience::Community,
            // Scanners and the strategist gather intelligence rather than
            // reaching anyone, but they still consume the workspace's own
            // dispatch budget, so they share its key.
            Self::RedditScanner
            | Self::TelegramScanner
            | Self::MetalArchivesScanner
            | Self::BandcampScanner
            | Self::GrowthStrategist
            // These write to the band's own audience.
            | Self::PressPitch
            | Self::SocialPost
            | Self::TelegramPoster
            | Self::DiscordPoster
            | Self::SignalInviter => TemplateAudience::Workspace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_round_trips_through_its_id() {
        for template in WorkerTemplate::ALL {
            assert_eq!(WorkerTemplate::parse(template.as_str()), Some(template));
        }
        assert_eq!(WorkerTemplate::parse("not-a-template"), None);
    }

    #[test]
    fn template_ids_are_unique() {
        let mut ids: Vec<&str> = WorkerTemplate::ALL.iter().map(|t| t.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two templates share an id");
    }

    #[test]
    fn only_the_community_engager_targets_one_community() {
        for template in WorkerTemplate::ALL {
            let expected = if template == WorkerTemplate::CommunityEngager {
                TemplateAudience::Community
            } else {
                TemplateAudience::Workspace
            };
            assert_eq!(template.audience(), expected, "{template:?}");
        }
    }

    #[test]
    fn the_serde_name_is_the_dispatch_id() {
        // The enum is persisted in a few payloads; the wire form must be the
        // string the agent service dispatches on, not the variant name.
        let json = serde_json::to_string(&WorkerTemplate::DiscordPoster).expect("serialize");
        assert_eq!(json, "\"discord-poster\"");
    }
}
