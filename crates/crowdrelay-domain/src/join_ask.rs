//! The weekly join-ask: the tenant's own words, rotated onto its own
//! social channels on a cadence the operator sets.
//!
//! This is the LLM-free growth loop (§5). The text a post carries is one of
//! the variants the tenant wrote, verbatim — the system rotates them and
//! never rewrites them, because the operator's words are the honest source
//! today (there are no `social_post_source_sync` rows to learn voice from,
//! and a model guessing at the band's voice is exactly the failure this
//! feature exists to avoid).
//!
//! One post per platform per ISO week: the decision key carries the week,
//! so however many cycles run, one ask exists. The cadence setting only
//! widens the window — a join-ask still inside its cooldown is held, not
//! re-asked.

use std::collections::BTreeMap;

use serde::Serialize;
use time::OffsetDateTime;

/// The platforms a `social.join_ask.publish` action can actually reach.
///
/// The social post executor claims Facebook, Instagram and Telegram rows —
/// Telegram goes through the Bot API with the tenant's own connected bot,
/// not the Graph path the other two share. Discord still has no executor
/// bridge, so a platform outside this set is held with a named reason
/// rather than emitted into an action nothing claims.
pub const EXECUTABLE_PLATFORMS: [&str; 3] = ["facebook", "instagram", "telegram"];

/// Platforms the settings validator accepts for `join_ask_platforms` —
/// a superset of [`EXECUTABLE_PLATFORMS`], because configuring a channel
/// whose executor is not wired is a legitimate "later" state, not an error.
pub const CONFIGURABLE_PLATFORMS: [&str; 4] = ["facebook", "instagram", "telegram", "discord"];

/// The settings contract, shared by the API validator (which refuses) and
/// the reader (which treats a refused-shape stored value as unset — the
/// same defense `cadence_settings` applies to a hand-edited row).
pub const JOIN_ASK_MAX_VARIANTS: usize = 5;
/// One variant's trim-to-post ceiling: a join-ask is a sentence or two,
/// and past five hundred characters it is a blog post wearing an ask's
/// clothes.
pub const JOIN_ASK_MAX_VARIANT_CHARS: usize = 500;
pub const JOIN_ASK_MIN_CADENCE_DAYS: u16 = 3;
pub const JOIN_ASK_MAX_CADENCE_DAYS: u16 = 30;
/// Weekly is the shipped cadence; absent means 7, not "ask every cycle".
pub const DEFAULT_JOIN_ASK_CADENCE_DAYS: u16 = 7;
/// The channels the default aims at — the two whose followers are already
/// standing on the band's own pages.
pub const DEFAULT_JOIN_ASK_PLATFORMS: &[&str] = &["facebook", "instagram"];

/// Parses `join_ask_variants` — a JSON array of 1–5 strings, each non-empty
/// and ≤ [`JOIN_ASK_MAX_VARIANT_CHARS`] after trim. `None` means the value
/// is not a variants array at all: malformed JSON, the wrong shape, an
/// empty list, or a variant that fails its own bounds.
#[must_use]
pub fn parse_variants(raw: &str) -> Option<Vec<String>> {
    let parsed: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let items = parsed.as_array()?;
    if items.is_empty() || items.len() > JOIN_ASK_MAX_VARIANTS {
        return None;
    }
    let mut variants = Vec::with_capacity(items.len());
    for item in items {
        let text = item.as_str()?.trim();
        if text.is_empty() || text.chars().count() > JOIN_ASK_MAX_VARIANT_CHARS {
            return None;
        }
        variants.push(text.to_owned());
    }
    Some(variants)
}

/// Parses `join_ask_cadence_days` — a bare integer inside
/// [`JOIN_ASK_MIN_CADENCE_DAYS`]..=[`JOIN_ASK_MAX_CADENCE_DAYS`].
#[must_use]
pub fn parse_cadence_days(raw: &str) -> Option<u16> {
    raw.trim()
        .parse::<u16>()
        .ok()
        .filter(|days| (JOIN_ASK_MIN_CADENCE_DAYS..=JOIN_ASK_MAX_CADENCE_DAYS).contains(days))
}

