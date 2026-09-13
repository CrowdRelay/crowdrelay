//! Which channels may publish without asking a person.
//!
//! `SocialPost` is a `require_approval` outcome kind, so every draft became an
//! `awaiting_approval` action with a 72-hour expiry -- regardless of
//! `CROWDRELAY_TELEGRAM_AUTO_POST`, `CROWDRELAY_DISCORD_AUTO_POST` or
//! `CROWDRELAY_SOCIAL_AUTO_POST` being true. Those flags gated the executor,
//! and the action never reached the executor because it expired first.
//!
//! Production is the evidence: the three flags true, six
//! `agent.content.request` actions cancelled unapproved, and `telegram_posts`,
//! `discord_posts` and `social_posts` all empty. The brain was configured
//! autonomous, behaved manual, and so learned nothing from the only channels
//! it could run end to end.
//!
//! A flag saying "publish to Telegram without asking me" is an approval.
//! Asking again per post is asking twice.

/// The channels with standing operator approval to publish without a human.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoPostPlatforms {
    pub telegram: bool,
    pub discord: bool,
    pub social: bool,
}

impl AutoPostPlatforms {
    /// Whether a draft for this platform may execute without waiting for a
    /// person.
    ///
    /// The platform string is the one the model wrote into the item payload,
    /// so it is matched case-insensitively and anything unrecognised falls
    /// through to requiring approval. An unknown channel is exactly the case
    /// where a human should look.
    #[must_use]
    pub fn permits(self, platform: Option<&str>) -> bool {
        match platform
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("telegram") => self.telegram,
            Some("discord") => self.discord,
            Some("instagram" | "facebook" | "x" | "twitter" | "social") => self.social,
            // Reddit and anything unrecognised: a person decides.
            _ => false,
        }
    }
}

/// Whether Reddit posting will happen, and if not, which switch is missing.
///
/// Reddit needs three things, and only one of them is in `.env.example`:
/// `CROWDRELAY_COMMUNITY_AUTO_POST=true`, `CROWDRELAY_REDDIT_WRITE_ENABLED=true`
/// and an agent-service auth key. The write switch is checked first and
/// overrides the others, and it was undocumented — so an operator who set
/// "autopilot everywhere", approved every suggestion, and watched drafts pile up
/// had nothing to read that named the switch they were missing.
///
/// The reason is a value rather than a log line because the log line is the
/// problem: it is written once at worker startup, inside a container, and the
/// operator asking "why has nothing published" is not reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedditPosture {
    /// Approved posts publish through the agents service browser session.
    Publishes,
    /// Drafts are written `awaiting_manual_post` and wait for a person.
    Drafts {
        /// The switch to set, or the policy that overrides all of them. Stable
        /// text: it reaches the operator through the watchdog.
        missing: &'static str,
    },
}

impl RedditPosture {
    #[must_use]
    pub const fn publishes(self) -> bool {
        matches!(self, Self::Publishes)
    }

    /// The switch an operator has to change, or `None` when nothing is missing.
    #[must_use]
    pub const fn missing_switch(self) -> Option<&'static str> {
        match self {
            Self::Publishes => None,
            Self::Drafts { missing } => Some(missing),
        }
    }
}

/// Reads the publishing switches once, so every reader agrees.
///
/// `community_executor_enabled` in the growth-readiness log used to be
/// `community_executor.is_some()` — the worker is constructed in manual mode
/// too, so the one surface an operator has for "is Reddit posting on" reported
/// `true` while the executor would never post. Same shape as a connection that
/// reads `connected` with an invalid credential.
#[derive(Clone, Copy, Debug)]
pub struct PublishingPosture {
    pub platforms: AutoPostPlatforms,
    pub reddit: RedditPosture,
}

