//! Durable event reminder scheduler.
//!
//! Polls the database for events approaching their start time and enqueues
//! reminder outbox events at configured offsets before each event.

use std::time::Duration;

use crowdrelay_application::beacon_release_activation_copy;
use serde_json::json;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const DEFAULT_BATCH_SIZE: i64 = 100;
const RELEASE_MEMBER_URL: &str = "https://virya.music/pl/latarnik/#wydania";

/// Periodic scheduler that enqueues event reminder outbox events.
#[derive(Clone, Debug)]
pub struct EventReminderScheduler {
    pool: PgPool,
    poll_interval: Duration,
    operation_timeout: Duration,
    batch_size: i64,
}

impl EventReminderScheduler {
    pub fn new(
        pool: PgPool,
        poll_interval: Duration,
        operation_timeout: Duration,
        lock_timeout: Duration,
    ) -> Result<Self, ReminderSchedulerBuildError> {
        if poll_interval.is_zero() || operation_timeout.is_zero() || lock_timeout.is_zero() {
            return Err(ReminderSchedulerBuildError);
        }

        Ok(Self {
            pool,
            poll_interval,
            operation_timeout,
            batch_size: DEFAULT_BATCH_SIZE,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout, self.enqueue_due()).await {
                        Ok(Ok(count)) if count > 0 => {
                            tracing::info!(count, "scheduled reminders and release follow-ups reconciled");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = ?error, "event reminder scheduling failed");
                        }
                        Err(_) => {
                            tracing::warn!("event reminder scheduling timed out");
                        }
                    }
                }
            }
        }
    }

    async fn enqueue_due(&self) -> Result<u64, ReminderSchedulerError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(ReminderSchedulerError::Database)?;
        cancel_ineligible_jobs(&mut transaction).await?;
        let rows = sqlx::query_as::<_, DueReminderRow>(
            r#"
            SELECT
                jobs.id,
                jobs.workspace_id,
                jobs.event_id,
                jobs.fan_id,
                jobs.reminder_kind,
                jobs.due_at,
                events.slug AS event_slug,
                events.title AS event_title,
                events.starts_at,
                events.doors_at,
                events.venue,
                events.venue_address,
                events.ticket_url,
                fans.normalized_email,
                fans.display_name,
                fans.locale
            FROM event_reminder_jobs AS jobs
            INNER JOIN events
                ON events.workspace_id = jobs.workspace_id
                AND events.id = jobs.event_id
            INNER JOIN fans
                ON fans.workspace_id = jobs.workspace_id
                AND fans.id = jobs.fan_id
            WHERE jobs.status = 'pending'
                AND jobs.due_at <= now()
                AND events.status = 'published'
                AND events.starts_at > now()
                AND fans.status = 'active'
            ORDER BY jobs.due_at, jobs.id
            FOR UPDATE OF jobs SKIP LOCKED
            LIMIT $1
            "#,
        )
        .bind(self.batch_size)
        .fetch_all(&mut *transaction)
        .await
        .map_err(ReminderSchedulerError::Database)?;

        for row in &rows {
            let payload = json!({
                "workspace_id": row.workspace_id,
                "event_id": row.event_id,
                "fan_id": row.fan_id,
                "reminder_kind": row.reminder_kind.as_str(),
                "due_at": row.due_at,
                "event": {
                    "slug": row.event_slug.as_str(),
                    "title": row.event_title.as_str(),
                    "starts_at": row.starts_at,
                    "doors_at": row.doors_at,
                    "venue": row.venue.as_deref(),
                    "venue_address": row.venue_address.as_deref(),
                    "ticket_url": row.ticket_url.as_deref(),
                },
                "fan": {
                    "email": row.normalized_email.as_str(),
                    "display_name": row.display_name.as_deref(),
                    "locale": row.locale.as_deref(),
                }
            });
            sqlx::query(
                r#"
                INSERT INTO outbox_events (
                    workspace_id, event_type, event_version, payload, request_id
                ) VALUES ($1, 'event.reminder_due', 1, $2, $3)
                "#,
            )
            .bind(row.workspace_id)
            .bind(payload)
            .bind(format!("event-reminder:{}", row.id))
            .execute(&mut *transaction)
            .await
            .map_err(ReminderSchedulerError::Database)?;
        }

        let checklist_emissions = enqueue_due_show_checklists(&mut transaction).await?;
        let activation_emissions =
            enqueue_due_beacon_release_activations(&mut transaction, self.batch_size).await?;

        let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
        if !ids.is_empty() {
            sqlx::query(
                r#"
                UPDATE event_reminder_jobs
                SET status = 'enqueued', enqueued_at = now()
                WHERE id = ANY($1::uuid[]) AND status = 'pending'
                "#,
            )
            .bind(&ids)
            .execute(&mut *transaction)
            .await
            .map_err(ReminderSchedulerError::Database)?;
        }
        transaction
            .commit()
            .await
            .map_err(ReminderSchedulerError::Database)?;
        Ok(u64::try_from(rows.len())
            .unwrap_or(u64::MAX)
            .saturating_add(checklist_emissions)
            .saturating_add(activation_emissions))
    }
}

