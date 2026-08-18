//! PostgreSQL AREA Designer repository.
//!
//! Published runtime rows and unpublished drafts live in the tenant CrowdRelay
//! database. Exact coordinates are never placed in audit metadata.

use crowdrelay_application::{
    AreaAdminError, AreaCity, AreaDropDetail, AreaDropSummary, AreaValidationResult,
    CreateAreaCityCommand, CreateAreaDropCommand,
};
use crowdrelay_domain::{
    AreaCollectible, AreaDropDraft, AreaDropStatus, AreaLocalizedClue, MAX_AREA_CLUE_CHARS,
    MAX_AREA_COLLECTIBLE_LINE_CHARS, MAX_AREA_LABEL_CHARS, WorkspaceId, changed_area_fields,
    derive_area_status, live_change_confirmation_issues, valid_area_drop_id,
};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct PostgresAreaAdminRepository {
    pool: PgPool,
}

impl PostgresAreaAdminRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, FromRow)]
struct CityRow {
    id: Uuid,
    slug: String,
    name: String,
    country_code: String,
    region: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    moderation_status: String,
}

impl From<CityRow> for AreaCity {
    fn from(row: CityRow) -> Self {
        Self {
            id: row.id,
            slug: row.slug,
            name: row.name,
            country_code: row.country_code,
            region: row.region,
            latitude: row.latitude,
            longitude: row.longitude,
            moderation_status: row.moderation_status,
        }
    }
}

#[derive(FromRow)]
struct DropRow {
    id: String,
    number: String,
    city_id: Uuid,
    city: String,
    region: String,
    map_x: i16,
    map_y: i16,
    approximate_lat: f64,
    approximate_lng: f64,
    exact_lat: Option<f64>,
    exact_lng: Option<f64>,
    radius_meters: i32,
    max_claims: i32,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
    clue_en: String,
    clue_pl: String,
    collectible_line: String,
    collectible_track: String,
    collectible_edition: String,
    collectible_riddle: String,
    active: bool,
    revision: i64,
    sort_order: i32,
    published_at: Option<OffsetDateTime>,
    archived_at: Option<OffsetDateTime>,
    claim_count: i64,
    draft_payload: Option<Value>,
    draft_base_revision: Option<i64>,
}

#[derive(FromRow)]
struct DraftOnlyRow {
    drop_id: String,
    base_revision: i64,
    payload: Value,
    city: Option<String>,
    region: Option<String>,
}

const DRAFT_ONLY_SELECT: &str = r#"
SELECT
    draft.drop_id,
    draft.base_revision,
    draft.payload,
    city.name AS city,
    city.region AS region
FROM area_drop_drafts AS draft
LEFT JOIN area_drops AS published
  ON published.workspace_id = draft.workspace_id
 AND published.id = draft.drop_id
LEFT JOIN cities AS city
  ON city.id::text = draft.payload->>'cityId'
WHERE draft.workspace_id = $1
  AND published.id IS NULL
"#;

const DROP_SELECT: &str = r#"
SELECT
    d.id,
    d.number,
    d.city_id,
    d.city,
    d.region,
    d.map_x,
    d.map_y,
    d.approximate_lat,
    d.approximate_lng,
    d.exact_lat,
    d.exact_lng,
    d.radius_meters,
    d.max_claims,
    d.starts_at,
    d.ends_at,
    d.clue_en,
    d.clue_pl,
    d.collectible_line,
    d.collectible_track,
    d.collectible_edition,
    d.collectible_riddle,
    d.active,
    d.revision,
    d.sort_order,
    d.published_at,
    d.archived_at,
    (
        SELECT count(*)::bigint
        FROM area_claims AS claim
        WHERE claim.workspace_id = d.workspace_id
          AND claim.drop_id = d.id
    ) AS claim_count,
    draft.payload AS draft_payload,
    draft.base_revision AS draft_base_revision
FROM area_drops AS d
LEFT JOIN area_drop_drafts AS draft
  ON draft.workspace_id = d.workspace_id
 AND draft.drop_id = d.id
"#;

