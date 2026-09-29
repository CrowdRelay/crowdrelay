//! Per-tenant settings: the seam that lets one deployment serve many tenants.
//!
//! Values that used to be compile-time constants of the first tenant (the
//! member-site URL, its area path, the synesthesia campaign slug) move behind
//! this repository. Every key carries a shipped default as fallback —
//! onboarding a new label is data, not a fork — except the member-site URL,
//! whose only possible default is some band's own site. That one defaults to
//! nothing: a tenant without its own gets no link, never another band's.
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
///
/// The member-site URL has none. It used to be the first tenant's own site,
/// which for every other tenant meant links to another band's signup page;
/// the first tenant now carries it as an explicit row (migration 0356).
pub const DEFAULT_MEMBER_SITE_BASE_URL: &str = "";
pub const DEFAULT_MEMBER_AREA_PATH: &str = "pl/latarnik";
/// Where the fan site serves a show's page, under the member-site root: the
/// door's check-in QR is `{site}/{live_page_path}/{event_slug}/#checkin=…`.
/// The default is the first tenant's layout, like the member-area path; a
/// tenant whose site is laid out differently sets it rather than forking the
/// URL shape.
pub const DEFAULT_LIVE_PAGE_PATH: &str = "pl/live";
pub const DEFAULT_SYNESTHESIA_CAMPAIGN_SLUG: &str = "virya-synesthesia-album-v1";
pub const DEFAULT_NORTH_STAR_METRIC: &str = "activated_fans_30d";
/// The language briefings are authored in. A tenant that never sets
/// `crew_locale` reads the source language, which is always complete.
pub const DEFAULT_CREW_LOCALE: &str = "en";

/// The keys an operator may edit. Anything else stays internal even if a row
/// somehow appears, so the HTTP surface cannot be used to smuggle state.
pub const EDITABLE_KEYS: [&str; 22] = [
    KEY_MEMBER_SITE_BASE_URL,
    KEY_MEMBER_AREA_PATH,
    KEY_LIVE_PAGE_PATH,
    KEY_SYNESTHESIA_CAMPAIGN_SLUG,
    KEY_SIGNAL_ENABLED,
    KEY_SYNESTHESIA_ENABLED,
    KEY_NORTH_STAR_METRIC,
    KEY_SOCIAL_AUTO_POST,
    KEY_GROWTH_CADENCE_MOMENTS_PER_MONTH,
    KEY_GROWTH_CADENCE_FILLERS_ENABLED,
    KEY_CREW_LOCALE,
    KEY_TEAM_WEEKLY_ASK_CEILING,
    KEY_TENANT_INTENT,
    KEY_ACT_STYLE,
    KEY_ACT_HOME_CITY,
    KEY_TICKETING_ENABLED,
    KEY_JOIN_ASK_VARIANTS,
    KEY_JOIN_ASK_CADENCE_DAYS,
    KEY_JOIN_ASK_PLATFORMS,
    KEY_JOIN_ASK_IMAGE_URL,
    KEY_BRAND_WORDMARK,
    KEY_SOCIAL_AUTOPOST_PLATFORMS,
];

