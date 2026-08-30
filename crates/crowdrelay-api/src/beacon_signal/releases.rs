//! Physical-release fulfillment layered onto the existing Latarnik relationship.
//!
//! CrowdRelay owns eligibility, stock reservation and fulfillment state. n8n only
//! executes the provider delivery requested through the transactional outbox.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::{BeaconReleaseCampaignState, BeaconReleaseProgress};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::*;

mod admin;
mod member;

pub use admin::{
    admin_close_release_campaign, admin_create_release_campaign, admin_launch_release_campaign,
    admin_list_release_campaigns, admin_list_release_recipients, admin_update_release_recipient,
};
pub use member::{confirm_release_delivery, decline_release_delivery, my_release_campaigns};

/// Upper bound on the recipient roster returned by the admin release listing.
///
/// Delivered recipients accumulate for the lifetime of the workspace, so an
/// unbounded read here grows with every release ever shipped and is loaded into
/// memory and serialised in full on each admin page load. The ordering is
/// deterministic, so the bound always keeps the most recent campaigns and the
/// recipients that still need operator action.
const MAX_ADMIN_RELEASE_RECIPIENTS: i64 = 2_000;
const MAX_TITLE_LEN: usize = 200;
const MAX_SLUG_LEN: usize = 128;
const MAX_SKU_LEN: usize = 128;
const MAX_NAME_LEN: usize = 160;
const MAX_PHONE_LEN: usize = 32;
const MAX_LOCKER_LEN: usize = 32;