#[derive(FromRow)]
struct DueBeaconReleaseActivationRow {
    workspace_id: Uuid,
    campaign_id: Uuid,
    beacon_id: Uuid,
    release_title: String,
    display_name: String,
    contact_email: Option<String>,
    locale: String,
    contactable: bool,
}

async fn enqueue_due_beacon_release_activations(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, ReminderSchedulerError> {
    let rows = sqlx::query_as::<_, DueBeaconReleaseActivationRow>(
        r#"
        SELECT recipient.workspace_id,
               recipient.campaign_id,
               recipient.beacon_id,
               campaign.title AS release_title,
               beacon.display_name,
               beacon.contact_email,
               COALESCE(profile.locale,'pl') AS locale,
               COALESCE(
                   profile.status='active'
                   AND beacon.active
                   AND beacon.verified
                   AND beacon.accepts_outreach
                   AND NOT beacon.do_not_contact
                   AND 'releases'=ANY(profile.topics)
                   AND beacon.contact_email IS NOT NULL
                   AND btrim(beacon.contact_email) <> '',
                   FALSE
               ) AS contactable
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacon_release_campaigns campaign
          ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=recipient.workspace_id AND beacon.id=recipient.beacon_id
        LEFT JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id=recipient.workspace_id AND profile.beacon_id=recipient.beacon_id
        WHERE recipient.status='delivered'
          AND recipient.activation_due_at IS NOT NULL
          AND recipient.activation_due_at<=now()
          AND recipient.activation_queued_at IS NULL
          AND recipient.activation_suppressed_at IS NULL
        ORDER BY recipient.activation_due_at,recipient.campaign_id,recipient.beacon_id
        FOR UPDATE OF recipient SKIP LOCKED
        LIMIT $1
        "#,
    )
    .bind(batch_size)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ReminderSchedulerError::Database)?;

    let mut queued = 0u64;
    let mut suppressed = 0u64;
    for row in &rows {
        if row.contactable {
            let Some(contact_email) = row
                .contact_email
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            else {
                sqlx::query(
                    "UPDATE viryaos_beacon_release_recipients SET activation_suppressed_at=now() WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3 AND activation_queued_at IS NULL AND activation_suppressed_at IS NULL",
                )
                .bind(row.workspace_id)
                .bind(row.campaign_id)
                .bind(row.beacon_id)
                .execute(&mut **transaction)
                .await
                .map_err(ReminderSchedulerError::Database)?;
                suppressed = suppressed.saturating_add(1);
                continue;
            };
            let copy = beacon_release_activation_copy(
                &row.locale,
                &row.display_name,
                &row.release_title,
                RELEASE_MEMBER_URL,
            );
            let payload = json!({
                "campaign_id": row.campaign_id,
                "release_title": row.release_title,
                "beacon_id": row.beacon_id,
                "display_name": row.display_name,
                "contact_email": contact_email,
                "member_url": RELEASE_MEMBER_URL,
                "message_kind": "activation_followup",
                "template": "beacon_physical_release_activation_v1",
                "subject": copy.subject,
                "text": copy.text,
            });
            sqlx::query(
                r#"
                INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id)
                VALUES ($1,'viryaos.beacon.release_delivery_confirmation_requested',1,$2,$3)
                "#,
            )
            .bind(row.workspace_id)
            .bind(payload)
            .bind(format!(
                "beacon-release-activation:{}:{}",
                row.campaign_id, row.beacon_id
            ))
            .execute(&mut **transaction)
            .await
            .map_err(ReminderSchedulerError::Database)?;
            sqlx::query(
                "UPDATE viryaos_beacon_release_recipients SET activation_queued_at=now() WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3 AND activation_queued_at IS NULL AND activation_suppressed_at IS NULL",
            )
            .bind(row.workspace_id)
            .bind(row.campaign_id)
            .bind(row.beacon_id)
            .execute(&mut **transaction)
            .await
            .map_err(ReminderSchedulerError::Database)?;
            queued = queued.saturating_add(1);
        } else {
            sqlx::query(
                "UPDATE viryaos_beacon_release_recipients SET activation_suppressed_at=now() WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3 AND activation_queued_at IS NULL AND activation_suppressed_at IS NULL",
            )
            .bind(row.workspace_id)
            .bind(row.campaign_id)
            .bind(row.beacon_id)
            .execute(&mut **transaction)
            .await
            .map_err(ReminderSchedulerError::Database)?;
            suppressed = suppressed.saturating_add(1);
        }
    }
    if !rows.is_empty() {
        tracing::debug!(
            queued,
            suppressed,
            "Latarnik release activation follow-ups reconciled"
        );
    }
    Ok(u64::try_from(rows.len()).unwrap_or(u64::MAX))
}

