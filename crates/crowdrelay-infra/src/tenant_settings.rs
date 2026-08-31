//! Per-tenant settings: the seam that lets one deployment serve many tenants.
//!
//! Values that used to be compile-time constants of the first tenant (the
//! member-site URL, its area path, the synesthesia campaign slug) move behind
//! this repository. Every key carries the shipped default as fallback, so an
//! empty table reproduces yesterday's behavior byte for byte — onboarding a
//! new label is now data, not a fork.
//!
//! Reads are cached per workspace behind a short TTL: these values change at
//! operator speed, while several call sites sit on warm request paths.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
    time::{Duration, Instant},
};

use sqlx::PgPool;
use uuid::Uuid;

/// Shipped defaults. They exist so the first tenant's behavior is unchanged
/// by this extraction; they are not special-cased anywhere else.
pub const DEFAULT_MEMBER_SITE_BASE_URL: &str = "https://virya.music";
pub const DEFAULT_MEMBER_AREA_PATH: &str = "pl/latarnik";
pub const DEFAULT_SYNESTHESIA_CAMPAIGN_SLUG: &str = "virya-synesthesia-album-v1";
pub const DEFAULT_NORTH_STAR_METRIC: &str = "signal_installs";

/// The keys an operator may edit. Anything else stays internal even if a row
/// somehow appears, so the HTTP surface cannot be used to smuggle state.
pub const EDITABLE_KEYS: [&str; 6] = [
    KEY_MEMBER_SITE_BASE_URL,
    KEY_MEMBER_AREA_PATH,
    KEY_SYNESTHESIA_CAMPAIGN_SLUG,
    KEY_SIGNAL_ENABLED,
    KEY_SYNESTHESIA_ENABLED,
    KEY_NORTH_STAR_METRIC,
];

const KEY_MEMBER_SITE_BASE_URL: &str = "member_site_base_url";
const KEY_MEMBER_AREA_PATH: &str = "member_area_path";
const KEY_SYNESTHESIA_CAMPAIGN_SLUG: &str = "synesthesia_campaign_slug";
const KEY_SIGNAL_ENABLED: &str = "signal_enabled";
const KEY_SYNESTHESIA_ENABLED: &str = "synesthesia_enabled";
const KEY_NORTH_STAR_METRIC: &str = "north_star_metric";

const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantBrandSettings {
    pub member_site_base_url: String,
    pub member_area_path: String,
    pub synesthesia_campaign_slug: String,
    /// Signal mobile app opt-in. Default true (preserves existing tenants).
    pub signal_enabled: bool,
    /// Synesthesia product opt-in. Default false.
    pub synesthesia_enabled: bool,
    /// Brain north star metric. Default "signal_installs".
    pub north_star_metric: String,
}

impl Default for TenantBrandSettings {
    fn default() -> Self {
        Self {
            member_site_base_url: DEFAULT_MEMBER_SITE_BASE_URL.to_owned(),
            member_area_path: DEFAULT_MEMBER_AREA_PATH.to_owned(),
            synesthesia_campaign_slug: DEFAULT_SYNESTHESIA_CAMPAIGN_SLUG.to_owned(),
            signal_enabled: true,
            synesthesia_enabled: false,
            north_star_metric: DEFAULT_NORTH_STAR_METRIC.to_owned(),
        }
    }
}

impl TenantBrandSettings {
    /// The member-area landing page, e.g. `https://virya.music/pl/latarnik`.
    #[must_use]
    pub fn member_area_url(&self) -> String {
        format!(
            "{}/{}",
            self.member_site_base_url.trim_end_matches('/'),
            self.member_area_path.trim_matches('/')
        )
    }

    /// Landing page with the releases anchor appended.
    #[must_use]
    pub fn member_releases_url(&self) -> String {
        format!("{}/#wydania", self.member_area_url())
    }

    /// Absolute invite link carrying the single-use token. Non-Polish locales
    /// keep the un-prefixed area path exactly as before the extraction.
    #[must_use]
    pub fn invite_url(&self, locale: &str, token: &str) -> String {
        let area = if locale.starts_with("pl") {
            &self.member_area_path
        } else {
            "latarnik"
        };
        format!(
            "{}/{}?invite={}",
            self.member_site_base_url.trim_end_matches('/'),
            area,
            token
        )
    }
}

#[derive(Clone)]
pub struct TenantSettingsRepository {
    pool: PgPool,
}

type SettingsCache = HashMap<Uuid, (Instant, Arc<TenantBrandSettings>)>;

fn cache() -> &'static RwLock<SettingsCache> {
    static CACHE: OnceLock<RwLock<SettingsCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

