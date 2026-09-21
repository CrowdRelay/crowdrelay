//! The approval queue's deadline must be real even where nobody claims.
//!
//! The claim path sweeps lapsed asks for its workspace every cycle, which
//! covered only tenants whose worker was alive and claiming — a parked or
//! disabled workspace accumulated rows that read `awaiting_approval` long
//! past `approval_expires_at`, hidden from `needs_you` and counted as
//! `awaiting_sweep` only by the lapsed read. The retention worker now runs
//! the same sweep globally once an hour, and decisions that produced no
//! action and no outcome age out of the audit table after 180 days.
//! These tests run the real worker's `run_once`, because a test that
//! reimplemented the query would prove nothing about the one that ships.

mod common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_worker::retention::{RetentionWorker, RetentionWorkerConfig};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("retention-{}", id.simple()))
        .bind("Retention")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

/// A decision row, as the evaluator would have written it. `confidence` is
/// the dial that decides which death an unanswered ask dies.
async fn decision(pool: &PgPool, workspace_id: Uuid, confidence: i32) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'outreach','workspace',$4,
                  'contact.attempt',$5,'require_approval','send the note',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$6)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(format!("decision-{id}"))
    .bind(workspace_id)
    .bind(confidence)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    Ok(id)
}

/// An `awaiting_approval` ask. `expires_hours` is signed: negative means the
/// window already closed, positive means the ask is still live.
async fn awaiting_approval(
    pool: &PgPool,
    workspace_id: Uuid,
    decision_id: Uuid,
    subject_kind: &str,
    subject_id: Uuid,
    expires_hours: i64,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approval_expires_at, trace_id
        ) VALUES ($1,$2,$3,'outreach','contact.attempt',$4,
                  $5,$6,$7,'awaiting_approval',
                  now() + ($8::int * interval '1 hour'), $9)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(format!("action-{id}"))
    .bind(serde_json::json!({"kind":"contact.attempt"}))
    .bind(expires_hours)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert awaiting_approval action")?;
    Ok(id)
}

fn worker(pool: &PgPool) -> Result<RetentionWorker> {
    RetentionWorker::new(
        pool.clone(),
        RetentionWorkerConfig {
            poll_interval: Duration::from_secs(3600),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
            terminal_outbox_retention: Duration::from_secs(30 * 24 * 3600),
            consumed_token_retention: Duration::from_secs(30 * 24 * 3600),
            terminal_push_retention: Duration::from_secs(30 * 24 * 3600),
            decision_audit_retention: Duration::from_secs(180 * 24 * 3600),
            batch_size: 500,
        },
    )
    .context("build retention worker")
}

async fn action_state(pool: &PgPool, id: Uuid) -> Result<(String, Option<String>)> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, last_error_kind FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .context("read action state")?;
    Ok(row)
}

/// The whole point: an ask past its window dies on the retention pass alone,
/// with no claim running anywhere. The parked-tenant case — nothing about
/// this workspace will ever invoke the claim path.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_expired_approval_dies_without_a_claim() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let decision_id = decision(&db, workspace_id, 9000).await?;
        let action_id = awaiting_approval(
            &db,
            workspace_id,
            decision_id,
            "workspace",
            workspace_id,
            -1,
        )
        .await?;

        let stats = worker(&db)?.run_once().await?;
        ensure!(stats.lapsed_autopilot_asks_swept >= 1);

        let (status, error_kind) = action_state(&db, action_id).await?;
        ensure!(status == "cancelled", "status: {status}");
        ensure!(
            error_kind.as_deref() == Some("approval_expired"),
            "error_kind: {error_kind:?}"
        );
        Ok(())
    }
    .await
}

/// One global pass covers every workspace — the claim path only ever
/// reached the workspace it was claiming for.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_sweep_reaches_every_workspace() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let mut action_ids = Vec::new();
        for _ in 0..2 {
            let workspace_id = workspace(&db).await?;
            let decision_id = decision(&db, workspace_id, 9000).await?;
            action_ids.push(
                awaiting_approval(
                    &db,
                    workspace_id,
                    decision_id,
                    "workspace",
                    workspace_id,
                    -2,
                )
                .await?,
            );
        }

        let stats = worker(&db)?.run_once().await?;
        ensure!(stats.lapsed_autopilot_asks_swept >= 2);
        for id in action_ids {
            let (status, _) = action_state(&db, id).await?;
            ensure!(status == "cancelled", "status: {status}");
        }
        Ok(())
    }
    .await
}

/// A live ask inside its window is untouched — the sweep only kills what is
/// already dead.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_fresh_approval_survives_the_sweep() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let decision_id = decision(&db, workspace_id, 9000).await?;
        let action_id = awaiting_approval(
            &db,
            workspace_id,
            decision_id,
            "workspace",
            workspace_id,
            24,
        )
        .await?;

        worker(&db)?.run_once().await?;

        let (status, error_kind) = action_state(&db, action_id).await?;
        ensure!(status == "awaiting_approval", "status: {status}");
        ensure!(error_kind.is_none(), "error_kind: {error_kind:?}");
        Ok(())
    }
    .await
}

