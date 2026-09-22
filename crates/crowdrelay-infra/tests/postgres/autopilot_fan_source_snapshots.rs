//! The fan-source snapshot is the operator's "where did our fans come from"
//! ledger. These tests pin the failure mode the feature exists to fix: the
//! attribution breakdowns reaching the table, not just a row existing — a
//! write that stored an empty `by_template` would be the old log line with
//! extra steps. Plus the two invariants the surface depends on: one row per
//! workspace per hour, and one workspace's ledger never leaking into
//! another's.

use crate::common;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use crowdrelay_infra::{autopilot::record_fan_source_snapshot, config::DatabaseConfig};
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("fan-source-{suffix}"))
        .bind("Fan Source Snapshot Tests")
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
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

/// One resolved evidence row, the way a dispatch that measured fans leaves it.
/// `creative_family` is what `by_template` groups on; `strategy` feeds
/// `by_strategy`.
#[allow(clippy::too_many_arguments)]
async fn insert_resolved_evidence(
    f: &Fixture,
    creative_family: &str,
    strategy: &str,
    observed_fans: f64,
    incremental_fans: f64,
    durable_fans: f64,
) {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("key-{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("decision");
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                   $4,$5,'{}'::jsonb,'succeeded','third_party',$6)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(f.now - time::Duration::days(31))
    .execute(&f.pool)
    .await
    .expect("action");
    sqlx::query(
        r#"INSERT INTO growth_evidence
           (workspace_id, action_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality,
            creative_family, strategy,
            observed_fans, observed_incremental_fans, durable_fans_30d, resolved_at)
           VALUES ($1,$2,$3,'recipient','reddit_post',100,'treatment',0.9,false,
                   2.0,1.0,'{}'::jsonb,'observational',
                   $4,$5,$6,$7,$8,$9)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(f.now - time::Duration::days(31))
    .bind(creative_family)
    .bind(strategy)
    .bind(observed_fans)
    .bind(incremental_fans)
    .bind(durable_fans)
    .bind(f.now - time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("evidence");
}

/// The exact chain the worker's snapshot phase runs: the evidence port and
/// the North Star series into the brain's pure functions into the guarded
/// insert. CUSUM gets the series oldest-first — `daily_north_star` returns
/// newest-first, which inverts every detected direction.
async fn write_snapshot(f: &Fixture, captured_at: OffsetDateTime) -> bool {
    let evidence = f
        .repository
        .load_growth_evidence(f.workspace_id, None)
        .await
        .expect("evidence load");
    let attribution = crowdrelay_brain::attribution::attribute_fan_growth(&evidence);
    let mut days = crowdrelay_infra::autopilot::daily_north_star(
        &f.pool,
        f.workspace_id,
        crowdrelay_infra::autopilot::NORTH_STAR_WINDOW_DAYS,
    )
    .await
    .expect("north star series");
    days.reverse();
    let series: Vec<f64> = days.iter().map(|day| day.value).collect();
    let points = crowdrelay_brain::change_point::detect_fan_growth_shifts(&series, 10.0, 2.0);
    let shifts = crowdrelay_infra::autopilot::resolve_shifts(&days, &points);
    record_fan_source_snapshot(&f.pool, f.workspace_id, &attribution, &shifts, captured_at)
        .await
        .expect("snapshot write")
}

#[tokio::test]
#[ignore]
async fn snapshot_stores_full_attribution_including_ineffective_templates() {
    let f = setup().await.expect("fixture");
    // One template that worked, one that ran and produced nothing — the
    // ineffective row is the part a winners-only view would drop.
    insert_resolved_evidence(&f, "story", "community_seeding", 5.0, 3.0, 2.0).await;
    insert_resolved_evidence(&f, "riff", "community_seeding", 0.0, 0.0, 0.0).await;

    assert!(write_snapshot(&f, f.now).await);

    let row: (serde_json::Value, f64, f64, f64, i32) = sqlx::query_as(
        r#"SELECT attribution, total_observed_fans, total_incremental_fans,
                  total_durable_fans, resolved_observations
           FROM fan_source_snapshots WHERE workspace_id = $1"#,
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("snapshot row");

    let (attribution, observed, incremental, durable, resolved) = row;
    // The blob is the authority; the columns are the same numbers denormalized.
    assert_eq!(observed, 5.0);
    assert_eq!(incremental, 3.0);
    assert_eq!(durable, 2.0);
    assert_eq!(resolved, 2);
    assert_eq!(attribution["total_observed_fans"].as_f64(), Some(5.0));
    assert_eq!(attribution["total_incremental_fans"].as_f64(), Some(3.0));
    assert_eq!(attribution["total_durable_fans"].as_f64(), Some(2.0));

    let by_template = attribution["by_template"]
        .as_array()
        .expect("by_template must be an array");
    assert_eq!(by_template.len(), 2, "both templates must survive");
    // The id is the stored snake_case family, not the enum's Debug repr —
    // a "Story" here means the surface can never join back to the evidence.
    let mut ids: Vec<&str> = by_template
        .iter()
        .map(|t| t["template_id"].as_str().expect("template_id"))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["riff", "story"]);

    let ineffective = by_template
        .iter()
        .find(|t| t["incremental_fans"].as_f64() == Some(0.0))
        .expect("the zero-incremental template must be in the snapshot");
    assert_eq!(ineffective["observations"].as_i64(), Some(1));

    let by_strategy = attribution["by_strategy"]
        .as_array()
        .expect("by_strategy must be an array");
    assert_eq!(by_strategy.len(), 1);
    assert_eq!(
        by_strategy[0]["strategy"].as_str(),
        Some("community_seeding")
    );
}

#[tokio::test]
#[ignore]
async fn snapshot_writes_at_most_once_per_hour_per_workspace() {
    let f = setup().await.expect("fixture");
    insert_resolved_evidence(&f, "story", "community_seeding", 2.0, 1.0, 1.0).await;

    assert!(write_snapshot(&f, f.now).await, "first write lands");
    assert!(
        !write_snapshot(&f, f.now + time::Duration::minutes(30)).await,
        "same-hour write is capped"
    );
    assert!(
        write_snapshot(&f, f.now + time::Duration::minutes(61)).await,
        "next hour writes again"
    );

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_source_snapshots WHERE workspace_id = $1")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("count");
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore]
async fn snapshots_are_workspace_scoped() {
    let f = setup().await.expect("fixture");
    insert_resolved_evidence(&f, "story", "community_seeding", 4.0, 2.0, 1.0).await;

    // A second workspace writes its own snapshot in the same hour — the cap
    // is per workspace, not global.
    let other = WorkspaceId::new();
    let suffix = other.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(other.into_uuid())
        .bind(format!("fan-source-other-{suffix}"))
        .bind("Other Workspace")
        .execute(&f.pool)
        .await
        .expect("other workspace");
    let empty = crowdrelay_brain::attribution::attribute_fan_growth(&[]);
    assert!(
        record_fan_source_snapshot(&f.pool, other, &empty, &[], f.now)
            .await
            .expect("other workspace write")
    );

    assert!(write_snapshot(&f, f.now).await);

    // Each workspace's read sees exactly its own ledger.
    let other_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_source_snapshots WHERE workspace_id = $1")
            .bind(other.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("other count");
    assert_eq!(other_count, 1, "the other's empty snapshot stands alone");
    let this_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_source_snapshots WHERE workspace_id = $1")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("this count");
    assert_eq!(this_count, 1);
}

/// One cycle run per day at `north_star_value` — the rows `daily_north_star`
/// reads.
async fn seed_north_star_series(f: &Fixture, days: &[(i64, f64)]) {
    for (days_ago, value) in days {
        let started = f.now - time::Duration::days(*days_ago);
        sqlx::query(
            r#"INSERT INTO autopilot_cycle_runs
               (id, workspace_id, trigger, started_at, finished_at, north_star_value)
               VALUES ($1, $2, 'scheduled', $3, $3, $4)"#,
        )
        .bind(uuid::Uuid::now_v7())
        .bind(f.workspace_id.into_uuid())
        .bind(started)
        .bind(i32::try_from(*value as i64).unwrap_or(i32::MAX))
        .execute(&f.pool)
        .await
        .expect("cycle run");
    }
}

#[tokio::test]
#[ignore]
async fn snapshot_shift_date_is_the_civil_date_not_the_index() {
    // The acceptance test the plan calls for: `ChangePoint.timestamp` is an
    // observation index wearing a timestamp's name. A series with a known
    // step must store the civil date of the day the detector fired — a test
    // that only asserts "a shift was detected" ships the index-as-date bug.
    let f = setup().await.expect("fixture");
    // 30 flat days ending 10 days ago, then a clean step to a higher rate
    // running to today. The step clears drift 2.0 in a single observation,
    // so the detector fires on the first post-step day.
    let mut series: Vec<(i64, f64)> = (10..40).rev().map(|d| (d, 1.0)).collect();
    // First post-step observation is days_ago = 9 — the day the detector
    // fires and the date that must land on the stored shift.
    let step_day = f.now.date() - time::Duration::days(9);
    for d in (0..10).rev() {
        series.push((d, 20.0));
    }
    seed_north_star_series(&f, &series).await;

    assert!(write_snapshot(&f, f.now).await);

    let shifts: serde_json::Value = sqlx::query_scalar(
        "SELECT north_star_shifts FROM fan_source_snapshots WHERE workspace_id = $1",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("shifts column");
    let shifts = shifts.as_array().expect("shifts must be an array");
    assert!(!shifts.is_empty(), "a 19-fan step must produce a shift");

    let first = &shifts[0];
    assert_eq!(first["direction"].as_str(), Some("upward"));
    assert_eq!(
        first["date"].as_str(),
        Some(step_day.to_string().as_str()),
        "the stored date must be the day the new regime first observed, \
         not the detector's observation index"
    );
    // And the rates around it read in the right order — pre below post for
    // an upward shift (they arrive swapped when the series is fed reversed).
    assert!(first["pre_mean"].as_f64() < first["post_mean"].as_f64());
}
