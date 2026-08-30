//! Postgres adapter for beacon signal and release admin operations.
//!
//! Implements the `BeaconReleaseAdminRepository` and `BeaconSignalRepository`
//! ports. All SQL writes that were previously in the API layer's
//! `beacon_signal/releases/admin.rs`, `beacon_signal.rs`, and
//! `beacon_signal/lifecycle/member.rs` are now here. The API handlers call
//! the port; this adapter executes the full transaction (reads + writes + audit).

mod admin;
mod signal;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

pub(super) const RELEASE_MEMBER_URL: &str = "https://virya.music/pl/latarnik/#wydania";

#[derive(Clone)]
pub struct PostgresBeaconReleaseRepository {
    pub(super) pool: PgPool,
}

impl PostgresBeaconReleaseRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Audit record for operator actions. Inserted within the same transaction
/// as the business write so the audit trail is atomically consistent.
pub(super) struct OperatorActionRecord<'a> {
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_id: Uuid,
    pub idempotency_key: &'a str,
    pub request_id: Option<&'a str>,
    pub details: serde_json::Value,
}

pub(super) async fn record_operator_action(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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

pub(super) struct InventoryAvailability {
    pub sku: String,
    pub on_hand: i64,
    pub reserved: i64,
}

pub(super) async fn inventory_availability_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    variant_id: Uuid,
) -> Result<Option<InventoryAvailability>, sqlx::Error> {
    sqlx::query_as::<_, (String, i64, i64)>(
        r#"
        SELECT mv.sku,
               COALESCE(SUM(il.delta),0)::bigint AS on_hand,
               COALESCE((
                 SELECT SUM(iri.quantity)::bigint
                 FROM inventory_reservation_items iri
                 JOIN inventory_reservations ir ON ir.id=iri.reservation_id AND ir.workspace_id=iri.workspace_id
                 WHERE iri.workspace_id=$1 AND iri.variant_id=$2 AND ir.status='active'
               ),0)::bigint AS reserved
        FROM merch_variants mv
        LEFT JOIN inventory_ledger il ON il.workspace_id=mv.workspace_id AND il.variant_id=mv.id
        WHERE mv.workspace_id=$1 AND mv.id=$2 AND mv.active
        GROUP BY mv.sku
        "#,
    )
    .bind(workspace_id)
    .bind(variant_id)
    .fetch_optional(&mut **tx)
    .await
    .map(|opt| opt.map(|(sku, on_hand, reserved)| InventoryAvailability { sku, on_hand, reserved }))
}

pub(super) fn request_hash(campaign_id: Uuid, variant_id: Uuid, quantity: i32) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(campaign_id.as_bytes());
    hasher.update(variant_id.as_bytes());
    hasher.update(quantity.to_be_bytes());
    hasher.finalize().to_vec()
}

pub(super) struct ReleaseDeliveryCopy {
    pub subject: String,
    pub text: String,
}

pub(super) fn release_delivery_copy(
    locale: &str,
    display_name: &str,
    title: &str,
    deadline: OffsetDateTime,
) -> ReleaseDeliveryCopy {
    let deadline_str = format!(
        "{:02}-{:02}-{:02}",
        deadline.year(),
        deadline.month() as u8,
        deadline.day()
    );
    if locale.starts_with("pl") {
        ReleaseDeliveryCopy {
            subject: format!("Dziękujemy Latarniku, {display_name}! Nowe wydanie: {title}"),
            text: format!(
                "Dziękujemy Latarniku, {display_name}!\n\nMamy nowe fizyczne wydanie Viryi: {title}. Twój egzemplarz jest zarezerwowany w puli Latarników. Żebyśmy faktycznie mogli go wysłać, wejdź do swojego panelu i potwierdź dla tej premiery imię i nazwisko odbiorcy, telefon oraz Paczkomat przed {deadline_str}.\n\n{RELEASE_MEMBER_URL}\n\nJeśli chcesz pomóc przy tej premierze, w Press Roomie masz gotowe materiały. Najbardziej pomagają nam: recenzja lub artykuł, radio/podcast/wywiad, zdjęcia albo wideo, udostępnienie premiery oraz kontakt do sensownego medium, promotora lub klubu. Nic z tego nie jest obowiązkiem — płyta jest naszym podziękowaniem za bycie częścią Latarnika.\n\nMasz pytanie? Wojtek: 784947481.\n\nVirya",
            ),
        }
    } else {
        ReleaseDeliveryCopy {
            subject: format!("Thank you, Beacon, {display_name}! New release: {title}"),
            text: format!(
                "Thank you, Beacon, {display_name}!\n\nWe have a new physical Virya release: {title}. Your copy is reserved in the Beacon pool. To receive it, open your Beacon panel and confirm the recipient name, phone number and parcel-locker destination for this release before {deadline_str}.\n\n{RELEASE_MEMBER_URL}\n\nThe Press Room contains ready-to-use material if you want to help with the release. Reviews/articles, radio/podcasts/interviews, live photos/video, sharing the release, and relevant media/promoter/venue introductions are especially useful. None of this is an obligation — the record is our thank-you for being part of Beacon.\n\nQuestions? Wojtek: +48 784947481.\n\nVirya",
            ),
        }
    }
}
