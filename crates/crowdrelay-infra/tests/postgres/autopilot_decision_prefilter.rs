//! The portfolio prefilter read: which of a cycle's candidate decision keys
//! already name a persisted decision.
//!
//! A candidate whose dedup key is taken can only conflict at persist — a
//! guaranteed no-op that still occupies a portfolio slot. Under a
//! health-scaled dispatch budget that slot is the whole budget, and a
//! template due on stale intelligence whose dispatch already committed this
//! window starves every other candidate for the life of the window.
//! `existing_decision_keys` is the read that keeps those keys out of the
//! pool; this test pins what it returns.

use std::collections::HashSet;
use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

async fn fixture()
-> Result<(sqlx::PgPool, PostgresAutopilotRepository, WorkspaceId), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("prefilter-{suffix}"))
        .bind("Prefilter Tests")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok((pool, repository, workspace_id))
}

async fn insert_decision(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    decision_key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,'request_agent_run',
                   9000,'auto_execute','scan due','{}','{}','{}',$5,$1)"#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(decision_key)
    .bind(Uuid::now_v7())
    .bind(OffsetDateTime::now_utc())
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn existing_keys_returns_only_the_keys_already_persisted()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repository, workspace_id) = fixture().await?;

    insert_decision(
        &pool,
        workspace_id,
        "decision:growth-intelligence:v10:reddit-scanner:6906",
    )
    .await?;

    let found: HashSet<String> = repository
        .existing_decision_keys(
            workspace_id,
            &[
                "decision:growth-intelligence:v10:reddit-scanner:6906".to_owned(),
                "decision:growth-intelligence:v10:fanbase-scout:497252".to_owned(),
                "decision:growth-intelligence:v10:strategy-consult:497252".to_owned(),
            ],
        )
        .await?;

    assert_eq!(
        found,
        HashSet::from(["decision:growth-intelligence:v10:reddit-scanner:6906".to_owned()]),
        "only the persisted key may come back — the other two must stay dispatchable"
    );

    // Keys are workspace-scoped: another workspace's identical key must not
    // mask this workspace's candidacy.
    let other = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(other.into_uuid())
        .bind(format!("prefilter-other-{}", other.into_uuid().simple()))
        .bind("Other")
        .execute(&pool)
        .await?;
    let cross: HashSet<String> = repository
        .existing_decision_keys(
            other,
            &["decision:growth-intelligence:v10:reddit-scanner:6906".to_owned()],
        )
        .await?;
    assert!(
        cross.is_empty(),
        "another workspace's decision must not count"
    );

    Ok(())
}
