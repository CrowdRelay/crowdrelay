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

// ── Growth readiness health: producing evidence in last 24h ────────────

/// Evidence of recent activity for each fan-growth component. "Active" means
/// the component is wired; "producing" means it actually did something in the
/// last 24 hours. A component that is active but not producing is the gap
/// between "the system is configured" and "the system is growing fans" —
/// which is the gap this struct exists to surface.
///
/// Each field is `true` if the component produced evidence in the last 24h,
/// `false` if it did not, and the query is best-effort: a database error
/// leaves the field `false` rather than panicking, because the boot log is
/// not the place to fail.
pub struct GrowthReadinessHealth {
    /// Did the autopilot run a cycle in the last 24h?
    pub autopilot_producing: bool,
    /// Were any agent outcomes consumed in the last 24h?
    pub agent_outcomes_producing: bool,
    /// Were any push deliveries sent in the last 24h?
    pub push_delivery_producing: bool,
    /// Were any community posts posted in the last 24h?
    pub community_executor_producing: bool,
    /// Were any Telegram posts sent in the last 24h?
    pub telegram_executor_producing: bool,
    /// Were any Discord posts sent in the last 24h?
    pub discord_executor_producing: bool,
    /// Were any social posts drafted in the last 24h?
    pub social_post_executor_producing: bool,
    /// Were any community joins attempted in the last 24h?
    pub community_join_executor_producing: bool,
    /// Were any Reddit discovery runs completed in the last 24h?
    pub reddit_discovery_producing: bool,
    /// Were any X discovery runs completed in the last 24h?
    pub x_discovery_producing: bool,
    /// Were any ad conversion events recorded in the last 24h?
    pub ad_conversion_producing: bool,
    /// Were any random draws executed in the last 24h?
    pub random_draws_producing: bool,
}

impl GrowthReadinessHealth {
    /// Queries the database for evidence of recent activity (last 24h) for
    /// each fan-growth component. Best-effort: a query failure for any
    /// component leaves it `false`, because the boot log is not the place
    /// to fail.
    ///
    /// Each query is a cheap `SELECT EXISTS(...)` with a time filter on an
    /// indexed column. The total cost is 12 cheap queries, run once at boot.
    pub async fn query(pool: &sqlx::PgPool) -> Self {
        let cutoff = time::OffsetDateTime::now_utc() - time::Duration::hours(24);

        // Each query is independent — a failure in one does not affect the
        // others. The `unwrap_or(false)` ensures a query error is logged by
        // sqlx but does not propagate.
        let autopilot_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM viryaos_autopilot_cycle_runs WHERE started_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let agent_outcomes_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM agent_outcomes WHERE created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let push_delivery_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM fan_push_deliveries WHERE created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let community_executor_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM community_posts WHERE posted_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let telegram_executor_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM telegram_posts WHERE posted_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let discord_executor_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM discord_posts WHERE posted_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let social_post_executor_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM social_posts WHERE status IN ('posting', 'posted') AND created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let community_join_executor_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM discovery_places WHERE status = 'active' AND updated_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let reddit_discovery_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM discovery_places WHERE place_kind = 'subreddit' AND created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let x_discovery_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM discovery_places WHERE place_kind IN ('instagram', 'tiktok', 'youtube') AND created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let ad_conversion_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM ad_conversion_deliveries WHERE sent_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        let random_draws_producing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM reward_draws WHERE created_at >= $1)",
        )
        .bind(cutoff)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

        Self {
            autopilot_producing,
            agent_outcomes_producing,
            push_delivery_producing,
            community_executor_producing,
            telegram_executor_producing,
            discord_executor_producing,
            social_post_executor_producing,
            community_join_executor_producing,
            reddit_discovery_producing,
            x_discovery_producing,
            ad_conversion_producing,
            random_draws_producing,
        }
    }

    /// Returns the producing components as (name, producing) pairs, in a
    /// stable order matching `GrowthReadiness::components()`.
    #[must_use]
    pub fn producing_components(&self) -> [(&'static str, bool); 12] {
        let Self {
            autopilot_producing,
            agent_outcomes_producing,
            push_delivery_producing,
            community_executor_producing,
            telegram_executor_producing,
            discord_executor_producing,
            social_post_executor_producing,
            community_join_executor_producing,
            reddit_discovery_producing,
            x_discovery_producing,
            ad_conversion_producing,
            random_draws_producing,
        } = *self;
        [
            ("autopilot", autopilot_producing),
            ("agent_outcomes", agent_outcomes_producing),
            ("push_delivery", push_delivery_producing),
            ("community_executor", community_executor_producing),
            ("telegram_executor", telegram_executor_producing),
            ("discord_executor", discord_executor_producing),
            ("social_post_executor", social_post_executor_producing),
            ("community_join_executor", community_join_executor_producing),
            ("reddit_discovery", reddit_discovery_producing),
            ("x_discovery", x_discovery_producing),
            ("ad_conversion", ad_conversion_producing),
            ("random_draws", random_draws_producing),
        ]
    }

    /// Names of the components that are NOT producing evidence, in order.
    #[must_use]
    pub fn not_producing(&self) -> Vec<&'static str> {
        self.producing_components()
            .into_iter()
            .filter_map(|(name, producing)| (!producing).then_some(name))
            .collect()
    }

    /// Logs a structured growth readiness health summary. This complements
    /// `GrowthReadiness::log` — that one says what's *configured*, this one
    /// says what's *actually producing*. A component that is active but
    /// not producing is the silent gap between configuration and growth.
    pub fn log(&self) {
        let components = self.producing_components();
        let total = components.len();
        let producing = components.iter().filter(|(_, value)| *value).count();
        let not_producing = self.not_producing().join(",");

        tracing::info!(
            producing_components = producing,
            total_components = total,
            not_producing = %not_producing,
            "growth health: {producing}/{total} fan-growth components produced evidence in last 24h",
        );

        if !self.autopilot_producing {
            tracing::warn!(
                "growth health: autopilot has NOT run a cycle in 24h — the brain is not making decisions"
            );
        }
        if !self.agent_outcomes_producing {
            tracing::warn!(
                "growth health: no agent outcomes in 24h — LLM workers are not feeding intelligence to the brain"
            );
        }
        if !self.community_executor_producing {
            tracing::warn!(
                "growth health: no community posts in 24h — community engagement is not reaching platforms"
            );
        }
        if !self.reddit_discovery_producing {
            tracing::warn!(
                "growth health: no reddit discovery outcomes in 24h — the brain is not finding new communities"
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
