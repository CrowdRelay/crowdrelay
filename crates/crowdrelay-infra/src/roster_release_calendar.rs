//! The measured half of the roster release calendar (5.16): one
//! organisation's upcoming releases and the shared-fan counts the collision
//! reasons cite. The ordering and the proposal live in
//! `crowdrelay_domain::roster_release_calendar`; this module fetches, it
//! does not decide.
//!
//! The releases are the same rows each act's own release ladder reads —
//! `viryaos_release_plans`, active and inside the lookahead. A release the
//! act cancelled is absent from the calendar entirely, which is the honest
//! answer to "what is coming" rather than a tombstone.
//!
//! `shared_fans` is the count-only overlap between two member workspaces'
//! active fanbases — the same "fans never leave home" shape
//! `audience_overlaps_by_city` uses, just not city-scoped: a release clash
//! is global (press and playlists), so the cited overlap is the pair's
//! whole shared audience. Intersection happens inside Postgres; only
//! counts return.

use std::collections::HashMap;

use crowdrelay_application::RepositoryError;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::release_autopilot::ReleaseTier;
use crowdrelay_domain::roster_release_calendar::{
    LOOKAHEAD_WEEKS, RosterRelease, RosterReleaseCalendar,
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

#[derive(Debug, FromRow)]
struct ReleaseRow {
    id: Uuid,
    workspace_id: Uuid,
    title: String,
    release_at: OffsetDateTime,
    tier: String,
    assets_ready: bool,
}

fn parse_tier(stored: &str) -> ReleaseTier {
    match stored {
        "single" => ReleaseTier::Single,
        "filler" => ReleaseTier::Filler,
        // The honest default the enum itself picks — an unrecognised stored
        // value reads as a Track rather than failing the whole calendar.
        _ => ReleaseTier::Track,
    }
}

/// The organisation's release calendar for the lookahead window.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn roster_release_calendar(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<RosterReleaseCalendar, RepositoryError> {
    let members = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, name
        FROM workspaces
        WHERE organization_id = $1
        ORDER BY name, id
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    if members.is_empty() {
        return Ok(crowdrelay_domain::roster_release_calendar::compose(
            organization_id,
            now,
            Vec::new(),
            HashMap::new(),
        ));
    }
    let member_ids: Vec<Uuid> = members.iter().map(|member| member.0).collect();
    let names: HashMap<Uuid, String> = members.into_iter().collect();

    let window_end = now + time::Duration::weeks(i64::from(LOOKAHEAD_WEEKS));
    let rows = sqlx::query_as::<_, ReleaseRow>(
        r#"
        SELECT id, workspace_id, title, release_at, tier, assets_ready
        FROM viryaos_release_plans
        WHERE workspace_id = ANY($1)
          AND active
          AND release_at >= $2
          AND release_at < $3
        ORDER BY release_at, id
        "#,
    )
    .bind(&member_ids)
    .bind(now)
    .bind(window_end)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // The pair-level shared-audience counts the collision reasons cite.
    // Intersection over `normalized_email`, counts only — no identity
    // crosses the boundary, matching the reach primitive's discipline.
    let shared_rows = sqlx::query_as::<_, (Uuid, Uuid, i64)>(
        r#"
        SELECT a.workspace_id, b.workspace_id, COUNT(*) AS shared
        FROM fans AS a
        JOIN fans AS b
          ON b.normalized_email = a.normalized_email
         AND b.workspace_id > a.workspace_id
        WHERE a.workspace_id = ANY($1)
          AND b.workspace_id = ANY($1)
          AND a.status = 'active'
          AND b.status = 'active'
          AND a.deleted_at IS NULL
          AND b.deleted_at IS NULL
        GROUP BY a.workspace_id, b.workspace_id
        "#,
    )
    .bind(&member_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let shared_by_pair: HashMap<(Uuid, Uuid), u32> = shared_rows
        .into_iter()
        .map(|(a, b, shared)| ((a, b), u32::try_from(shared.max(0)).unwrap_or(u32::MAX)))
        .collect();

    let releases = rows
        .into_iter()
        .filter_map(|row| {
            let act_name = names.get(&row.workspace_id)?.clone();
            Some(RosterRelease {
                workspace_id: WorkspaceId::from_uuid(row.workspace_id),
                act_name,
                release_id: row.id,
                title: row.title,
                release_at: row.release_at,
                tier: parse_tier(&row.tier),
                assets_ready: row.assets_ready,
            })
        })
        .collect();

    Ok(crowdrelay_domain::roster_release_calendar::compose(
        organization_id,
        now,
        releases,
        shared_by_pair,
    ))
}

/// Narrows a database failure to the repository's error vocabulary — the same
/// mapping every sibling module applies, so the read fails the way the rest
/// of the surface fails.
fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}