#[derive(Clone, Debug, FromRow)]
struct ReleaseCampaignRow {
    id: Uuid,
    slug: String,
    title: String,
    sku: String,
    product_name: String,
    variant_label: String,
    status: String,
    claim_deadline: OffsetDateTime,
    eligible_count: i32,
    reserved_quantity: i32,
    reservation_id: Option<Uuid>,
    launched_at: Option<OffsetDateTime>,
    closed_at: Option<OffsetDateTime>,
    cancelled_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    notified_count: i64,
    confirmed_count: i64,
    prepared_count: i64,
    sent_count: i64,
    delivered_count: i64,
    declined_count: i64,
    expired_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseCampaignView {
    id: Uuid,
    slug: String,
    title: String,
    sku: String,
    product_name: String,
    variant_label: String,
    status: String,
    phase: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    claim_deadline: OffsetDateTime,
    eligible_count: i32,
    reserved_quantity: i32,
    reservation_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339::option")]
    launched_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    closed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    cancelled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    notified_count: i64,
    confirmed_count: i64,
    prepared_count: i64,
    sent_count: i64,
    delivered_count: i64,
    declined_count: i64,
    expired_count: i64,
}

impl From<ReleaseCampaignRow> for ReleaseCampaignView {
    fn from(row: ReleaseCampaignRow) -> Self {
        let phase = BeaconReleaseCampaignState::try_from(row.status.as_str())
            .map(|state| {
                state
                    .phase(
                        row.claim_deadline,
                        BeaconReleaseProgress {
                            confirmed: row.confirmed_count,
                            prepared: row.prepared_count,
                            sent: row.sent_count,
                        },
                        OffsetDateTime::now_utc(),
                    )
                    .as_str()
            })
            .unwrap_or("unknown");
        Self {
            id: row.id,
            slug: row.slug,
            title: row.title,
            sku: row.sku,
            product_name: row.product_name,
            variant_label: row.variant_label,
            status: row.status,
            phase,
            claim_deadline: row.claim_deadline,
            eligible_count: row.eligible_count,
            reserved_quantity: row.reserved_quantity,
            reservation_id: row.reservation_id,
            launched_at: row.launched_at,
            closed_at: row.closed_at,
            cancelled_at: row.cancelled_at,
            created_at: row.created_at,
            notified_count: row.notified_count,
            confirmed_count: row.confirmed_count,
            prepared_count: row.prepared_count,
            sent_count: row.sent_count,
            delivered_count: row.delivered_count,
            declined_count: row.declined_count,
            expired_count: row.expired_count,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolSummary {
    active_release_latarnicy: i64,
    contactable_latarnicy: i64,
    missing_email: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateReleaseCampaignRequest {
    slug: String,
    title: String,
    sku: String,
    #[serde(with = "time::serde::rfc3339")]
    claim_deadline: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminReleaseCampaignsResponse {
    pool: PoolSummary,
    campaigns: Vec<ReleaseCampaignView>,
    recipients: Vec<AdminReleaseRecipientView>,
    /// True when the recipient roster was cut off at [`MAX_ADMIN_RELEASE_RECIPIENTS`].
    /// The operator UI must say so rather than present a silently partial roster
    /// as if it were the whole release.
    recipients_truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdateReleaseRecipientRequest {
    status: String,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminReleaseRecipientView {
    campaign_id: Uuid,
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    city: Option<String>,
    status: String,
    recipient_name: Option<String>,
    recipient_phone: Option<String>,
    parcel_locker_code: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    prepared_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    sent_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    activation_due_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    activation_queued_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    activation_suppressed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminReleaseRecipientsResponse {
    campaign_id: Uuid,
    recipients: Vec<AdminReleaseRecipientView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConfirmReleaseDeliveryRequest {
    recipient_name: String,
    recipient_phone: String,
    parcel_locker_code: String,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct MemberReleaseCampaignView {
    campaign_id: Uuid,
    slug: String,
    title: String,
    product_name: String,
    variant_label: String,
    status: String,
    recipient_status: String,
    #[serde(with = "time::serde::rfc3339")]
    claim_deadline: OffsetDateTime,
    recipient_name: Option<String>,
    recipient_phone: Option<String>,
    parcel_locker_code: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberReleaseCampaignsResponse {
    campaigns: Vec<MemberReleaseCampaignView>,
}

fn clean_text(value: &str, max_len: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= max_len).then(|| value.to_owned())
}

fn clean_slug(value: &str) -> Option<String> {
    let value = value.trim();
    let valid = !value.is_empty()
        && value.len() <= MAX_SLUG_LEN
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_'))
        });
    valid.then(|| value.to_owned())
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get(&crate::IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len())
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        })
        .map(str::to_owned)
}

async fn pool_summary(pool: &sqlx::PgPool, workspace_id: Uuid) -> Result<PoolSummary, sqlx::Error> {
    let (active, contactable, missing_email) = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT
          count(*) FILTER (
            WHERE beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
          )::bigint,
          count(*) FILTER (
            WHERE beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
              AND beacon.contact_email IS NOT NULL AND btrim(beacon.contact_email) <> ''
          )::bigint,
          count(*) FILTER (
            WHERE beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
              AND (beacon.contact_email IS NULL OR btrim(beacon.contact_email) = '')
          )::bigint
        FROM viryaos_beacon_signal_profiles profile
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
        WHERE profile.workspace_id=$1 AND profile.status='active'
          AND 'releases'=ANY(profile.topics)
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;
    Ok(PoolSummary {
        active_release_latarnicy: active,
        contactable_latarnicy: contactable,
        missing_email,
    })
}

async fn load_campaigns(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<Vec<ReleaseCampaignView>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ReleaseCampaignRow>(
        r#"
        SELECT campaign.id,campaign.slug,campaign.title,variant.sku,
               product.name AS product_name,variant.label AS variant_label,
               campaign.status,campaign.claim_deadline,campaign.eligible_count,
               campaign.reserved_quantity,campaign.reservation_id,campaign.launched_at,
               campaign.closed_at,campaign.cancelled_at,campaign.created_at,
               count(*) FILTER (WHERE recipient.status IN ('notified','confirmed','prepared','sent','delivered'))::bigint AS notified_count,
               count(*) FILTER (WHERE recipient.status='confirmed')::bigint AS confirmed_count,
               count(*) FILTER (WHERE recipient.status='prepared')::bigint AS prepared_count,
               count(*) FILTER (WHERE recipient.status='sent')::bigint AS sent_count,
               count(*) FILTER (WHERE recipient.status='delivered')::bigint AS delivered_count,
               count(*) FILTER (WHERE recipient.status='declined')::bigint AS declined_count,
               count(*) FILTER (WHERE recipient.status='expired')::bigint AS expired_count
        FROM viryaos_beacon_release_campaigns campaign
        JOIN merch_variants variant
          ON variant.workspace_id=campaign.workspace_id AND variant.id=campaign.variant_id
        JOIN merch_products product
          ON product.workspace_id=variant.workspace_id AND product.id=variant.product_id
        LEFT JOIN viryaos_beacon_release_recipients recipient
          ON recipient.workspace_id=campaign.workspace_id AND recipient.campaign_id=campaign.id
        WHERE campaign.workspace_id=$1
        GROUP BY campaign.id,variant.sku,product.name,variant.label
        ORDER BY campaign.created_at DESC,campaign.id DESC
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub(super) struct OperatorActionRecord<'a> {
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_id: Uuid,
    pub idempotency_key: &'a str,
    pub request_id: Option<&'a str>,
    pub details: serde_json::Value,
}

pub(super) async fn record_operator_action(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    record: OperatorActionRecord<'_>,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO operator_actions (
          id,workspace_id,action,target_type,target_id,idempotency_key,request_id,details
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
        ON CONFLICT (workspace_id,idempotency_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(record.action)
    .bind(record.target_type)
    .bind(record.target_id)
    .bind(record.idempotency_key)
    .bind(record.request_id)
    .bind(record.details)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(inserted.is_some())
}

pub(super) async fn executor_capability_available_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    capability: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM viryaos_executor_capabilities capability_row
            JOIN viryaos_executor_instances executor
              ON executor.workspace_id=capability_row.workspace_id
             AND executor.executor_id=capability_row.executor_id
            LEFT JOIN viryaos_executor_circuit_breakers breaker
              ON breaker.workspace_id=executor.workspace_id
             AND breaker.executor_id=executor.executor_id
            WHERE capability_row.workspace_id=$1
              AND capability_row.capability=$2
              AND capability_row.expires_at>now()
              AND executor.expires_at>now()
              AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
        )
        "#,
    )
    .bind(workspace_id)
    .bind(capability)
    .fetch_one(&mut **tx)
    .await
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}
