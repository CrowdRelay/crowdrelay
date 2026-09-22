//! The two limits on what a person receives, against a real schema.
//!
//! Every other limit in this system bounds what somebody outside the workspace
//! receives. Nothing bounded what the tenant's own crew received, and that is
//! the quantity that ran out: 26 contexts at 50 actions a day each, every
//! `awaiting_approval` action becoming an assignment, every assignment owing a
//! first notice plus up to three reminders, against one person.
//!
//! Two limits now bound it from two directions, and both are only as good as
//! the numbers they are fed:
//!
//! * the workspace's weekly attention budget, spent by asks counted from the
//!   durable action rows;
//! * the crew's per-member weekly ask ceiling, which no longer reads an unset
//!   tenant setting as uncapped.
//!
//! The rules themselves are pure and pinned in the domain. What only a
//! database can show is that the readers agree with them.

use crate::common;

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::growth_envelope::check_attention;
use crowdrelay_domain::team_operations::DEFAULT_WEEKLY_ASK_CEILING;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("attention-{}", id.simple()))
        .bind("Attention Budget Test")
        .execute(pool)
        .await?;
    Ok(id)
}

/// Writes one action.
///
/// `asked` decides whether it carries an `approval_expires_at`, which is the
/// durable mark that a person was put in front of this decision. It survives
/// the approval on purpose: an operator who answers quickly has still been
/// asked, and a budget that forgot them the moment they answered would bound
/// nothing.
async fn action(
    pool: &PgPool,
    workspace_id: Uuid,
    asked: bool,
    status: &str,
    created_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions \
         (id, workspace_id, decision_key, context, subject_kind, subject_id, decision_kind, \
          confidence_basis_points, disposition, reason, input_snapshot, policy_snapshot, \
          recommendation, trace_id) \
         VALUES ($1,$2,$3,'outreach','test',$4,'test',5000,'require_approval','test', \
                 '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)",
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("attention-{}", decision_id.simple()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions \
         (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id, \
          idempotency_key, payload, status, action_class, approval_expires_at, created_at, \
          finished_at, trace_id) \
         VALUES ($1,$2,$3,'outreach','test.action','test',$4,$5,'{}'::jsonb,$6, \
                 'first_party_reversible',$7,$8,$10,$9)",
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("attention-action-{}", action_id.simple()))
    .bind(status)
    .bind(asked.then(|| created_at + Duration::from_secs(72 * 3600)))
    .bind(created_at)
    .bind(Uuid::now_v7())
    // A terminal action must say when it finished; the CHECK refuses one that
    // does not, and the fixture has no business writing a row production
    // could not.
    .bind((status == "cancelled").then_some(created_at))
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
async fn the_attention_spend_counts_asks_made_not_asks_waiting()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;

    // Two still waiting, one already answered, one the operator cancelled.
    // All four interrupted somebody, so all four count.
    action(&pool, workspace_id, true, "awaiting_approval", now).await?;
    action(&pool, workspace_id, true, "awaiting_approval", now).await?;
    action(&pool, workspace_id, true, "queued", now).await?;
    action(&pool, workspace_id, true, "cancelled", now).await?;
    // An action that ran unattended asked nobody.
    action(&pool, workspace_id, false, "queued", now).await?;
    // And one from last month: the budget is weekly, not lifetime.
    action(
        &pool,
        workspace_id,
        true,
        "queued",
        now - Duration::from_secs(8 * 24 * 3600),
    )
    .await?;

    let (envelope, usage) = repository(&pool, &url)
        .load_growth_envelope(WorkspaceId::from_uuid(workspace_id), now)
        .await?;

    assert_eq!(
        usage.approval_requests_7d, 4,
        "four decisions were put in front of a person this week"
    );
    assert_eq!(
        envelope.weekly_approval_requests, 20,
        "a fresh workspace may ask, and the number is the migration's"
    );
    assert!(
        check_attention(&envelope, &usage).may_ask(),
        "four of twenty leaves room"
    );
    Ok(())
}

/// The budget has to actually bind, or it is a column nobody reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_spent_attention_budget_stops_the_agent_asking() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;

    sqlx::query(
        "UPDATE viryaos_growth_envelope SET weekly_approval_requests = 2 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    for _ in 0..2 {
        action(&pool, workspace_id, true, "awaiting_approval", now).await?;
    }

    let repository = repository(&pool, &url);
    let (envelope, usage) = repository
        .load_growth_envelope(WorkspaceId::from_uuid(workspace_id), now)
        .await?;
    assert_eq!(usage.approval_requests_7d, 2);
    assert!(
        !check_attention(&envelope, &usage).may_ask(),
        "the week's budget is spent"
    );
    Ok(())
}