const KEY_MEMBER_SITE_BASE_URL: &str = "member_site_base_url";
const KEY_MEMBER_AREA_PATH: &str = "member_area_path";
const KEY_LIVE_PAGE_PATH: &str = "live_page_path";
const KEY_SYNESTHESIA_CAMPAIGN_SLUG: &str = "synesthesia_campaign_slug";
const KEY_SIGNAL_ENABLED: &str = "signal_enabled";
const KEY_SYNESTHESIA_ENABLED: &str = "synesthesia_enabled";
const KEY_NORTH_STAR_METRIC: &str = "north_star_metric";
pub const KEY_SOCIAL_AUTO_POST: &str = "social_auto_post";
/// Per-platform autopost lanes, comma-separated. Checked only while
/// `social_auto_post` is on: a platform in the list publishes itself,
/// a platform outside is held for a person. Absent means every platform
/// the executor can post — see `domain::social_autopost`.
pub const KEY_SOCIAL_AUTOPOST_PLATFORMS: &str = "social_autopost_platforms";
pub const KEY_GROWTH_CADENCE_MOMENTS_PER_MONTH: &str = "growth_cadence_moments_per_month";
pub const KEY_GROWTH_CADENCE_FILLERS_ENABLED: &str = "growth_cadence_fillers_enabled";
/// The language the crew reads task briefings in.
///
/// Briefings are authored in English and localised at the edge, so this is the
/// setting that decides which words a band member actually gets in their email
/// and in the staff panel. It is a tenant preference, not a compiled-in
/// assumption: Virya is Polish and the next tenant may not be.
pub const KEY_CREW_LOCALE: &str = "crew_locale";
/// §4i-6: the weekly ceiling on asks any one team member can be handed — the
/// composer's own number, enforced by `select_team_assignee`. Absent means
/// uncapped; a present value binds every active member the same way.
pub const KEY_TEAM_WEEKLY_ASK_CEILING: &str = "team_weekly_ask_ceiling";
/// §4G.2: what the tenant says they are working on, which the gig planner
/// outranks its own evidence with.
///
/// Absent means `unstated`, and absent is what it stays until the band says
/// otherwise — this value is never inferred from activity. Guessing a band is
/// heads-down silently withholds every gig proposal they would have wanted,
/// and the band never learns a suggestion was withheld.
pub const KEY_TENANT_INTENT: &str = "tenant_intent";
/// §4h-8 / 5.21: what this act sounds like, in the act's own words.
///
/// `genre_fit` exists on the content-format catalogue and nothing describes
/// the *act*. The roster's package matcher needs it to judge whether two acts
/// belong on one bill, and the declaration is the operator's — never a model's
/// guess from the catalogue, for the same reason `growth_debt` reads the
/// tenant's own declarations about release assets rather than inferring them:
/// an act mislabelled by a guess gets proposed onto bills it does not fit, and
/// nobody can see why.
///
/// Absent means the act has not said. That is a real state and the planner
/// reads it as unmeasured, not as "no style".
pub const KEY_ACT_STYLE: &str = "act_style";
/// The city the act calls home, in the act's own words ("Wrocław").
///
/// Letters open with it — "Piszemy w imieniu {act} ({home})". It is a declaration, never a measurement: the sender identity
/// used to take the city the act had played most, which counted *upcoming*
/// shows as played and once announced a band as being from the city of its
/// next gig. An act that has not said has no city in the sentence — the
/// absent state is real and reads better than a guess.
pub const KEY_ACT_HOME_CITY: &str = "act_home_city";
/// The name this act signs its own messages with — push titles, play pushes,
/// crew mail, invitations. Read through `crowdrelay_workspace_wordmark`, which
/// falls back to the workspace's own name, so absent is the ordinary state: set
/// it only when the act's name is styled differently (`VIRYA`, `MGŁA`).
pub const KEY_BRAND_WORDMARK: &str = "brand_wordmark";
/// Whether this tenant sells tickets through first-party checkout.
///
/// Opt-in, and absent means off: a tenant who never asked for a Stripe
/// checkout gets no ticket-order surface at all. Migration 0343 seeded the
/// workspaces that existed then — they were already selling, so their "yes"
/// is recorded rather than defaulted.
pub const KEY_TICKETING_ENABLED: &str = "ticketing_enabled";

