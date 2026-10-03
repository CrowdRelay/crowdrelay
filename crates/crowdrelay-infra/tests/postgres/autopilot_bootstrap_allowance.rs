//! The warm-up allowance, against a real schema.
//!
//! `disposition_with_evidence` downgrades unattended execution to approval
//! until a context has twenty distinct externally-observed interventions, and
//! trap: acting is how those observations get made, so a gate that blocks
//! action below the floor guarantees the floor is never reached. Routing
//! below-floor work through approval instead of denial was the intended way
//! out and, against one operator and a 72-hour expiry, the same thing —
//! measured, zero authority-earning outcomes against a floor of twenty.
//!
//! The rule itself is pure and pinned in `domain::autonomy`. What only a
//! database can show is the two numbers it is fed: the operator's cap comes
//! back off the envelope, and the spend is counted from the durable action
//! rows rather than a second ledger. A rule fed the wrong numbers is a rule
//! that does not do what its tests say.

use crate::common;

use std::time::Duration;

use crowdrelay_application::autopilot::{AutopilotContext, AutopilotDecisionRepository};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("warmup-{}", id.simple()))
        .bind("Warm-up Test")
        .execute(pool)
        .await?;
    Ok(id)
}

/// Writes one action as the persist path writes it.
///
/// `approved_by` is the whole distinction the spend query turns on:
/// `policy:bounded_auto` is what an action nobody approved carries.
async fn action(
    pool: &PgPool,
    workspace_id: Uuid,
    context: &str,
    approved_by: Option<&str>,
    created_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    classed_action(
        pool,
        workspace_id,
        context,
        "third_party",
        approved_by,
        created_at,
    )
    .await?;
    Ok(())
}

/// The same, in a chosen action class.
async fn classed_action(
    pool: &PgPool,
    workspace_id: Uuid,
    context: &str,
    class: &str,
    approved_by: Option<&str>,
    created_at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions \
         (id, workspace_id, decision_key, context, subject_kind, subject_id, decision_kind, \
          confidence_basis_points, disposition, reason, input_snapshot, policy_snapshot, \
          recommendation, trace_id) \
         VALUES ($1,$2,$3,$4,'test',$5,'test',5000,'auto_execute','test', \
                 '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$6)",
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("warmup-{}", decision_id.simple()))
    .bind(context)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions \
         (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id, \
          idempotency_key, payload, status, action_class, approved_by, approved_at, created_at, \
          trace_id) \
         VALUES ($1,$2,$3,$4,'test.action','test',$5,$6,'{}'::jsonb,'queued', \
                 $10,$7,$8,$8,$9)",
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(context)
    .bind(Uuid::now_v7())
    .bind(format!("warmup-action-{}", action_id.simple()))
    .bind(approved_by)
    .bind(created_at)
    .bind(Uuid::now_v7())
    .bind(class)
    .execute(pool)
    .await?;
    Ok(action_id)
}

async fn resolved_evidence_action(
    pool: &PgPool,
    workspace_id: Uuid,
    context: &str,
    kinds: &[&str],
    assessment: &str,
    at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let action_id = classed_action(
        pool,
        workspace_id,
        context,
        "third_party",
        Some("policy:bounded_auto"),
        at,
    )
    .await?;
    let decision_id: Uuid =
        sqlx::query_scalar("SELECT decision_id FROM autopilot_actions WHERE id=$1")
            .bind(action_id)
            .fetch_one(pool)
            .await?;

    sqlx::query(
        r#"INSERT INTO growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality,
            resolved_at)
           VALUES ($1,$2,$3,$4,'recipient','reddit_post',1,'treatment',1.0,false,
                   0.0,0.0,'{}'::jsonb,'observational',$4)"#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(format!("authority-{action_id}"))
    .bind(at)
    .execute(pool)
    .await?;

    for kind in kinds {
        let measurement_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_measurements
               (id, workspace_id, action_id, measurement_kind, subject_id,
                action_finished_at, baseline_value, due_at, available_at,
                status, finished_at)
               VALUES ($1,$2,$3,$4,$3,$5,0.0,$5,$5,'succeeded',$5)"#,
        )
        .bind(measurement_id)
        .bind(workspace_id)
        .bind(action_id)
        .bind(*kind)
        .bind(at)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"INSERT INTO autopilot_outcomes
               (workspace_id, decision_id, action_id, measurement_id, metric_key,
                observed_value, baseline_value, effect_assessment,
                delta_basis_points, observed_at)
               VALUES ($1,$2,$3,$4,$5,0.0,0.0,$6,
                      CASE WHEN $6='improved' THEN 1000
                           WHEN $6='worsened' THEN -1000 ELSE 0 END,$7)"#,
        )
        .bind(workspace_id)
        .bind(decision_id)
        .bind(action_id)
        .bind(measurement_id)
        .bind(format!("effect.{kind}"))
        .bind(assessment)
        .bind(at)
        .execute(pool)
        .await?;
    }
    Ok(action_id)
}