impl TenantSettingsRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Brand-relevant settings for one workspace, cache-first. A cache miss
    /// reads at most six rows; an empty result set yields the defaults.
    pub async fn brand_settings(
        &self,
        workspace_id: Uuid,
    ) -> Result<Arc<TenantBrandSettings>, sqlx::Error> {
        if let Some((read_at, cached)) = cache()
            .read()
            .ok()
            .and_then(|cache| cache.get(&workspace_id).cloned())
            && read_at.elapsed() < CACHE_TTL
        {
            return Ok(cached);
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT key, value FROM tenant_settings
            WHERE workspace_id = $1
              AND key IN ($2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(workspace_id)
        .bind(KEY_MEMBER_SITE_BASE_URL)
        .bind(KEY_MEMBER_AREA_PATH)
        .bind(KEY_SYNESTHESIA_CAMPAIGN_SLUG)
        .bind(KEY_SIGNAL_ENABLED)
        .bind(KEY_SYNESTHESIA_ENABLED)
        .bind(KEY_NORTH_STAR_METRIC)
        .fetch_all(&self.pool)
        .await?;
        let mut settings = TenantBrandSettings::default();
        for (key, value) in rows {
            match key.as_str() {
                KEY_MEMBER_SITE_BASE_URL => settings.member_site_base_url = value,
                KEY_MEMBER_AREA_PATH => settings.member_area_path = value,
                KEY_SYNESTHESIA_CAMPAIGN_SLUG => settings.synesthesia_campaign_slug = value,
                KEY_SIGNAL_ENABLED => settings.signal_enabled = value == "true",
                KEY_SYNESTHESIA_ENABLED => settings.synesthesia_enabled = value == "true",
                KEY_NORTH_STAR_METRIC => settings.north_star_metric = value,
                _ => {}
            }
        }
        let shared = Arc::new(settings);
        if let Ok(mut cache) = cache().write() {
            cache.insert(workspace_id, (Instant::now(), Arc::clone(&shared)));
        }
        Ok(shared)
    }

    /// Raw overrides for the workspace (may be empty). The caller merges them
    /// over [`TenantBrandSettings::default`] to present effective values plus
    /// an "is overridden" marker per key.
    pub async fn list_overrides(
        &self,
        workspace_id: Uuid,
    ) -> Result<HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM tenant_settings WHERE workspace_id = $1")
                .bind(workspace_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// Upserts one override and drops the workspace's cache entry so the next
    /// read observes it without waiting out the TTL.
    pub async fn set_setting(
        &self,
        workspace_id: Uuid,
        key: &str,
        value: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO tenant_settings (workspace_id, key, value)
            VALUES ($1, $2, $3)
            ON CONFLICT (workspace_id, key) DO UPDATE SET
                value = EXCLUDED.value, updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        if let Ok(mut cache) = cache().write() {
            cache.remove(&workspace_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reproduce_the_first_tenant_constants() {
        let settings = TenantBrandSettings::default();
        assert_eq!(settings.member_site_base_url, "https://virya.music");
        assert_eq!(
            settings.member_area_url(),
            "https://virya.music/pl/latarnik"
        );
        assert_eq!(
            settings.member_releases_url(),
            "https://virya.music/pl/latarnik/#wydania"
        );
        assert_eq!(
            settings.synesthesia_campaign_slug,
            "virya-synesthesia-album-v1"
        );
        // Product opt-in defaults: Signal on, Synesthesia off.
        assert!(settings.signal_enabled);
        assert!(!settings.synesthesia_enabled);
        assert_eq!(settings.north_star_metric, "signal_installs");
    }

    #[test]
    fn invite_urls_match_the_previous_locale_branching() {
        let settings = TenantBrandSettings::default();
        assert_eq!(
            settings.invite_url("pl-PL", "tok"),
            "https://virya.music/pl/latarnik?invite=tok"
        );
        assert_eq!(
            settings.invite_url("en", "tok"),
            "https://virya.music/latarnik?invite=tok"
        );
    }

    #[test]
    fn overrides_change_the_urls_without_touching_defaults_elsewhere() {
        let settings = TenantBrandSettings {
            member_site_base_url: "https://fans.mystic-coalition.example".to_owned(),
            member_area_path: "members".to_owned(),
            ..TenantBrandSettings::default()
        };
        assert_eq!(
            settings.member_area_url(),
            "https://fans.mystic-coalition.example/members"
        );
        assert_eq!(
            settings.invite_url("de", "tok"),
            "https://fans.mystic-coalition.example/latarnik?invite=tok"
        );
        // The default object stays untouched — this is data, not global state.
        assert_eq!(
            TenantBrandSettings::default().member_area_path,
            "pl/latarnik"
        );
    }
}