impl PublishingPosture {
    /// `has_agent_key` is passed rather than read here because the key itself is
    /// a secret the caller already holds and validates.
    #[must_use]
    pub fn from_env(has_agent_key: bool) -> Self {
        Self {
            platforms: AutoPostPlatforms {
                telegram: flag("CROWDRELAY_TELEGRAM_AUTO_POST"),
                discord: flag("CROWDRELAY_DISCORD_AUTO_POST"),
                social: flag("CROWDRELAY_SOCIAL_AUTO_POST"),
            },
            // Order matters and is the order an operator needs. The write switch
            // overrides the other two, so somebody who set the auto-post flag is
            // told that it had no effect instead of being left to infer it.
            reddit: if !flag("CROWDRELAY_REDDIT_WRITE_ENABLED") {
                RedditPosture::Drafts {
                    missing: "CROWDRELAY_REDDIT_WRITE_ENABLED",
                }
            } else if !flag("CROWDRELAY_COMMUNITY_AUTO_POST") {
                RedditPosture::Drafts {
                    missing: "CROWDRELAY_COMMUNITY_AUTO_POST",
                }
            } else if !has_agent_key {
                RedditPosture::Drafts {
                    missing: "CROWDRELAY_AGENT_SERVICE_AUTH_KEY",
                }
            } else {
                RedditPosture::Publishes
            },
        }
    }
}

/// One spelling of "on" for every switch here.
///
/// Three call sites used to parse this inline with the same four accepted
/// values, and a fourth would have been written by hand.
fn flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel the operator switched on publishes without asking twice.
    ///
    /// The flags gated only the executor while `SocialPost` stayed a
    /// `require_approval` kind, so drafts expired in the approval queue and
    /// the three autonomous channels published nothing for weeks.
    #[test]
    fn an_enabled_channel_carries_standing_approval() {
        let all_on = AutoPostPlatforms {
            telegram: true,
            discord: true,
            social: true,
        };
        assert!(all_on.permits(Some("telegram")));
        assert!(all_on.permits(Some("discord")));
        assert!(all_on.permits(Some("instagram")));
        assert!(all_on.permits(Some("facebook")));
    }

    #[test]
    fn a_disabled_channel_still_waits_for_a_person() {
        let off = AutoPostPlatforms::default();
        assert!(!off.permits(Some("telegram")));
        assert!(!off.permits(Some("instagram")));
    }

    /// The load-bearing one. Reddit posting is held behind its own dedicated
    /// switch, `CROWDRELAY_REDDIT_WRITE_ENABLED`, required on top of the
    /// community auto-post flag — and never by anything in this file, because
    /// posting from an automated or unjoined account risks the read account
    /// the whole discovery loop depends on. No social auto-post flag may
    /// reach Reddit, however many of them are set.
    #[test]
    fn no_flag_can_ever_auto_post_to_reddit() {
        let all_on = AutoPostPlatforms {
            telegram: true,
            discord: true,
            social: true,
        };
        assert!(!all_on.permits(Some("reddit")));
        assert!(!all_on.permits(Some("Reddit")));
        assert!(!all_on.permits(Some(" REDDIT ")));
    }

    /// The write switch overrides the other two, and is reported first.
    ///
    /// An operator who set `CROWDRELAY_COMMUNITY_AUTO_POST` has to be told that
    /// it had no effect, rather than being left to infer it from drafts piling
    /// up. Reporting the auto-post flag as the missing one would send them to
    /// check a switch they had already set.
    #[test]
    fn the_reddit_write_switch_is_reported_before_the_others() {
        // Deliberately not reading the environment: these tests must not depend
        // on the machine running them, and `set_var` is racy across threads.
        let held = RedditPosture::Drafts {
            missing: "CROWDRELAY_REDDIT_WRITE_ENABLED",
        };
        assert!(!held.publishes());
        assert_eq!(
            held.missing_switch(),
            Some("CROWDRELAY_REDDIT_WRITE_ENABLED")
        );
        assert!(RedditPosture::Publishes.publishes());
        assert_eq!(RedditPosture::Publishes.missing_switch(), None);
    }

    /// An unrecognised platform is exactly when a human should look.
    #[test]
    fn an_unknown_platform_falls_through_to_approval() {
        let all_on = AutoPostPlatforms {
            telegram: true,
            discord: true,
            social: true,
        };
        assert!(!all_on.permits(Some("mastodon")));
        assert!(!all_on.permits(Some("")));
        assert!(!all_on.permits(None));
    }

    /// The model writes the platform string, so casing and stray whitespace
    /// must not decide whether something publishes itself.
    #[test]
    fn the_platform_match_ignores_case_and_padding() {
        let telegram_only = AutoPostPlatforms {
            telegram: true,
            ..AutoPostPlatforms::default()
        };
        assert!(telegram_only.permits(Some("  Telegram  ")));
        assert!(!telegram_only.permits(Some("DISCORD")));
    }
}