/// Parses `join_ask_platforms` — a comma list drawn entirely from
/// [`CONFIGURABLE_PLATFORMS`], deduplicated in first-seen order. `None`
/// means at least one token is a platform this feature does not know.
#[must_use]
pub fn parse_platforms(raw: &str) -> Option<Vec<String>> {
    let mut platforms: Vec<String> = Vec::new();
    for token in raw.split(',') {
        let platform = token.trim().to_ascii_lowercase();
        if !CONFIGURABLE_PLATFORMS.contains(&platform.as_str()) {
            return None;
        }
        if !platforms.contains(&platform) {
            platforms.push(platform);
        }
    }
    if platforms.is_empty() {
        return None;
    }
    Some(platforms)
}

/// The longest URL `join_ask_image_url` accepts — a bound so a pasted essay
/// cannot be stored as "an image".
pub const JOIN_ASK_MAX_IMAGE_URL_CHARS: usize = 2_048;

/// Parses `join_ask_image_url` — the app screenshot (or other image) every
/// join-ask post carries when set. Must be `https://`: the URL is fetched by
/// Meta's crawler at publish time and by Telegram's, so it leaves our
/// control either way, but `http://` would leak the image fetch
/// unencrypted on the way out and an empty/absent value is simply "no
/// fixed image". `None` means the value is not a usable image URL.
///
/// The reader treats an absent or blank setting the same as a refused-shape
/// one — no fixed image — which is why the writer can refuse outright rather
/// than storing a value the reader would silently ignore.
#[must_use]
pub fn parse_image_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.chars().count() > JOIN_ASK_MAX_IMAGE_URL_CHARS {
        return None;
    }
    if !url.starts_with("https://") || url.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(url.to_owned())
}

/// What the settings keys resolve to for one tenant — `None` overall when
/// the tenant never wrote a usable variants list, because a join-ask with
/// no words is the feature switched off, not a half-configured one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinAskConfig {
    pub variants: Vec<String>,
    pub cadence_days: u16,
    pub platforms: Vec<String>,
    /// The image every ask carries, when the tenant set one — the app
    /// screenshot is the join-ask's honest visual. `None` leaves each
    /// platform to its own fallback (Instagram rotates press assets,
    /// Facebook and Telegram post without a photo).
    pub image_url: Option<String>,
}

impl JoinAskConfig {
    /// What a tenant nobody has set up yet resolves to: no words, the
    /// default cadence, and the platforms the feature aims at by default.
    ///
    /// The loader substitutes this rather than returning nothing, so a cold
    /// workspace still reaches the gates and reports what it is missing.
    /// Returning nothing is what made a brand-new tenant indistinguishable
    /// from an evaluator that never ran.
    #[must_use]
    pub fn unconfigured() -> Self {
        Self {
            variants: Vec::new(),
            cadence_days: DEFAULT_JOIN_ASK_CADENCE_DAYS,
            platforms: DEFAULT_JOIN_ASK_PLATFORMS
                .iter()
                .map(|platform| (*platform).to_owned())
                .collect(),
            image_url: None,
        }
    }
}

