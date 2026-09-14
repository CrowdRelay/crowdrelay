//! R0c — outcome-created actions must carry the same learning envelope as
//! evaluator-created ones.
//!
//! The worker outcome path (`agent_outcomes.rs`) inserts a bare
//! `viryaos_autopilot_actions` row: no decision persist ran for it, so no
//! `viryaos_dispatch_predictions` or `viryaos_growth_evidence` row exists.
//! Production showed the result — every `community.engage.request` action had
//! its measurements scheduled but zero evidence and zero prediction rows, so
//! each measurement resolved into nothing and the causal model learned
//! nothing from the work.
//!
//! `ensure_dispatch_envelope` now writes the cold-prior envelope at the
//! dispatch seam, conflict-safely. What is asserted here:
//!
//! - A `community.engage.request` action leaves execution with a prediction
//!   row, an unresolved evidence row, and its measurement rows.
//! - The prediction carries the honest cold prior, not an invented number.
//! - The evidence names the community channel and keeps outcome fields NULL —
//!   the forum is measured as a fan *source*, and nothing pretends a post
//!   already converted anyone.
//! - An executor-required action gets no envelope at dispatch: its evidence
//!   is committed when the provider-confirmed receipt arrives, so a queued
//!   webhook can never masquerade as completed work.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("envelope-{suffix}"))
        .bind("Dispatch Envelope Tests")
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
    })
}

/// The bare action insert the worker outcome path performs: a decision row
/// for lineage, then a queued action with no prediction or evidence.
async fn seed_outcome_action(
    f: &Fixture,
    action_kind: &str,
    payload: Value,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'growth_intelligence','agent_outcome',$4,$5,9000,
                   'auto_execute','outcome-created action','{}','{}','{}',$6,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(action_kind)
    .bind(now)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'growth_intelligence',$4,'agent_outcome',$5,$6,$7,
                   'queued','third_party',$8,'policy:bounded_auto',$8)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(action_kind)
    .bind(Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(payload)
    .bind(now)
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_community_engagement_action_leaves_execution_with_a_learning_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let action_id = seed_outcome_action(
        &f,
        "community.engage.request",
        json!({
            "kind": "request_community_engagement",
            "target_id": Uuid::now_v7(),
            "platform": "reddit",
            "subreddit": "r/envelope_test",
            "title": "test title",
            "body": "test body",
            "smart_link": null,
        }),
        now,
    )
    .await?;

    // The production path: the autonomous claim picks the queued action up
    // and `execute_action` runs it.
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued community action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    // The prediction the model would have returned for an unseen template —
    // the cold prior, not an invented success.
    let prediction = sqlx::query_as::<_, (String, f64, f64)>(
        "SELECT template_id, expected_new_fans, expected_signal_installs \
         FROM viryaos_dispatch_predictions WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(prediction.0, "community-engager");
    assert!(
        (prediction.1 - crowdrelay_brain::DEFAULT_EXPECTED_FANS).abs() < f64::EPSILON,
        "an outcome-created action predicts the cold prior, got {}",
        prediction.1
    );
    assert!(
        (prediction.2 - crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL).abs() < f64::EPSILON,
        "the signal prior is the cold value, got {}",
        prediction.2
    );

    // The evidence row exists, unresolved, on the community channel — with
    // every outcome field NULL. Nothing pretends the post converted anyone.
    let evidence = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<f64>,
            Option<f64>,
            Option<OffsetDateTime>,
        ),
    >(
        "SELECT channel, treatment, observed_fans, observed_incremental_fans, resolved_at \
         FROM viryaos_growth_evidence WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(evidence.0, "reddit_post");
    assert_eq!(evidence.1, "treatment");
    assert!(
        evidence.2.is_none() && evidence.3.is_none() && evidence.4.is_none(),
        "the dispatch envelope is an expectation, not an outcome: {evidence:?}"
    );

    // The measurements the engagement schedule owns: reception plus the
    // incremental fan-growth counterfactuals.
    let kinds = sqlx::query_scalar::<_, String>(
        "SELECT measurement_kind FROM viryaos_autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    for expected in [
        "agent_run_community_engagement_7d",
        "incremental_fan_growth_14d",
        "incremental_fan_growth_3d",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "measurement {expected} missing; scheduled: {kinds:?}"
        );
    }

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(status, "succeeded");

    // Exactly once: a second claim finds nothing to redo, and the envelope
    // rows are single.
    let reclaims = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now + time::Duration::minutes(1))
        .await?;
    assert!(
        !reclaims.iter().any(|a| a.id.into_uuid() == action_id),
        "a succeeded action must not be re-claimed"
    );
    let (prediction_rows, evidence_rows) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT \
           (SELECT COUNT(*) FROM viryaos_dispatch_predictions WHERE action_id = $1)::bigint, \
           (SELECT COUNT(*) FROM viryaos_growth_evidence \
            WHERE workspace_id = $2 AND action_id = $1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (prediction_rows, evidence_rows),
        (1, 1),
        "the envelope is written exactly once under replay"
    );
    Ok(())
}

/// An action whose work only counts when the executor reports back must not
/// leave dispatch with evidence — the envelope belongs to the receipt.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_executor_required_action_gets_no_premature_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let action_id = seed_outcome_action(
        &f,
        "outreach.discovery.request",
        json!({
            "kind": "request_outreach_discovery",
            "requested_candidates": 5
        }),
        now,
    )
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued discovery action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let (prediction_rows, evidence_rows, measurement_rows) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
               (SELECT COUNT(*) FROM viryaos_dispatch_predictions WHERE action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM viryaos_growth_evidence \
                WHERE workspace_id = $2 AND action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM viryaos_autopilot_measurements \
                WHERE workspace_id = $2 AND action_id = $1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (prediction_rows, evidence_rows, measurement_rows),
        (0, 0, 0),
        "a dispatched intent is not evidence: the executor receipt writes those rows"
    );
    Ok(())
}