/// §5: the weekly join-ask — the tenant's own words, rotated onto its own
/// social pages. `join_ask_variants` is a JSON array of 1–5 strings; absent
/// or unparseable means the feature is off, which is a state, not an error —
/// a join-ask with no words behind it must never post a guess instead.
pub const KEY_JOIN_ASK_VARIANTS: &str = "join_ask_variants";
/// Days between join-ask posts on one platform, 3–30. Absent means weekly.
pub const KEY_JOIN_ASK_CADENCE_DAYS: &str = "join_ask_cadence_days";
/// The channels the asks may publish to, comma-separated. Absent means
/// `facebook,instagram` — the two whose followers are already standing on
/// the band's own pages.
pub const KEY_JOIN_ASK_PLATFORMS: &str = "join_ask_platforms";
/// The image every join-ask post carries — the app screenshot the band
/// wants under its words. Absent means per-platform fallback: Instagram
/// rotates press assets, Facebook and Telegram post without a photo.
/// `https://` only — Meta's and Telegram's crawlers fetch it at publish
/// time, and a value that is not a fetchable image URL is refused at the
/// edge rather than stored and silently ignored.
pub const KEY_JOIN_ASK_IMAGE_URL: &str = "join_ask_image_url";

const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantBrandSettings {
    pub member_site_base_url: String,
    pub member_area_path: String,
    /// See [`DEFAULT_LIVE_PAGE_PATH`].
    pub live_page_path: String,
    pub synesthesia_campaign_slug: String,
    /// Signal mobile app opt-in. Default true (preserves existing tenants).
    pub signal_enabled: bool,
    /// Synesthesia product opt-in. Default false.
    pub synesthesia_enabled: bool,
    /// Brain north star metric. Default "activated_fans_30d" — real fans who
    /// signed up, consented, and acted within 30 days, not raw installs.
    pub north_star_metric: String,
    /// Social auto-posting: when true, the social post executor publishes to
    /// platforms that have credentials (Facebook Pages, Instagram) instead of
    /// drafting for manual review. Default false — the operator turns it on.
    pub social_auto_post: bool,
    /// The platforms `social_auto_post` is allowed to post — the automatic
    /// lane. Any drafted platform outside this list waits for a person
    /// instead, so removing a platform moves it to the human queue without
    /// touching the master switch. Default: everything the executor can post.
    pub social_autopost_platforms: Vec<String>,
    /// First-party ticket checkout opt-in. Default false: a tenant that never
    /// asked for ticket sales gets a refusal at the reserve, not a silent
    /// order row. Migration 0343 seeded the tenants who were already selling.
    pub ticketing_enabled: bool,
}

impl Default for TenantBrandSettings {
    fn default() -> Self {
        Self {
            member_site_base_url: DEFAULT_MEMBER_SITE_BASE_URL.to_owned(),
            member_area_path: DEFAULT_MEMBER_AREA_PATH.to_owned(),
            live_page_path: DEFAULT_LIVE_PAGE_PATH.to_owned(),
            synesthesia_campaign_slug: DEFAULT_SYNESTHESIA_CAMPAIGN_SLUG.to_owned(),
            signal_enabled: true,
            social_autopost_platforms:
                crowdrelay_domain::social_autopost::DEFAULT_AUTOPOST_PLATFORMS
                    .iter()
                    .map(|platform| (*platform).to_owned())
                    .collect(),
            synesthesia_enabled: false,
            north_star_metric: DEFAULT_NORTH_STAR_METRIC.to_owned(),
            social_auto_post: false,
            ticketing_enabled: false,
        }
    }
}

impl TenantBrandSettings {
    /// `member_site_base_url` without its trailing slash, or `None` when the
    /// tenant has no site of its own. Every link below is built on this, so
    /// none of them can point at a site that is not the tenant's.
    #[must_use]
    pub fn site_root(&self) -> Option<&str> {
        let root = self.member_site_base_url.trim().trim_end_matches('/');
        (!root.is_empty()).then_some(root)
    }

    /// The member-area landing page, e.g. `https://virya.music/pl/latarnik`.
    #[must_use]
    pub fn member_area_url(&self) -> Option<String> {
        Some(format!(
            "{}/{}",
            self.site_root()?,
            self.member_area_path.trim_matches('/')
        ))
    }