/// A `social_posts` row bound to a `social.join_ask.publish` action — the
/// ledger the cadence check and the variant rotation both read.
#[derive(Clone, Debug, Serialize)]
pub struct JoinAskPostRow {
    pub platform: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Everything one cycle needs to decide this week's asks, assembled by the
/// repository port so the evaluator itself holds no SQL.
///
/// `None` config is represented by the port returning no snapshot at all:
/// a tenant who never wrote variants has the feature off, which is a state,
/// not an error.
#[derive(Clone, Debug, Serialize)]
pub struct JoinAskSnapshot {
    /// The operator's own texts, trimmed and non-empty by the writer's
    /// validation. Rotation reads `prior_count % variants.len()`.
    pub variants: Vec<String>,
    /// Minimum days between join-ask posts on one platform.
    pub cadence_days: u16,
    /// The platforms the operator allows, from `join_ask_platforms`.
    pub platforms: Vec<String>,
    /// `member_site_base_url` resolved through the brand-settings seam —
    /// `None` when it is blank, because a join-ask with no destination is a
    /// post without an ask.
    /// A tenant that never set one has none: the shipped default used to be
    /// the first tenant's own site, which for anyone else was another band's
    /// signup page.
    pub member_site_base_url: Option<String>,
    /// The `social_auto_post` tenant flag — the channel's standing publish
    /// approval. The same flag `AutoPostPlatforms::permits` reads for the
    /// agent-draft path: on means the operator already approved this
    /// channel publishing unattended, off means a person sees the post
    /// first.
    pub social_auto_post: bool,
    /// `fanbase_connections.platform` values whose row is `connected`.
    pub connected_platforms: Vec<String>,
    /// Every join-ask `social_posts` row, any status — the rotation count
    /// comes from the same set the cadence window filters.
    pub posts: Vec<JoinAskPostRow>,
    /// Active `beacon_press_assets` photo rows. Instagram has no text-only
    /// post — without a photo the ask cannot exist there, unless
    /// `image_url` already names the image it carries.
    pub instagram_photo_count: u32,
    /// `join_ask_image_url` resolved — the fixed image every ask carries
    /// when the tenant set one. `None` means per-platform fallback.
    pub image_url: Option<String>,
}

/// One platform's ask for the week — the payload the decision carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinAskAsk {
    pub platform: String,
    /// Which of the tenant's variants posts this week — `prior_count %
    /// variants.len()`, so a fixed rotation cannot prefer one forever.
    pub variant_index: u32,
    /// The variant text, verbatim. The tracked link is appended by the
    /// executor at publish time, not here — the decision stores what the
    /// tenant wrote.
    pub text: String,
    /// The destination the executor wraps in a `/l/` smart link: the
    /// member site's `/signal` page tagged so a click attributes back to
    /// this platform and week.
    pub cta_url: String,
    /// `{iso_year}-W{iso_week:02}` — the decision and idempotency key's
    /// time component, so one ask per platform per week stands however
    /// many cycles run.
    pub week_key: String,
    /// The fixed join-ask image, when `join_ask_image_url` is set —
    /// published as the post's photo on platforms that carry one.
    pub image_url: Option<String>,
}

/// Why a configured platform cannot take this week's ask.
///
/// A hold is recorded rather than dropped silently: a platform that stays
/// quiet must read as *held* in the cycle report, or it is
/// indistinguishable from an evaluator that never looked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinAskHold {
    /// The platform is configurable but nothing claims its actions yet —
    /// Discord still needs its own executor bridge.
    NoExecutor,
    /// No `fanbase_connections` row is `connected` for this platform.
    NotConnected,
    /// A live join-ask post exists inside the cadence window.
    OnCadence,
    /// Instagram carries no text-only post, and the tenant has no active
    /// photo press asset for the executor to publish with.
    NoInstagramPhoto,
    /// `member_site_base_url` is unset — the CTA would have no destination.
    NoSiteUrl,
    /// The tenant's variant list is empty — the ask has no words.
    ///
    /// This is the ordinary state of a workspace nobody has set up yet, and
    /// it is the first thing a cold tenant is waiting on. It was previously
    /// unreachable: the snapshot loader returned nothing at all for a tenant
    /// with no variants, so the cycle skipped the context and the hold was
    /// documented as defense in depth against a hand-edited row. The loader
    /// now assembles a snapshot either way, which makes this the cold-start
    /// signal rather than a corruption check.
    NoVariants,
}

