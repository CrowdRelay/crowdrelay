//! The learning loop closing, against a real schema.
//!
//! The brain's learning has been real for a while and unprovable: the strategy
//! posterior lives in `viryaos_brain_state`, one row per module, updated in
//! place. "The posterior says community_first is worth 4.2 fans" was
//! answerable; "the posterior changed because of what happened to action X"
//! was not. Migration 0252 added the belief-revision ledger to close that, and
//! the write side of it runs inside the causal model load — the least
//! observable place in the cycle.
//!
//! In production the first entry cannot appear until an
//! `IncrementalFanGrowth14d` measurement resolves, because that is the only
//! measurement kind that writes `observed_incremental_fans`, and the strategy
//! posterior updates from nothing else. That is a 14-day wait on a brain four
//! days old. This test supplies the resolved evidence directly and drives the
//! real loader, so the chain is proven now rather than believed until then.
//!
//! What is asserted, and why each one can break silently:
//!
//! - A resolved outcome moves the posterior AND leaves a revision behind. The
//!   ledger write is best-effort by design; a failure logs a warning and the
//!   cycle continues, so nothing else would notice it stopped happening.
//! - The revision cites the action whose outcome moved the belief. An
//!   unattributed revision is the exact thing the ledger exists to prevent,
//!   and the writer drops those — so a citation bug reads as "no learning".
//! - The citation reaches the decision and the trace. The proof endpoint joins
//!   `caused_by_action_ids` to actions, actions to decisions, decisions to
//!   outcomes; a column rename anywhere on that path compiles, lints and tests
//!   clean under runtime SQLx and fails on first request.
//! - A second replay of the same evidence records no second revision. The
//!   posterior is rebuilt from scratch on full replay, so a diff taken against
//!   the wrong baseline would append a duplicate every cycle forever.
//!
//! What is NOT asserted: the exact SQL in
//! `crowdrelay-api/src/autopilot/learning_proof.rs`. That query lives behind a
//! private module and restating it here would test a copy of itself. The joins
//! it depends on are asserted below against the same schema.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

/// One dispatch the brain made, measured and resolved.
///
/// The evidence row is what the learner reads; the decision and action exist
/// because the evidence points at them and because the proof endpoint walks
/// back along that path.
struct ResolvedDispatch {
    decision_id: Uuid,
    action_id: Uuid,
    trace_id: Uuid,
}