/// A budget of zero is a posture — read the board, send me nothing — and the
/// schema has to let a tenant express it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_tenant_may_choose_never_to_be_asked() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;

    sqlx::query(
        "UPDATE viryaos_growth_envelope SET weekly_approval_requests = 0 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    let (envelope, usage) = repository(&pool, &url)
        .load_growth_envelope(WorkspaceId::from_uuid(workspace_id), now)
        .await?;
    assert!(!check_attention(&envelope, &usage).may_ask());

    let refused = sqlx::query(
        "UPDATE viryaos_growth_envelope SET weekly_approval_requests = 1001 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "the budget stays inside its range");
    Ok(())
}

/// Fix five, through the sweep that enforces it: a tenant who never set a
/// ceiling used to be uncapped, so one member absorbed every ask the system
/// could produce. An unset setting now reads as the default, and the router
/// stops at it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unset_crew_ceiling_stops_at_the_default_instead_of_never()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let workspace_id = workspace(&pool).await?;
    // No `team_weekly_ask_ceiling` row anywhere in this test: the whole point
    // is what an unset setting does.
    advertise_team_email(&pool, workspace_id, now).await?;
    let alice = member(&pool, workspace_id, "alice").await?;

    // Alice has already had her week — exactly the default ceiling's worth of
    // asks, all inside the window.
    for index in 0..DEFAULT_WEEKLY_ASK_CEILING {
        settled_ask(&pool, workspace_id, alice, now, i64::from(index)).await?;
    }
    // One more decision arrives, with nobody else on the roster to take it.
    awaiting_approval(&pool, workspace_id, now).await?;

    let repository = repository(&pool, &url);
    repository
        .reconcile_team_handoffs(WorkspaceId::from_uuid(workspace_id), now)
        .await?;

    let handed_over = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_team_assignments \
         WHERE workspace_id = $1 AND source_kind = 'autopilot_action'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        handed_over, 0,
        "a member at the default ceiling must not be handed an eleventh ask"
    );

    // The same roster one ask lighter takes it, so the refusal above is the
    // ceiling and not an empty roster, a missing skill or a parked executor.
    sqlx::query(
        "DELETE FROM viryaos_team_assignments \
         WHERE workspace_id = $1 AND source_ref = 'seeded-ask-0'",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    repository
        .reconcile_team_handoffs(WorkspaceId::from_uuid(workspace_id), now)
        .await?;
    let handed_over = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_team_assignments \
         WHERE workspace_id = $1 AND source_kind = 'autopilot_action'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        handed_over, 1,
        "one ask under the ceiling the same work routes normally"
    );
    Ok(())
}

/// Without an executor advertising `team.email` the sweep queues nothing and
/// the ceiling test would pass for the wrong reason.
async fn advertise_team_email(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_executor_instances \
         (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at) \
         VALUES ($1,'n8n-attention-test','test','test-manifest',$2,$3)",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities \
         (workspace_id, executor_id, capability, capability_version, observed_at, expires_at) \
         VALUES ($1,'n8n-attention-test','team.email','1',$2,$3)",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

async fn member(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let member_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status) \
         VALUES ($1, $2, $3, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("{key}-{}@example.test", workspace_id.simple()))
    .bind(format!("Crew {key}"))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_team_profiles (workspace_id, member_id, member_key, active, skills) \
         VALUES ($1, $2, $3, true, ARRAY['approval','operations','social']::text[])",
    )
    .bind(workspace_id)
    .bind(member_id)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(member_id)
}

/// One ask already handed to this member inside the weekly window.
///
/// `show_task` rather than `autopilot_action` so the count the assertion makes
/// about autopilot handoffs is not pre-loaded by its own fixture, and closed
/// so it does not also count as open load.
async fn settled_ask(
    pool: &PgPool,
    workspace_id: Uuid,
    member_id: Uuid,
    now: OffsetDateTime,
    index: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_team_assignments \
         (id, workspace_id, source_kind, source_id, source_ref, assignee_member_id, \
          required_skill, status, assigned_at, completed_at) \
         VALUES ($1,$2,'show_task',$3,$4,$5,'approval','done',$6,$6)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(Uuid::now_v7())
    .bind(format!("seeded-ask-{index}"))
    .bind(member_id)
    .bind(now - Duration::from_secs(3600))
    .execute(pool)
    .await?;
    Ok(())
}

/// One unanswered approval: the handoff sweep's raw material.
///
/// Deliberately the shape production produces — `growth_intelligence` raised
/// the flood's batch, and `community.engage.request` is the kind the router
/// maps to the `approval` skill. A synthetic action kind would find no skill,
/// and then the ceiling assertion above would pass for the wrong reason.
async fn awaiting_approval(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_autopilot_decisions \
             (id, workspace_id, decision_key, context, subject_kind, subject_id, \
              decision_kind, confidence_basis_points, disposition, reason, \
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id) \
         VALUES ($1,$2,$3,'growth_intelligence','target_community',$4, \
                 'request_community_engagement',9000,'require_approval','seeded approval', \
                 '{}','{}','{}',$5,$6) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("attention-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(now)
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions \
             (id, workspace_id, decision_id, context, action_kind, subject_kind, \
              subject_id, idempotency_key, payload, status, approval_expires_at) \
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request','target_community', \
                 $4,$5,'{}'::jsonb,'awaiting_approval',$6)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("attention-approval-{}", Uuid::now_v7()))
    .bind(now + time::Duration::hours(72))
    .execute(pool)
    .await?;
    Ok(())
}
