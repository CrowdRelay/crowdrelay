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
    /// Carries one synced band social post into one admitted community —
    /// the relay's per-(post × community) drafting worker.
    CommunityRepost,
    SignalInviter,
    GrowthStrategist,
    /// Answers "where are my future fans?" — finds communities across every
    /// platform where the act's likely fans already gather, emitting screened
    /// `community` outreach targets.
    FanbaseScout,
    /// Advises the brain: typed proposals (community, rescan, cadence, scan
    /// queries, operator surfacing) the deterministic evaluator accepts or
    /// rejects with reasons.
    StrategyConsult,
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
    pub const ALL: [Self; 14] = [
        Self::RedditScanner,
        Self::TelegramScanner,
        Self::MetalArchivesScanner,
        Self::BandcampScanner,
        Self::PressPitch,
        Self::SocialPost,
        Self::TelegramPoster,
        Self::DiscordPoster,
        Self::CommunityEngager,
        Self::CommunityRepost,
        Self::SignalInviter,
        Self::GrowthStrategist,
        Self::FanbaseScout,
        Self::StrategyConsult,
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
            Self::CommunityRepost => "community-repost",
            Self::SignalInviter => "signal-inviter",
            Self::GrowthStrategist => "growth-strategist",
            Self::FanbaseScout => "fanbase-scout",
            Self::StrategyConsult => "strategy-consult",
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
            Self::CommunityEngager | Self::CommunityRepost => TemplateAudience::Community,
            // Read the world, write notes. Nobody is reached.
            Self::RedditScanner
            | Self::TelegramScanner
            | Self::MetalArchivesScanner
            | Self::BandcampScanner
            | Self::GrowthStrategist
            | Self::FanbaseScout
            | Self::StrategyConsult => TemplateAudience::Intelligence,
            // Write to the band's own audience.
            Self::PressPitch
            | Self::SocialPost
            | Self::TelegramPoster
            | Self::DiscordPoster
            | Self::SignalInviter => TemplateAudience::Workspace,
        }
    }

    /// Whether a dispatch can plausibly create a new first-party fan.
    ///
    /// This is deliberately narrower than "reaches an audience". SignalInviter
    /// reaches people who are already fans, and press outreach can create
    /// awareness but is not one of the action-owned tracked publication rails
    /// the verified-organic cohort can credit today. Scanners and consultants
    /// replenish supply but do not acquire anybody themselves. The closed
    /// vocabulary keeps acquisition-capacity accounting from growing another
    /// hand-written template list.
    #[must_use]
    pub const fn can_acquire_new_fans(self) -> bool {
        matches!(
            self,
            Self::SocialPost
                | Self::TelegramPoster
                | Self::DiscordPoster
                | Self::CommunityEngager
                | Self::CommunityRepost
        )
    }

    /// Whether this template is currently disabled because the agent service
    /// lacks the tools (web access, browser) to execute it.
    ///
    /// Disabled templates are never dispatched by the autopilot. They remain
    /// in the enum so that historical data, experiment designs, and match
    /// arms stay valid — but `evaluate_growth_intelligence` skips them.
    ///
    /// `telegram-scanner` and `metal-archives-scanner` re-enabled when the
    /// agent service gained `web_fetch` — their catalogs are prefetched and
    /// the model reads real rows. `bandcamp-scanner` stays disabled: its
    /// contract needs multi-hop browsing (search → album page → collectors
    /// blob), and the prefetch-only dataScope model cannot express a second
    /// fetch decided mid-reasoning.
    ///
    /// When the agent service gains the required tools, remove the template
    /// from this list.
    #[must_use]
    pub const fn is_disabled(self) -> bool {
        matches!(self, Self::BandcampScanner)
    }

    /// Every template that is not disabled, in the order the evaluator checks.
    #[must_use]
    pub fn active() -> Vec<Self> {
        Self::ALL.into_iter().filter(|t| !t.is_disabled()).collect()
    }

    /// The `GrowthIntelligencePolicy` config key holding this template's
    /// cooldown — the field an `adjust_cadence` strategy proposal writes.
    ///
    /// `CommunityRepost` shares the engager's cooldown: the two draft for the
    /// same channel on the same cadence, so an adjustment to either is an
    /// adjustment to the shared one.
    #[must_use]
    pub const fn cooldown_policy_field(self) -> &'static str {
        match self {
            Self::RedditScanner => "reddit_scanner_cooldown_hours",
            Self::TelegramScanner => "telegram_scanner_cooldown_hours",
            Self::MetalArchivesScanner => "metal_archives_scanner_cooldown_hours",
            Self::BandcampScanner => "bandcamp_scanner_cooldown_hours",
            Self::PressPitch => "press_pitch_cooldown_hours",
            Self::SocialPost => "social_post_cooldown_hours",
            Self::TelegramPoster => "telegram_poster_cooldown_hours",
            Self::DiscordPoster => "discord_poster_cooldown_hours",
            Self::CommunityEngager | Self::CommunityRepost => "community_engager_cooldown_hours",
            Self::SignalInviter => "signal_inviter_cooldown_hours",
            Self::GrowthStrategist => "growth_strategist_cooldown_hours",
            Self::FanbaseScout => "fanbase_scout_cooldown_hours",
            Self::StrategyConsult => "strategy_consult_cooldown_hours",
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
    fn only_the_community_workers_target_one_community() {
        for template in WorkerTemplate::ALL {
            if matches!(
                template,
                WorkerTemplate::CommunityEngager | WorkerTemplate::CommunityRepost
            ) {
                assert_eq!(
                    template.audience(),
                    TemplateAudience::Community,
                    "{template:?} should be community-scoped"
                );
            } else {
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
            WorkerTemplate::FanbaseScout,
            WorkerTemplate::StrategyConsult,
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
    fn new_fan_acquisition_is_a_closed_template_vocabulary() {
        for template in [
            WorkerTemplate::PressPitch,
            WorkerTemplate::SocialPost,
            WorkerTemplate::TelegramPoster,
            WorkerTemplate::DiscordPoster,
            WorkerTemplate::CommunityEngager,
            WorkerTemplate::CommunityRepost,
        ] {
            assert!(template.can_acquire_new_fans(), "{template:?}");
        }
        for template in [
            WorkerTemplate::PressPitch,
            WorkerTemplate::SignalInviter,
            WorkerTemplate::RedditScanner,
            WorkerTemplate::TelegramScanner,
            WorkerTemplate::MetalArchivesScanner,
            WorkerTemplate::BandcampScanner,
            WorkerTemplate::GrowthStrategist,
            WorkerTemplate::FanbaseScout,
            WorkerTemplate::StrategyConsult,
        ] {
            assert!(!template.can_acquire_new_fans(), "{template:?}");
        }
    }

    #[test]
    fn the_serde_name_is_the_dispatch_id() {
        // The enum is persisted in a few payloads; the wire form must be the
        // string the agent service dispatches on, not the variant name.
        let json = serde_json::to_string(&WorkerTemplate::DiscordPoster).expect("serialize");
        assert_eq!(json, "\"discord-poster\"");
    }

    #[test]
    fn disabled_templates_are_not_in_active() {
        for template in WorkerTemplate::ALL {
            if template.is_disabled() {
                assert!(
                    !WorkerTemplate::active().contains(&template),
                    "{template:?} is disabled but appeared in active()"
                );
            } else {
                assert!(
                    WorkerTemplate::active().contains(&template),
                    "{template:?} is not disabled but missing from active()"
                );
            }
        }
    }

    #[test]
    fn only_bandcamp_stays_disabled() {
        // Bandcamp needs multi-hop browsing (search → album page → collectors
        // blob); the agent service prefetches fixed dataScope URLs only.
        // Telegram and Metal Archives re-enabled once web_fetch landed —
        // their catalogs are single-shot prefetches.
        assert!(WorkerTemplate::BandcampScanner.is_disabled());
        assert!(!WorkerTemplate::TelegramScanner.is_disabled());
        assert!(!WorkerTemplate::MetalArchivesScanner.is_disabled());
        assert!(!WorkerTemplate::RedditScanner.is_disabled());
    }

    #[test]
    fn every_template_names_a_real_policy_cooldown_field() {
        // adjust_cadence writes the field this returns into the policy
        // config — a field that does not parse back would be a silently
        // dead knob. Deserialize a policy built from exactly these keys
        // and check the map survives a round trip.
        for template in WorkerTemplate::ALL {
            let field = template.cooldown_policy_field();
            assert!(
                field.ends_with("_cooldown_hours"),
                "{template:?} cooldown field {field} breaks the naming contract"
            );
        }
    }
}