impl JoinAskHold {
    /// Stable machine-readable reason, for the cycle report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoExecutor => "no_executor",
            Self::NotConnected => "not_connected",
            Self::OnCadence => "on_cadence",
            Self::NoInstagramPhoto => "no_instagram_photo",
            Self::NoSiteUrl => "no_site_url",
            Self::NoVariants => "no_variants",
        }
    }

    /// What a person would do to clear this hold.
    ///
    /// A hold nobody can act on is a log line with extra steps. These name
    /// the surface the fix lives on rather than the field that is empty,
    /// because an operator reading "no_site_url" still has to be told where
    /// the site URL is set.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::NoExecutor => {
                "nothing publishes to this platform yet — drop it from the join-ask \
                 platforms, or wait for its executor"
            }
            Self::NotConnected => "connect the account on Settings → Destinations",
            Self::OnCadence => "nothing to do — this week's ask already went out",
            Self::NoInstagramPhoto => {
                "add a join-ask image on Settings → Workspace, or activate a press photo"
            }
            Self::NoSiteUrl => "set the member site URL on Settings → Workspace",
            Self::NoVariants => {
                "write the join-ask in the band's own words on Settings → Workspace"
            }
        }
    }

    /// Whether this hold is work waiting on a person.
    ///
    /// [`Self::OnCadence`] is the one hold that means the feature is
    /// working — the ask went out and the next is not due. Listing it beside
    /// genuine gaps would teach an operator that the readiness list is
    /// mostly noise, which is how a readiness surface dies.
    #[must_use]
    pub const fn needs_a_person(self) -> bool {
        !matches!(self, Self::OnCadence)
    }
}

/// One thing the join-ask loop needs and does not have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinAskBlocker {
    /// The platform this stops, or `None` when the gap stops every platform
    /// at once — the words and the destination are written per tenant, not
    /// per channel, and repeating them under each platform would read as
    /// four problems where there is one.
    pub platform: Option<String>,
    pub hold: JoinAskHold,
}

/// Everything standing between this tenant and its first join-ask, at once.
///
/// [`evaluate_join_ask`] stops at the first gate a platform fails, which is
/// right for a decision: it only needs to know it cannot proceed. Somebody
/// setting a tenant up needs the opposite. Cold start is precisely the case
/// where every gate fails together, so first-match reporting turns a
/// ten-minute setup into a six-week drip — clear the words, wait a cycle,
/// learn about the site URL, clear that, wait a cycle, learn about the
/// connection.
///
/// Pure, and reads the same snapshot the decision path reads, so the board
/// and the cycle cannot drift into two answers about what is missing.
///
/// Ordered: the workspace-wide gaps first, because they stop every platform,
/// then per-platform gaps in the tenant's own configured order.
#[must_use]
pub fn join_ask_readiness(snapshot: &JoinAskSnapshot) -> Vec<JoinAskBlocker> {
    let mut blockers = Vec::new();
    if snapshot.variants.is_empty() {
        blockers.push(JoinAskBlocker {
            platform: None,
            hold: JoinAskHold::NoVariants,
        });
    }
    if snapshot.member_site_base_url.is_none() {
        blockers.push(JoinAskBlocker {
            platform: None,
            hold: JoinAskHold::NoSiteUrl,
        });
    }
    for platform in &snapshot.platforms {
        if !EXECUTABLE_PLATFORMS.contains(&platform.as_str()) {
            // Nothing else about this platform is worth reporting: no
            // executor claims its actions, so a missing connection or photo
            // is not what stands in the way.
            blockers.push(JoinAskBlocker {
                platform: Some(platform.clone()),
                hold: JoinAskHold::NoExecutor,
            });
            continue;
        }
        if !snapshot
            .connected_platforms
            .iter()
            .any(|connected| connected == platform)
        {
            blockers.push(JoinAskBlocker {
                platform: Some(platform.clone()),
                hold: JoinAskHold::NotConnected,
            });
        }
        if platform == "instagram"
            && snapshot.instagram_photo_count == 0
            && snapshot.image_url.is_none()
        {
            blockers.push(JoinAskBlocker {
                platform: Some(platform.clone()),
                hold: JoinAskHold::NoInstagramPhoto,
            });
        }
    }
    blockers
}

