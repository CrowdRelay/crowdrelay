//! Regression test for `recover_stale_posting`'s assignment transition.
//!
//! The step-4 UPDATE named `experiment_assignments.action_id` — a relation
//! that does not exist — so every recovery threw at runtime after steps 1-3
//! had already committed, and on the next tick the empty stale scan made the
//! function early-return. The assignment stayed `dispatched` forever and the
//! existing integrity test never saw it, because it replayed the UPDATEs by
//! hand instead of calling the function. This test calls the function.

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::community_executor::CommunityExecutorWorker;
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
        let name = format!("crowdrelay_recovery_{}", Uuid::now_v7().simple());
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

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("recovery-{}", id.simple()))
        .bind("Recovery")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A stale `posting` row: a succeeded engage action, its dispatched
/// assignment, and a community_post whose `updated_at` is past the stale
/// threshold — the shape a crash between "post sent" and "row updated"
/// leaves behind.
async fn stale_posting_row(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
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
    .context("insert engage decision")?;
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
    .context("insert engage action")?;
    let experiment_uuid = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
           (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
            unit_kind, holdout_probability, interference_policy)
           VALUES ($1,$2,'community.engage','cycle-1','target_community',0.0,'none')"#,
    )
    .bind(experiment_uuid)
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await
    .context("insert experiment design")?;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','community-engager',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','dispatched',$5)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(experiment_uuid)
    .bind(Uuid::now_v7())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert dispatched assignment")?;
    let post_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO community_posts
            (id, workspace_id, action_id, subreddit, title, body, status, updated_at)
        VALUES ($1,$2,$3,'r/test','title','body','posting', now() - interval '10 minutes')
        "#,
    )
    .bind(post_id)
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert stale posting row")?;
    Ok(action_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn stale_posting_recovery_reaches_the_assignment() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        let action_id = stale_posting_row(&db.pool, ws).await?;
        let worker = CommunityExecutorWorker::new(
            db.pool.clone(),
            ws,
            Duration::from_secs(30),
            true,
            None,
            "http://agents.invalid".to_owned(),
            None,
        )
        .context("build executor")?;
        worker.recover_stale_posting().await?;

        let post_status: String =
            sqlx::query_scalar("SELECT status FROM community_posts WHERE action_id = $1")
                .bind(action_id)
                .fetch_one(&db.pool)
                .await
                .context("read post status")?;
        assert_eq!(post_status, "failed", "stale post must be marked failed");

        let action_status: String =
            sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
                .bind(action_id)
                .fetch_one(&db.pool)
                .await
                .context("read action status")?;
        assert_eq!(action_status, "unknown", "action must be unknown");

        // The assertion that matters: the UPDATE that used to throw on a
        // nonexistent `experiment_assignments` qualifier now lands.
        let assignment_status: String = sqlx::query_scalar(
            "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
        )
        .bind(action_id)
        .fetch_one(&db.pool)
        .await
        .context("read assignment status")?;
        assert_eq!(
            assignment_status, "unknown",
            "assignment must resolve to unknown — this UPDATE used to throw"
        );

        // And the recovered assignment still carries its trace id, taken
        // from the action row — the COALESCE arm that was silently broken.
        let traced: bool = sqlx::query_scalar(
            "SELECT trace_id IS NOT NULL FROM viryaos_experiment_assignments WHERE action_id = $1",
        )
        .bind(action_id)
        .fetch_one(&db.pool)
        .await
        .context("read assignment trace")?;
        assert!(traced, "recovery must backfill trace_id from the action");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}
