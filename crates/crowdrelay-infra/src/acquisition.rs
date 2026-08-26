//! PostgreSQL acquisition repository: smart-link resolution, click batching,
//! fan signup, and city signal listing.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use crowdrelay_application::{
    AcquisitionRepository, RepositoryError, SignupFanCommand, UpsertSmartLinkCommand,
    UpsertedSmartLink,
};
use crowdrelay_domain::{
    CampaignId, CityId, CitySignal, CitySlug, ClickEvent, CountryCode, DestinationUrl,
    FanActionToken, FanId, FanSignup, FanSignupEmailKind, FanSignupResult, FanStatus, ReferralCode,
    ResolvedSmartLink, SmartLinkId, SmartLinkSlug, WorkspaceId, WorkspaceSlug,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
    fan_lifecycle::{issue_confirmation_token, issue_fan_action_token},
    referrals::{
        issue_fan_session, qualify_signup_referral_and_rewards, record_pending_signup_referral,
    },
    sensitive_response::SensitiveResponseCodec,
};

const IDEMPOTENCY_SCOPE: &str = "fan_signup";
const JSON_CONTENT_TYPE: &str = "application/json";
const ENCRYPTED_JSON_CONTENT_TYPE: &str = "application/vnd.crowdrelay.encrypted+json";
const IDEMPOTENCY_RETENTION_MILLISECONDS: i64 = 86_400_000;
const MAX_CLICK_BATCH_ROWS: usize = 1_000;
const MAX_CITY_SIGNAL_ROWS: u32 = 1_000;
const CONFIRMATION_RESEND_COOLDOWN_MINUTES: i64 = 1;
const CONFIRMATION_RESEND_COOLDOWN_SECONDS: u32 = 60;

/// Tenant-scoped PostgreSQL implementation of the acquisition repository.
///
/// The workspace slug comes from trusted process configuration, never from a
/// public request. Repository queries additionally verify any workspace ID in
/// a command against that configured workspace.
#[derive(Clone, Debug)]
pub struct PostgresAcquisitionRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    default_country_code: CountryCode,
    operation_timeout: Duration,
    require_double_opt_in: bool,
    sensitive_response_codec: SensitiveResponseCodec,
}

struct FanActiveOutboxArgs<'a> {
    workspace_id: WorkspaceId,
    command: &'a SignupFanCommand,
    fan_id: FanId,
    referral_code: &'a ReferralCode,
    unsubscribe_token: &'a FanActionToken,
    created: bool,
}

include!("acquisition/ingress_methods.rs");
include!("acquisition/persistence_methods.rs");

#[async_trait]
impl AcquisitionRepository for PostgresAcquisitionRepository {
    async fn resolve_workspace(
        &self,
        slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, RepositoryError> {
        self.bounded(self.resolve_workspace_inner(slug))
            .await
            .map_err(Into::into)
    }

    async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
        self.bounded(self.load_active_smart_links_inner())
            .await
            .map_err(Into::into)
    }

