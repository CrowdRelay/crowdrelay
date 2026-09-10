//! What the brain hands back to the LLM workers it dispatches.
//!
//! An insight produced by one worker run is fed into the next dispatch of the
//! same template — "here is what your last run already found, do not repeat
//! it". That routing is by `template_id`, and the brain only builds a snapshot
//! for a template that is currently active, so an insight attributed to
//! anything else can never be delivered.
//!
//! Undeliverable is not the whole problem. Only insights that reach a snapshot
//! are ever marked consumed, and retention deletes consumed rows only, so an
//! undeliverable insight stays `consumed_at IS NULL` permanently — and the
//! loader takes the 50 newest unconsumed rows. Enough of them and the window
//! holds nothing that can be delivered: the block the worker receives is
//! empty while every log line still says insights were loaded.
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
/// for it and nothing it produced can be routed anywhere.
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
async fn an_insight_reaches_the_template_that_produced_it() -> Result<(), Box<dyn std::error::Error>>
{
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

    assert_eq!(
        headlines(&snapshots, ACTIVE_TEMPLATE),
        vec!["r/metal responds to tour posts".to_owned()],
        "an active template's own insight has to reach its snapshot, or the \
         worker is dispatched with no memory of its last run"
    );
    Ok(())
}

/// The starvation this window is vulnerable to.
///
/// Insights from a disabled template can never be delivered and are never
/// marked consumed, so they accumulate without bound. The loader takes the 50
/// newest unconsumed rows; once more than 50 undeliverable rows are newer than
/// a deliverable one, the deliverable one falls out of the window and the
/// active template is dispatched with an empty context block.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn undeliverable_insights_do_not_crowd_out_deliverable_ones()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "insight-starvation").await?;
    let now = OffsetDateTime::now_utc();

    // The one insight that can actually be delivered, and the oldest row.
    seed_insight(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        "the deliverable one",
        now - time::Duration::days(30),
    )
    .await?;
    // Sixty newer rows from a disabled template — more than the window holds.
    for n in 0..60 {
        seed_insight(
            &pool,
            workspace_id,
            DISABLED_TEMPLATE,
            &format!("undeliverable {n}"),
            now - time::Duration::hours(i64::from(n) + 1),
        )
        .await?;
    }

    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;

    assert_eq!(
        headlines(&snapshots, ACTIVE_TEMPLATE),
        vec!["the deliverable one".to_owned()],
        "sixty undeliverable insights must not push the one deliverable \
         insight out of the window"
    );
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot.template_id != DISABLED_TEMPLATE),
        "a disabled template gets no snapshot, which is why its insights can \
         never be delivered or consumed"
    );
    Ok(())
}
