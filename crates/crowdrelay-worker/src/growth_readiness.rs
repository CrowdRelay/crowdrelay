//! What the operator sees at boot: which fan-growth systems are live.
//!
//! Kept out of `main.rs` because the wiring file is at the source-size ratchet
//! and this is reporting, not wiring. Nothing here decides anything -- it reads
//! the decisions `main` already made and says them out loud once, so a silently
//! disabled component is visible from the first log line rather than from the
//! absence of fans weeks later.

/// Growth readiness state: which fan-growth systems are active at startup.
/// Logged once at boot so the operator can immediately see what's running
/// and what needs configuration. Maps directly to the North Star loop:
///   aggregate → grow → convert → learn
pub struct GrowthReadiness {
    /// The deterministic brain. Without this, no growth decisions are made.
    /// Env: CROWDRELAY_AUTOPILOT_ENABLED=true
    pub autopilot_enabled: bool,
    /// LLM worker outcome ingestion. Without this, the brain can't see what
    /// the agents produced. Env: CROWDRELAY_AGENT_OUTCOMES_ENABLED (default: true)
    pub agent_outcomes_enabled: bool,
    /// Fan push notification delivery. Without this, Signal invites can't
    /// be sent. Env: CROWDRELAY_PUSH_DELIVERY_ENABLED=true
    pub push_delivery_enabled: bool,
    /// Nearby-show announcements to fans who asked for them. Always on: it is
    /// the only automatic reason an installed app reopens itself, and it had no
    /// caller at all until it got this loop. The mail half needs marketing
    /// consent, the push half additionally needs `push_delivery_enabled`.
    pub nearby_shows_enabled: bool,
    /// Automatic coordinates for fan-requested cities. Without this a requested
    /// city never gets a latitude, and the fans waiting in it are unreachable by
    /// the nearby-show loop above.
    /// Env: CROWDRELAY_CITY_GEOCODING_CONTACT
    pub city_geocoding_enabled: bool,
    /// Reddit posting executor. Without this, community engagement posts are
    /// drafted but never posted. Env: CROWDRELAY_AGENT_SERVICE_AUTH_KEY
    pub community_executor_enabled: bool,
    /// Telegram channel posting executor. Without this, telegram-poster
    /// drafts are emitted to the outbox but never posted. The bot token
    /// must be stored on the telegram fanbase_connections row.
    /// Env: CROWDRELAY_TELEGRAM_AUTO_POST=true
    pub telegram_executor_enabled: bool,
    /// Discord channel posting executor. Without this, discord-poster
    /// drafts are emitted to the outbox but never posted. The bot token
    /// must be stored on the discord fanbase_connections row.
    /// Env: CROWDRELAY_DISCORD_AUTO_POST=true
    pub discord_executor_enabled: bool,
    /// Social post executor (Instagram, Facebook, X). Tracks LLM-drafted
    /// social posts. Currently runs in manual mode — posts are marked
    /// `awaiting_manual_post` and the operator publishes them manually.
    /// Env: CROWDRELAY_SOCIAL_AUTO_POST=true
    pub social_post_executor_enabled: bool,
    /// Community join executor. Auto-joins Reddit communities found by the
    /// discovery worker. In manual mode (default), places stay `not_joined`.
    /// Env: CROWDRELAY_COMMUNITY_AUTO_JOIN=true
    pub community_join_executor_enabled: bool,
    /// Reddit subreddit discovery. Without this, the system can't find new
    /// communities to engage with. Env: CROWDRELAY_DISCOVERY_REDDIT_QUERIES
    pub reddit_discovery_enabled: bool,
    /// X (Twitter) account discovery. Without this, the system can't find
    /// X curators and communities. Env: CROWDRELAY_DISCOVERY_X_QUERIES
    pub x_discovery_enabled: bool,
    /// Ad conversion tracking (Meta/Google/Bandsintown). Attribution, not
    /// fan creation. Env: CROWDRELAY_META_CAPI_ENABLED, etc.
    pub ad_conversion_enabled: bool,
    /// Referral-weighted reward draws. Fan-led growth mechanic.
    /// Env: CROWDRELAY_RANDOM_DRAWS_ENABLED=true
    pub random_draws_enabled: bool,
}

impl GrowthReadiness {
    /// Every component, named, in one place.
    ///
    /// The count and the log used to be two separate lists, and they diverged:
    /// fourteen components were counted while ten were named, so
    /// `telegram_executor`, `discord_executor`, `social_post_executor` and
    /// `community_join_executor` could each be off with nothing in the log to
    /// say which. That is precisely the silence this module exists to break, so
    /// there is now one list and both readings derive from it.
    ///
    /// `self` is destructured exhaustively on purpose: a new component field
    /// stops compiling here until it is named, so the two lists cannot drift
    /// apart again the way they did.
    #[must_use]
    pub fn components(&self) -> [(&'static str, bool); 14] {
        let Self {
            autopilot_enabled,
            agent_outcomes_enabled,
            push_delivery_enabled,
            nearby_shows_enabled,
            city_geocoding_enabled,
            community_executor_enabled,
            telegram_executor_enabled,
            discord_executor_enabled,
            social_post_executor_enabled,
            community_join_executor_enabled,
            reddit_discovery_enabled,
            x_discovery_enabled,
            ad_conversion_enabled,
            random_draws_enabled,
        } = *self;
        [
            ("autopilot", autopilot_enabled),
            ("agent_outcomes", agent_outcomes_enabled),
            ("push_delivery", push_delivery_enabled),
            ("nearby_shows", nearby_shows_enabled),
            ("city_geocoding", city_geocoding_enabled),
            ("community_executor", community_executor_enabled),
            ("telegram_executor", telegram_executor_enabled),
            ("discord_executor", discord_executor_enabled),
            ("social_post_executor", social_post_executor_enabled),
            ("community_join_executor", community_join_executor_enabled),
            ("reddit_discovery", reddit_discovery_enabled),
            ("x_discovery", x_discovery_enabled),
            ("ad_conversion", ad_conversion_enabled),
            ("random_draws", random_draws_enabled),
        ]
    }

    /// Names of the components that are switched off, in declaration order.
    #[must_use]
    pub fn disabled(&self) -> Vec<&'static str> {
        self.components()
            .into_iter()
            .filter_map(|(name, enabled)| (!enabled).then_some(name))
            .collect()
    }