/// What one cycle decided: the asks to emit, and the platforms held back
/// with the reason — `(platform, hold)`.
#[derive(Clone, Debug, Default)]
pub struct JoinAskPlan {
    pub asks: Vec<JoinAskAsk>,
    /// `(platform, hold)` — recorded in the cycle report so a quiet
    /// platform reads as held, not absent.
    pub held: Vec<(String, JoinAskHold)>,
}

/// Statuses that count as a live post for the cadence window. `failed` and
/// `rate_limited` do not block: a failed post is a post that never went
/// out, and a rate-limited one is retrying on its own clock — neither is
/// this week's ask already answered.
const LIVE_POST_STATUSES: [&str; 4] = ["pending", "posting", "posted", "awaiting_manual_post"];

/// Evaluates every configured platform for this week.
///
/// Pure: every fact the decision needs is on the snapshot. Per platform the
/// gates run in the order an operator would ask them — can anything publish
/// there at all, is an account connected, did we already post recently,
/// does Instagram have a photo, is there somewhere to send people.
#[must_use]
pub fn evaluate_join_ask(snapshot: &JoinAskSnapshot, now: OffsetDateTime) -> JoinAskPlan {
    let (iso_year, iso_week, _) = now.date().to_iso_week_date();
    let week_key = format!("{iso_year}-W{iso_week:02}");
    let mut prior_counts: BTreeMap<&str, u32> = BTreeMap::new();
    for post in &snapshot.posts {
        *prior_counts.entry(post.platform.as_str()).or_default() += 1;
    }
    let mut plan = JoinAskPlan::default();
    for platform in &snapshot.platforms {
        let platform = platform.as_str();
        let hold = if snapshot.variants.is_empty() {
            Some(JoinAskHold::NoVariants)
        } else if !EXECUTABLE_PLATFORMS.contains(&platform) {
            Some(JoinAskHold::NoExecutor)
        } else if !snapshot
            .connected_platforms
            .iter()
            .any(|connected| connected == platform)
        {
            Some(JoinAskHold::NotConnected)
        } else if snapshot.posts.iter().any(|post| {
            post.platform == platform
                && LIVE_POST_STATUSES.contains(&post.status.as_str())
                && post.created_at > now - time::Duration::days(i64::from(snapshot.cadence_days))
        }) {
            Some(JoinAskHold::OnCadence)
        } else if platform == "instagram"
            && snapshot.instagram_photo_count == 0
            && snapshot.image_url.is_none()
        {
            // A fixed join-ask image satisfies Instagram's photo requirement
            // on its own — the press-asset rotation is the fallback, not a
            // second source the ask must wait for.
            Some(JoinAskHold::NoInstagramPhoto)
        } else if snapshot.member_site_base_url.is_none() {
            Some(JoinAskHold::NoSiteUrl)
        } else {
            None
        };
        if let Some(hold) = hold {
            plan.held.push((platform.to_owned(), hold));
            continue;
        }
        // `is_none` was one of the gates above, so reaching here means a
        // configured base URL exists.
        let Some(base) = snapshot.member_site_base_url.as_deref() else {
            continue;
        };
        let prior = prior_counts.get(platform).copied().unwrap_or(0);
        let variant_index =
            prior % u32::try_from(snapshot.variants.len().max(1)).unwrap_or(u32::MAX);
        plan.asks.push(JoinAskAsk {
            platform: platform.to_owned(),
            variant_index,
            text: snapshot
                .variants
                .get(variant_index as usize)
                .cloned()
                .unwrap_or_default(),
            cta_url: format!(
                "{}/signal?utm_source={platform}&utm_medium=join_ask&utm_campaign=join_ask_w{iso_week:02}",
                base.trim_end_matches('/')
            ),
            week_key: week_key.clone(),
            image_url: snapshot.image_url.clone(),
        });
    }
    plan
}