    /// The check-in path for one show, relative to the member-site root:
    /// `pl/live/{slug}/#checkin={token}` under the default. Relative, because
    /// the door view joins it onto the deployment's public site.
    #[must_use]
    pub fn live_checkin_path(&self, event_slug: &str, token: &str) -> String {
        format!(
            "{}/{event_slug}/#checkin={token}",
            self.live_page_path.trim_matches('/')
        )
    }

    /// Landing page with the releases anchor appended.
    #[must_use]
    pub fn member_releases_url(&self) -> Option<String> {
        Some(format!("{}/#wydania", self.member_area_url()?))
    }

    /// The member-area path in the reader's locale: the configured path for
    /// `pl*`, and the same path with a leading `pl/` segment dropped for
    /// everyone else — `pl/latarnik` reads `latarnik` to an English fan.
    #[must_use]
    pub fn member_area_path_for(&self, locale: &str) -> String {
        let path = self.member_area_path.trim_matches('/');
        if locale.starts_with("pl") {
            path.to_owned()
        } else {
            path.strip_prefix("pl/").unwrap_or(path).to_owned()
        }
    }

    /// Absolute invite link carrying the single-use token. Non-Polish locales
    /// get the member-area path without its `pl/` segment.
    #[must_use]
    pub fn invite_url(&self, locale: &str, token: &str) -> Option<String> {
        Some(format!(
            "{}/{}?invite={}",
            self.site_root()?,
            self.member_area_path_for(locale),
            token
        ))
    }
}

/// The growth cadence a tenant commits to (§4i-0b). One serious moment a
/// month plus machine-scheduled fillers is the shipped default — a tenant who
/// beats the default moves their own number up; nothing is compiled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TenantCadenceSettings {
    /// Serious moments (release, video, or show) the tenant commits to per
    /// month. Range 1–4: past weekly, nothing is a "serious" moment any more.
    pub serious_moments_per_month: u8,
    /// Whether the machine may schedule fillers (demos, harvest output,
    /// catalogue rotation, show material) between serious moments.
    pub fillers_enabled: bool,
}