    /// Logs a structured growth readiness summary. Each component is logged
    /// as a field so it can be searched/alerted on in log aggregation.
    pub fn log(&self) {
        let components = self.components();
        // The count was compared against a hard-coded 8 while the array had
        // grown to twelve, so a healthy boot could report "11/8 active".
        let total = components.len();
        let active = components.iter().filter(|(_, value)| *value).count();
        // The one field an operator reads first: what is not running. Derived
        // from the same array as the count, so a component cannot be counted
        // and then left out of the report.
        let disabled = self.disabled().join(",");

        tracing::info!(
            active_components = active,
            total_components = total,
            disabled = %disabled,
            autopilot = self.autopilot_enabled,
            agent_outcomes = self.agent_outcomes_enabled,
            push_delivery = self.push_delivery_enabled,
            nearby_shows = self.nearby_shows_enabled,
            city_geocoding = self.city_geocoding_enabled,
            community_executor = self.community_executor_enabled,
            telegram_executor = self.telegram_executor_enabled,
            discord_executor = self.discord_executor_enabled,
            social_post_executor = self.social_post_executor_enabled,
            community_join_executor = self.community_join_executor_enabled,
            reddit_discovery = self.reddit_discovery_enabled,
            x_discovery = self.x_discovery_enabled,
            ad_conversion = self.ad_conversion_enabled,
            random_draws = self.random_draws_enabled,
            "growth readiness: {active}/{total} fan-growth components active",
        );

        if !self.autopilot_enabled {
            tracing::warn!(
                "growth readiness: autopilot is OFF — set CROWDRELAY_AUTOPILOT_ENABLED=true to enable the deterministic brain"
            );
        }
        if !self.agent_outcomes_enabled {
            tracing::warn!(
                "growth readiness: agent outcomes are OFF — set CROWDRELAY_AGENT_OUTCOMES_ENABLED=true to feed LLM worker results to the brain"
            );
        }
        if !self.community_executor_enabled {
            tracing::warn!(
                "growth readiness: community executor is OFF — set CROWDRELAY_AGENT_SERVICE_AUTH_KEY for automatic posting via the agents service browser, or the executor will run in manual mode (operator posts manually)"
            );
        }
        if !self.reddit_discovery_enabled {
            tracing::warn!(
                "growth readiness: reddit discovery is OFF — set CROWDRELAY_DISCOVERY_REDDIT_QUERIES to find new communities to engage with"
            );
        }
        if !self.x_discovery_enabled {
            tracing::info!(
                "growth readiness: x discovery is OFF — set CROWDRELAY_DISCOVERY_X_QUERIES to find X curators and communities"
            );
        }
        if !self.push_delivery_enabled {
            tracing::warn!(
                "growth readiness: push delivery is OFF — set CROWDRELAY_PUSH_DELIVERY_ENABLED=true to send Signal push notifications"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(enabled: bool) -> GrowthReadiness {
        GrowthReadiness {
            autopilot_enabled: enabled,
            agent_outcomes_enabled: enabled,
            push_delivery_enabled: enabled,
            nearby_shows_enabled: enabled,
            city_geocoding_enabled: enabled,
            community_executor_enabled: enabled,
            telegram_executor_enabled: enabled,
            discord_executor_enabled: enabled,
            social_post_executor_enabled: enabled,
            community_join_executor_enabled: enabled,
            reddit_discovery_enabled: enabled,
            x_discovery_enabled: enabled,
            ad_conversion_enabled: enabled,
            random_draws_enabled: enabled,
        }
    }

    #[test]
    fn every_component_is_named() {
        // Four components were counted and never named, so an operator reading
        // "10/14 active" had no way to learn which four were off.
        let names: Vec<&str> = all(true)
            .components()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names.len(), 14);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate component name in {names:?}"
        );
        assert!(names.iter().all(|name| !name.is_empty()));
    }

    #[test]
    fn a_fully_disabled_worker_names_all_of_them() {
        assert_eq!(all(false).disabled().len(), 14);
        assert!(all(true).disabled().is_empty());
    }

    #[test]
    fn the_disabled_list_is_exactly_what_is_off() {
        let mut readiness = all(true);
        readiness.telegram_executor_enabled = false;
        readiness.community_join_executor_enabled = false;
        assert_eq!(
            readiness.disabled(),
            vec!["telegram_executor", "community_join_executor"]
        );
    }
}