/// Whether the channel carries the standing approval to publish without a
/// per-post ask — the evaluator-side mirror of
/// `AutoPostPlatforms::permits` for the platforms the social executor
/// claims. Discord resolves false here: its executor bridge does not exist,
/// so asking a person is the only honest disposition. Telegram's own
/// worker-side kill switch (`CROWDRELAY_TELEGRAM_AUTO_POST`) still gates at
/// publish time — a channel that permits here can still draft there.
#[must_use]
pub fn join_ask_channel_permits(snapshot: &JoinAskSnapshot, platform: &str) -> bool {
    snapshot.social_auto_post && EXECUTABLE_PLATFORMS.contains(&platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn snapshot() -> JoinAskSnapshot {
        JoinAskSnapshot {
            variants: vec!["join us".to_owned(), "come along".to_owned()],
            cadence_days: 7,
            platforms: vec!["facebook".to_owned(), "instagram".to_owned()],
            member_site_base_url: Some("https://virya.music".to_owned()),
            social_auto_post: true,
            connected_platforms: vec!["facebook".to_owned(), "instagram".to_owned()],
            posts: Vec::new(),
            instagram_photo_count: 1,
            image_url: None,
        }
    }

    #[test]
    fn an_eligible_platform_gets_one_ask_per_week() {
        let now = datetime!(2026-09-23 10:00 UTC);
        let plan = evaluate_join_ask(&snapshot(), now);
        assert_eq!(plan.asks.len(), 2);
        assert_eq!(plan.asks[0].week_key, "2026-W39");
        assert!(plan.asks[0].cta_url.contains("utm_campaign=join_ask_w39"));
        assert!(plan.held.is_empty());
    }

    #[test]
    fn a_live_post_inside_the_window_holds_the_platform() {
        let mut snapshot = snapshot();
        snapshot.posts.push(JoinAskPostRow {
            platform: "facebook".to_owned(),
            status: "posted".to_owned(),
            created_at: datetime!(2026-09-20 10:00 UTC),
        });
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 1);
        assert_eq!(plan.asks[0].platform, "instagram");
        assert_eq!(
            plan.held,
            vec![("facebook".to_owned(), JoinAskHold::OnCadence)]
        );
        // And the prior post advanced the rotation for the next ask.
        assert_eq!(plan.asks[0].variant_index, 0);
    }

    #[test]
    fn a_post_older_than_the_cadence_does_not_hold() {
        let mut snapshot = snapshot();
        snapshot.posts.push(JoinAskPostRow {
            platform: "facebook".to_owned(),
            status: "posted".to_owned(),
            created_at: datetime!(2026-09-10 10:00 UTC),
        });
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 2);
        // One prior facebook post → variant index 1 for this week's ask.
        assert_eq!(plan.asks[0].variant_index, 1);
    }

    #[test]
    fn instagram_without_a_photo_is_held_not_emitted() {
        let mut snapshot = snapshot();
        snapshot.instagram_photo_count = 0;
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 1);
        assert_eq!(
            plan.held,
            vec![("instagram".to_owned(), JoinAskHold::NoInstagramPhoto)]
        );
    }

    #[test]
    fn an_unconnected_platform_is_held_not_emitted() {
        let mut snapshot = snapshot();
        snapshot.connected_platforms.retain(|p| p != "facebook");
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 1);
        assert_eq!(
            plan.held,
            vec![("facebook".to_owned(), JoinAskHold::NotConnected)]
        );
    }

    #[test]
    fn a_configured_platform_with_no_executor_is_held() {
        let mut snapshot = snapshot();
        snapshot.platforms.push("discord".to_owned());
        snapshot.connected_platforms.push("discord".to_owned());
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 2);
        assert_eq!(
            plan.held,
            vec![("discord".to_owned(), JoinAskHold::NoExecutor)]
        );
    }

    #[test]
    fn a_connected_telegram_gets_its_ask() {
        let mut snapshot = snapshot();
        snapshot.platforms.push("telegram".to_owned());
        snapshot.connected_platforms.push("telegram".to_owned());
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 3);
        assert_eq!(plan.asks[2].platform, "telegram");
        assert!(plan.held.is_empty());
    }

    #[test]
    fn a_fixed_image_carries_into_every_ask() {
        let mut snapshot = snapshot();
        snapshot.image_url = Some("https://signal-api.virya.music/v1/public/media/abc".to_owned());
        // Instagram's photo requirement is met by the fixed image alone —
        // zero press assets stops being a hold.
        snapshot.instagram_photo_count = 0;
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert_eq!(plan.asks.len(), 2);
        assert!(plan.held.is_empty());
        assert!(plan.asks.iter().all(|ask| ask.image_url.as_deref()
            == Some("https://signal-api.virya.music/v1/public/media/abc")));
    }

    #[test]
    fn image_url_parses_https_and_refuses_the_rest() {
        assert_eq!(
            parse_image_url(" https://virya.music/press/app.png "),
            Some("https://virya.music/press/app.png".to_owned())
        );
        assert!(parse_image_url("http://virya.music/x.png").is_none());
        assert!(parse_image_url("not a url").is_none());
        assert!(parse_image_url("").is_none());
        assert!(parse_image_url("https://has space/x.png").is_none());
        let long = format!("https://virya.music/{}", "x".repeat(2_050));
        assert!(parse_image_url(&long).is_none());
    }

    #[test]
    fn no_member_site_url_holds_every_platform() {
        let mut snapshot = snapshot();
        snapshot.member_site_base_url = None;
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert!(plan.asks.is_empty());
        assert_eq!(plan.held.len(), 2);
    }

    #[test]
    fn empty_variants_hold_every_platform() {
        let mut snapshot = snapshot();
        snapshot.variants.clear();
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert!(plan.asks.is_empty());
        assert_eq!(
            plan.held,
            vec![
                ("facebook".to_owned(), JoinAskHold::NoVariants),
                ("instagram".to_owned(), JoinAskHold::NoVariants),
            ]
        );
    }

    #[test]
    fn rotation_cycles_through_variants() {
        let mut snapshot = snapshot();
        for index in 0..3 {
            snapshot.posts.push(JoinAskPostRow {
                platform: "facebook".to_owned(),
                status: "posted".to_owned(),
                created_at: datetime!(2026-09-01 10:00 UTC) + time::Duration::days(index),
            });
        }
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        // Three priors over two variants → index 1.
        assert_eq!(plan.asks[0].variant_index, 1);
        assert_eq!(plan.asks[0].text, "come along");
    }

    /// A workspace nobody has set up: no words, no destination, nothing
    /// connected, no photo. The settings reader supplies the default
    /// platforms, so this is what every new tenant looks like on day one.
    fn cold_snapshot() -> JoinAskSnapshot {
        JoinAskSnapshot {
            variants: Vec::new(),
            cadence_days: DEFAULT_JOIN_ASK_CADENCE_DAYS,
            platforms: DEFAULT_JOIN_ASK_PLATFORMS
                .iter()
                .map(|platform| (*platform).to_owned())
                .collect(),
            // What a new tenant resolves to: no site. It used to resolve to
            // the first tenant's, through a shipped default, which is how
            // `NoSiteUrl` never fired for anyone.
            member_site_base_url: None,
            social_auto_post: false,
            connected_platforms: Vec::new(),
            posts: Vec::new(),
            instagram_photo_count: 0,
            image_url: None,
        }
    }

    #[test]
    fn a_cold_tenant_reports_every_gap_at_once() {
        // The point of the readiness list: clearing one item must not be the
        // only way to discover the next. A new tenant sees the whole setup.
        let blockers = join_ask_readiness(&cold_snapshot());
        assert_eq!(
            blockers,
            vec![
                JoinAskBlocker {
                    platform: None,
                    hold: JoinAskHold::NoVariants,
                },
                JoinAskBlocker {
                    platform: None,
                    hold: JoinAskHold::NoSiteUrl,
                },
                JoinAskBlocker {
                    platform: Some("facebook".to_owned()),
                    hold: JoinAskHold::NotConnected,
                },
                JoinAskBlocker {
                    platform: Some("instagram".to_owned()),
                    hold: JoinAskHold::NotConnected,
                },
                JoinAskBlocker {
                    platform: Some("instagram".to_owned()),
                    hold: JoinAskHold::NoInstagramPhoto,
                },
            ]
        );
    }

    #[test]
    fn the_cold_tenant_is_held_rather_than_skipped() {
        // The decision path's half of the same fix: an unconfigured tenant
        // reaches the gates and records a hold per platform, instead of the
        // evaluator never running and the cycle reading as empty.
        let plan = evaluate_join_ask(&cold_snapshot(), datetime!(2026-09-23 10:00 UTC));
        assert!(plan.asks.is_empty());
        assert_eq!(
            plan.held,
            vec![
                ("facebook".to_owned(), JoinAskHold::NoVariants),
                ("instagram".to_owned(), JoinAskHold::NoVariants),
            ]
        );
    }

    #[test]
    fn a_configured_tenant_has_nothing_waiting_on_a_person() {
        assert!(join_ask_readiness(&snapshot()).is_empty());
    }

    #[test]
    fn a_posted_ask_inside_the_window_is_not_a_blocker() {
        // `OnCadence` is the feature working. A readiness list that reports
        // it teaches the operator to stop reading the list.
        let mut snapshot = snapshot();
        snapshot.posts.push(JoinAskPostRow {
            platform: "facebook".to_owned(),
            status: "posted".to_owned(),
            created_at: datetime!(2026-09-20 10:00 UTC),
        });
        assert!(join_ask_readiness(&snapshot).is_empty());
        assert!(!JoinAskHold::OnCadence.needs_a_person());
        assert!(JoinAskHold::NoVariants.needs_a_person());
    }

    #[test]
    fn a_platform_with_no_executor_reports_only_that() {
        // Telling somebody to connect Discord when nothing would publish
        // there is a remedy that wastes their afternoon.
        let mut snapshot = snapshot();
        snapshot.platforms.push("discord".to_owned());
        let blockers = join_ask_readiness(&snapshot);
        assert_eq!(
            blockers,
            vec![JoinAskBlocker {
                platform: Some("discord".to_owned()),
                hold: JoinAskHold::NoExecutor,
            }]
        );
    }

    #[test]
    fn a_fixed_image_clears_the_instagram_photo_gap() {
        let mut snapshot = cold_snapshot();
        snapshot.image_url = Some("https://virya.music/join.png".to_owned());
        let blockers = join_ask_readiness(&snapshot);
        assert!(
            !blockers
                .iter()
                .any(|blocker| blocker.hold == JoinAskHold::NoInstagramPhoto)
        );
    }

    #[test]
    fn every_hold_names_a_remedy() {
        for hold in [
            JoinAskHold::NoExecutor,
            JoinAskHold::NotConnected,
            JoinAskHold::OnCadence,
            JoinAskHold::NoInstagramPhoto,
            JoinAskHold::NoSiteUrl,
            JoinAskHold::NoVariants,
        ] {
            assert!(!hold.remedy().is_empty(), "{} has no remedy", hold.as_str());
        }
    }

    /// A tenant with no site of its own is told so on the board, and its
    /// ask is held rather than posted with somebody else's link. While the
    /// default was the first tenant's site this was reported and deliberately
    /// not enforced; with no default there is nothing to protect by waiting.
    #[test]
    fn a_missing_site_url_is_reported_and_holds_the_ask() {
        let mut snapshot = snapshot();
        snapshot.member_site_base_url = None;
        assert_eq!(
            join_ask_readiness(&snapshot),
            vec![JoinAskBlocker {
                platform: None,
                hold: JoinAskHold::NoSiteUrl,
            }]
        );
        let plan = evaluate_join_ask(&snapshot, datetime!(2026-09-23 10:00 UTC));
        assert!(plan.asks.is_empty(), "no site of its own means no post");
        assert!(
            plan.held
                .iter()
                .all(|(_, hold)| *hold == JoinAskHold::NoSiteUrl),
            "{:?}",
            plan.held
        );
        assert!(!plan.held.is_empty());
    }
}
