//! Vetting a proposed community against what the audience graph measured.
//!
//! A `social_post` outcome names a subreddit; whether that subreddit may be
//! posted in is decided here — never by the model's say-so. The audience
//! graph's place row supplies the measurements (size, activity, the place's
//! own description and genre tags) and the domain's screening policy turns
//! them into admit or refuse. The r/metalgearsolid incident is why the
//! place's own `name`/`notes`/`genres` are part of the snapshot: a community
//! admitted on member count alone can be about anything at all.

use crowdrelay_domain::audience_graph::canonical_place_url;
use crowdrelay_domain::target_discovery::{
    CommunityCandidateSnapshot, community_topic_signal, fit_to_act,
};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Row shape of the audience-graph lookup for a proposed community.
#[allow(clippy::type_complexity)]
type CommunityPlaceRow = (
    Uuid,
    Option<i32>,
    Option<i32>,
    String,
    String,
    Option<i16>,
    String,
    Option<String>,
    Vec<String>,
);

/// What the audience graph already knows about a proposed community.
#[derive(Clone, Debug)]
pub struct CommunityPlace {
    pub id: Uuid,
    member_count: Option<i32>,
    activity_bp: Option<i32>,
    status: String,
    membership_state: String,
    self_promo_ratio_percent: Option<i16>,
    name: String,
    notes: Option<String>,
    genres: Vec<String>,
}

/// Looks up the audience-graph place for a proposed community, matching
/// on the subreddit slug in the place URL. Returns `None` when discovery
/// has not seen the community yet — that is common for a fresh proposal
/// and is not a refusal.
pub async fn community_place(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    subreddit: Option<&str>,
) -> Result<Option<CommunityPlace>, sqlx::Error> {
    let Some(subreddit) = subreddit.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let row: Option<CommunityPlaceRow> = sqlx::query_as(
        r#"
            SELECT place.id, place.member_count, place.activity_bp,
                   place.status, place.membership_state,
                   rules.self_promo_ratio_percent,
                   place.name, place.notes,
                   COALESCE(place.genres, '{}'::text[])
            FROM discovery_places AS place
            LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
            WHERE place.workspace_id = $1
              AND place.place_kind = 'subreddit'
              AND lower(substring(place.url from '/r/([^/?#]+)')) = lower($2)
            ORDER BY place.updated_at DESC
            LIMIT 1
            "#,
    )
    .bind(workspace_id)
    .bind(subreddit)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(place_from_row))
}

/// The non-Reddit half of the same lookup: a community the scout found on
/// Discord, Telegram, a forum or elsewhere identifies by its URL, and the
/// place table dedupes on `(workspace_id, platform, url)`. The comparison
/// normalizes both sides through the same SQL function the dedup index
/// uses, so 'Discord.gg/Metal' and 'https://www.discord.gg/metal/' are the
/// same room here too.
pub async fn community_place_by_url(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    platform: &str,
    url: &str,
) -> Result<Option<CommunityPlace>, sqlx::Error> {
    let row: Option<CommunityPlaceRow> = sqlx::query_as(
        r#"
            SELECT place.id, place.member_count, place.activity_bp,
                   place.status, place.membership_state,
                   rules.self_promo_ratio_percent,
                   place.name, place.notes,
                   COALESCE(place.genres, '{}'::text[])
            FROM discovery_places AS place
            LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
            WHERE place.workspace_id = $1
              AND place.platform = $2
              AND normalize_community_url(place.url) = normalize_community_url($3)
            ORDER BY place.updated_at DESC
            LIMIT 1
            "#,
    )
    .bind(workspace_id)
    .bind(platform)
    .bind(url)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(place_from_row))
}

/// The agent proposed a community discovery has never seen — create the
/// audience-graph place so it can be screened, joined, and observed like
/// any other. This is "add as a fanbase to join": the place lands at
/// `not_joined` with no measurements, and the screening policy treats the
/// missing numbers as unmeasured, not as zero.
///
/// `member_count` the model reported is deliberately NOT written: the
/// column holds measured values, and a model's claim about a page it read
/// is not a measurement. The claim survives in the target's evidence.
///
/// The URL is normalized and then canonicalized before the upsert:
/// `normalize_place_url` strips presentation noise (scheme/host case,
/// trailing slash) and refuses anything that is not a fetchable http(s)
/// address, and `canonical_place_url` rewrites a subreddit URL to the one
/// form `discovery_places_url_is_canonical` accepts — the migration's CHECK
/// exists so a call site that forgets this step fails loudly instead of
/// storing a second copy of a community. Only http(s) URLs with a real host
/// produce a place: the value is stored and shown to an operator, and a
/// `javascript:` or host-less string is not a joinable community.
///
/// Returns `None` when the URL cannot produce a sane row rather than
/// failing the outcome — a malformed community proposal rejects upstream,
/// but a place row that cannot be built must not sink the whole outcome.
pub async fn ensure_community_place(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    place_kind: &str,
    platform: &str,
    name: &str,
    url: &str,
    language: Option<&str>,
) -> Result<Option<CommunityPlace>, sqlx::Error> {
    let Some(url) = normalize_place_url(url).map(|u| canonical_place_url(&u)) else {
        return Ok(None);
    };
    // Two-letter lowercase only — the column is char(2). An agent's
    // "en-US" truncates to "en"; anything less readable drops to NULL.
    let language = language
        .map(str::trim)
        .map(|l| l.get(..2).unwrap_or(l))
        .filter(|l| l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase()));
    sqlx::query(
        r#"
            INSERT INTO discovery_places
                (workspace_id, place_kind, platform, name, url, language,
                 status, membership_state)
            VALUES ($1, $2, $3, $4, $5, $6, 'active', 'not_joined')
            ON CONFLICT (workspace_id, platform, url) DO UPDATE SET
                updated_at = now()
            "#,
    )
    .bind(workspace_id)
    .bind(place_kind)
    .bind(platform)
    .bind(name.trim())
    .bind(&url)
    .bind(language)
    .execute(&mut **tx)
    .await?;
    // Read back through the lookup so the screen sees the rules row an
    // existing place may carry — the upsert's RETURNING cannot.
    community_place_by_url(tx, workspace_id, platform, &url).await
}