fn repository(pool: &PgPool, url: &str) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(2),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_warm_up_spend_counts_unattended_actions_per_context()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;

    // Three unattended outreach actions inside the window.
    for _ in 0..3 {
        action(
            &pool,
            workspace_id,
            "outreach",
            Some("policy:bounded_auto"),
            now - Duration::from_secs(3600),
        )
        .await?;
    }
    // One in another context: the allowance is per context, so this must not
    // spend outreach's.
    action(
        &pool,
        workspace_id,
        "fan_lifecycle",
        Some("policy:bounded_auto"),
        now,
    )
    .await?;
    // An action a person approved is not warm-up spend — the allowance bounds
    // what went out *without* them.
    action(
        &pool,
        workspace_id,
        "outreach",
        Some("operator:admin_api_key"),
        now,
    )
    .await?;
    // Unattended internal work spends nothing: an artifact render never
    // needed the allowance, and counting it starved the outward work that
    // does (production, 2026-09-25: 67 renders against a cap of 5).
    for _ in 0..4 {
        classed_action(
            &pool,
            workspace_id,
            "outreach",
            "first_party_reversible",
            Some("policy:bounded_auto"),
            now,
        )
        .await?;
    }
    // A parked action nobody has approved yet has spent nothing either.
    action(&pool, workspace_id, "outreach", None, now).await?;
    // And one outside the window: the allowance is weekly, not lifetime, or it
    // would be spent once and never again.
    action(
        &pool,
        workspace_id,
        "outreach",
        Some("policy:bounded_auto"),
        now - Duration::from_secs(8 * 24 * 3600),
    )
    .await?;

    let spend = repository(&pool, &url)
        .load_bootstrap_spend(WorkspaceId::from_uuid(workspace_id), now)
        .await?;

    assert_eq!(
        spend.get(&AutopilotContext::Outreach).copied(),
        Some(3),
        "only unattended outreach actions inside the week count: {spend:?}"
    );
    assert_eq!(
        spend.get(&AutopilotContext::FanLifecycle).copied(),
        Some(1),
        "a second context keeps its own spend: {spend:?}"
    );
    assert_eq!(
        spend.get(&AutopilotContext::Plays).copied(),
        None,
        "a context that acted not at all is absent, not zero: {spend:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unattended_authority_is_earned_from_people_not_the_system_itself()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;
    let repo = repository(&pool, &url);

    // All three are real completed measurements, but none says a human did
    // anything. They may teach diagnostics and production quality; they may
    // not make content_supply more trusted to act unattended.
    resolved_evidence_action(
        &pool,
        workspace_id,
        "content_supply",
        &["agent_run_outcome_quality_1h"],
        "neutral",
        now - time::Duration::minutes(3),
    )
    .await?;
    resolved_evidence_action(
        &pool,
        workspace_id,
        "content_supply",
        &["scanner_discovery_quality_1h"],
        "neutral",
        now - time::Duration::minutes(2),
    )
    .await?;
    resolved_evidence_action(
        &pool,
        workspace_id,
        "content_supply",
        &["artifact_outcome_7d"],
        "neutral",
        now - time::Duration::minutes(1),
    )
    .await?;

    let self_only = repo
        .load_resolved_evidence_counts(WorkspaceId::from_uuid(workspace_id))
        .await?;
    assert_eq!(
        self_only
            .for_context(AutopilotContext::ContentSupply)
            .observations
            .0,
        0,
        "the machine cannot earn external authority by grading its own work"
    );

    // Even a real audience-facing intervention that honestly measured zero
    // does not earn more freedom. "We learned it did nothing" is not evidence
    // that the machine deserves to do more of it unattended.
    resolved_evidence_action(
        &pool,
        workspace_id,
        "content_supply",
        &["content_link_clicks_7d"],
        "neutral",
        now - time::Duration::seconds(10),
    )
    .await?;
    let external_zero = repo
        .load_resolved_evidence_counts(WorkspaceId::from_uuid(workspace_id))
        .await?;
    assert_eq!(
        external_zero
            .for_context(AutopilotContext::ContentSupply)
            .observations
            .0,
        0,
        "a measured zero teaches the learner but earns no additional authority"
    );

    // One audience-facing action carries two legitimate measurements. It
    // still counts as one observed intervention, not two votes for authority.
    resolved_evidence_action(
        &pool,
        workspace_id,
        "content_supply",
        &["content_link_clicks_7d", "content_fan_acquisition_7d"],
        "improved",
        now,
    )
    .await?;
    let external = repo
        .load_resolved_evidence_counts(WorkspaceId::from_uuid(workspace_id))
        .await?;
    assert_eq!(
        external
            .for_context(AutopilotContext::ContentSupply)
            .observations
            .0,
        1,
        "one externally observed action earns one unit of authority regardless of measurement count"
    );
    Ok(())
}

/// The operator's cap has to survive the round trip, or the rule is fed a
/// number nobody chose. The default is five rather than zero deliberately:
/// zero is what the system already did, and it is why the floor never moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_warm_up_cap_comes_back_off_the_envelope() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;
    let repository = repository(&pool, &url);

    let (envelope, _) = repository
        .load_growth_envelope(WorkspaceId::from_uuid(workspace_id), now)
        .await?;
    assert_eq!(
        envelope.weekly_bootstrap_actions, 5,
        "a fresh workspace gets the warm-up, not zero"
    );

    sqlx::query("UPDATE growth_envelope SET weekly_bootstrap_actions = 0 WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await?;
    let (envelope, _) = repository
        .load_growth_envelope(WorkspaceId::from_uuid(workspace_id), now)
        .await?;
    assert_eq!(
        envelope.weekly_bootstrap_actions, 0,
        "an operator who switches the warm-up off gets it switched off"
    );
    Ok(())
}

/// Money is not in this mechanism at all, but the column it shares a table
/// with is bounded, and a cap nobody can exceed is worth one assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_warm_up_cap_is_bounded_by_the_schema() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = workspace(&pool).await?;
    let refused = sqlx::query(
        "UPDATE growth_envelope SET weekly_bootstrap_actions = 101 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await;
    assert!(
        refused.is_err(),
        "the warm-up cap must stay inside its reviewed range"
    );
    Ok(())
}
