//! Vetting a proposed community against what the audience graph measured.
//!
//! A `social_post` outcome names a subreddit; whether that subreddit may be
//! posted in is decided here — never by the model's say-so. The audience
//! graph's place row supplies the measurements (size, activity, the place's
//! own description and genre tags) and the domain's screening policy turns
//! them into admit or refuse. The r/metalgearsolid incident is why the
//! place's own `name`/`notes`/`genres` are part of the snapshot: a community
//! admitted on member count alone can be about anything at all.

use crowdrelay_domain::target_discovery::{CommunityCandidateSnapshot, community_topic_signal};
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
    Ok(row.map(
        |(
            id,
            member_count,
            activity_bp,
            status,
            membership_state,
            self_promo,
            name,
            notes,
            genres,
        )| {
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
        },
    ))
}

/// Builds the screening snapshot for a proposed community from the agent's
/// evidence and whatever the audience graph has measured.
///
/// Reddit places are never sold placement through this path — the discovery
/// adapters import public subreddits, not sponsorship inventory — so
/// `sells_placement` stays false rather than being guessed from prose.
pub fn community_snapshot(
    evidence: &Value,
    place: Option<&CommunityPlace>,
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
        snapshot.topic_signal =
            community_topic_signal(&place.name, place.notes.as_deref(), &place.genres);
    }
    snapshot
}