/// An ask whose decision carried zero confidence is withdrawn, not expired —
/// the operator never had a real proposal to answer.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_zero_confidence_ask_is_withdrawn_not_expired() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let decision_id = decision(&db, workspace_id, 0).await?;
        // Still inside its window: the withdrawal is about evidence, not time.
        let action_id = awaiting_approval(
            &db,
            workspace_id,
            decision_id,
            "workspace",
            workspace_id,
            24,
        )
        .await?;

        worker(&db)?.run_once().await?;

        let (status, error_kind) = action_state(&db, action_id).await?;
        ensure!(status == "cancelled", "status: {status}");
        ensure!(
            error_kind.as_deref() == Some("insufficient_evidence"),
            "error_kind: {error_kind:?}"
        );
        Ok(())
    }
    .await
}

/// A suggestion whose ask died must resolve with it, in the same pass —
/// otherwise it holds the open queue's slot forever.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dead_ask_expires_its_suggestion() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let suggestion_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO viryaos_content_suggestions (
                id, workspace_id, concept, status
            ) VALUES ($1, $2, 'a beat the brain proposed', 'raised')
            "#,
        )
        .bind(suggestion_id)
        .bind(workspace_id)
        .execute(&db)
        .await
        .context("insert suggestion")?;
        let decision_id = decision(&db, workspace_id, 9000).await?;
        awaiting_approval(
            &db,
            workspace_id,
            decision_id,
            "content_suggestion",
            suggestion_id,
            -1,
        )
        .await?;

        worker(&db)?.run_once().await?;

        let status: String =
            sqlx::query_scalar("SELECT status FROM viryaos_content_suggestions WHERE id = $1")
                .bind(suggestion_id)
                .fetch_one(&db)
                .await?;
        ensure!(status == "expired", "suggestion status: {status}");
        let outcomes: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM viryaos_suggestion_outcomes
             WHERE suggestion_id = $1 AND outcome = 'expired'",
        )
        .bind(suggestion_id)
        .fetch_one(&db)
        .await?;
        ensure!(outcomes == 1, "outcome rows: {outcomes}");
        Ok(())
    }
    .await
}

/// A second pass must be a no-op: the transitions are terminal and the
/// outcome insert is single-fire on the suggestion's own status flip.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_second_pass_changes_nothing() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let suggestion_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO viryaos_content_suggestions (id, workspace_id, concept, status)
             VALUES ($1, $2, 'a beat the brain proposed', 'raised')",
        )
        .bind(suggestion_id)
        .bind(workspace_id)
        .execute(&db)
        .await
        .context("insert suggestion")?;
        let decision_id = decision(&db, workspace_id, 9000).await?;
        awaiting_approval(
            &db,
            workspace_id,
            decision_id,
            "content_suggestion",
            suggestion_id,
            -1,
        )
        .await?;

        let worker = worker(&db)?;
        worker.run_once().await?;
        let stats = worker.run_once().await?;
        ensure!(stats.lapsed_autopilot_asks_swept == 0, "stats: {stats:?}");
        let outcomes: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM viryaos_suggestion_outcomes WHERE suggestion_id = $1",
        )
        .bind(suggestion_id)
        .fetch_one(&db)
        .await?;
        ensure!(outcomes == 1, "outcome rows: {outcomes}");
        Ok(())
    }
    .await
}

/// Decisions that produced nothing age out; a decision that became an action
/// is audit and stays forever.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn orphan_decisions_age_out_but_audit_rows_stay() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        // Old, produced nothing: eligible.
        let orphan = decision(&db, workspace_id, 9000).await?;
        sqlx::query(
            "UPDATE viryaos_autopilot_decisions SET evaluated_at = now() - interval '181 days'
             WHERE id = $1",
        )
        .bind(orphan)
        .execute(&db)
        .await?;
        // Old, produced an action: audit, never eligible.
        let audit = decision(&db, workspace_id, 9000).await?;
        sqlx::query(
            "UPDATE viryaos_autopilot_decisions SET evaluated_at = now() - interval '181 days'
             WHERE id = $1",
        )
        .bind(audit)
        .execute(&db)
        .await?;
        awaiting_approval(&db, workspace_id, audit, "workspace", workspace_id, 24).await?;
        // Young, produced nothing: inside the window.
        let fresh = decision(&db, workspace_id, 9000).await?;

        let stats = worker(&db)?.run_once().await?;
        ensure!(
            stats.orphan_autopilot_decisions_deleted == 1,
            "stats: {stats:?}"
        );

        let mut remaining: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM viryaos_autopilot_decisions WHERE workspace_id = $1",
        )
        .bind(workspace_id)
        .fetch_all(&db)
        .await?;
        remaining.sort();
        let mut expected = vec![audit, fresh];
        expected.sort();
        ensure!(remaining == expected, "remaining: {remaining:?}");
        Ok(())
    }
    .await
}
