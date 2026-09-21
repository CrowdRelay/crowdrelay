//! The warm-up allowance, against a real schema.
//!
//! `disposition_with_evidence` downgrades unattended execution to approval
//! until a context has twenty resolved outcomes, and its own comment names the
//! trap: acting is how those observations get made, so a gate that blocks
//! action below the floor guarantees the floor is never reached. Routing
//! below-floor work through approval instead of denial was the intended way
//! out and, against one operator and a 72-hour expiry, the same thing —
//! measured, zero resolved outcomes against a floor of twenty.
//!
//! The rule itself is pure and pinned in `domain::autonomy`. What only a
//! database can show is the two numbers it is fed: the operator's cap comes
//! back off the envelope, and the spend is counted from the durable action
//! rows rather than a second ledger. A rule fed the wrong numbers is a rule
//! that does not do what its tests say.

mod common;

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
                 'first_party_reversible',$7,$8,$8,$9)",
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
    .execute(pool)
    .await?;
    Ok(())
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
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let url = std::env::var("CROWDRELAY_TEST_DATABASE_URL").expect("suite database url");
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

/// The operator's cap has to survive the round trip, or the rule is fed a
/// number nobody chose. The default is five rather than zero deliberately:
/// zero is what the system already did, and it is why the floor never moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_warm_up_cap_comes_back_off_the_envelope() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let url = std::env::var("CROWDRELAY_TEST_DATABASE_URL").expect("suite database url");
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
