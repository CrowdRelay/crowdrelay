//! Live-database proof that the watchdog survives a CrowdRelay-only schema.
//!
//! `agent_service_credentials` belongs to the agents service — no migration
//! here creates it. The snapshot's Reddit session readings are guarded by a
//! `to_regclass` probe: without the table they must read "no usable session"
//! rather than abort the whole cycle (and every condition with it). With the
//! table present, a dead credential beside queued drafts must raise
//! `publishing.session_dead`.

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::auto_post_platforms::{AutoPostPlatforms, PublishingPosture, RedditPosture};
use crowdrelay_worker::ops_watchdog::OpsWatchdogWorker;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

struct DisposableDatabase {
    admin_url: String,
    name: String,
    pool: PgPool,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let (prefix, _) = base_url
            .rsplit_once('/')
            .context("database URL has no database name")?;
        let admin_url = format!("{prefix}/postgres");
        let name = format!("crowdrelay_watchdog_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .context("connect to the maintenance database")?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .context("create the disposable database")?;
        drop(admin);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{prefix}/{name}"))
            .await
            .context("connect to the disposable database")?;
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .context("apply migrations")?;
        Ok(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop_database(self) {
        self.pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {} (FORCE)", self.name))
                .execute(&mut admin)
                .await;
        }
    }
}

fn watchdog(pool: PgPool, workspace_id: WorkspaceId) -> OpsWatchdogWorker {
    OpsWatchdogWorker::new(
        pool,
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
        PublishingPosture {
            platforms: AutoPostPlatforms {
                telegram: true,
                discord: true,
                social: true,
            },
            reddit: RedditPosture::Publishes,
        },
    )
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("watchdog-{}", id.simple()))
        .bind("Watchdog")
        .execute(pool)
        .await
        .context("insert workspace")?;
    // The snapshot selects from `viryaos_executor_instances` — one live
    // executor row, the shape a healthy workspace carries.
    sqlx::query(
        r#"INSERT INTO viryaos_executor_instances
               (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
           VALUES ($1,'worker-1','1.0.0','abc123',now(),now() + interval '1 hour')"#,
    )
    .bind(id)
    .execute(pool)
    .await
    .context("insert executor instance")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A queued community draft — the posting demand `session_dead` reads.
async fn queued_draft(pool: &PgPool, workspace_id: WorkspaceId) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                  'community.engage',9000,'auto_execute','post to a community',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("engage-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                  'workspace',$4,$5,'{}'::jsonb,'succeeded',now(),$6)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert action")?;
    sqlx::query(
        r#"
        INSERT INTO community_posts
            (workspace_id, action_id, subreddit, title, body, status)
        VALUES ($1,$2,'r/test','title','body','pending')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert queued draft")?;
    Ok(())
}

/// The agents-service credential table, the columns the snapshot reads —
/// same convention as `agent_run_assignment_postgres.rs`.
async fn create_credentials_table(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_credentials (
            id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
            workspace_id uuid NOT NULL,
            provider text NOT NULL,
            status text NOT NULL DEFAULT 'active',
            last_validated_at timestamptz,
            last_validation_error text,
            created_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create foreign credentials table")?;
    Ok(())
}

async fn active_alerts(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT alert_key FROM viryaos_ops_alert_state \
         WHERE workspace_id = $1 AND active ORDER BY alert_key",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .context("read alert state")
}

/// A deployment without the agents schema must still run the cycle: the
/// snapshot's Reddit readings default to "no usable session" and every other
/// condition still evaluates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_missing_credentials_table_does_not_blind_the_watchdog() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        queued_draft(&db.pool, ws).await?;
        // No create_credentials_table call — the relation does not exist.
        let transitions = watchdog(db.pool.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db.pool, ws).await?;
        assert!(
            alerts.contains(&"publishing.session_dead".to_owned()),
            "a queued draft with no credential service at all is a dead \
             session — got {alerts:?}"
        );
        assert!(transitions > 0, "the cycle ran and recorded the alert");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// With the table present, an `invalid` credential beside queued drafts
/// fires the same alert — and an `active` one silences it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dead_credential_raises_session_dead_and_a_live_one_clears_it() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        queued_draft(&db.pool, ws).await?;
        create_credentials_table(&db.pool).await?;
        sqlx::query(
            "INSERT INTO agent_service_credentials (workspace_id, provider, status, last_validation_error) \
             VALUES ($1,'reddit-browser','invalid','login rejected')",
        )
        .bind(ws.into_uuid())
        .execute(&db.pool)
        .await
        .context("insert invalid credential")?;

        watchdog(db.pool.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db.pool, ws).await?;
        assert!(
            alerts.contains(&"publishing.session_dead".to_owned()),
            "invalid credential + queued draft must fire: {alerts:?}"
        );

        sqlx::query(
            "UPDATE agent_service_credentials SET status='active', last_validated_at=now() \
             WHERE workspace_id=$1 AND provider='reddit-browser'",
        )
        .bind(ws.into_uuid())
        .execute(&db.pool)
        .await
        .context("revive credential")?;
        watchdog(db.pool.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db.pool, ws).await?;
        assert!(
            !alerts.contains(&"publishing.session_dead".to_owned()),
            "an active credential means the queue can be worked: {alerts:?}"
        );
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// An approval cancelled unanswered must reach the operator, against a real
/// schema.
///
/// The unit tests prove the condition's predicate. This proves the reading
/// behind it: three subqueries over `viryaos_autopilot_actions`, one of them an
/// `EXTRACT(EPOCH …)` that returns `numeric` on PostgreSQL 14+ and has to be cast
/// before sqlx can decode it. An uncast one compiles, lints and passes every unit
/// test, then aborts the whole snapshot — and with it all eighteen conditions —
/// on first contact with a server.
///
/// Also pins the distinction the alarm turns on: an approval merely waiting is
/// the system working, and only one already discarded is the finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_approval_cancelled_unanswered_reaches_the_operator() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;

        // An approval still outstanding, well inside its window.
        approval_action(&db.pool, ws, "awaiting_approval", Some(48), None).await?;
        watchdog(db.pool.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db.pool, ws).await?;
        assert!(
            !alerts.contains(&"approval.expired_unanswered".to_owned()),
            "work in the queue is the system working, not a finding: {alerts:?}"
        );

        // One the sweep cancelled because nobody answered it.
        approval_action(
            &db.pool,
            ws,
            "cancelled",
            Some(-1),
            Some("approval_expired"),
        )
        .await?;
        watchdog(db.pool.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db.pool, ws).await?;
        assert!(
            alerts.contains(&"approval.expired_unanswered".to_owned()),
            "an approval discarded unanswered must be reported: {alerts:?}"
        );
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// One approval action, with its decision. `expires_in_hours` may be negative to
/// place the deadline in the past; `last_error_kind` is what separates an expiry
/// from an operator's own rejection.
async fn approval_action(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    status: &str,
    expires_in_hours: Option<i32>,
    last_error_kind: Option<&str>,
) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','workspace',$4,
                  'apply_live_opportunity',7700,'require_approval','a festival',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("live-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approval_expires_at, last_error_kind, finished_at, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','apply_live_opportunity',
                  'workspace',$4,$5,'{}'::jsonb,$6,
                  CASE WHEN $7::int IS NULL THEN NULL
                       ELSE now() + make_interval(hours => $7::int) END,
                  $8,
                  CASE WHEN $6 = 'cancelled' THEN now() ELSE NULL END,
                  $9)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(status)
    .bind(expires_in_hours)
    .bind(last_error_kind)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert approval action")?;
    Ok(())
}