fn map_repo(error: sqlx::Error) -> AreaAdminError {
    if matches!(error, sqlx::Error::RowNotFound) {
        AreaAdminError::NotFound
    } else {
        AreaAdminError::Repository(error.to_string())
    }
}

fn map_drop_write(error: sqlx::Error) -> AreaAdminError {
    let constraint = error
        .as_database_error()
        .and_then(|database| database.constraint());
    match constraint {
        Some("area_drops_workspace_current_city_uidx") => {
            AreaAdminError::Conflict("CITY_ALREADY_USED")
        }
        Some("area_drops_workspace_current_number_uidx") => {
            AreaAdminError::Conflict("NUMBER_ALREADY_USED")
        }
        Some("area_drops_pkey") => AreaAdminError::Conflict("DROP_ALREADY_EXISTS"),
        Some(_) => AreaAdminError::Conflict("INVALID_DROP"),
        None => map_repo(error),
    }
}

fn map_draft_write(error: sqlx::Error) -> AreaAdminError {
    let constraint = error
        .as_database_error()
        .and_then(|database| database.constraint());
    match constraint {
        Some("area_drop_drafts_pkey") => AreaAdminError::Conflict("DROP_ALREADY_EXISTS"),
        Some(_) => AreaAdminError::Conflict("INVALID_DRAFT"),
        None => map_repo(error),
    }
}

fn draft_from_row(row: &DropRow) -> AreaDropDraft {
    AreaDropDraft {
        number: row.number.clone(),
        city_id: row.city_id,
        map_x: row.map_x,
        map_y: row.map_y,
        approximate_lat: row.approximate_lat,
        approximate_lng: row.approximate_lng,
        exact_lat: row.exact_lat,
        exact_lng: row.exact_lng,
        radius_meters: row.radius_meters,
        max_claims: row.max_claims,
        starts_at: row.starts_at,
        ends_at: row.ends_at,
        clue: AreaLocalizedClue {
            en: row.clue_en.clone(),
            pl: row.clue_pl.clone(),
        },
        collectible: AreaCollectible {
            line: row.collectible_line.clone(),
            track: row.collectible_track.clone(),
            edition: row.collectible_edition.clone(),
            riddle: row.collectible_riddle.clone(),
        },
        sort_order: row.sort_order,
    }
}

fn detail_from_row(row: DropRow) -> Result<AreaDropDetail, AreaAdminError> {
    let draft = row
        .draft_payload
        .as_ref()
        .map(|payload| serde_json::from_value::<AreaDropDraft>(payload.clone()))
        .transpose()
        .map_err(|error| {
            AreaAdminError::Repository(format!("invalid stored AREA draft: {error}"))
        })?;
    let status = derive_area_status(
        row.active,
        row.starts_at,
        row.ends_at,
        row.archived_at,
        draft.is_some(),
        row.published_at,
        OffsetDateTime::now_utc(),
    );
    let summary = AreaDropSummary {
        id: row.id.clone(),
        number: row.number.clone(),
        city_id: row.city_id,
        city: row.city.clone(),
        region: row.region.clone(),
        status,
        active: row.active,
        revision: row.revision,
        has_draft: draft.is_some(),
        has_exact_location: row.exact_lat.is_some() && row.exact_lng.is_some(),
        claim_count: row.claim_count,
        max_claims: row.max_claims,
        starts_at: row.starts_at,
        ends_at: row.ends_at,
    };
    Ok(AreaDropDetail {
        summary,
        published: draft_from_row(&row),
        draft,
        draft_base_revision: row.draft_base_revision,
    })
}

