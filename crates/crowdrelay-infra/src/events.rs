//! PostgreSQL event discovery, interest registration and bounded conversion analytics.
//!
//! Implements the `EventRepository` port with PostgreSQL, including an
//! in-process click analytics buffer for event action tracking.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use crowdrelay_application::{EventRepository, RegisterEventInterestCommand, RepositoryError};
use crowdrelay_domain::{
    CampaignId, CityId, EventAction, EventCity, EventId, EventInterestResult, EventSlug,
    FanEventInterest, FanId, PublicEvent, VisitorId, WorkspaceId, WorkspaceSlug,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::{
    sync::{OnceCell, mpsc, watch},
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

use crate::config::{ClickBufferConfig, DatabaseConfig};
use crate::database::{SqlxErrorClass, classify_sqlx_error};

const INTEREST_IDEMPOTENCY_SCOPE: &str = "event_interest";
const IDEMPOTENCY_RETENTION_MILLISECONDS: i64 = 86_400_000;
const MAX_EVENT_ACTION_BATCH_ROWS: usize = 1_000;
const MAX_FAN_INTEREST_ROWS: u32 = 100;

#[derive(Clone, Debug)]
pub struct PostgresEventRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    workspace_id: Arc<OnceCell<WorkspaceId>>,
    operation_timeout: Duration,
    lock_timeout: Duration,
    reminder_offsets_minutes: Arc<Vec<u32>>,
}

impl PostgresEventRepository {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_slug: WorkspaceSlug,
        database: &DatabaseConfig,
        reminder_offsets_minutes: Vec<u32>,
    ) -> Self {
        Self {
            pool,
            workspace_slug,
            workspace_id: Arc::new(OnceCell::new()),
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
            reminder_offsets_minutes: Arc::new(reminder_offsets_minutes),
        }
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, EventStoreError>>,
    ) -> Result<T, EventStoreError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| EventStoreError::Unavailable)?
    }

    async fn trusted_workspace_id(&self) -> Result<WorkspaceId, EventStoreError> {
        self.workspace_id
            .get_or_try_init(|| async {
                let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
                    .bind(self.workspace_slug.as_str())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(EventStoreError::from_sqlx)?
                    .ok_or(EventStoreError::NotFound)?;
                Ok(WorkspaceId::from_uuid(id))
            })
            .await
            .copied()
    }

    async fn load_published_events_inner(&self) -> Result<Vec<PublicEvent>, EventStoreError> {
        let rows = sqlx::query_as::<_, PublicEventRow>(
            r#"
            SELECT
                events.id,
                events.slug,
                events.title,
                events.description,
                events.city_id,
                cities.slug AS city_slug,
                cities.name AS city_name,
                cities.country_code::text AS country_code,
                cities.region,
                events.venue,
                events.venue_address,
                events.timezone,
                events.starts_at,
                events.doors_at,
                events.ends_at,
                events.ticket_url,
                events.listen_url,
                events.image_url,
                events.trailer_url,
                events.external_event_url,
                events.updated_at
            FROM events
            INNER JOIN workspaces ON workspaces.id = events.workspace_id
            LEFT JOIN cities ON cities.id = events.city_id
            WHERE workspaces.slug = $1
                AND events.status = 'published'
                AND events.starts_at >= now() - interval '12 hours'
            ORDER BY events.starts_at, events.id
            "#,
        )
        .bind(self.workspace_slug.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(EventStoreError::from_sqlx)?;

        rows.into_iter()
            .map(PublicEvent::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| EventStoreError::Unexpected)
    }

    async fn persist_event_action_inner(
        &self,
        actions: &[EventAction],
    ) -> Result<(), EventStoreError> {
        if actions.is_empty() {
            return Ok(());
        }
        if actions.len() > MAX_EVENT_ACTION_BATCH_ROWS {
            return Err(EventStoreError::Conflict);
        }
        let workspace_id = self.trusted_workspace_id().await?;
        if actions
            .iter()
            .any(|action| action.workspace_id() != workspace_id)
        {
            return Err(EventStoreError::NotFound);
        }

        let workspace_ids = vec![workspace_id.into_uuid(); actions.len()];
        let event_ids: Vec<Uuid> = actions
            .iter()
            .map(|action| action.event_id().into_uuid())
            .collect();
        let action_names: Vec<String> = actions
            .iter()
            .map(|action| action.action().as_str().to_owned())
            .collect();
        let campaign_ids: Vec<Option<Uuid>> = actions
            .iter()
            .map(|action| action.campaign_id().map(Into::into))
            .collect();
        let visitor_ids: Vec<Option<Uuid>> = actions
            .iter()
            .map(|action| action.visitor_id().map(Into::into))
            .collect();
        let referrer_hosts: Vec<Option<String>> = actions
            .iter()
            .map(|action| action.referrer_host().map(str::to_owned))
            .collect();
        let occurred_at: Vec<OffsetDateTime> =
            actions.iter().map(EventAction::occurred_at).collect();

        let result = sqlx::query(
            r#"
            WITH candidates (
                workspace_id, event_id, action, campaign_id,
                anonymous_visitor_id, referrer_host, occurred_at
            ) AS (
                SELECT * FROM UNNEST(
                    $1::uuid[], $2::uuid[], $3::text[], $4::uuid[],
                    $5::uuid[], $6::text[], $7::timestamptz[]
                )
            ), normalized_candidates AS (
                SELECT
                    candidates.workspace_id,
                    candidates.event_id,
                    candidates.action,
                    CASE WHEN campaigns.active THEN campaigns.id ELSE NULL END AS campaign_id,
                    candidates.anonymous_visitor_id,
                    candidates.referrer_host,
                    candidates.occurred_at
                FROM candidates
                INNER JOIN events
                    ON events.workspace_id = candidates.workspace_id
                    AND events.id = candidates.event_id
                LEFT JOIN campaigns
                    ON campaigns.workspace_id = candidates.workspace_id
                    AND campaigns.id = candidates.campaign_id
            ), validated_candidates AS (
                SELECT *
                FROM normalized_candidates
                WHERE (
                    SELECT count(*)::bigint
                    FROM normalized_candidates
                ) = $8
            )
            INSERT INTO event_action_events (
                workspace_id, event_id, action, campaign_id,
                anonymous_visitor_id, referrer_host, occurred_at
            )
            SELECT * FROM validated_candidates
            "#,
        )
        .bind(&workspace_ids)
        .bind(&event_ids)
        .bind(&action_names)
        .bind(&campaign_ids)
        .bind(&visitor_ids)
        .bind(&referrer_hosts)
        .bind(&occurred_at)
        .bind(i64::try_from(actions.len()).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await
        .map_err(EventStoreError::from_sqlx)?;
        if result.rows_affected() != u64::try_from(actions.len()).unwrap_or(u64::MAX) {
            return Err(EventStoreError::Conflict);
        }
        Ok(())
    }

    async fn register_interest_inner(
        &self,
        command: &RegisterEventInterestCommand,
    ) -> Result<EventInterestResult, EventStoreError> {
        let request_hash = interest_request_hash(command)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(EventStoreError::from_sqlx)?;
        configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;
        let workspace_id =
            trusted_workspace_id_in_transaction(&mut transaction, &self.workspace_slug).await?;
        if workspace_id != command.workspace_id() {
            return Err(EventStoreError::NotFound);
        }

        let inserted = start_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            self.operation_timeout,
        )
        .await?;
        if !inserted {
            let row = lock_idempotency(&mut transaction, workspace_id, command).await?;
            if row.request_hash != request_hash {
                return Err(EventStoreError::Conflict);
            }
            if row.state == "completed" {
                let response = row.response_body.ok_or(EventStoreError::Unexpected)?;
                let result =
                    serde_json::from_str(&response).map_err(|_| EventStoreError::Unexpected)?;
                transaction
                    .commit()
                    .await
                    .map_err(EventStoreError::from_sqlx)?;
                return Ok(result);
            }
            if !row.lease_expired {
                return Err(EventStoreError::Conflict);
            }
            reclaim_idempotency(
                &mut transaction,
                workspace_id,
                command,
                self.operation_timeout,
            )
            .await?;
        }

        let fan_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE fan_sessions
            SET last_seen_at = now()
            WHERE workspace_id = $1
                AND session_token_hash = digest($2, 'sha256')
                AND revoked_at IS NULL
                AND expires_at > now()
            RETURNING fan_id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.fan_session().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?
        .ok_or(EventStoreError::NotFound)?;

        let event = sqlx::query_as::<_, InterestEventRow>(
            r#"
            SELECT id, starts_at
            FROM events
            WHERE workspace_id = $1 AND slug = $2 AND status = 'published'
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.event_slug().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?
        .ok_or(EventStoreError::NotFound)?;

        let campaign_id = match command.campaign_id() {
            Some(campaign_id) => sqlx::query_scalar::<_, Uuid>(
                r#"
                    SELECT id
                    FROM campaigns
                    WHERE workspace_id = $1 AND id = $2 AND active
                    "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(campaign_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(EventStoreError::from_sqlx)?
            .map(CampaignId::from_uuid),
            None => None,
        };

        let referral_code_id =
            current_referral_code_id(&mut transaction, workspace_id, fan_id).await?;
        let inserted_interest = sqlx::query_scalar::<_, bool>(
            r#"
            INSERT INTO event_interests (
                workspace_id, event_id, fan_id, campaign_id, source,
                anonymous_visitor_id, referral_code_id, request_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (workspace_id, event_id, fan_id) DO UPDATE SET
                campaign_id = COALESCE(event_interests.campaign_id, EXCLUDED.campaign_id),
                anonymous_visitor_id = COALESCE(
                    event_interests.anonymous_visitor_id,
                    EXCLUDED.anonymous_visitor_id
                ),
                referral_code_id = COALESCE(
                    event_interests.referral_code_id,
                    EXCLUDED.referral_code_id
                ),
                request_id = EXCLUDED.request_id,
                source = EXCLUDED.source
            RETURNING xmax = 0
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event.id)
        .bind(fan_id)
        .bind(campaign_id.map(Into::<Uuid>::into))
        .bind(command.source())
        .bind(command.visitor_id().map(Into::<Uuid>::into))
        .bind(referral_code_id)
        .bind(command.request_id().as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?;

        let reminder_count = schedule_reminders(
            &mut transaction,
            workspace_id,
            EventId::from_uuid(event.id),
            FanId::from_uuid(fan_id),
            event.starts_at,
            &self.reminder_offsets_minutes,
        )
        .await?;
        let result = EventInterestResult {
            event_id: EventId::from_uuid(event.id),
            fan_id: FanId::from_uuid(fan_id),
            created: inserted_interest,
            reminder_count,
        };
        if result.created {
            append_outbox(
                &mut transaction,
                workspace_id,
                "event.interest_registered",
                command.request_id().as_str(),
                json!({
                    "workspace_id": workspace_id,
                    "event_id": result.event_id,
                    "fan_id": result.fan_id,
                    "campaign_id": campaign_id,
                    "source": command.source(),
                    "created": true,
                    "reminder_count": result.reminder_count,
                }),
            )
            .await?;
        }
        complete_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(EventStoreError::from_sqlx)?;
        Ok(result)
    }

    async fn list_fan_interests_inner(
        &self,
        workspace_id: WorkspaceId,
        session_token: &crowdrelay_domain::FanSessionToken,
        limit: u32,
    ) -> Result<Vec<FanEventInterest>, EventStoreError> {
        if !(1..=MAX_FAN_INTEREST_ROWS).contains(&limit) {
            return Err(EventStoreError::Conflict);
        }
        if self.trusted_workspace_id().await? != workspace_id {
            return Err(EventStoreError::NotFound);
        }
        let fan_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE fan_sessions SET last_seen_at = now()
            WHERE workspace_id = $1
                AND session_token_hash = digest($2, 'sha256')
                AND revoked_at IS NULL AND expires_at > now()
            RETURNING fan_id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(session_token.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(EventStoreError::from_sqlx)?
        .ok_or(EventStoreError::NotFound)?;

        let rows = sqlx::query_as::<_, FanInterestRow>(
            r#"
            SELECT
                events.id,
                events.slug,
                events.title,
                events.description,
                events.city_id,
                cities.slug AS city_slug,
                cities.name AS city_name,
                cities.country_code::text AS country_code,
                cities.region,
                events.venue,
                events.venue_address,
                events.timezone,
                events.starts_at,
                events.doors_at,
                events.ends_at,
                events.ticket_url,
                events.listen_url,
                events.image_url,
                events.trailer_url,
                events.external_event_url,
                events.updated_at,
                event_interests.created_at AS interested_at
            FROM event_interests
            INNER JOIN events
                ON events.workspace_id = event_interests.workspace_id
                AND events.id = event_interests.event_id
            LEFT JOIN cities ON cities.id = events.city_id
            WHERE event_interests.workspace_id = $1
                AND event_interests.fan_id = $2
            ORDER BY events.starts_at, events.id
            LIMIT $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(EventStoreError::from_sqlx)?;

        rows.into_iter()
            .map(FanEventInterest::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| EventStoreError::Unexpected)
    }
}

#[async_trait]
impl EventRepository for PostgresEventRepository {
    async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
        self.bounded(self.load_published_events_inner())
            .await
            .map_err(Into::into)
    }

    async fn persist_event_action(&self, actions: &[EventAction]) -> Result<(), RepositoryError> {
        self.bounded(self.persist_event_action_inner(actions))
            .await
            .map_err(Into::into)
    }

    async fn register_interest(
        &self,
        command: &RegisterEventInterestCommand,
    ) -> Result<EventInterestResult, RepositoryError> {
        self.bounded(self.register_interest_inner(command))
            .await
            .map_err(Into::into)
    }

    async fn list_fan_interests(
        &self,
        workspace_id: WorkspaceId,
        session_token: &crowdrelay_domain::FanSessionToken,
        limit: u32,
    ) -> Result<Vec<FanEventInterest>, RepositoryError> {
        self.bounded(self.list_fan_interests_inner(workspace_id, session_token, limit))
            .await
            .map_err(Into::into)
    }
}

#[derive(Clone, Debug)]
pub struct EventActionBuffer {
    sender: mpsc::Sender<EventAction>,
    metrics: Arc<EventActionBufferMetrics>,
}

impl EventActionBuffer {
    pub fn new(
        repository: Arc<dyn EventRepository>,
        config: ClickBufferConfig,
    ) -> Result<(Self, EventActionBatchWorker), EventActionBufferBuildError> {
        if config.capacity == 0
            || config.batch_size == 0
            || config.batch_size > config.capacity
            || config.batch_size > MAX_EVENT_ACTION_BATCH_ROWS
            || config.flush_interval.is_zero()
        {
            return Err(EventActionBufferBuildError);
        }
        let (sender, receiver) = mpsc::channel(config.capacity);
        let metrics = Arc::new(EventActionBufferMetrics::default());
        Ok((
            Self {
                sender,
                metrics: Arc::clone(&metrics),
            },
            EventActionBatchWorker {
                receiver,
                repository,
                batch_size: config.batch_size,
                flush_interval: config.flush_interval,
                metrics,
            },
        ))
    }

    pub fn try_send(&self, action: EventAction) -> EventActionEnqueueOutcome {
        match self.sender.try_send(action) {
            Ok(()) => {
                self.metrics.queued.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::DroppedClosed
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<EventActionBufferMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventActionEnqueueOutcome {
    Queued,
    DroppedFull,
    DroppedClosed,
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
#[error("event action buffer configuration is invalid")]
pub struct EventActionBufferBuildError;

#[derive(Debug, Default)]
pub struct EventActionBufferMetrics {
    queued: AtomicU64,
    persisted: AtomicU64,
    dropped: AtomicU64,
    persistence_failed: AtomicU64,
}

impl EventActionBufferMetrics {
    #[must_use]
    pub fn snapshot(&self) -> EventActionBufferSnapshot {
        EventActionBufferSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            persistence_failed: self.persistence_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventActionBufferSnapshot {
    pub queued: u64,
    pub persisted: u64,
    pub dropped: u64,
    pub persistence_failed: u64,
}

pub struct EventActionBatchWorker {
    receiver: mpsc::Receiver<EventAction>,
    repository: Arc<dyn EventRepository>,
    batch_size: usize,
    flush_interval: Duration,
    metrics: Arc<EventActionBufferMetrics>,
}

impl EventActionBatchWorker {
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.flush_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut batch = Vec::with_capacity(self.batch_size);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                action = self.receiver.recv() => {
                    match action {
                        Some(action) => {
                            batch.push(action);
                            if batch.len() >= self.batch_size {
                                self.flush(&mut batch).await;
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => self.flush(&mut batch).await,
            }
        }
        while let Ok(action) = self.receiver.try_recv() {
            if batch.len() >= self.batch_size {
                self.flush(&mut batch).await;
            }
            batch.push(action);
        }
        self.flush(&mut batch).await;
    }

    async fn flush(&self, batch: &mut Vec<EventAction>) {
        if batch.is_empty() {
            return;
        }
        let count = batch.len();
        match self.repository.persist_event_action(batch).await {
            Ok(()) => {
                self.metrics
                    .persisted
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
            Err(error) => {
                self.metrics
                    .persistence_failed
                    .fetch_add(count as u64, Ordering::Relaxed);
                self.metrics
                    .dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
                tracing::warn!(%error, count, "event action batch persistence failed");
            }
        }
        batch.clear();
    }
}

#[derive(FromRow)]
struct PublicEventRow {
    id: Uuid,
    slug: String,
    title: String,
    description: Option<String>,
    city_id: Option<Uuid>,
    city_slug: Option<String>,
    city_name: Option<String>,
    country_code: Option<String>,
    region: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    ticket_url: Option<String>,
    listen_url: Option<String>,
    image_url: Option<String>,
    trailer_url: Option<String>,
    external_event_url: Option<String>,
    updated_at: OffsetDateTime,
}

impl TryFrom<PublicEventRow> for PublicEvent {
    type Error = EventStoreError;

    fn try_from(row: PublicEventRow) -> Result<Self, Self::Error> {
        let city = match (row.city_id, row.city_slug, row.city_name, row.country_code) {
            (Some(id), Some(slug), Some(name), Some(country_code)) => Some(EventCity {
                id: CityId::from_uuid(id),
                slug,
                name,
                country_code,
                region: row.region,
            }),
            (None, None, None, None) => None,
            _ => return Err(EventStoreError::Unexpected),
        };
        let event = PublicEvent {
            id: EventId::from_uuid(row.id),
            slug: EventSlug::parse(row.slug).map_err(|_| EventStoreError::Unexpected)?,
            title: row.title,
            description: row.description,
            city,
            venue: row.venue,
            venue_address: row.venue_address,
            timezone: row.timezone,
            starts_at: row.starts_at,
            doors_at: row.doors_at,
            ends_at: row.ends_at,
            ticket_url: row.ticket_url,
            listen_url: row.listen_url,
            image_url: row.image_url,
            trailer_url: row.trailer_url,
            external_event_url: row.external_event_url,
            updated_at: row.updated_at,
        };
        event.validate().map_err(|_| EventStoreError::Unexpected)?;
        Ok(event)
    }
}

#[derive(FromRow)]
struct FanInterestRow {
    id: Uuid,
    slug: String,
    title: String,
    description: Option<String>,
    city_id: Option<Uuid>,
    city_slug: Option<String>,
    city_name: Option<String>,
    country_code: Option<String>,
    region: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    ticket_url: Option<String>,
    listen_url: Option<String>,
    image_url: Option<String>,
    trailer_url: Option<String>,
    external_event_url: Option<String>,
    updated_at: OffsetDateTime,
    interested_at: OffsetDateTime,
}

impl TryFrom<FanInterestRow> for FanEventInterest {
    type Error = EventStoreError;
    fn try_from(row: FanInterestRow) -> Result<Self, Self::Error> {
        let event = PublicEvent::try_from(PublicEventRow {
            id: row.id,
            slug: row.slug,
            title: row.title,
            description: row.description,
            city_id: row.city_id,
            city_slug: row.city_slug,
            city_name: row.city_name,
            country_code: row.country_code,
            region: row.region,
            venue: row.venue,
            venue_address: row.venue_address,
            timezone: row.timezone,
            starts_at: row.starts_at,
            doors_at: row.doors_at,
            ends_at: row.ends_at,
            ticket_url: row.ticket_url,
            listen_url: row.listen_url,
            image_url: row.image_url,
            trailer_url: row.trailer_url,
            external_event_url: row.external_event_url,
            updated_at: row.updated_at,
        })?;
        Ok(Self {
            event,
            interested_at: row.interested_at,
        })
    }
}

#[derive(FromRow)]
struct InterestEventRow {
    id: Uuid,
    starts_at: OffsetDateTime,
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<String>,
    lease_expired: bool,
}

#[derive(Serialize)]
struct InterestFingerprint<'a> {
    workspace_id: WorkspaceId,
    event_slug: &'a str,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    source: &'a str,
}

fn interest_request_hash(
    command: &RegisterEventInterestCommand,
) -> Result<Vec<u8>, EventStoreError> {
    let fingerprint = InterestFingerprint {
        workspace_id: command.workspace_id(),
        event_slug: command.event_slug().as_str(),
        campaign_id: command.campaign_id(),
        visitor_id: command.visitor_id(),
        source: command.source(),
    };
    let encoded = serde_json::to_vec(&fingerprint).map_err(|_| EventStoreError::Unexpected)?;
    Ok(Sha256::digest(encoded).to_vec())
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    operation_timeout: Duration,
    lock_timeout: Duration,
) -> Result<(), EventStoreError> {
    let statement_ms = duration_as_milliseconds(operation_timeout)?;
    let lock_ms = duration_as_milliseconds(lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(())
}

async fn trusted_workspace_id_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_slug: &WorkspaceSlug,
) -> Result<WorkspaceId, EventStoreError> {
    let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
        .bind(workspace_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?
        .ok_or(EventStoreError::NotFound)?;
    Ok(WorkspaceId::from_uuid(id))
}

async fn current_referral_code_id(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
) -> Result<Option<Uuid>, EventStoreError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT referral_code_id
        FROM fan_acquisition_events
        WHERE workspace_id = $1 AND fan_id = $2 AND referral_code_id IS NOT NULL
        ORDER BY occurred_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)
}

async fn schedule_reminders(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    fan_id: FanId,
    starts_at: OffsetDateTime,
    offsets: &[u32],
) -> Result<u32, EventStoreError> {
    let mut inserted = 0_u32;
    for offset_minutes in offsets {
        let offset = time::Duration::minutes(i64::from(*offset_minutes));
        let due_at = starts_at
            .checked_sub(offset)
            .ok_or(EventStoreError::Unexpected)?;
        if due_at <= OffsetDateTime::now_utc() {
            continue;
        }
        let kind = format!("{}m_before", offset_minutes);
        let result = sqlx::query(
            r#"
            INSERT INTO event_reminder_jobs (
                workspace_id, event_id, fan_id, reminder_kind, due_at
            ) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, event_id, fan_id, reminder_kind) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(kind)
        .bind(due_at)
        .execute(&mut **transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?;
        inserted = inserted.saturating_add(u32::try_from(result.rows_affected()).unwrap_or(0));
    }
    Ok(inserted)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), EventStoreError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
        VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(())
}

async fn start_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    request_hash: &[u8],
    operation_timeout: Duration,
) -> Result<bool, EventStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    sqlx::query(
        r#"
        DELETE FROM idempotency_keys
        WHERE workspace_id = $1
            AND scope = $2
            AND key = $3
            AND expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;

    let result = sqlx::query(
        r#"
        INSERT INTO idempotency_keys (
            workspace_id, scope, key, request_hash, state,
            lease_owner, lease_expires_at, expires_at
        ) VALUES (
            $1, $2, $3, $4, 'in_progress', $5,
            now() + ($6::bigint * interval '1 millisecond'),
            now() + ($7::bigint * interval '1 millisecond')
        ) ON CONFLICT (workspace_id, scope, key) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .bind(IDEMPOTENCY_RETENTION_MILLISECONDS)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(result.rows_affected() == 1)
}

async fn lock_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
) -> Result<IdempotencyRow, EventStoreError> {
    sqlx::query_as::<_, IdempotencyRow>(
        r#"
        SELECT request_hash, state, response_body::text AS response_body,
               coalesce(lease_expires_at <= now(), false) AS lease_expired
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)
}

async fn reclaim_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    operation_timeout: Duration,
) -> Result<(), EventStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    let result = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET lease_owner = $4,
            lease_expires_at = now() + ($5::bigint * interval '1 millisecond')
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND state = 'in_progress' AND lease_expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    if result.rows_affected() != 1 {
        return Err(EventStoreError::Conflict);
    }
    Ok(())
}

async fn complete_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    request_hash: &[u8],
    result: &EventInterestResult,
) -> Result<(), EventStoreError> {
    let response = serde_json::to_value(result).map_err(|_| EventStoreError::Unexpected)?;
    let updated = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET state = 'completed', lease_owner = NULL, lease_expires_at = NULL,
            response_status = 200, response_body = $5,
            response_content_type = 'application/json', completed_at = now()
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND request_hash = $4 AND state = 'in_progress'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(response)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    if updated.rows_affected() != 1 {
        return Err(EventStoreError::Conflict);
    }
    Ok(())
}

fn duration_as_milliseconds(duration: Duration) -> Result<i64, EventStoreError> {
    i64::try_from(duration.as_millis()).map_err(|_| EventStoreError::Unexpected)
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
enum EventStoreError {
    #[error("event store is unavailable")]
    Unavailable,
    #[error("event resource was not found")]
    NotFound,
    #[error("event request conflicts with durable state")]
    Conflict,
    #[error("event store failed unexpectedly")]
    Unexpected,
}

impl EventStoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<EventStoreError> for RepositoryError {
    fn from(value: EventStoreError) -> Self {
        match value {
            EventStoreError::Unavailable => Self::Unavailable,
            EventStoreError::NotFound => Self::NotFound,
            EventStoreError::Conflict => Self::Conflict,
            EventStoreError::Unexpected => Self::Unexpected,
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use crowdrelay_application::{EventRepository, RegisterEventInterestCommand, RepositoryError};
    use crowdrelay_domain::{
        EventAction, EventActionKind, EventId, EventInterestResult, FanEventInterest,
        FanSessionToken, PublicEvent, WorkspaceId,
    };
    use time::OffsetDateTime;

    use super::*;

    struct FailingEventRepository;

    #[async_trait]
    impl EventRepository for FailingEventRepository {
        async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_event_action(
            &self,
            _actions: &[EventAction],
        ) -> Result<(), RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn register_interest(
            &self,
            _command: &RegisterEventInterestCommand,
        ) -> Result<EventInterestResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn list_fan_interests(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
            _limit: u32,
        ) -> Result<Vec<FanEventInterest>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    #[tokio::test]
    async fn failed_event_action_batches_are_counted_as_dropped()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_sender, receiver) = mpsc::channel(1);
        let metrics = Arc::new(EventActionBufferMetrics::default());
        let worker = EventActionBatchWorker {
            receiver,
            repository: Arc::new(FailingEventRepository),
            batch_size: 1,
            flush_interval: Duration::from_secs(1),
            metrics: Arc::clone(&metrics),
        };
        let mut batch = vec![EventAction::new(
            WorkspaceId::new(),
            EventId::new(),
            EventActionKind::PageView,
            None,
            None,
            None,
            OffsetDateTime::now_utc(),
        )?];

        worker.flush(&mut batch).await;

        assert!(batch.is_empty());
        assert_eq!(
            metrics.snapshot(),
            EventActionBufferSnapshot {
                queued: 0,
                persisted: 0,
                dropped: 1,
                persistence_failed: 1,
            }
        );
        Ok(())
    }
}