async fn enqueue_due_show_checklists(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<u64, ReminderSchedulerError> {
    let inserted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH due AS (
            SELECT event.workspace_id,
                   event.id AS event_id,
                   event.slug AS event_slug,
                   event.title,
                   event.starts_at,
                   CASE
                       WHEN event.starts_at BETWEEN now() + interval '6 days 18 hours'
                                                AND now() + interval '7 days' THEN 'week'
                       WHEN event.starts_at BETWEEN now() + interval '42 hours'
                                                AND now() + interval '48 hours' THEN 'two_days'
                   END AS phase
            FROM events AS event
            WHERE event.status = 'published'
              AND event.starts_at > now()
        ), inserted_events AS (
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
            SELECT due.workspace_id,
                   'show.checklist_due',
                   1,
                   jsonb_build_object(
                       'event_id', due.event_id,
                       'event_slug', due.event_slug,
                       'event_title', due.title,
                       'starts_at', due.starts_at,
                       'checklist', due.phase,
                       'severity', 'info',
                       'summary', CASE due.phase
                           WHEN 'week' THEN '7 dni do koncertu: zacznij odhaczać checklistę przygotowań.'
                           ELSE '2 dni do koncertu: domknij sprzęt, pliki, merch, strój i logistykę.'
                       END
                   ),
                   'show-checklist:' || due.event_id::text || ':' || due.phase
            FROM due
            WHERE due.phase IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM show_notification_emissions emission
                  WHERE emission.workspace_id = due.workspace_id
                    AND emission.event_id = due.event_id
                    AND emission.phase = due.phase
              )
            RETURNING id, workspace_id, payload
        ), inserted_push AS (
            INSERT INTO fan_push_deliveries (
                workspace_id, fan_id, audience_kind, endpoint_id,
                source_kind, source_id, category, title, body, target_path,
                collapse_key, status, available_at
            )
            SELECT event.workspace_id,
                   NULL,
                   'staff',
                   endpoint.id,
                   'show_checklist',
                   event.id,
                   'staff',
                   CASE event.payload ->> 'checklist'
                       WHEN 'week' THEN 'VIRYA · koncert za 7 dni'
                       ELSE 'VIRYA · koncert za 2 dni'
                   END,
                   (event.payload ->> 'event_title') || ' — otwórz checklistę i odhacz przygotowania.',
                   '/staff/checklist?event=' || (event.payload ->> 'event_slug'),
                   'show-checklist:' || (event.payload ->> 'event_id') || ':' || (event.payload ->> 'checklist'),
                   'queued',
                   now()
            FROM inserted_events event
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id = event.workspace_id
             AND endpoint.audience_kind = 'staff'
             AND endpoint.active
             AND endpoint.invalidated_at IS NULL
            WHERE event.payload ->> 'checklist' IN ('week', 'two_days')
            ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
            RETURNING 1
        ), emissions AS (
            INSERT INTO show_notification_emissions (
                workspace_id, event_id, phase, outbox_event_id
            )
            SELECT event.workspace_id,
                   (event.payload ->> 'event_id')::uuid,
                   event.payload ->> 'checklist',
                   event.id
            FROM inserted_events event
            RETURNING 1
        )
        SELECT count(*)::bigint FROM emissions
        "#,
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReminderSchedulerError::Database)?;
    Ok(u64::try_from(inserted).unwrap_or(0))
}

