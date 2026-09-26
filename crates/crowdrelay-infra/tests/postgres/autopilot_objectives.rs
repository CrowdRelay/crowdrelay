//! Objectives against a real Postgres.
//!
//! The rule is unit-tested. What cannot be: that the baseline is frozen from
//! the series at declaration, that re-declaring returns the same target rather
//! than opening a second one, that retiring keeps the row, and that the state
//! is derived on read from whatever the series says now.

use std::time::Duration;

use crate::common;
use crowdrelay_application::{
    IdempotencyKey,
    autopilot::{
        AutopilotDecisionRepository, AutopilotObjectiveRepository, DeclareGrowthObjective,
    },
};
use crowdrelay_domain::{
    WorkspaceId,
    growth_metrics::{MetricDirection, MetricPlatform},
    objectives::ObjectiveScope,
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

fn key() -> IdempotencyKey {
    IdempotencyKey::parse("objective-e2e-000000000000001").expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_target_freezes_its_baseline_declares_once_and_is_read_back_from_the_series()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
        "INSERT INTO growth_metric_series (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1,$2,'bandsintown','trackers','Bandsintown trackers')",
    )
    .bind(series_id)
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let now = OffsetDateTime::now_utc();
    for (day, value) in [(-10_i64, 100_i64), (-1, 130)] {
        sqlx::query(
            "INSERT INTO growth_metric_points (workspace_id, series_id, captured_at, value, source)
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
            "SELECT count(*) FROM growth_objectives WHERE workspace_id=$1"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        1,
        "and it is kept: a target that was declared and removed is what a review needs to see"
    );
    Ok(())
}

/// The brain sees the objective the operator sees.
///
/// Declared ten days ago at 100 with a target of 300 in thirty days, the series
/// read 130 yesterday: 30 in nine days projects to 200, so the objective is
/// `Behind` by 170. The snapshot loader must carry exactly that verdict on
/// `WorldModel.objective` — the same read the objectives endpoint serves, not
/// a second assessment — or the goal cannot change a decision.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_behind_objective_reaches_the_world_model() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("objective-brain-{suffix}"))
        .bind("Objective reaches the brain")
        .execute(&pool)
        .await?;
    let series_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO growth_metric_series (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1,$2,'bandsintown','trackers','Bandsintown trackers')",
    )
    .bind(series_id)
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO growth_metric_points (workspace_id, series_id, captured_at, value, source)
         VALUES ($1,$2,$3,130,'test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(series_id)
    .bind(now - time::Duration::days(1))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    // Nothing declared: the brain has no goal, not a zero one.
    let before = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;
    assert!(
        before
            .iter()
            .all(|snapshot| snapshot.world_model.objective.is_none())
    );

    let declared = repository
        .declare_growth_objective(
            workspace_id,
            DeclareGrowthObjective {
                platform: MetricPlatform::Bandsintown,
                metric_key: "trackers".to_owned(),
                scope: ObjectiveScope::Workspace,
                direction: MetricDirection::HigherIsBetter,
                target_value: 300,
                deadline: now + time::Duration::days(20),
                declared_by: "band".to_owned(),
            },
            &key(),
            None,
        )
        .await?;
    // Put the declaration ten days back at a baseline of 100, so a pace exists.
    sqlx::query(
        "UPDATE growth_objectives SET declared_at = $2, baseline_value = 100
         WHERE id = $1",
    )
    .bind(declared.objective_id)
    .bind(now - time::Duration::days(10))
    .execute(&pool)
    .await?;

    let operator = repository.load_growth_objectives(workspace_id, now).await?;
    let operator = operator.first().ok_or("the objective is read back")?;
    assert_eq!(operator.state.as_str(), "behind");

    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;
    let world = &snapshots
        .first()
        .ok_or("at least one snapshot")?
        .world_model;
    let objective = world
        .objective
        .as_ref()
        .ok_or("the world model carries the declared objective")?;
    assert_eq!(objective.objective_id, declared.objective_id);
    assert_eq!(
        objective.state, operator.state,
        "one verdict, two readers — the brain and the operator must agree"
    );
    assert!(objective.is_behind());
    assert_eq!(objective.observed_value, Some(130));
    Ok(())
}

/// A `social` series measures the size of somebody else's subreddit — reach
/// we can address, never audience we own. Even with data present, declaring
/// an objective on it must be refused: otherwise the plan would optimize
/// for growing a forum, which is precisely the victory condition the
/// product does not have.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_community_size_cannot_be_declared_as_an_objective()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
        "INSERT INTO growth_metric_series (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1,$2,'social','members','r/Metal members')",
    )
    .bind(series_id)
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO growth_metric_points (workspace_id, series_id, captured_at, value, source)
         VALUES ($1,$2,$3,$4,'test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(series_id)
    .bind(OffsetDateTime::now_utc())
    .bind(4_000_000_i64)
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let refused = repository
        .declare_growth_objective(
            workspace_id,
            DeclareGrowthObjective {
                platform: MetricPlatform::Social,
                metric_key: "members".to_owned(),
                scope: ObjectiveScope::Workspace,
                direction: MetricDirection::HigherIsBetter,
                target_value: 5_000_000,
                deadline: OffsetDateTime::now_utc() + time::Duration::days(90),
                declared_by: "band".to_owned(),
            },
            &key(),
            None,
        )
        .await;

    assert!(
        refused.is_err(),
        "growing a forum is a source-health signal, never a goal — the objective must be refused"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM growth_objectives WHERE workspace_id=$1"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        0,
        "and nothing is stored"
    );
    Ok(())
}
