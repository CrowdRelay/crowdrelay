use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use url::Url;
use uuid::Uuid;

use super::*;

mod admin;
mod member;

pub use admin::{
    admin_candidates, admin_coverage, admin_dashboard, admin_engagements, admin_press_assets,
    admin_press_requests, admin_resolve_press_request, admin_set_state, admin_upsert_press_asset,
    create_invite_batch,
};
pub use member::{leave, my_press_requests, press_room, record_event_engagement, submit_coverage};

const MAX_BATCH_INVITES: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchInviteRequest {
    beacon_ids: Vec<Uuid>,
    #[serde(default = "default_invite_ttl_days")]
    ttl_days: i64,
    #[serde(default = "default_radius_km")]
    radius_km: i32,
    #[serde(default = "default_locale")]
    locale: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BatchInviteItem {
    beacon_id: Uuid,
    display_name: String,
    contact_email: String,
    invite_url: String,
    delivery: InviteDeliveryCopy,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BatchInviteResponse {
    pub(super) version: u8,
    pub(super) created: usize,
    pub(super) skipped: usize,
    pub(super) invitations: Vec<BatchInviteItem>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn mint_invite_batch_tx(
    tx: &mut Transaction<'_, Postgres>,
    brand: &crowdrelay_infra::tenant_settings::TenantBrandSettings,
    workspace_id: Uuid,
    beacon_ids: &[Uuid],
    ttl_days: i64,
    radius_km: i32,
    locale: &str,
    source_invite_job_id: Option<Uuid>,
) -> Result<BatchInviteResponse, BeaconSignalError> {
    // An invitation with no site of the tenant's own to land on is a broken
    // link — or, while the site URL defaulted to the first tenant's, another
    // band's signup page — in a stranger's inbox. Refuse before minting.
    if brand.site_root().is_none() {
        tracing::warn!("beacon batch invite refused: member_site_base_url is blank");
        return Err(BeaconSignalError::Conflict);
    }
    // Batch eligibility is new outreach only: an unverified beacon (no profile
    // row yet) or one whose invite has lapsed. `paused`/`revoked` are
    // operator-set states a bulk click must not undo — the per-beacon invite
    // endpoint is the deliberate revive path, and `active`/`invited`-live are
    // already covered.
    //
    // One invitation per email address. The beacon unique key is
    // (kind, city, email), so the same address can sit on two rows; inviting
    // both mails the person twice. Within the batch the strongest row wins
    // (relevance, then relationship); a live invite or an active membership
    // under any sibling row already covers the address.
    let eligible = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT beacon.id, beacon.display_name, beacon.contact_email
        FROM beacons beacon
        JOIN (
            SELECT DISTINCT ON (lower(other.contact_email)) other.id
            FROM beacons other
            LEFT JOIN beacon_signal_profiles profile
              ON profile.workspace_id=other.workspace_id
             AND profile.beacon_id=other.id
            WHERE other.workspace_id=$1 AND other.id=ANY($2)
              AND other.active AND other.verified AND other.accepts_outreach
              AND NOT other.do_not_contact AND other.contact_email IS NOT NULL
              AND COALESCE(profile.status,'') NOT IN ('active','paused','revoked')
              -- A never-invited beacon has no profile row at all. Three-valued
              -- logic turns the bare NOT(...) into NULL there and silently
              -- drops exactly the first-wave candidates this flow exists to
              -- reach, so fold NULL to false.
              AND NOT COALESCE(
                  profile.status='invited' AND profile.invite_expires_at > now(),
                  false
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM beacons covered
                  JOIN beacon_signal_profiles covered_profile
                    ON covered_profile.workspace_id=covered.workspace_id
                   AND covered_profile.beacon_id=covered.id
                  WHERE covered.workspace_id=other.workspace_id
                    AND covered.id <> other.id
                    AND lower(covered.contact_email)=lower(other.contact_email)
                    AND (
                        covered_profile.status='active'
                        OR (covered_profile.status='invited'
                            AND covered_profile.invite_expires_at > now())
                    )
              )
            ORDER BY lower(other.contact_email),
                     other.relevance_basis_points DESC,
                     other.relationship_score DESC, other.id
        ) picked ON picked.id = beacon.id
        ORDER BY beacon.id
        FOR UPDATE OF beacon
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "Beacon batch invite eligibility lookup failed");
        BeaconSignalError::Unavailable
    })?;

    // The eligibility SELECT's coverage check ran on a snapshot that a
    // concurrent mint cannot see — two transactions over sibling rows with the
    // same address would both pass it and mail the person twice. Serialize on
    // the normalized email itself: advisory locks in a fixed (sorted) order,
    // then re-run the coverage check under the locks so the loser sees the
    // winner's invite. Lock order stays row-then-email everywhere: the beacon
    // FOR UPDATE above runs first in every caller.
    let mut lock_emails: Vec<&str> = eligible
        .iter()
        .map(|(_, _, email)| email.as_str())
        .collect();
    lock_emails.sort_unstable();
    lock_emails.dedup();
    for email in &lock_emails {
        if let Err(error) =
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(lower($1), 0))")
                .bind(*email)
                .execute(&mut **tx)
                .await
        {
            tracing::warn!(%error, "Beacon invite email lock failed");
            return Err(BeaconSignalError::Unavailable);
        }
    }
    let covered_ids: std::collections::HashSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT other.id
        FROM beacons other
        WHERE other.workspace_id=$1 AND other.id=ANY($2)
          AND EXISTS (
              SELECT 1
              FROM beacons covered
              JOIN beacon_signal_profiles covered_profile
                ON covered_profile.workspace_id=covered.workspace_id
               AND covered_profile.beacon_id=covered.id
              WHERE covered.workspace_id=other.workspace_id
                AND covered.id <> other.id
                AND lower(covered.contact_email)=lower(other.contact_email)
                AND (
                    covered_profile.status='active'
                    OR (covered_profile.status='invited'
                        AND covered_profile.invite_expires_at > now())
                )
          )
        "#,
    )
    .bind(workspace_id)
    .bind(eligible.iter().map(|(id, _, _)| *id).collect::<Vec<_>>())
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "Beacon invite coverage re-check failed");
        BeaconSignalError::Unavailable
    })?
    .into_iter()
    .collect();
    let eligible: Vec<(Uuid, String, String)> = eligible
        .into_iter()
        .filter(|(id, _, _)| !covered_ids.contains(id))
        .collect();

    let expires_at = OffsetDateTime::now_utc() + Duration::days(ttl_days);
    let mut invitations = Vec::with_capacity(eligible.len());

    // One statement for the whole batch.
    //
    // This was an INSERT per beacon, and bulk invite is the reason this
    // endpoint exists — the roster console's own note says inviting a city's
    // worth of beacons one form at a time is how it does not get done. The
    // Import button now feeds it dozens at once, each previously costing a
    // round trip inside the request's transaction.
    //
    // Tokens are generated up front because each beacon needs its own and SQL
    // cannot mint them. `RETURNING beacon_id` then reports exactly which rows
    // the `status <> 'active'` guard let through, which is the same fact the
    // per-row `rows_affected() == 1` check was reading.
    let mut beacon_ids: Vec<Uuid> = Vec::with_capacity(eligible.len());
    let mut token_hashes: Vec<Vec<u8>> = Vec::with_capacity(eligible.len());
    let mut tokens: std::collections::HashMap<Uuid, String> =
        std::collections::HashMap::with_capacity(eligible.len());
    for (beacon_id, _, _) in &eligible {
        let Some(invite_token) = random_token::<24>() else {
            return Err(BeaconSignalError::Unavailable);
        };
        beacon_ids.push(*beacon_id);
        token_hashes.push(token_hash(&invite_token));
        tokens.insert(*beacon_id, invite_token);
    }

    let invited_ids: Vec<Uuid> = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO beacon_signal_profiles (
            workspace_id, beacon_id, status, invite_token_hash, invite_expires_at,
            radius_km, locale, nearby_gigs_enabled, invite_count, last_invited_at,
            paused_at, revoked_at, pending_invite_job_id
        )
        SELECT $1, batch.beacon_id, 'invited', batch.token_hash, $4,
               $5, $6, true, 1, now(), NULL, NULL, $7
        FROM UNNEST($2::uuid[], $3::bytea[]) AS batch(beacon_id, token_hash)
        ON CONFLICT (workspace_id, beacon_id) DO UPDATE SET
            status='invited', invite_token_hash=EXCLUDED.invite_token_hash,
            invite_expires_at=EXCLUDED.invite_expires_at, radius_km=EXCLUDED.radius_km,
            locale=EXCLUDED.locale, nearby_gigs_enabled=true,
            invite_count=beacon_signal_profiles.invite_count + 1,
            last_invited_at=now(), paused_at=NULL, revoked_at=NULL,
            pending_invite_job_id=EXCLUDED.pending_invite_job_id, updated_at=now()
        WHERE beacon_signal_profiles.status <> 'active'
        RETURNING beacon_id
        "#,
    )
    .bind(workspace_id)
    .bind(&beacon_ids)
    .bind(&token_hashes)
    .bind(expires_at)
    .bind(radius_km)
    .bind(locale)
    .bind(source_invite_job_id)
    .fetch_all(&mut **tx)
    .await
    {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "Beacon batch invite persistence failed");
            return Err(BeaconSignalError::Unavailable);
        }
    };

    // Build the invitation payloads for the rows that actually took effect,
    // in the roster's order rather than whatever order the insert returned.
    let invited: std::collections::HashSet<Uuid> = invited_ids.iter().copied().collect();
    let (wordmark, app_name, _) =
        super::invite_names(&mut **tx, workspace_id)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "beacon invite names lookup failed");
                BeaconSignalError::Unavailable
            })?;
    let names = super::invite_copy::InviteBrand {
        wordmark: &wordmark,
        app_name: &app_name,
    };
    for (beacon_id, display_name, contact_email) in eligible {
        if !invited.contains(&beacon_id) {
            continue;
        }
        let Some(invite_token) = tokens.get(&beacon_id) else {
            continue;
        };
        let Some(invite_url) = brand.invite_url(locale, invite_token) else {
            return Err(BeaconSignalError::Conflict);
        };
        let delivery = invite_delivery_copy(locale, &display_name, &invite_url, &names);
        invitations.push(BatchInviteItem {
            beacon_id,
            display_name,
            contact_email,
            invite_url,
            delivery,
            expires_at,
        });
    }

    if !invited_ids.is_empty()
        && let Err(error) = sqlx::query(
            r#"
            UPDATE beacon_signal_sessions
            SET revoked_at=COALESCE(revoked_at, now())
            WHERE workspace_id=$1 AND beacon_id=ANY($2) AND revoked_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(&invited_ids)
        .execute(&mut **tx)
        .await
    {
        tracing::warn!(%error, "Beacon batch old-session revocation failed");
        return Err(BeaconSignalError::Unavailable);
    }
    Ok(BatchInviteResponse {
        version: 2,
        created: invitations.len(),
        skipped: beacon_ids.len().saturating_sub(invitations.len()),
        invitations,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PressRoomQuery {
    event_id: Option<Uuid>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct PressAssetView {
    id: Uuid,
    event_id: Option<Uuid>,
    asset_key: String,
    asset_kind: String,
    label_pl: String,
    label_en: String,
    url: String,
    sort_order: i32,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct PressRoomEventView {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    city: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    doors_at: Option<OffsetDateTime>,
    ticket_url: Option<String>,
    description: Option<String>,
    image_url: Option<String>,
    listen_url: Option<String>,
    trailer_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PressRoomResponse {
    version: u8,
    event_id: Option<Uuid>,
    event: Option<PressRoomEventView>,
    assets: Vec<PressAssetView>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct MyPressRequestView {
    id: Uuid,
    event_id: Option<Uuid>,
    event_title: Option<String>,
    request_kind: String,
    details: Option<String>,
    status: String,
    resolution_note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    resolved_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MyPressRequestsResponse {
    requests: Vec<MyPressRequestView>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EngagementAction {
    Opened,
    Interested,
    Helping,
    Completed,
    Declined,
}

impl EngagementAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Interested => "interested",
            Self::Helping => "helping",
            Self::Completed => "completed",
            Self::Declined => "declined",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HelpKind {
    Article,
    Radio,
    Podcast,
    Photos,
    Share,
    Contact,
    Other,
}

impl HelpKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Article => "article",
            Self::Radio => "radio",
            Self::Podcast => "podcast",
            Self::Photos => "photos",
            Self::Share => "share",
            Self::Contact => "contact",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EngagementRequest {
    action: EngagementAction,
    help_kind: Option<HelpKind>,
    help_details: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EngagementResponse {
    event_id: Uuid,
    status: String,
    help_kind: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CoverageKind {
    Article,
    Radio,
    Video,
    Photo,
    Social,
    Podcast,
    Other,
}

impl CoverageKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Article => "article",
            Self::Radio => "radio",
            Self::Video => "video",
            Self::Photo => "photo",
            Self::Social => "social",
            Self::Podcast => "podcast",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoverageRequest {
    coverage_kind: CoverageKind,
    url: String,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverageResponse {
    coverage_id: Uuid,
    event_id: Uuid,
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LeaveRequest {
    #[serde(default)]
    do_not_contact: bool,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminProfileView {
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    contact_email: Option<String>,
    city: Option<String>,
    status: String,
    radius_km: i32,
    locale: String,
    nearby_gigs_enabled: bool,
    invite_count: i32,
    #[serde(with = "time::serde::rfc3339::option")]
    last_invited_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    invite_expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    joined_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    active_sessions: i64,
    active_push_endpoints: i64,
    open_press_requests: i64,
    active_engagements: i64,
    coverage_count: i64,
    relevance_basis_points: i32,
    relationship_score: i32,
    destination_url: Option<String>,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminDashboardResponse {
    total: usize,
    active: usize,
    invited: usize,
    paused: usize,
    revoked: usize,
    profiles: Vec<AdminProfileView>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminCandidateView {
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    contact_email: String,
    city: Option<String>,
    relevance_basis_points: i32,
    relationship_score: i32,
    signal_status: Option<String>,
    invite_count: i32,
    #[serde(with = "time::serde::rfc3339::option")]
    last_invited_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminCandidatesResponse {
    candidates: Vec<AdminCandidateView>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdminProfileState {
    Active,
    Paused,
    Revoked,
}

impl AdminProfileState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdminProfileStateRequest {
    status: AdminProfileState,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminProfileStateResponse {
    beacon_id: Uuid,
    status: &'static str,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResolvePressStatus {
    Resolved,
    Cancelled,
}

impl ResolvePressStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResolvePressRequest {
    status: ResolvePressStatus,
    resolution_note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvePressResponse {
    request_id: Uuid,
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpsertPressAssetRequest {
    asset_id: Option<Uuid>,
    event_id: Option<Uuid>,
    asset_key: String,
    asset_kind: String,
    label_pl: String,
    label_en: String,
    url: String,
    #[serde(default = "default_asset_sort_order")]
    sort_order: i32,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_asset_sort_order() -> i32 {
    100
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpsertPressAssetResponse {
    asset_id: Uuid,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminPressAssetView {
    id: Uuid,
    event_id: Option<Uuid>,
    event_title: Option<String>,
    asset_key: String,
    asset_kind: String,
    label_pl: String,
    label_en: String,
    url: String,
    sort_order: i32,
    active: bool,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminPressAssetsResponse {
    assets: Vec<AdminPressAssetView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminEngagementQuery {
    status: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminEngagementView {
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    event_id: Uuid,
    event_title: String,
    event_slug: String,
    status: String,
    help_kind: Option<String>,
    help_details: Option<String>,
    notification_count: i32,
    coverage_count: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    last_notified_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminEngagementsResponse {
    engagements: Vec<AdminEngagementView>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminCoverageView {
    id: Uuid,
    beacon_id: Uuid,
    display_name: String,
    event_id: Uuid,
    event_title: String,
    coverage_kind: String,
    url: String,
    title: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminCoverageResponse {
    coverage: Vec<AdminCoverageView>,
}

fn clean_optional_text(value: Option<String>, max_chars: usize) -> Option<Option<String>> {
    match value {
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() || value.chars().count() > max_chars {
                None
            } else {
                Some(Some(value))
            }
        }
        None => Some(None),
    }
}

fn valid_https_url(value: &str) -> bool {
    if value.len() > 2048 {
        return false;
    }
    Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn valid_press_url(value: &str) -> bool {
    if value.len() > 2048 {
        return false;
    }
    Url::parse(value).is_ok_and(|url| match url.scheme() {
        "https" => url.host_str().is_some(),
        "mailto" => !url.path().trim().is_empty(),
        _ => false,
    })
}

fn valid_asset_key(value: &str) -> bool {
    let value = value.as_bytes();
    (2..=64).contains(&value.len())
        && value.first().is_some_and(u8::is_ascii_lowercase)
        && value.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_asset_kind(value: &str) -> bool {
    matches!(
        value,
        "epk"
            | "photo"
            | "logo"
            | "bio"
            | "audio"
            | "video"
            | "rider"
            | "social"
            | "contact"
            | "link"
    )
}

fn valid_engagement_status(value: &str) -> bool {
    matches!(
        value,
        "eligible" | "notified" | "opened" | "interested" | "helping" | "completed" | "declined"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_fail_closed() {
        assert!(valid_https_url("https://example.com/story"));
        assert!(!valid_https_url("http://example.com/story"));
        assert!(!valid_https_url("javascript:alert(1)"));
        assert!(valid_press_url("mailto:press@example.com"));
        assert!(valid_press_url("https://example.com/epk"));
    }

    #[test]
    fn asset_keys_are_bounded() {
        assert!(valid_asset_key("press_photo"));
        assert!(!valid_asset_key("Press Photo"));
    }
}
