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
///
/// This is about *attention*, not budget. The portfolio's overlap penalty and
/// fatigue decay both key on the audience, and they exist to model the same
/// people being reached twice. How many dispatches a cycle affords is
/// `max_dispatches`, which is a separate question.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateAudience {
    /// One specific community. Two dispatches to different communities reach
    /// different people and do not overlap.
    Community,
    /// The band's own audience — its channels, its press list, its fans.
    /// Every dispatch here competes with every other for the same attention,
    /// which is what the overlap penalty is for.
    Workspace,
    /// Reaches nobody. Scanners and the strategist read the world and write
    /// notes; no human sees a scan.
    ///
    /// They were `Workspace`, on the reasoning that they consume the cycle's
    /// budget — but the penalty is about fatigue, not budget, and the
    /// arithmetic was brutal. Every workspace-wide candidate after the first
    /// is worth 0.7 x 0.9, the third 0.4 x 0.81, the fourth 0.1 x 0.73;
    /// meanwhile each community carries its own key and stays at full value.
    /// So one Reddit scan being selected suppressed every posting template
    /// behind it by 37%, then 68%, then 93%, and twenty-eight untouched
    /// community candidates took the remaining slots. `discord-poster` and
    /// `telegram-poster` have never once been selected in production.
    Intelligence,
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
            // Read the world, write notes. Nobody is reached.
            Self::RedditScanner
            | Self::TelegramScanner
            | Self::MetalArchivesScanner
            | Self::BandcampScanner
            | Self::GrowthStrategist => TemplateAudience::Intelligence,
            // Write to the band's own audience.
            Self::PressPitch
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
        assert_eq!(
            WorkerTemplate::CommunityEngager.audience(),
            TemplateAudience::Community
        );
        for template in WorkerTemplate::ALL {
            if template != WorkerTemplate::CommunityEngager {
                assert_ne!(
                    template.audience(),
                    TemplateAudience::Community,
                    "{template:?} is not community-scoped"
                );
            }
        }
    }

    #[test]
    fn gathering_intelligence_reaches_nobody() {
        // A scan fatigues no audience. Classing scanners as workspace-wide
        // made one selected scan cut every posting template behind it by 37%,
        // then 68%, then 93%.
        for template in [
            WorkerTemplate::RedditScanner,
            WorkerTemplate::TelegramScanner,
            WorkerTemplate::MetalArchivesScanner,
            WorkerTemplate::BandcampScanner,
            WorkerTemplate::GrowthStrategist,
        ] {
            assert_eq!(
                template.audience(),
                TemplateAudience::Intelligence,
                "{template:?}"
            );
        }
    }

    #[test]
    fn everything_that_posts_shares_the_bands_own_audience() {
        for template in [
            WorkerTemplate::PressPitch,
            WorkerTemplate::SocialPost,
            WorkerTemplate::TelegramPoster,
            WorkerTemplate::DiscordPoster,
            WorkerTemplate::SignalInviter,
        ] {
            assert_eq!(
                template.audience(),
                TemplateAudience::Workspace,
                "{template:?}"
            );
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
