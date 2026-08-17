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
use crowdrelay_domain::{BeaconReleaseCampaignState, BeaconReleaseRecipientState};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
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

const RELEASE_MEMBER_URL: &str = "https://virya.music/pl/latarnik/#wydania";
const MAX_TITLE_LEN: usize = 200;
const MAX_SLUG_LEN: usize = 128;
const MAX_SKU_LEN: usize = 128;
const MAX_NAME_LEN: usize = 160;
const MAX_PHONE_LEN: usize = 32;
const MAX_LOCKER_LEN: usize = 32;

#[derive(Debug, Serialize)]
struct ReleaseMailCopy {
    subject: String,
    text: String,
}

fn release_delivery_copy(
    locale: &str,
    display_name: &str,
    title: &str,
    deadline: OffsetDateTime,
) -> ReleaseMailCopy {
    if locale.starts_with("pl") {
        ReleaseMailCopy {
            subject: format!("Dziękujemy Latarniku — {title} czeka na Ciebie"),
            text: format!(
                "Dziękujemy Latarniku, {display_name}!\n\nMamy nowe fizyczne wydanie Viryi: {title}. Twój egzemplarz jest zarezerwowany w puli Latarników. Żebyśmy faktycznie mogli go wysłać, wejdź do swojego panelu i potwierdź dla tej premiery imię i nazwisko odbiorcy, telefon oraz Paczkomat przed {deadline}.\n\n{RELEASE_MEMBER_URL}\n\nJeśli chcesz pomóc przy tej premierze, w Press Roomie masz gotowe materiały. Najbardziej pomagają nam: recenzja lub artykuł, radio/podcast/wywiad, zdjęcia albo wideo, udostępnienie premiery oraz kontakt do sensownego medium, promotora lub klubu. Nic z tego nie jest obowiązkiem — płyta jest naszym podziękowaniem za bycie częścią Latarnika.\n\nMasz pytanie? Wojtek: 784947481.\n\nVirya",
                deadline = deadline.date(),
            ),
        }
    } else {
        ReleaseMailCopy {
            subject: format!("Thank you, Beacon — {title} is reserved for you"),
            text: format!(
                "Thank you, Beacon, {display_name}!\n\nWe have a new physical Virya release: {title}. Your copy is reserved in the Beacon pool. To receive it, open your Beacon panel and confirm the recipient name, phone number and parcel-locker destination for this release before {deadline}.\n\n{RELEASE_MEMBER_URL}\n\nThe Press Room contains ready-to-use material if you want to help with the release. Reviews/articles, radio/podcasts/interviews, live photos/video, sharing the release, and relevant media/promoter/venue introductions are especially useful. None of this is an obligation — the record is our thank-you for being part of Beacon.\n\nQuestions? Wojtek: +48 784947481.\n\nVirya",
                deadline = deadline.date(),
            ),
        }
    }
}

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
        Self {
            id: row.id,
            slug: row.slug,
            title: row.title,
            sku: row.sku,
            product_name: row.product_name,
            variant_label: row.variant_label,
            status: row.status,
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

#[derive(Debug, FromRow)]
struct InventoryAvailability {
    sku: String,
    on_hand: i64,
    reserved: i64,
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

async fn inventory_availability_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    variant_id: Uuid,
) -> Result<Option<InventoryAvailability>, sqlx::Error> {
    sqlx::query_as::<_, InventoryAvailability>(
        r#"
        SELECT variant.sku,
          COALESCE((SELECT SUM(ledger.delta)::bigint FROM inventory_ledger ledger
                    WHERE ledger.workspace_id=variant.workspace_id AND ledger.variant_id=variant.id),0)::bigint AS on_hand,
          COALESCE((SELECT SUM(item.quantity)::bigint
                    FROM inventory_reservation_items item
                    JOIN inventory_reservations reservation
                      ON reservation.workspace_id=item.workspace_id AND reservation.id=item.reservation_id
                    WHERE item.workspace_id=variant.workspace_id AND item.variant_id=variant.id
                      AND reservation.status='active'
                      AND (reservation.expires_at IS NULL OR reservation.expires_at>now())),0)::bigint AS reserved
        FROM merch_variants variant
        WHERE variant.workspace_id=$1 AND variant.id=$2 AND variant.active
        FOR UPDATE OF variant
        "#,
    )
    .bind(workspace_id)
    .bind(variant_id)
    .fetch_optional(&mut **tx)
    .await
}

// The arguments intentionally mirror one append-only operator_actions record.
// Keeping them explicit at this transaction boundary is clearer than a second
// DTO that would only be unpacked immediately into the SQL bind list.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_operator_action(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    details: serde_json::Value,
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
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(idempotency_key)
    .bind(request_id_value)
    .bind(details)
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

fn request_hash(campaign_id: Uuid, variant_id: Uuid, quantity: i32) -> Vec<u8> {
    Sha256::digest(format!("beacon-release:{campaign_id}:{variant_id}:{quantity}").as_bytes())
        .to_vec()
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}
