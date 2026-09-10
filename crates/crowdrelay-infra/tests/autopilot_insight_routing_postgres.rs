//! What the brain hands back to the LLM workers it dispatches.
//!
//! Insights are routed by kind, not by the worker that produced them. The
//! three kinds fed into prompts — campaign insight, release plan note, generic
//! insight — are statements about the workspace, and none is a fact only one
//! worker can use, so each one goes to whichever template dispatches next.
//!
//! Routing by producer is what these tests pin against, because it failed in
//! two ways. A snapshot exists only for an *active* template, so an insight
//! from a disabled one reached no prompt; and since only insights that reach a
//! snapshot are ever marked consumed, and retention deletes consumed rows
//! only, such a row stayed `consumed_at IS NULL` forever while still taking a
//! slot in the window the loader returns. Past the limit the window held
//! nothing deliverable at all: the block the worker received went empty while
//! every log line still said insights were loaded.
//!
//! These tests run against a real database because the behavior lives in the
//! SQL, and because `agent_service_tasks` belongs to the TypeScript agent
//! service — it is not in this repository's migrations, so the tests create
//! the shape they depend on.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

/// A template the brain builds a snapshot for.
const ACTIVE_TEMPLATE: &str = "community-engager";
/// A template `WorkerTemplate::is_disabled` excludes, so no snapshot is built
/// for it. What it produced is still knowledge, so it still gets delivered.
const DISABLED_TEMPLATE: &str = "telegram-scanner";

/// The agent service owns this table, so it is absent from a migrated test
/// database. Create the columns the snapshot loader reads.
async fn create_agent_service_tasks(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_tasks (
            id           UUID PRIMARY KEY,
            workspace_id UUID NOT NULL,
            template_id  TEXT NOT NULL,
            model_id     TEXT NOT NULL DEFAULT 'auto',
            prompt       TEXT NOT NULL DEFAULT '',
            status       TEXT NOT NULL DEFAULT 'completed',
            tier         TEXT NOT NULL DEFAULT 'basic',
            metadata     JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts one completed task for `template_id` and one processed insight
/// produced by it, created at `created_at`.
async fn seed_insight(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    template_id: &str,
    headline: &str,
    created_at: OffsetDateTime,
) -> Result<Uuid, sqlx::Error> {
    let task_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_service_tasks (id, workspace_id, template_id, created_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(task_id)
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(created_at)
    .execute(pool)
    .await?;

    let outcome_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_outcomes
           (id, workspace_id, task_id, result_id, kind, payload,
            confidence_basis_points, status, idempotency_key, created_at)
         VALUES ($1, $2, $3, $4, 'campaign_insight', $5, 8000, 'processed', $6, $7)",
    )
    .bind(outcome_id)
    .bind(workspace_id.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(serde_json::json!({
        "item": { "headline": headline, "detail": "detail", "recommended_action": "act" }
    }))
    .bind(outcome_id.simple().to_string())
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(outcome_id)
}

async fn workspace(pool: &sqlx::PgPool, slug: &str) -> Result<WorkspaceId, sqlx::Error> {
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{slug}-{suffix}"))
        .bind("Insight routing")
        .execute(pool)
        .await?;
    Ok(workspace_id)
}

async fn connect() -> Result<(sqlx::PgPool, DatabaseConfig), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    create_agent_service_tasks(&pool).await?;
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    Ok((pool, database))
}

fn headlines(
    snapshots: &[crowdrelay_brain::GrowthIntelligenceSnapshot],
    template_id: &str,
) -> Vec<String> {
    snapshots
        .iter()
        .filter(|snapshot| snapshot.template_id == template_id)
        .flat_map(|snapshot| snapshot.recent_insights.iter())
        .map(|insight| insight.headline.clone())
        .collect()
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_insight_reaches_every_template_not_only_its_producer()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "insight-reaches").await?;
    let now = OffsetDateTime::now_utc();
    seed_insight(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        "r/metal responds to tour posts",
        now - time::Duration::hours(1),
    )
    .await?;

    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;

    assert!(
        !snapshots.is_empty(),
        "the brain builds a snapshot per active template"
    );
    for snapshot in &snapshots {
        assert_eq!(
            headlines(&snapshots, &snapshot.template_id),
            vec!["r/metal responds to tour posts".to_owned()],
            "what one worker noticed about a community is worth as much to \
             every other worker, so {} should carry it too",
            snapshot.template_id
        );
    }
    Ok(())
}

/// The failure routing-by-producer had, stated as the behavior that replaced
/// it. A disabled template gets no snapshot of its own, so under the old rule
/// its insights reached no prompt at all and — never having reached a snapshot
/// — were never marked consumed either, which is what made them accumulate.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_insight_from_a_disabled_template_is_still_delivered()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "insight-disabled").await?;
    let now = OffsetDateTime::now_utc();
    seed_insight(
        &pool,
        workspace_id,
        DISABLED_TEMPLATE,
        "Polish metal channels cross-promote on Thursdays",
        now - time::Duration::hours(1),
    )
    .await?;

    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;

    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot.template_id != DISABLED_TEMPLATE),
        "a disabled template still gets no snapshot — that part is unchanged"
    );
    assert_eq!(
        headlines(&snapshots, ACTIVE_TEMPLATE),
        vec!["Polish metal channels cross-promote on Thursdays".to_owned()],
        "and its insight is delivered anyway, because the knowledge does not \
         stop being true when the worker that found it is switched off"
    );
    Ok(())
}

/// The prompt budget. Every template now receives every insight, so the cap
/// is what keeps a task brief from opening with a hundred prior findings.
/// Nothing is lost: the newest unconsumed are returned first, and consumption
/// advances the window.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_prompt_carries_the_newest_insights_up_to_the_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "insight-budget").await?;
    let now = OffsetDateTime::now_utc();
    // Twenty insights, newest last by headline number.
    for n in 0..20 {
        seed_insight(
            &pool,
            workspace_id,
            ACTIVE_TEMPLATE,
            &format!("insight {n}"),
            now - time::Duration::hours(20 - i64::from(n)),
        )
        .await?;
    }

    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;

    let delivered = headlines(&snapshots, ACTIVE_TEMPLATE);
    assert_eq!(
        delivered.len(),
        8,
        "the prompt budget caps what rides along in one dispatch"
    );
    assert_eq!(
        delivered.first().map(String::as_str),
        Some("insight 19"),
        "and it is the newest that ride, not an arbitrary eight"
    );
    assert!(
        !delivered.iter().any(|headline| headline == "insight 0"),
        "the oldest waits for the next window rather than crowding this one"
    );
    Ok(())
}
