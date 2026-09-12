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
