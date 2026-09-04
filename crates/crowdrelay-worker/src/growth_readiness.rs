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
    /// Logs a structured growth readiness summary. Each component is logged
    /// as a field so it can be searched/alerted on in log aggregation.
    pub fn log(&self) {
        let components = [
            self.autopilot_enabled,
            self.agent_outcomes_enabled,
            self.push_delivery_enabled,
            self.nearby_shows_enabled,
            self.city_geocoding_enabled,
            self.community_executor_enabled,
            self.telegram_executor_enabled,
            self.discord_executor_enabled,
            self.social_post_executor_enabled,
            self.community_join_executor_enabled,
            self.reddit_discovery_enabled,
            self.x_discovery_enabled,
            self.ad_conversion_enabled,
            self.random_draws_enabled,
        ];
        // The count was compared against a hard-coded 8 while the array had
        // grown to twelve, so a healthy boot could report "11/8 active".
        let total = components.len();
        let active = components.iter().filter(|&&value| value).count();

        tracing::info!(
            active_components = active,
            total_components = total,
            autopilot = self.autopilot_enabled,
            agent_outcomes = self.agent_outcomes_enabled,
            push_delivery = self.push_delivery_enabled,
            nearby_shows = self.nearby_shows_enabled,
            city_geocoding = self.city_geocoding_enabled,
            community_executor = self.community_executor_enabled,
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