    async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
        self.bounded(self.persist_click_batch_inner(clicks))
            .await
            .map_err(Into::into)
    }

    async fn persist_fan_signup(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, RepositoryError> {
        self.bounded(self.persist_fan_signup_inner(command))
            .await
            .map_err(Into::into)
    }

    async fn list_city_signals(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, RepositoryError> {
        self.bounded(self.list_city_signals_inner(workspace_id, limit))
            .await
            .map_err(Into::into)
    }

    async fn upsert_smart_link<'a>(
        &self,
        command: &UpsertSmartLinkCommand<'a>,
    ) -> Result<UpsertedSmartLink, RepositoryError> {
        self.bounded(async move {
            let row = sqlx::query_as::<_, SmartLinkUpsertRow>(
                r#"
                INSERT INTO smart_links (
                    workspace_id, slug, destination_url, active,
                    channel_source, channel_community, channel_creative,
                    campaign_id
                )
                VALUES ($1, $2, $3, true, $4, $5, $6, $7)
                ON CONFLICT (workspace_id, slug) DO UPDATE SET
                    destination_url = EXCLUDED.destination_url,
                    active = true,
                    channel_source = EXCLUDED.channel_source,
                    channel_community = EXCLUDED.channel_community,
                    channel_creative = EXCLUDED.channel_creative,
                    campaign_id = EXCLUDED.campaign_id,
                    version = smart_links.version + 1
                RETURNING id, slug, destination_url, active,
                          channel_source, channel_community, channel_creative,
                          campaign_id
                "#,
            )
            .bind(command.workspace_id.into_uuid())
            .bind(command.slug.as_str())
            .bind(command.destination_url)
            .bind(command.channel_source)
            .bind(command.channel_community)
            .bind(command.channel_creative)
            .bind(command.campaign_id.map(|id| id.into_uuid()))
            .fetch_one(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?;
            Ok(UpsertedSmartLink {
                id: row.id,
                slug: SmartLinkSlug::parse(&row.slug).map_err(|_| StoreError::Unexpected)?,
                destination_url: row.destination_url,
                active: row.active,
                channel_source: row.channel_source,
                channel_community: row.channel_community,
                channel_creative: row.channel_creative,
                campaign_id: row.campaign_id.map(CampaignId::from_uuid),
            })
        })
        .await
        .map_err(Into::into)
    }

    async fn list_smart_links(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<UpsertedSmartLink>, RepositoryError> {
        self.bounded(async move {
            let rows = sqlx::query_as::<_, SmartLinkUpsertRow>(
                r#"
                SELECT id, slug, destination_url, active,
                       channel_source, channel_community, channel_creative,
                       campaign_id
                FROM smart_links
                WHERE workspace_id = $1
                ORDER BY channel_source NULLS LAST, slug
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?;
            rows.into_iter()
                .map(|row| {
                    Ok(UpsertedSmartLink {
                        id: row.id,
                        slug: SmartLinkSlug::parse(&row.slug)
                            .map_err(|_| StoreError::Unexpected)?,
                        destination_url: row.destination_url,
                        active: row.active,
                        channel_source: row.channel_source,
                        channel_community: row.channel_community,
                        channel_creative: row.channel_creative,
                        campaign_id: row.campaign_id.map(CampaignId::from_uuid),
                    })
                })
                .collect()
        })
        .await
        .map_err(Into::into)
    }

    async fn load_or_create_fan_referral_code(
        &self,
        workspace_id: WorkspaceId,
        fan_id: FanId,
    ) -> Result<ReferralCode, RepositoryError> {
        self.bounded(async move {
            let mut transaction = self.pool.begin().await.map_err(StoreError::from_sqlx)?;
            // Verify the fan exists in this workspace.
            let fan_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM fans WHERE workspace_id = $1 AND id = $2)",
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if !fan_exists {
                return Err(StoreError::NotFound);
            }
            // Return existing active code if one exists.
            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT code
                FROM referral_codes
                WHERE workspace_id = $1 AND fan_id = $2 AND active
                ORDER BY created_at, id
                LIMIT 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if let Some(code) = existing {
                transaction.commit().await.map_err(StoreError::from_sqlx)?;
                return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
            }
            // Generate a new code. Retry on the unlikely collision: 18 random
            // bytes hex-encoded is 72 bits of entropy, and the unique
            // constraint is on (workspace_id, code).
            for _ in 0..3 {
                let inserted = sqlx::query_scalar::<_, String>(
                    r#"
                    INSERT INTO referral_codes (workspace_id, fan_id, code)
                    VALUES ($1, $2, encode(gen_random_bytes(18), 'hex'))
                    ON CONFLICT DO NOTHING
                    RETURNING code
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(fan_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(StoreError::from_sqlx)?;
                if let Some(code) = inserted {
                    transaction.commit().await.map_err(StoreError::from_sqlx)?;
                    return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
                }
                // Collision — check whether another transaction created one.
                let retry = sqlx::query_scalar::<_, String>(
                    r#"
                    SELECT code
                    FROM referral_codes
                    WHERE workspace_id = $1 AND fan_id = $2 AND active
                    ORDER BY created_at, id
                    LIMIT 1
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(fan_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(StoreError::from_sqlx)?;
                if let Some(code) = retry {
                    transaction.commit().await.map_err(StoreError::from_sqlx)?;
                    return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
                }
            }
            Err(StoreError::Unexpected)
        })
        .await
        .map_err(Into::into)
    }
}

#[derive(sqlx::FromRow)]
struct SmartLinkUpsertRow {
    id: Uuid,
    slug: String,
    destination_url: String,
    active: bool,
    channel_source: Option<String>,
    channel_community: Option<String>,
    channel_creative: Option<String>,
    campaign_id: Option<Uuid>,
}

/// Non-blocking sender used directly by the redirect fast path.
#[derive(Clone, Debug)]
pub struct ClickBuffer {
    sender: mpsc::Sender<ClickEvent>,
    metrics: Arc<ClickBufferMetrics>,
}

impl ClickBuffer {
    /// Builds a fixed-capacity channel and its single batch-writing consumer.
    pub fn new(
        repository: Arc<dyn AcquisitionRepository>,
        config: crate::config::ClickBufferConfig,
    ) -> Result<(Self, ClickBatchWorker), ClickBufferBuildError> {
        if config.capacity == 0
            || config.batch_size == 0
            || config.batch_size > config.capacity
            || config.batch_size > MAX_CLICK_BATCH_ROWS
            || config.flush_interval.is_zero()
        {
            return Err(ClickBufferBuildError);
        }

        let (sender, receiver) = mpsc::channel(config.capacity);
        let metrics = Arc::new(ClickBufferMetrics::default());
        Ok((
            Self {
                sender,
                metrics: Arc::clone(&metrics),
            },
            ClickBatchWorker {
                receiver,
                repository,
                metrics,
                batch_size: config.batch_size,
                flush_interval: config.flush_interval,
            },
        ))
    }

    /// Attempts to queue a click without waiting or allocating an unbounded
    /// retry. Overload and a stopped consumer both drop analytics only.
    #[must_use]
    pub fn try_send(&self, event: ClickEvent) -> ClickEnqueueOutcome {
        match self.sender.try_send(event) {
            Ok(()) => {
                self.metrics.queued.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::DroppedClosed
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<ClickBufferMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClickEnqueueOutcome {
    Queued,
    DroppedFull,
    DroppedClosed,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("click buffer configuration is invalid")]
pub struct ClickBufferBuildError;

#[derive(Debug, Default)]
pub struct ClickBufferMetrics {
    queued: AtomicU64,
    persisted: AtomicU64,
    dropped: AtomicU64,
    persistence_failed: AtomicU64,
}

impl ClickBufferMetrics {
    #[must_use]
    pub fn snapshot(&self) -> ClickBufferSnapshot {
        ClickBufferSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            persistence_failed: self.persistence_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClickBufferSnapshot {
    pub queued: u64,
    pub persisted: u64,
    pub dropped: u64,
    pub persistence_failed: u64,
}

/// Cancellation-aware single consumer that persists fixed-size click batches.
pub struct ClickBatchWorker {
    receiver: mpsc::Receiver<ClickEvent>,
    repository: Arc<dyn AcquisitionRepository>,
    metrics: Arc<ClickBufferMetrics>,
    batch_size: usize,
    flush_interval: Duration,
}

impl ClickBatchWorker {
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut batch = Vec::with_capacity(self.batch_size);
        let mut flush_interval = interval(self.flush_interval);
        flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        flush_interval.tick().await;

        if *shutdown.borrow() {
            self.shutdown(&mut batch).await;
            return;
        }

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        self.shutdown(&mut batch).await;
                        return;
                    }
                }
                event = self.receiver.recv() => {
                    let Some(event) = event else {
                        self.flush(&mut batch).await;
                        return;
                    };
                    batch.push(event);
                    if batch.len() >= self.batch_size {
                        self.flush(&mut batch).await;
                    }
                }
                _ = flush_interval.tick() => {
                    self.flush(&mut batch).await;
                }
            }
        }
    }

    async fn shutdown(&mut self, batch: &mut Vec<ClickEvent>) {
        self.receiver.close();
        while batch.len() < self.batch_size {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
            batch.push(event);
        }
        self.flush(batch).await;

        let mut dropped = 0_u64;
        while self.receiver.try_recv().is_ok() {
            dropped = dropped.saturating_add(1);
        }
        self.metrics.dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    async fn flush(&self, batch: &mut Vec<ClickEvent>) {
        if batch.is_empty() {
            return;
        }
        let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        match self.repository.persist_click_batch(batch).await {
            Ok(()) => {
                self.metrics
                    .persisted
                    .fetch_add(batch_len, Ordering::Relaxed);
            }
            Err(_) => {
                self.metrics
                    .persistence_failed
                    .fetch_add(batch_len, Ordering::Relaxed);
                self.metrics.dropped.fetch_add(batch_len, Ordering::Relaxed);
                tracing::warn!(
                    batch_size = batch_len,
                    "click analytics batch persistence failed"
                );
            }
        }
        batch.clear();
    }
}

fn map_referral_error(error: crate::referrals::ReferralStoreError) -> StoreError {
    match error {
        crate::referrals::ReferralStoreError::Unavailable => StoreError::Unavailable,
        crate::referrals::ReferralStoreError::NotFound => StoreError::NotFound,
        crate::referrals::ReferralStoreError::Conflict => StoreError::Conflict,
        crate::referrals::ReferralStoreError::Unexpected => StoreError::Unexpected,
    }
}

fn map_lifecycle_error(error: crate::fan_lifecycle::LifecycleStoreError) -> StoreError {
    match error {
        crate::fan_lifecycle::LifecycleStoreError::Unavailable => StoreError::Unavailable,
        crate::fan_lifecycle::LifecycleStoreError::NotFound => StoreError::NotFound,
        crate::fan_lifecycle::LifecycleStoreError::Conflict => StoreError::Conflict,
        crate::fan_lifecycle::LifecycleStoreError::Unexpected => StoreError::Unexpected,
    }
}

#[derive(Debug, FromRow)]
struct SmartLinkRow {
    id: Uuid,
    workspace_id: Uuid,
    campaign_id: Option<Uuid>,
    slug: String,
    destination_url: String,
    version: i64,
}

impl TryFrom<SmartLinkRow> for ResolvedSmartLink {
    type Error = InvalidStoredData;

    fn try_from(row: SmartLinkRow) -> Result<Self, Self::Error> {
        let version = u64::try_from(row.version).map_err(|_| InvalidStoredData)?;
        ResolvedSmartLink::new(
            SmartLinkId::from_uuid(row.id),
            WorkspaceId::from_uuid(row.workspace_id),
            row.campaign_id.map(CampaignId::from_uuid),
            SmartLinkSlug::parse(row.slug).map_err(|_| InvalidStoredData)?,
            DestinationUrl::parse(row.destination_url).map_err(|_| InvalidStoredData)?,
            version,
        )
        .map_err(|_| InvalidStoredData)
    }
}

#[derive(Debug, FromRow)]
struct CitySignalRow {
    city_id: Uuid,
    slug: String,
    name: String,
    country_code: String,
    fan_count: i64,
}

impl TryFrom<CitySignalRow> for CitySignal {
    type Error = InvalidStoredData;

    fn try_from(row: CitySignalRow) -> Result<Self, Self::Error> {
        let fan_count = u64::try_from(row.fan_count).map_err(|_| InvalidStoredData)?;
        CitySignal::new(
            CityId::from_uuid(row.city_id),
            CitySlug::parse(row.slug).map_err(|_| InvalidStoredData)?,
            row.name,
            CountryCode::parse(row.country_code).map_err(|_| InvalidStoredData)?,
            fan_count,
        )
        .map_err(|_| InvalidStoredData)
    }
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<serde_json::Value>,
    response_content_type: Option<String>,
    lease_expired: bool,
}

#[derive(Debug, FromRow)]
struct FanRow {
    id: Uuid,
    status: String,
}

#[derive(Clone, Copy, Debug)]
struct StoredFan {
    id: FanId,
    status: FanStatus,
}

#[derive(Clone, Copy, Debug)]
struct FanUpsert {
    fan: StoredFan,
    created: bool,
    became_active: bool,
    already_active: bool,
    already_pending: bool,
}

impl TryFrom<FanRow> for StoredFan {
    type Error = StoreError;

    fn try_from(row: FanRow) -> Result<Self, Self::Error> {
        let status = match row.status.as_str() {
            "pending" => FanStatus::Pending,
            "active" => FanStatus::Active,
            "unsubscribed" => FanStatus::Unsubscribed,
            "suppressed" => FanStatus::Suppressed,
            _ => return Err(StoreError::Unexpected),
        };
        Ok(Self {
            id: FanId::from_uuid(row.id),
            status,
        })
    }
}

#[derive(Clone, Copy, Debug, FromRow)]
struct ReferralOwnerRow {
    id: Uuid,
    fan_id: Uuid,
}

#[derive(Clone, Copy, Debug)]
struct InvalidStoredData;

#[derive(Clone, Copy, Debug)]
enum StoreError {
    Unavailable,
    NotFound,
    Conflict,
    Unexpected,
}

impl StoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<StoreError> for RepositoryError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Unavailable => Self::Unavailable,
            StoreError::NotFound => Self::NotFound,
            StoreError::Conflict => Self::Conflict,
            StoreError::Unexpected => Self::Unexpected,
        }
    }
}

fn duration_as_milliseconds(duration: Duration) -> Result<i64, StoreError> {
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::Unexpected)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crowdrelay_domain::VisitorId;

    use super::*;

    #[derive(Default)]
    struct FakeRepository {
        persisted: Mutex<Vec<Vec<ClickEvent>>>,
    }

    #[async_trait]
    impl AcquisitionRepository for FakeRepository {
        async fn resolve_workspace(
            &self,
            _slug: &WorkspaceSlug,
        ) -> Result<Option<WorkspaceId>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
            self.persisted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(clicks.to_vec());
            Ok(())
        }

        async fn persist_fan_signup(
            &self,
            _command: &SignupFanCommand,
        ) -> Result<FanSignupResult, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn list_city_signals(
            &self,
            _workspace_id: WorkspaceId,
            _limit: u32,
        ) -> Result<Vec<CitySignal>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn upsert_smart_link<'a>(
            &self,
            _command: &UpsertSmartLinkCommand<'a>,
        ) -> Result<UpsertedSmartLink, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn list_smart_links(
            &self,
            _workspace_id: WorkspaceId,
        ) -> Result<Vec<UpsertedSmartLink>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_or_create_fan_referral_code(
            &self,
            _workspace_id: WorkspaceId,
            _fan_id: FanId,
        ) -> Result<ReferralCode, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }
    }

    fn click_event() -> Result<ClickEvent, Box<dyn std::error::Error>> {
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            WorkspaceId::new(),
            None,
            SmartLinkSlug::parse("tour")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
        )?;
        Ok(ClickEvent::from_link(
            &link,
            Some(VisitorId::new()),
            Some("example.com".to_owned()),
            OffsetDateTime::UNIX_EPOCH,
        )?)
    }

    #[test]
    fn rejects_invalid_smart_link_rows_instead_of_loading_unsafe_urls() {
        let row = SmartLinkRow {
            id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            campaign_id: None,
            slug: "safe-link".to_owned(),
            destination_url: "javascript:alert(1)".to_owned(),
            version: 1,
        };

        assert!(ResolvedSmartLink::try_from(row).is_err());
    }

    #[test]
    fn rejects_negative_database_counters() {
        let row = CitySignalRow {
            city_id: Uuid::now_v7(),
            slug: "wroclaw".to_owned(),
            name: "Wrocław".to_owned(),
            country_code: "PL".to_owned(),
            fan_count: -1,
        };

        assert!(CitySignal::try_from(row).is_err());
    }

    #[test]
    fn full_click_channel_drops_without_waiting() -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, _worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 1,
                batch_size: 1,
                flush_interval: Duration::from_secs(1),
            },
        )?;

        assert_eq!(buffer.try_send(click_event()?), ClickEnqueueOutcome::Queued);
        assert_eq!(
            buffer.try_send(click_event()?),
            ClickEnqueueOutcome::DroppedFull
        );
        assert_eq!(
            buffer.metrics().snapshot(),
            ClickBufferSnapshot {
                queued: 1,
                persisted: 0,
                dropped: 1,
                persistence_failed: 0,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_flush_is_bounded_to_one_batch() -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 4,
                batch_size: 2,
                flush_interval: Duration::from_secs(60),
            },
        )?;
        for _ in 0..4 {
            assert_eq!(buffer.try_send(click_event()?), ClickEnqueueOutcome::Queued);
        }
        let (shutdown_sender, shutdown) = watch::channel(true);
        worker.run(shutdown).await;
        drop(shutdown_sender);

        let snapshot = buffer.metrics().snapshot();
        assert_eq!(snapshot.queued, 4);
        assert_eq!(snapshot.persisted, 2);
        assert_eq!(snapshot.dropped, 2);
        assert_eq!(snapshot.persistence_failed, 0);
        Ok(())
    }
}