async fn cancel_ineligible_jobs(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), ReminderSchedulerError> {
    sqlx::query(
        r#"
        UPDATE event_reminder_jobs AS jobs
        SET status = 'cancelled', cancelled_at = now()
        FROM events, fans
        WHERE jobs.status = 'pending'
            AND events.workspace_id = jobs.workspace_id
            AND events.id = jobs.event_id
            AND fans.workspace_id = jobs.workspace_id
            AND fans.id = jobs.fan_id
            AND (
                events.status <> 'published'
                OR events.starts_at <= now()
                OR fans.status <> 'active'
            )
        "#,
    )
    .execute(&mut **transaction)
    .await
    .map_err(ReminderSchedulerError::Database)?;
    Ok(())
}

#[derive(FromRow)]
struct DueReminderRow {
    id: Uuid,
    workspace_id: Uuid,
    event_id: Uuid,
    fan_id: Uuid,
    reminder_kind: String,
    due_at: OffsetDateTime,
    event_slug: String,
    event_title: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    venue: Option<String>,
    venue_address: Option<String>,
    ticket_url: Option<String>,
    normalized_email: String,
    display_name: Option<String>,
    locale: Option<String>,
}

/// Error returned when the reminder scheduler is constructed with invalid durations.
#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
#[error("event reminder scheduler durations must be non-zero")]
pub struct ReminderSchedulerBuildError;

#[derive(Debug, thiserror::Error)]
enum ReminderSchedulerError {
    #[error("event reminder scheduler database operation failed")]
    Database(#[source] sqlx::Error),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::postgres::PgPoolOptions;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::EventReminderScheduler;

    #[tokio::test]
    async fn scheduler_rejects_zero_durations_before_starting_tokio_interval()
    -> Result<(), Box<dyn std::error::Error>> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://crowdrelay:crowdrelay@localhost/crowdrelay")?;

        assert!(
            EventReminderScheduler::new(
                pool.clone(),
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(
            EventReminderScheduler::new(
                pool.clone(),
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(
            EventReminderScheduler::new(
                pool,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::ZERO,
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires CROWDRELAY_REMINDER_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn due_reminder_is_enqueued_exactly_once() -> Result<(), Box<dyn std::error::Error>> {
        let database_url = std::env::var("CROWDRELAY_REMINDER_TEST_DATABASE_URL").map_err(|e| {
            format!("CROWDRELAY_REMINDER_TEST_DATABASE_URL must target a disposable database: {e}")
        })?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

        let workspace_id = Uuid::now_v7();
        let city_id = Uuid::now_v7();
        let fan_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let reminder_id = Uuid::now_v7();
        let slug = format!("reminder-test-{}", workspace_id.simple());
        let starts_at = OffsetDateTime::now_utc() + time::Duration::hours(2);

        let mut transaction = pool.begin().await?;
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Reminder test')")
            .bind(workspace_id)
            .bind(slug)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, 'Reminder City', 'PL')",
        )
        .bind(city_id)
        .bind(format!("city-{}", city_id.simple()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
        )
        .bind(fan_id)
        .bind(workspace_id)
        .bind(format!("reminder-{}@example.test", fan_id.simple()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO events (
                id, workspace_id, city_id, slug, title, timezone, starts_at,
                status, published_at
            ) VALUES ($1, $2, $3, 'reminder-live', 'Reminder live',
                      'Europe/Warsaw', $4, 'published', now())
            "#,
        )
        .bind(event_id)
        .bind(workspace_id)
        .bind(city_id)
        .bind(starts_at)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO event_reminder_jobs (
                id, workspace_id, event_id, fan_id, reminder_kind, due_at
            ) VALUES ($1, $2, $3, $4, 'test_due', now() - interval '1 minute')
            "#,
        )
        .bind(reminder_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(fan_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        let scheduler = EventReminderScheduler::new(
            pool.clone(),
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(1),
        )?;
        let enqueued = scheduler.enqueue_due().await?;
        assert!(enqueued >= 1, "Should enqueue at least the event reminder");
        assert_eq!(scheduler.enqueue_due().await?, 0);

        let status =
            sqlx::query_scalar::<_, String>("SELECT status FROM event_reminder_jobs WHERE id = $1")
                .bind(reminder_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "enqueued");

        let outbox_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'event.reminder_due'",
        )
        .bind(workspace_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(outbox_count, 1);

        pool.close().await;
        Ok(())
    }
}