/// Canonical form for a place URL: lowercase scheme and host, trailing
/// slashes stripped, fragment dropped. `None` for anything that is not a
/// fetchable http(s) address — the stored row is the thing an operator
/// clicks, so "cannot be opened" is a reason not to store it.
fn normalize_place_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw.trim()).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = parsed.path().trim_end_matches('/');
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    let normalized = format!("{scheme}://{host}{port}{path}{query}");
    (normalized.len() <= 512).then_some(normalized)
}

fn place_from_row(row: CommunityPlaceRow) -> CommunityPlace {
    let (id, member_count, activity_bp, status, membership_state, self_promo, name, notes, genres) =
        row;
    CommunityPlace {
        id,
        member_count,
        activity_bp,
        status,
        membership_state,
        self_promo_ratio_percent: self_promo,
        name,
        notes,
        genres,
    }
}

/// Builds the screening snapshot for a proposed community from the agent's
/// evidence and whatever the audience graph has measured.
///
/// Reddit places are never sold placement through this path — the discovery
/// adapters import public subreddits, not sponsorship inventory — so
/// `sells_placement` stays false rather than being guessed from prose.
///
/// `act_style` is the tenant's declared `act_style` setting; a community whose
/// own text names only other scenes screens as a poor fit (see
/// `community_topic::fit_to_act`).
pub fn community_snapshot(
    evidence: &Value,
    place: Option<&CommunityPlace>,
    act_style: Option<&str>,
) -> CommunityCandidateSnapshot {
    let has_evidence = evidence.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item.as_str().is_some_and(|s| !s.trim().is_empty()))
    });
    let mut snapshot = CommunityCandidateSnapshot {
        has_evidence,
        ..CommunityCandidateSnapshot::default()
    };
    if let Some(place) = place {
        snapshot.member_count = place.member_count.and_then(|v| u32::try_from(v).ok());
        snapshot.activity_basis_points = place.activity_bp.and_then(|v| u16::try_from(v).ok());
        snapshot.self_promo_ratio_percent = place
            .self_promo_ratio_percent
            .and_then(|v| u8::try_from(v).ok());
        snapshot.refused_by_us_or_them = place.status == "blocked"
            || matches!(place.membership_state.as_str(), "rejected" | "not_a_fit");
        snapshot.topic_signal = fit_to_act(
            community_topic_signal(&place.name, place.notes.as_deref(), &place.genres),
            act_style,
            &place.name,
            place.notes.as_deref(),
            &place.genres,
        );
    }
    snapshot
}

#[cfg(test)]
mod url_tests {
    use super::normalize_place_url;

    #[test]
    fn place_url_normalizes_presentation_not_identity() {
        assert_eq!(
            normalize_place_url("HTTPS://WWW.Discord.GG/Metal/").as_deref(),
            Some("https://www.discord.gg/Metal")
        );
        assert_eq!(
            normalize_place_url("https://reddit.com/r/deathcore/#comments").as_deref(),
            Some("https://reddit.com/r/deathcore")
        );
        assert_eq!(
            normalize_place_url("https://forum.example:8443/board/metal/?page=2").as_deref(),
            Some("https://forum.example:8443/board/metal?page=2")
        );
    }

    #[test]
    fn place_url_refuses_non_web_and_hostless() {
        assert_eq!(normalize_place_url("javascript:alert(1)"), None);
        assert_eq!(normalize_place_url("file:///etc/passwd"), None);
        assert_eq!(normalize_place_url("not a url"), None);
        assert_eq!(normalize_place_url(""), None);
    }
}

#[cfg(test)]
mod act_fit_tests {
    use super::{CommunityPlace, community_snapshot};
    use crowdrelay_domain::target_discovery::{
        CommunityTopicSignal, ScreeningVerdict, TargetDiscoveryPolicy, screen_community_candidate,
    };

    fn place(name: &str) -> CommunityPlace {
        CommunityPlace {
            id: uuid::Uuid::nil(),
            name: name.to_owned(),
            member_count: Some(150_000),
            activity_bp: Some(2_000),
            status: "active".to_owned(),
            membership_state: "none".to_owned(),
            self_promo_ratio_percent: Some(10),
            notes: None,
            genres: Vec::new(),
        }
    }

    #[test]
    fn a_proposed_black_metal_space_is_refused_for_a_metalcore_act() {
        let evidence = serde_json::json!(["https://www.reddit.com/r/BlackMetal/"]);
        let snapshot =
            community_snapshot(&evidence, Some(&place("r/BlackMetal")), Some("metalcore"));
        assert_eq!(snapshot.topic_signal, CommunityTopicSignal::OtherSubgenre);
        assert!(matches!(
            screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()),
            ScreeningVerdict::Refuse(_)
        ));
    }

    #[test]
    fn the_same_space_is_admitted_when_no_style_is_declared() {
        let evidence = serde_json::json!(["https://www.reddit.com/r/BlackMetal/"]);
        let snapshot = community_snapshot(&evidence, Some(&place("r/BlackMetal")), None);
        assert!(matches!(
            screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()),
            ScreeningVerdict::Admit { .. }
        ));
    }
}