fn detail_from_draft_only(row: DraftOnlyRow) -> Result<AreaDropDetail, AreaAdminError> {
    let draft: AreaDropDraft = serde_json::from_value(row.payload).map_err(|error| {
        AreaAdminError::Repository(format!("invalid stored AREA draft: {error}"))
    })?;
    let city = row.city.ok_or_else(|| {
        AreaAdminError::Repository("AREA draft references a missing canonical city".to_owned())
    })?;
    let region = row
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AreaAdminError::Repository("AREA draft canonical city has no region".to_owned())
        })?
        .to_owned();
    let summary = AreaDropSummary {
        id: row.drop_id,
        number: draft.number.clone(),
        city_id: draft.city_id,
        city,
        region,
        status: AreaDropStatus::Draft,
        active: false,
        revision: row.base_revision,
        has_draft: true,
        has_exact_location: draft.exact_lat.is_some() && draft.exact_lng.is_some(),
        claim_count: 0,
        max_claims: draft.max_claims,
        starts_at: draft.starts_at,
        ends_at: draft.ends_at,
    };
    Ok(AreaDropDetail {
        summary,
        published: draft.clone(),
        draft: Some(draft),
        draft_base_revision: Some(row.base_revision),
    })
}

async fn get_draft_only_pool(
    pool: &PgPool,
    workspace_id: Uuid,
    drop_id: &str,
) -> Result<DraftOnlyRow, AreaAdminError> {
    let query = format!("{DRAFT_ONLY_SELECT} AND draft.drop_id = $2");
    sqlx::query_as::<_, DraftOnlyRow>(&query)
        .bind(workspace_id)
        .bind(drop_id)
        .fetch_one(pool)
        .await
        .map_err(map_repo)
}

async fn get_row_pool(
    pool: &PgPool,
    workspace_id: Uuid,
    drop_id: &str,
) -> Result<DropRow, AreaAdminError> {
    let query = format!("{DROP_SELECT} WHERE d.workspace_id=$1 AND d.id=$2");
    sqlx::query_as::<_, DropRow>(&query)
        .bind(workspace_id)
        .bind(drop_id)
        .fetch_one(pool)
        .await
        .map_err(map_repo)
}

async fn get_row_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
) -> Result<DropRow, AreaAdminError> {
    let query = format!("{DROP_SELECT} WHERE d.workspace_id=$1 AND d.id=$2");
    sqlx::query_as::<_, DropRow>(&query)
        .bind(workspace_id)
        .bind(drop_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_repo)
}

// Matches the audit_events column list one-to-one; grouping these into a
// struct would only move the same fields behind another name. Same choice
// as the other audit writers in this crate.
#[allow(clippy::too_many_arguments)]
async fn audit_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: &str,
    actor: &str,
    request_id: Option<&str>,
    detail: Value,
) -> Result<(), AreaAdminError> {
    let metadata = json!({
        "actor": actor,
        "detail": detail,
    });
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
        )
        VALUES ($1, 'service', $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(request_id)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .map_err(map_repo)?;
    Ok(())
}

