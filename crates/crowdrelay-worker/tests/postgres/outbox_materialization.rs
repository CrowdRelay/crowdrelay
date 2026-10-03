//! A failed delivery materialization must not burn the event's attempts.
//!
//! `materialize_deliveries_batch` never reads the event payload, so its
//! failure is always infrastructure — a broken index, an outage, a lease
//! race — never the event's fault. On 2026-09-22 a corrupt
//! `outbox_claim_idx` page failed every batch for ~25h; each claim released
//! with `materialization_database` while keeping the burned attempt, so
//! twelve cycles dead-lettered all 44 pending events. The release now
//! refunds the attempt, and this test drives the real worker path: claim,
//! materialize against a schema the insert cannot satisfy, release.

use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::common;
use anyhow::{Context, Result, ensure};
use crowdrelay_worker::outbox::{MapSecretProvider, OutboxWorker, OutboxWorkerConfig, SecretValue};
use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_OUTBOX_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn materialization_failure_refunds_the_attempt() -> Result<()> {
    // Its own database, for two reasons: the outbox worker claims pending
    // events database-wide, so on the shared suite database it claims other
    // tests' leftovers; and the poisoned constraint below is DDL that would
    // break every concurrent test's deliveries while it stands.
    let database = common::isolated_database("CROWDRELAY_OUTBOX_TEST_DATABASE_URL").await?;
    let outcome = run(&database.pool).await;
    database
        .drop()
        .await
        .context("drop the isolated database")?;
    outcome
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_OUTBOX_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn confirmation_without_a_subscribed_endpoint_is_dead_not_delivered() -> Result<()> {
    let database = common::isolated_database("CROWDRELAY_OUTBOX_TEST_DATABASE_URL").await?;
    let pool = database.pool.clone();
    let outcome = async {
        let workspace_id = seed_workspace(&pool).await?;
        seed_endpoint(&pool, workspace_id).await?;
        // There is a healthy webhook endpoint, but it explicitly does not
        // accept confirmation mail. This is the subtle production shape:
        // "webhooks exist" is not the same as "double opt-in can deliver".
        sqlx::query(
            "UPDATE webhook_endpoints
             SET event_types = ARRAY['fan.created']::text[]
             WHERE workspace_id = $1",
        )
        .bind(workspace_id)
        .execute(&pool)
        .await?;

        let event_id = seed_confirmation_event(&pool, workspace_id).await?;
        let worker = test_worker(&pool, event_id)?;
        let stats = worker.run_once().await.context("run unrouted confirmation")?;
        ensure!(stats.outbox_claimed == 1, "confirmation event was not claimed");
        ensure!(
            stats.deliveries_materialized == 0,
            "an unsubscribed endpoint must not receive confirmation mail"
        );

        let row = sqlx::query_as::<_, (String, Option<String>, bool, bool)>(
            "SELECT status, last_error_kind, delivered_at IS NOT NULL, dead_at IS NOT NULL
             FROM outbox_events WHERE id=$1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;
        ensure!(
            row.0 == "dead"
                && row.1.as_deref() == Some("endpoint_missing_route")
                && !row.2
                && row.3,
            "unrouted authentication mail must be terminal and visible, got {row:?}"
        );

        let deliveries: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM webhook_deliveries
             WHERE workspace_id=$1 AND outbox_event_id=$2",
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_one(&pool)
        .await?;
        ensure!(deliveries == 0, "no route means no invented delivery row");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    database
        .drop()
        .await
        .context("drop the isolated database")?;
    outcome
}

async fn run(pool: &PgPool) -> Result<()> {
    let pool = pool.clone();

    let workspace_id = seed_workspace(&pool).await?;
    seed_endpoint(&pool, workspace_id).await?;
    let event_id = seed_event(&pool, workspace_id).await?;

    // Break the delivery insert in this database only: a CHECK(false) NOT VALID
    // constraint is enforced for new writes without validating existing rows,
    // so it stands even when the cloned template already holds deliveries.
    // The release path writes a different table, so it still works.
    sqlx::query(
        "ALTER TABLE webhook_deliveries \
         ADD CONSTRAINT poisoned_for_test CHECK (false) NOT VALID",
    )
    .execute(&pool)
    .await
    .context("break delivery materialization")?;

    let worker = test_worker(&pool, event_id)?;

    // Repeated cycles keep failing materialization; with the refund the event
    // returns to pending with zero attempts burned instead of marching to dead.
    // The release schedules `available_at` on the retry backoff, so each cycle
    // after the first fast-forwards it instead of sleeping.
    for cycle in 0..2 {
        if cycle > 0 {
            sqlx::query("UPDATE outbox_events SET available_at = now() WHERE id = $1")
                .bind(event_id)
                .execute(&pool)
                .await
                .context("fast-forward retry availability")?;
        }
        let stats = worker.run_once().await.context("run outbox cycle")?;
        ensure!(
            stats.outbox_claimed == 1,
            "cycle {cycle} must claim the event"
        );
        ensure!(
            stats.deliveries_materialized == 0,
            "cycle {cycle} must not materialize against the poisoned table"
        );
        let (status, attempts, error_kind, dead_at): (String, i32, Option<String>, bool) =
            sqlx::query_as(
                "SELECT status, attempts, last_error_kind, dead_at IS NOT NULL \
                 FROM outbox_events WHERE id = $1",
            )
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .context("read event after failed cycle")?;
        ensure!(
            status == "pending",
            "cycle {cycle}: event is {status}, not pending"
        );
        ensure!(
            attempts == 0,
            "cycle {cycle}: attempt was not refunded ({attempts})"
        );
        ensure!(
            error_kind.as_deref() == Some("materialization_database"),
            "cycle {cycle}: unexpected error kind {error_kind:?}"
        );
        ensure!(
            !dead_at,
            "cycle {cycle}: event died on an infrastructure error"
        );
    }

    // Heal the schema and prove the event still delivers — nothing was lost.
    // The delivery itself will fail transport (example.invalid), but the
    // event's job ends at materialization, which is what the assertion covers.
    sqlx::query("ALTER TABLE webhook_deliveries DROP CONSTRAINT poisoned_for_test")
        .execute(&pool)
        .await
        .context("heal delivery materialization")?;
    sqlx::query("UPDATE outbox_events SET available_at = now() WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await
        .context("fast-forward retry availability")?;
    let stats = worker.run_once().await.context("run healed cycle")?;
    ensure!(
        stats.deliveries_materialized == 1,
        "healed event materializes"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outbox_events WHERE id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await?;
    ensure!(status == "delivered", "healed event did not complete");

    sqlx::query("DELETE FROM outbox_events WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await?;
    Ok(())
}

async fn seed_workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("outbox-mat-{}", id.simple()))
        .bind("Outbox Materialization")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

async fn seed_endpoint(pool: &PgPool, workspace_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO webhook_endpoints (id, workspace_id, name, url, signing_secret_ref, timeout_ms, max_attempts, active) \
         VALUES ($1, $2, 'mat-test', 'https://example.invalid/hook', 'test/mat', 3000, 3, true)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .execute(pool)
    .await
    .context("insert endpoint")?;
    Ok(())
}

async fn seed_event(pool: &PgPool, workspace_id: Uuid) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, event_version, payload, request_id, available_at) \
         VALUES ($1, $2, 'fan.created', 1, '{}'::jsonb, $3, TIMESTAMPTZ '1970-01-01 00:00:00+00')",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(format!("request-mat-{id}"))
    .execute(pool)
    .await
    .context("insert outbox event")?;
    Ok(id)
}

async fn seed_confirmation_event(pool: &PgPool, workspace_id: Uuid) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events
             (id, workspace_id, event_type, event_version, payload, request_id, available_at)
         VALUES ($1, $2, 'fan.confirmation_requested', 1, '{}'::jsonb, $3,
                 TIMESTAMPTZ '1970-01-01 00:00:00+00')",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(format!("request-confirmation-{id}"))
    .execute(pool)
    .await
    .context("insert confirmation event")?;
    Ok(id)
}

fn test_worker(pool: &PgPool, event_id: Uuid) -> Result<OutboxWorker> {
    let secret = SecretValue::new(b"materialization-test-secret-32bytes!".to_vec())
        .context("construct test secret")?;
    let provider = MapSecretProvider::new(HashMap::from([("test/mat".to_owned(), secret)]));
    OutboxWorker::new(
        pool.clone(),
        Arc::new(provider),
        OutboxWorkerConfig {
            worker_id: format!("mat-test-{}", event_id.simple()),
            outbox_batch_size: 4,
            max_concurrent_deliveries: 1,
            database_operation_timeout: Duration::from_secs(3),
            secret_resolution_timeout: Duration::from_secs(1),
            http_connect_timeout: Duration::from_secs(1),
            lease_duration: Duration::from_secs(70),
            allow_http_endpoints: true,
            ..OutboxWorkerConfig::default()
        },
    )
    .context("build outbox worker")
}
