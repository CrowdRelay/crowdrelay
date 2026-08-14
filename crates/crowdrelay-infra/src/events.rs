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

include!("events/buffer.rs");
include!("events/support.rs");
include!("events/tests.rs");
