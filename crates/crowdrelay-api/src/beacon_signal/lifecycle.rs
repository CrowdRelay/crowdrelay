use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
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
struct BatchInviteItem {
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
struct BatchInviteResponse {
    version: u8,
    created: usize,
    skipped: usize,
    invitations: Vec<BatchInviteItem>,
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
    joined_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    active_sessions: i64,
    active_push_endpoints: i64,
    open_press_requests: i64,
    active_engagements: i64,
    coverage_count: i64,
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
        && value
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'))
}

fn valid_asset_kind(value: &str) -> bool {
    matches!(
        value,
        "epk" | "photo" | "logo" | "bio" | "audio" | "video" | "rider" | "social" | "contact" | "link"
    )
}

fn valid_engagement_status(value: &str) -> bool {
    matches!(
        value,
        "eligible" | "notified" | "opened" | "interested" | "helping" | "completed" | "declined"
    )
}

fn engagement_rank(value: &str) -> u8 {
    match value {
        "eligible" => 0,
        "notified" => 1,
        "opened" => 2,
        "interested" => 3,
        "helping" => 4,
        "completed" => 5,
        "declined" => 6,
        _ => 0,
    }
}

fn next_engagement_status(current: Option<&str>, action: EngagementAction) -> Result<&'static str, ()> {
    let target = action.as_str();
    let Some(current) = current else {
        return Ok(target);
    };
    if current == "completed" {
        return if action == EngagementAction::Completed { Ok("completed") } else { Err(()) };
    }
    if current == "declined" {
        return if action == EngagementAction::Declined { Ok("declined") } else { Err(()) };
    }
    if action == EngagementAction::Declined {
        return Ok("declined");
    }
    if engagement_rank(target) >= engagement_rank(current) {
        Ok(target)
    } else {
        Ok(match current {
            "notified" => "notified",
            "opened" => "opened",
            "interested" => "interested",
            "helping" => "helping",
            _ => "eligible",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engagement_state_is_monotonic_and_terminal() {
        assert_eq!(next_engagement_status(Some("notified"), EngagementAction::Opened), Ok("opened"));
        assert_eq!(next_engagement_status(Some("helping"), EngagementAction::Opened), Ok("helping"));
        assert_eq!(next_engagement_status(Some("helping"), EngagementAction::Completed), Ok("completed"));
        assert_eq!(next_engagement_status(Some("declined"), EngagementAction::Interested), Err(()));
        assert_eq!(next_engagement_status(Some("completed"), EngagementAction::Opened), Err(()));
    }

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