async fn seed_resolved_dispatch(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    template_id: &str,
    strategy: &str,
    observed_incremental_fans: f64,
) -> Result<ResolvedDispatch, Box<dyn std::error::Error>> {
    let workspace = workspace_id.into_uuid();
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    let trace_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();

    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1, $2, $3, 'growth_intelligence', 'workspace', $2,
                  'request_agent_run', 10000, 'auto_execute', 'learning proof fixture',
                  $4, '{}'::jsonb, $5, $6, $7)
        "#,
    )
    .bind(decision_id)
    .bind(workspace)
    .bind(format!("decision:learning-proof:{decision_id}"))
    // The `learning` block the proof endpoint matches on. Written by the
    // evaluator in production; supplied here because this fixture does not run
    // a cycle.
    .bind(serde_json::json!({
        "learning": {
            "strategy_prior": "broadcast",
            "strategy_applied": strategy,
            "strategy_source": "posterior",
        }
    }))
    .bind(serde_json::json!({ "template_id": template_id }))
    .bind(now - time::Duration::days(14))
    .bind(trace_id)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1, $2, $3, 'growth_intelligence', 'request_agent_run', 'workspace',
                  $2, $4, $5, 'succeeded', $6, $7)
        "#,
    )
    .bind(action_id)
    .bind(workspace)
    .bind(decision_id)
    .bind(format!("action:learning-proof:{action_id}"))
    .bind(serde_json::json!({ "template_id": template_id }))
    .bind(now - time::Duration::days(14))
    .bind(trace_id)
    .execute(pool)
    .await?;

    // The measurement the outcome came from. Not optional scaffolding: the
    // outcomes table refuses an assessed row without one, which is the schema
    // saying an assessment nobody can trace to a measurement is not an
    // assessment.
    let measurement_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_measurements (
            id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, status, available_at,
            finished_at
        ) VALUES ($1, $2, $3, 'incremental_fan_growth_14d', $2, $4, 0, now(),
                  'succeeded', now(), now())
        "#,
    )
    .bind(measurement_id)
    .bind(workspace)
    .bind(action_id)
    .bind(now - time::Duration::days(14))
    .execute(pool)
    .await?;

    // The measured outcome. The proof endpoint reports this beside the action,
    // so "because of" says what happened rather than only that something did.
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_outcomes (
            workspace_id, decision_id, action_id, measurement_id, metric_key,
            observed_value, baseline_value, effect_assessment,
            delta_basis_points, observed_at
        ) VALUES ($1, $2, $3, $4, 'effect.incremental_fan_growth_14d', $5, 0,
                  'improved', 400, now())
        "#,
    )
    .bind(workspace)
    .bind(decision_id)
    .bind(action_id)
    .bind(measurement_id)
    .bind(observed_incremental_fans)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO viryaos_growth_evidence (
            workspace_id, action_id, opportunity_id, timestamp, audience,
            recipient_id, channel, estimated_reach, treatment, propensity,
            observed_fans, observed_incremental_fans, predicted_fans,
            context, strategy, evidence_quality, resolved_at
        ) VALUES ($1, $2, $3, $4, 'r/testsubreddit', 'testsubreddit', 'reddit_post',
                  1, 'treatment', 1.0, $5, $5, 1.0, $6, $7, 'observational', now())
        "#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(format!("{template_id}:community:post:ctxhash"))
    .bind(now - time::Duration::days(14))
    .bind(observed_incremental_fans)
    // The state the posterior conditions on. `steady` and no event put the
    // cell key at `<strategy>:steady:far`.
    .bind(serde_json::json!({ "fan_growth_trend": "steady" }))
    .bind(strategy)
    .execute(pool)
    .await?;

    Ok(ResolvedDispatch {
        decision_id,
        action_id,
        trace_id,
    })
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_resolved_outcome_moves_a_belief_and_leaves_a_citable_record()
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
        .bind(format!("learning-proof-{suffix}"))
        .bind("Learning proof E2E")
        .execute(&pool)
        .await?;

    // Five dispatches of one strategy, all resolved well. One observation
    // would move the cell too; five put the movement clear of the material
    // threshold and give the posterior a confidence worth reporting.
    let mut dispatched = Vec::new();
    for _ in 0..5 {
        dispatched.push(
            seed_resolved_dispatch(
                &pool,
                workspace_id,
                "community-engager",
                "community_first",
                8.0,
            )
            .await?,
        );
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

    // The real path. Nothing here calls the ledger writer directly: the causal
    // model load is where the replay happens in production, and a test that
    // reached past it would pass while the cycle recorded nothing.
    repository.load_causal_model(workspace_id).await?;

    // ── The belief moved ──
    let posterior: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT state FROM viryaos_brain_state
         WHERE workspace_id = $1 AND module = 'strategy_posterior'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&pool)
    .await?;
    let posterior = posterior
        .expect("the replay must write a strategy posterior")
        .0;
    let cell = &posterior["posteriors"]["community_first:steady:far"];
    assert!(
        !cell.is_null(),
        "the posterior must hold the cell the evidence was in, got {posterior}"
    );
    assert_eq!(
        cell["n"], 5,
        "every resolved observation must reach the cell"
    );

    // ── And left a record of moving ──
    #[derive(sqlx::FromRow)]
    struct RevisionRow {
        belief_key: String,
        change_summary: String,
        previous_value: serde_json::Value,
        current_value: serde_json::Value,
        caused_by_action_ids: Vec<Uuid>,
    }
    let revisions = sqlx::query_as::<_, RevisionRow>(
        "SELECT belief_key, change_summary, previous_value, current_value,
                caused_by_action_ids
         FROM viryaos_brain_belief_revisions
         WHERE workspace_id = $1 AND module = 'strategy_posterior'
         ORDER BY recorded_at DESC",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        revisions.len(),
        1,
        "one cell moved, so exactly one revision — the ledger must not report \
         a belief per observation"
    );
    let revision = &revisions[0];
    assert_eq!(revision.belief_key, "community_first:steady:far");
    assert_eq!(
        revision.previous_value["observations"], 0,
        "the cell had no evidence before this batch, and the revision must say so"
    );
    assert!(
        revision.current_value["mean"].as_f64().unwrap_or_default() > 0.0,
        "a belief that moved must record where it moved to, got {}",
        revision.current_value
    );
    assert!(
        revision
            .change_summary
            .contains("community_first:steady:far"),
        "the summary is what an operator reads without decoding the values: {}",
        revision.change_summary
    );

    // ── The citation reaches the decision, the outcome and the trace ──
    let cited = &revision.caused_by_action_ids;
    assert_eq!(cited.len(), 5, "every contributing dispatch must be cited");
    for dispatch in &dispatched {
        assert!(
            cited.contains(&dispatch.action_id),
            "action {} moved the belief and is not cited",
            dispatch.action_id
        );
    }
    // The joins the proof endpoint walks, against the same schema. A rename on
    // this path compiles and lints clean under runtime SQLx.
    #[derive(sqlx::FromRow)]
    struct ChainRow {
        decision_id: Uuid,
        trace_id: Option<Uuid>,
        effect_assessment: Option<String>,
        strategy_applied: Option<String>,
    }
    let chain = sqlx::query_as::<_, ChainRow>(
        r#"
        SELECT a.decision_id,
               d.trace_id,
               o.effect_assessment,
               d.input_snapshot -> 'learning' ->> 'strategy_applied' AS strategy_applied
        FROM viryaos_autopilot_actions a
        JOIN viryaos_autopilot_decisions d
          ON d.workspace_id = a.workspace_id AND d.id = a.decision_id
        LEFT JOIN LATERAL (
            SELECT effect_assessment
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = a.workspace_id
              AND action_id = a.id
              AND effect_assessment IS NOT NULL
            ORDER BY observed_at DESC
            LIMIT 1
        ) o ON true
        WHERE a.workspace_id = $1 AND a.id = ANY($2)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(cited)
    .fetch_all(&pool)
    .await?;
    assert_eq!(chain.len(), 5, "every cited action must still be reachable");
    for row in &chain {
        assert!(
            dispatched
                .iter()
                .any(|dispatch| dispatch.decision_id == row.decision_id),
            "a cited action resolved to a decision nothing dispatched"
        );
        assert!(
            row.trace_id
                .is_some_and(|trace| dispatched.iter().any(|dispatch| dispatch.trace_id == trace)),
            "the chain must reach the trace, or the operator cannot follow it"
        );
        assert_eq!(
            row.effect_assessment.as_deref(),
            Some("improved"),
            "the proof must be able to say what was measured, not only that \
             something was"
        );
        assert_eq!(
            row.strategy_applied.as_deref(),
            Some("community_first"),
            "the decision's own record of which strategy it acted on is how a \
             later decision is matched to the belief that moved"
        );
    }

    // ── Replaying the same evidence records nothing new ──
    //
    // Full replay rebuilds the posterior from a skeptical prior, so the diff
    // is taken against that rebuild's own starting point. Taken against the
    // stored posterior instead, every cycle would append the same revision
    // again and the ledger would grow without anything having been learned.
    repository.load_causal_model(workspace_id).await?;
    let revision_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_brain_belief_revisions WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        revision_count, 1,
        "a replay that learns nothing new must record nothing new"
    );

    Ok(())
}
