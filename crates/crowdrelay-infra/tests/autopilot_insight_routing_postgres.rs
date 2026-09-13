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

/// A task + optional outcome row, with control over the fields the
/// effective-run predicate reads: the outcome's status, its
/// rejection_reason, and whether the payload carries an `item`.
#[allow(clippy::too_many_arguments)]
async fn seed_run(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    template_id: &str,
    created_at: OffsetDateTime,
    outcome_status: Option<&str>,
    rejection_reason: Option<&str>,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
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

    if let Some(status) = outcome_status {
        let outcome_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO agent_outcomes
               (id, workspace_id, task_id, result_id, kind, payload,
                confidence_basis_points, status, rejection_reason,
                idempotency_key, created_at)
             VALUES ($1, $2, $3, $4, 'generic_insight', $5, 8000, $6, $7, $8, $9)",
        )
        .bind(outcome_id)
        .bind(workspace_id.into_uuid())
        .bind(task_id)
        .bind(Uuid::now_v7())
        .bind(payload)
        .bind(status)
        .bind(rejection_reason)
        .bind(outcome_id.simple().to_string())
        .bind(created_at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn last_runs(
    pool: &sqlx::PgPool,
    database: &DatabaseConfig,
    workspace_id: WorkspaceId,
    template_id: &str,
    now: OffsetDateTime,
) -> Result<(Option<u32>, Option<u32>), Box<dyn std::error::Error>> {
    let repository = PostgresAutopilotRepository::new(pool.clone(), database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == template_id)
        .ok_or("no snapshot for the seeded template")?;
    Ok((
        snapshot.hours_since_last_run,
        snapshot.hours_since_last_effective_run,
    ))
}

/// "Effective" used to mean "the outcome carried an item", so a scanner
/// that finished and found nothing — a real answer — was retried every
/// failed-run window forever. The run completed; it counts.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_empty_scan_counts_as_an_effective_run() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "empty-scan").await?;
    let now = OffsetDateTime::now_utc();
    seed_run(
        &pool,
        workspace_id,
        "reddit-scanner",
        now - time::Duration::hours(3),
        Some("processed"),
        None,
        serde_json::json!({ "rationale": "no relevant threads this week" }),
    )
    .await?;

    let (last_run, last_effective) =
        last_runs(&pool, &database, workspace_id, "reddit-scanner", now).await?;
    assert_eq!(last_run, Some(3), "the task ran three hours ago");
    assert_eq!(
        last_effective,
        Some(3),
        "a completed scan that found nothing is still an answer"
    );
    Ok(())
}

/// The verifier never ran — nothing about the world was learned, so the
/// run is retried on the short cadence instead of resetting the cooldown.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_verifier_outage_does_not_count_as_effective() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "verifier-outage").await?;
    let now = OffsetDateTime::now_utc();
    seed_run(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        now - time::Duration::hours(5),
        Some("rejected"),
        Some("NOT_GROUNDING_CHECKED: verification status is NotVerified, not GroundingCheckPassed"),
        serde_json::json!({ "item": { "headline": "draft", "detail": "d" } }),
    )
    .await?;

    let (last_run, last_effective) =
        last_runs(&pool, &database, workspace_id, ACTIVE_TEMPLATE, now).await?;
    assert_eq!(last_run, Some(5), "the failed run still paces the retry");
    assert_eq!(
        last_effective, None,
        "a dead verifier proves nothing about the world"
    );
    Ok(())
}

/// The verifier ran and refused the draft — that IS a verdict about the
/// content, so the cooldown applies. Otherwise the same refused draft
/// regenerates on every retry window.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_verifier_refusal_counts_as_effective() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "verifier-refusal").await?;
    let now = OffsetDateTime::now_utc();
    seed_run(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        now - time::Duration::hours(5),
        Some("rejected"),
        Some(
            "NOT_GROUNDING_CHECKED: verification status is GroundingCheckRejected, not GroundingCheckPassed",
        ),
        serde_json::json!({ "item": { "headline": "draft", "detail": "d" } }),
    )
    .await?;

    let (_, last_effective) =
        last_runs(&pool, &database, workspace_id, ACTIVE_TEMPLATE, now).await?;
    assert_eq!(
        last_effective,
        Some(5),
        "a verifier that ran and said no is a completed run"
    );
    Ok(())
}

/// A data source that never loaded means an absence in the output proves
/// nothing — retry on the short cadence.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_degraded_context_does_not_count_as_effective() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "degraded-context").await?;
    let now = OffsetDateTime::now_utc();
    seed_run(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        now - time::Duration::hours(2),
        Some("rejected"),
        Some(
            "DEGRADED_CONTEXT: a data source did not complete, so an absence in this output proves nothing",
        ),
        serde_json::json!({ "item": { "headline": "draft", "detail": "d" } }),
    )
    .await?;

    let (last_run, last_effective) =
        last_runs(&pool, &database, workspace_id, ACTIVE_TEMPLATE, now).await?;
    assert_eq!(last_run, Some(2));
    assert_eq!(
        last_effective, None,
        "an unloaded source makes the outcome uninformative"
    );
    Ok(())
}

/// A task that produced no outcome row at all — the agents service died
/// mid-run — is retried, not mistaken for a completed scan.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_run_without_an_outcome_is_retried() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database) = connect().await?;
    let workspace_id = workspace(&pool, "no-outcome").await?;
    let now = OffsetDateTime::now_utc();
    seed_run(
        &pool,
        workspace_id,
        ACTIVE_TEMPLATE,
        now - time::Duration::hours(4),
        None,
        None,
        serde_json::json!({}),
    )
    .await?;

    let (last_run, last_effective) =
        last_runs(&pool, &database, workspace_id, ACTIVE_TEMPLATE, now).await?;
    assert_eq!(last_run, Some(4), "the task row still paces the retry");
    assert_eq!(last_effective, None, "no outcome row means nothing learned");
    Ok(())
}