impl Default for TenantCadenceSettings {
    fn default() -> Self {
        Self {
            serious_moments_per_month: 1,
            fillers_enabled: true,
        }
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
              AND key IN ($2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(workspace_id)
        .bind(KEY_MEMBER_SITE_BASE_URL)
        .bind(KEY_MEMBER_AREA_PATH)
        .bind(KEY_SYNESTHESIA_CAMPAIGN_SLUG)
        .bind(KEY_SIGNAL_ENABLED)
        .bind(KEY_SYNESTHESIA_ENABLED)
        .bind(KEY_NORTH_STAR_METRIC)
        .bind(KEY_SOCIAL_AUTO_POST)
        .bind(KEY_TICKETING_ENABLED)
        .bind(KEY_LIVE_PAGE_PATH)
        .bind(KEY_SOCIAL_AUTOPOST_PLATFORMS)
        .fetch_all(&self.pool)
        .await?;
        let mut settings = TenantBrandSettings::default();
        for (key, value) in rows {
            match key.as_str() {
                KEY_MEMBER_SITE_BASE_URL => settings.member_site_base_url = value,
                KEY_MEMBER_AREA_PATH => settings.member_area_path = value,
                KEY_LIVE_PAGE_PATH => settings.live_page_path = value,
                KEY_SYNESTHESIA_CAMPAIGN_SLUG => settings.synesthesia_campaign_slug = value,
                KEY_SIGNAL_ENABLED => settings.signal_enabled = value == "true",
                KEY_SYNESTHESIA_ENABLED => settings.synesthesia_enabled = value == "true",
                KEY_NORTH_STAR_METRIC => settings.north_star_metric = value,
                KEY_SOCIAL_AUTO_POST => settings.social_auto_post = value == "true",
                KEY_SOCIAL_AUTOPOST_PLATFORMS => {
                    if let Some(platforms) =
                        crowdrelay_domain::social_autopost::parse_autopost_platforms(&value)
                    {
                        settings.social_autopost_platforms = platforms;
                    }
                }
                KEY_TICKETING_ENABLED => settings.ticketing_enabled = value == "true",
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

    /// The growth cadence for one workspace, defaults over the two override
    /// rows. Not cached: it is read by the scheduler, not on a request path,
    /// and two rows are cheaper than a second cache entry to keep honest.
    /// The crew's language tag, or the source language when unset.
    ///
    /// Returns the raw tag rather than a parsed enum so this crate stays free
    /// of the briefing vocabulary; the caller resolves it with
    /// `BriefingLocale::from_tag`, which treats anything it has no wording for
    /// as English.
    pub async fn crew_locale(&self, workspace_id: Uuid) -> Result<String, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(KEY_CREW_LOCALE)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| DEFAULT_CREW_LOCALE.to_owned()))
    }

    /// The crew's clock: `crew_timezone` when it names a known IANA zone,
    /// otherwise `"UTC"`. Text a crew member reads converts to it and names
    /// it; with none recorded the text stays UTC and says so.
    pub async fn crew_timezone(&self, workspace_id: Uuid) -> Result<String, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_timezone'",
        )
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|zone| zone.trim().to_owned())
            .filter(|zone| crate::regional::is_known_iana_timezone(zone))
            .unwrap_or_else(|| "UTC".to_owned()))
    }

    /// The crew's language tag only when the tenant actually set one —
    /// `None` stays `None` here. `crew_locale` substitutes the default for
    /// readers that must produce text; a payload that records which locale
    /// applied cannot substitute one, because an unmeasured locale must
    /// serialize as `null`, not as a guessed `"en"`.
    pub async fn crew_locale_if_set(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(KEY_CREW_LOCALE)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty()))
    }

    /// What the tenant last said they are working on, as stored.
    ///
    /// Returns the raw value rather than a parsed intent so this repository
    /// stays free of the planner's vocabulary, exactly as `crew_locale` stays
    /// free of the briefing vocabulary. `None` means the band has never stated
    /// one, which the planner reads as `Unstated` — and it must stay `None`
    /// rather than becoming a stored "unstated", because the console shows the
    /// difference between a band that chose to say nothing and one that was
    /// never asked.
    ///
    /// Not cached: the planner reads it once per plan, and a band that switches
    /// to heads-down expects the next proposal to stop, not the one after the
    /// TTL.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn tenant_intent(&self, workspace_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(KEY_TENANT_INTENT)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    /// What the act says it sounds like, or `None` when it has never said.
    ///
    /// Raw text, exactly as `crew_locale` returns a raw tag: this repository
    /// stays free of the pairing vocabulary, and the caller decides what to do
    /// with a declaration nobody has made.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn act_style(&self, workspace_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(KEY_ACT_STYLE)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    /// Where the act says it is from, or `None` when it has never said.
    ///
    /// Same contract as `act_style`: raw text, trimmed, and the empty string
    /// reads as absent — a declaration nobody made stays unsaid in letters.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn act_home_city(&self, workspace_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(KEY_ACT_HOME_CITY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    pub async fn cadence_settings(
        &self,
        workspace_id: Uuid,
    ) -> Result<TenantCadenceSettings, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT key, value FROM tenant_settings
            WHERE workspace_id = $1
              AND key IN ($2, $3)
            "#,
        )
        .bind(workspace_id)
        .bind(KEY_GROWTH_CADENCE_MOMENTS_PER_MONTH)
        .bind(KEY_GROWTH_CADENCE_FILLERS_ENABLED)
        .fetch_all(&self.pool)
        .await?;
        let mut settings = TenantCadenceSettings::default();
        for (key, value) in rows {
            match key.as_str() {
                KEY_GROWTH_CADENCE_MOMENTS_PER_MONTH => {
                    // An out-of-range stored value resolves to the default
                    // rather than poisoning the reader — writes are validated
                    // at the edge, this is defense against a hand edit.
                    settings.serious_moments_per_month = value
                        .parse::<u8>()
                        .ok()
                        .filter(|moments| (1..=4).contains(moments))
                        .unwrap_or(1);
                }
                KEY_GROWTH_CADENCE_FILLERS_ENABLED => {
                    settings.fillers_enabled = value == "true";
                }
                _ => {}
            }
        }
        Ok(settings)
    }

    /// The join-ask configuration for one workspace (§5), or `None` when the
    /// tenant never wrote a usable variants list — a feature-off read, not a
    /// failure.
    ///
    /// Parsing reuses the domain's own contract (`join_ask::parse_*`), the
    /// same one the HTTP validator refuses on: a stored value that cannot
    /// have been written through the edge resolves to the default or to
    /// unset rather than being trusted — a hand edit is indistinguishable
    /// from a bug here, and "treat it as absent" is the honest read of both.
    ///
    /// Not cached: the evaluator reads it once per cycle.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn join_ask_config(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<crowdrelay_domain::join_ask::JoinAskConfig>, sqlx::Error> {
        use crowdrelay_domain::join_ask;
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT key, value FROM tenant_settings
            WHERE workspace_id = $1
              AND key IN ($2, $3, $4, $5)
            "#,
        )
        .bind(workspace_id)
        .bind(KEY_JOIN_ASK_VARIANTS)
        .bind(KEY_JOIN_ASK_CADENCE_DAYS)
        .bind(KEY_JOIN_ASK_PLATFORMS)
        .bind(KEY_JOIN_ASK_IMAGE_URL)
        .fetch_all(&self.pool)
        .await?;
        let mut variants = None;
        let mut cadence_days = join_ask::DEFAULT_JOIN_ASK_CADENCE_DAYS;
        let mut platforms: Vec<String> = join_ask::DEFAULT_JOIN_ASK_PLATFORMS
            .iter()
            .map(|platform| (*platform).to_owned())
            .collect();
        let mut image_url = None;
        for (key, value) in rows {
            match key.as_str() {
                KEY_JOIN_ASK_VARIANTS => variants = join_ask::parse_variants(&value),
                KEY_JOIN_ASK_CADENCE_DAYS => {
                    if let Some(days) = join_ask::parse_cadence_days(&value) {
                        cadence_days = days;
                    }
                }
                KEY_JOIN_ASK_PLATFORMS => {
                    if let Some(parsed) = join_ask::parse_platforms(&value) {
                        platforms = parsed;
                    }
                }
                KEY_JOIN_ASK_IMAGE_URL => {
                    image_url = join_ask::parse_image_url(&value);
                }
                _ => {}
            }
        }
        Ok(variants.map(|variants| join_ask::JoinAskConfig {
            variants,
            cadence_days,
            platforms,
            image_url,
        }))
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

    /// The first tenant's site as an explicit row — what migration 0356
    /// writes for it.
    fn first_tenant() -> TenantBrandSettings {
        TenantBrandSettings {
            member_site_base_url: "https://virya.music".to_owned(),
            ..TenantBrandSettings::default()
        }
    }

    #[test]
    fn defaults_reproduce_the_first_tenant_constants() {
        let settings = first_tenant();
        assert_eq!(
            settings.member_area_url().as_deref(),
            Some("https://virya.music/pl/latarnik")
        );
        assert_eq!(
            settings.member_releases_url().as_deref(),
            Some("https://virya.music/pl/latarnik/#wydania")
        );
        assert_eq!(
            settings.live_checkin_path("hydro-2026", "abc"),
            "pl/live/hydro-2026/#checkin=abc"
        );
        let settings = TenantBrandSettings::default();
        assert_eq!(
            settings.synesthesia_campaign_slug,
            "virya-synesthesia-album-v1"
        );
        // Product opt-in defaults: Signal on, Synesthesia off, ticketing off —
        // the last one is opted into per tenant, and workspaces that existed
        // before the flag arrived were seeded on by migration 0343.
        assert!(settings.signal_enabled);
        assert!(!settings.synesthesia_enabled);
        assert_eq!(settings.north_star_metric, "activated_fans_30d");
        assert!(!settings.social_auto_post);
        assert!(!settings.ticketing_enabled);
    }

    /// A tenant with no site of its own gets no link at all. The default used
    /// to be the first tenant's site, so every other tenant's invitations and
    /// release mails pointed at another band's signup page.
    #[test]
    fn no_site_of_its_own_means_no_link_rather_than_somebody_elses() {
        let settings = TenantBrandSettings::default();
        assert_eq!(settings.site_root(), None);
        assert_eq!(settings.member_area_url(), None);
        assert_eq!(settings.member_releases_url(), None);
        assert_eq!(settings.invite_url("pl-PL", "tok"), None);
        let blank = TenantBrandSettings {
            member_site_base_url: "  / ".to_owned(),
            ..TenantBrandSettings::default()
        };
        assert_eq!(
            blank.invite_url("en", "tok"),
            None,
            "a blanked value is no site"
        );
    }

    #[test]
    fn invite_urls_match_the_previous_locale_branching() {
        let settings = first_tenant();
        assert_eq!(
            settings.invite_url("pl-PL", "tok").as_deref(),
            Some("https://virya.music/pl/latarnik?invite=tok")
        );
        assert_eq!(
            settings.invite_url("en", "tok").as_deref(),
            Some("https://virya.music/latarnik?invite=tok")
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
            settings.member_area_url().as_deref(),
            Some("https://fans.mystic-coalition.example/members")
        );
        assert_eq!(
            settings.invite_url("de", "tok").as_deref(),
            Some("https://fans.mystic-coalition.example/members?invite=tok")
        );
        // The default object stays untouched — this is data, not global state.
        assert_eq!(
            TenantBrandSettings::default().member_area_path,
            "pl/latarnik"
        );
    }

    /// The member-area path follows the configured value in every locale:
    /// `pl*` keeps it whole, everyone else drops a leading `pl/` segment.
    /// The default `pl/latarnik` therefore still reads `latarnik` to an
    /// English fan — the first tenant's URLs are unchanged.
    /// The door QR follows the tenant's own page path; slashes around the
    /// setting do not double up in the URL.
    #[test]
    fn live_checkin_path_follows_the_tenant_setting() {
        let settings = TenantBrandSettings {
            live_page_path: "/shows/".to_owned(),
            ..TenantBrandSettings::default()
        };
        assert_eq!(
            settings.live_checkin_path("night-one", "t0k"),
            "shows/night-one/#checkin=t0k"
        );
    }

    #[test]
    fn member_area_path_for_locale_strips_the_polish_segment() {
        let settings = TenantBrandSettings::default();
        assert_eq!(settings.member_area_path_for("en"), "latarnik");
        assert_eq!(settings.member_area_path_for("pl-PL"), "pl/latarnik");

        let flat = TenantBrandSettings {
            member_area_path: "members".to_owned(),
            ..TenantBrandSettings::default()
        };
        assert_eq!(flat.member_area_path_for("en"), "members");
        assert_eq!(flat.member_area_path_for("pl"), "members");

        let nested = TenantBrandSettings {
            member_area_path: "pl/strefa".to_owned(),
            ..TenantBrandSettings::default()
        };
        assert_eq!(nested.member_area_path_for("en"), "strefa");
        assert_eq!(nested.member_area_path_for("pl-PL"), "pl/strefa");
    }
}
