//! Objectives against a real Postgres.
//!
//! The rule is unit-tested. What cannot be: that the baseline is frozen from
//! the series at declaration, that re-declaring returns the same target rather
//! than opening a second one, that retiring keeps the row, and that the state
//! is derived on read from whatever the series says now.

use std::time::Duration;

use crowdrelay_application::{
    IdempotencyKey,
    autopilot::{AutopilotObjectiveRepository, DeclareGrowthObjective},
};
use crowdrelay_domain::{
    WorkspaceId,
    growth_metrics::{MetricDirection, MetricPlatform},
    objectives::ObjectiveScope,
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

fn key() -> IdempotencyKey {
    IdempotencyKey::parse("objective-e2e-000000000000001").expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_target_freezes_its_baseline_declares_once_and_is_read_back_from_the_series()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("objective-e2e-{suffix}"))
        .bind("Objectives E2E")
        .execute(&pool)
        .await?;
    let series_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_growth_metric_series (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1,$2,'bandsintown','trackers','Bandsintown trackers')",
    )
    .bind(series_id)
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let now = OffsetDateTime::now_utc();
    for (day, value) in [(-10_i64, 100_i64), (-1, 130)] {
        sqlx::query(
            "INSERT INTO viryaos_growth_metric_points (workspace_id, series_id, captured_at, value, source)
             VALUES ($1,$2,$3,$4,'test')",
        )
        .bind(workspace_id.into_uuid())
        .bind(series_id)
        .bind(now + time::Duration::days(day))
        .bind(value)
        .execute(&pool)
        .await?;
    }

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let command = DeclareGrowthObjective {
        platform: MetricPlatform::Bandsintown,
        metric_key: "trackers".to_owned(),
        scope: ObjectiveScope::Workspace,
        direction: MetricDirection::HigherIsBetter,
        target_value: 300,
        deadline: now + time::Duration::days(90),
        declared_by: "band".to_owned(),
    };

    let first = repository
        .declare_growth_objective(workspace_id, command.clone(), &key(), None)
        .await?;
    assert!(!first.replayed);
    assert_eq!(
        first.baseline_value,
        Some(130),
        "the baseline is the series as it stands, not zero and not the oldest point"
    );

    let second = repository
        .declare_growth_objective(workspace_id, command, &key(), None)
        .await?;
    assert!(
        second.replayed,
        "one live target per series and scope; a second would be two answers to one question"
    );
    assert_eq!(second.objective_id, first.objective_id);

    let objectives = repository.load_growth_objectives(workspace_id, now).await?;
    let objective = objectives.first().ok_or("the target is read back")?;
    assert_eq!(objective.baseline_value, 130);
    assert_eq!(objective.observed_value, Some(130));
    // Declared moments ago, so no pace can be inferred yet.
    assert_eq!(objective.state.as_str(), "unmeasurable");

    let retired = repository
        .retire_growth_objective(workspace_id, first.objective_id, &key(), None)
        .await?;
    assert!(!retired.replayed);
    assert!(
        repository
            .load_growth_objectives(workspace_id, now)
            .await?
            .is_empty(),
        "a retired target stops counting"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM viryaos_growth_objectives WHERE workspace_id=$1"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        1,
        "and it is kept: a target that was declared and removed is what a review needs to see"
    );
    Ok(())
}