fn valid_city_slug(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    let first_valid = first.is_ascii_lowercase() || first.is_ascii_digit();
    value.len() <= 128
        && first_valid
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn valid_public_coordinates(lat: f64, lng: f64) -> bool {
    lat.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && lng.is_finite()
        && (-180.0..=180.0).contains(&lng)
}

fn draft_storage_safe(draft: &AreaDropDraft) -> bool {
    let exact_location_safe = match (draft.exact_lat, draft.exact_lng) {
        (None, None) => true,
        (Some(lat), Some(lng)) => valid_public_coordinates(lat, lng),
        _ => false,
    };
    draft.number.len() == 3
        && draft.number.bytes().all(|byte| byte.is_ascii_digit())
        && (0..=100).contains(&draft.map_x)
        && (0..=100).contains(&draft.map_y)
        && valid_public_coordinates(draft.approximate_lat, draft.approximate_lng)
        && exact_location_safe
        && (25..=500).contains(&draft.radius_meters)
        && (1..=500).contains(&draft.max_claims)
        && draft.ends_at > draft.starts_at
        && draft.clue.en.chars().count() <= MAX_AREA_CLUE_CHARS
        && draft.clue.pl.chars().count() <= MAX_AREA_CLUE_CHARS
        && draft.collectible.line.chars().count() <= MAX_AREA_COLLECTIBLE_LINE_CHARS
        && draft.collectible.track.chars().count() <= MAX_AREA_LABEL_CHARS
        && draft.collectible.edition.chars().count() <= MAX_AREA_LABEL_CHARS
        && draft.collectible.riddle.chars().count() <= MAX_AREA_LABEL_CHARS
}

async fn validation_issues_pool(
    pool: &PgPool,
    workspace_id: Uuid,
    drop_id: &str,
    detail: &AreaDropDetail,
    draft: &AreaDropDraft,
) -> Result<Vec<crowdrelay_domain::AreaValidationIssue>, AreaAdminError> {
    let mut issues = draft.validate(detail.summary.claim_count);
    let city_ok = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM cities
            WHERE id=$1
              AND moderation_status='approved'
              AND region IS NOT NULL
              AND btrim(region) <> ''
        )
        "#,
    )
    .bind(draft.city_id)
    .fetch_one(pool)
    .await
    .map_err(map_repo)?;
    if !city_ok {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "CITY_UNKNOWN",
            field: "cityId",
            message: "Canonical city is missing, unapproved, or has no region.",
            confirmation_required: false,
        });
    }
    let duplicate_city = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id=$1
              AND city_id=$2
              AND id<>$3
              AND archived_at IS NULL
        )
        "#,
    )
    .bind(workspace_id)
    .bind(draft.city_id)
    .bind(drop_id)
    .fetch_one(pool)
    .await
    .map_err(map_repo)?;
    if duplicate_city {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "CITY_ALREADY_USED",
            field: "cityId",
            message: "Another current AREA drop already uses this city.",
            confirmation_required: false,
        });
    }
    let duplicate_number = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id=$1
              AND number=$2
              AND id<>$3
              AND archived_at IS NULL
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&draft.number)
    .bind(drop_id)
    .fetch_one(pool)
    .await
    .map_err(map_repo)?;
    if duplicate_number {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "NUMBER_ALREADY_USED",
            field: "number",
            message: "Another current AREA drop already uses this number.",
            confirmation_required: false,
        });
    }
    issues.extend(live_change_confirmation_issues(
        &detail.published,
        draft,
        detail.summary.status == AreaDropStatus::Live,
    ));
    Ok(issues)
}

async fn validation_issues_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
    detail: &AreaDropDetail,
    draft: &AreaDropDraft,
) -> Result<Vec<crowdrelay_domain::AreaValidationIssue>, AreaAdminError> {
    // The caller holds the current drop row lock when a published row exists.
    // Re-count claims here so capacity cannot race a concurrent claim between
    // preview validation and the publish transaction.
    let claim_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM area_claims WHERE workspace_id=$1 AND drop_id=$2",
    )
    .bind(workspace_id)
    .bind(drop_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_repo)?;
    let mut issues = draft.validate(claim_count);
    let city_ok = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM cities
            WHERE id=$1
              AND moderation_status='approved'
              AND region IS NOT NULL
              AND btrim(region) <> ''
        )
        "#,
    )
    .bind(draft.city_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_repo)?;
    if !city_ok {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "CITY_UNKNOWN",
            field: "cityId",
            message: "Canonical city is missing, unapproved, or has no region.",
            confirmation_required: false,
        });
    }
    let duplicate_city = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id=$1
              AND city_id=$2
              AND id<>$3
              AND archived_at IS NULL
        )
        "#,
    )
    .bind(workspace_id)
    .bind(draft.city_id)
    .bind(drop_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_repo)?;
    if duplicate_city {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "CITY_ALREADY_USED",
            field: "cityId",
            message: "Another current AREA drop already uses this city.",
            confirmation_required: false,
        });
    }
    let duplicate_number = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id=$1
              AND number=$2
              AND id<>$3
              AND archived_at IS NULL
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&draft.number)
    .bind(drop_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_repo)?;
    if duplicate_number {
        issues.push(crowdrelay_domain::AreaValidationIssue {
            code: "NUMBER_ALREADY_USED",
            field: "number",
            message: "Another current AREA drop already uses this number.",
            confirmation_required: false,
        });
    }
    issues.extend(live_change_confirmation_issues(
        &detail.published,
        draft,
        detail.summary.status == AreaDropStatus::Live,
    ));
    Ok(issues)
}

mod repository_impl;
