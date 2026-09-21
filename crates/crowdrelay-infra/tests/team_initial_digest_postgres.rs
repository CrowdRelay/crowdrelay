//! One crew member, one first-notice email per sweep — not one per approval.
//!
//! Reported from a real inbox: fifteen emails inside one minute, all first
//! notices, because a batch of approvals landing in a single cycle queued a
//! mail per new assignment. The reminder path already digests; the initial
//! send did not, and the initial send is the louder half — reminders arrive
//! on a ladder, approvals land in bursts. A mailbox trained on fifteen-at-once
//! learns to ignore VIRYA entirely, which costs more than every one of those
//! approvals was worth.
//!
//! The property is the sweep's, not a formatter's: it comes from holding the
//! notices inside one transaction until every producer has offered its asks,
//! so only a database can show it.

mod common;

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn five_new_approvals_are_one_email() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let url = std::env::var("CROWDRELAY_TEST_DATABASE_URL").expect("suite database url");

    run(&database, &url).await
}

async fn run(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let workspace = workspace(pool).await?;
    advertise_team_email(pool, workspace, now).await?;
    let alice = member(pool, workspace, "alice").await?;

    // Five approvals of one kind landing in the same cycle — the reported
    // inbox. Different deadlines so the earliest-due one leads the digest.
    for index in 0..5_i64 {
        awaiting_approval(pool, workspace, now, index).await?;
    }
    // A published show a day out adds its checklist asks to the same
    // member — the digest has to fold producers, not just approvals.
    published_show(pool, workspace, now).await?;

    let database = DatabaseConfig {
        url: url.to_owned(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let assigned = repository
        .reconcile_team_handoffs(WorkspaceId::from_uuid(workspace), now)
        .await?;
    assert!(assigned >= 12, "the handoffs never got an owner");

    // Every approval got its own assignment — batching the mail changes
    // interruptions, not the work index.
    let assignments = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM team_assignments
         WHERE workspace_id = $1 AND source_kind = 'autopilot_action'
           AND status = 'open' AND next_reminder_at IS NOT NULL
           AND assignee_member_id = $2",
    )
    .bind(workspace)
    .bind(alice)
    .fetch_one(pool)
    .await?;
    assert_eq!(assignments, 5, "an approval went unassigned");

    // One email action for the handoffs — the digest — not five.
    let emails = sqlx::query_as::<_, (String, String)>(
        "SELECT payload->>'task_title', payload->>'task_detail'
         FROM autopilot_actions action
         JOIN team_assignments assignment
           ON assignment.workspace_id = action.workspace_id
          AND assignment.id = (action.payload->>'assignment_id')::uuid
         WHERE action.workspace_id = $1
           AND action.action_kind = 'team.assignment.email'
           AND assignment.source_kind = 'autopilot_action'",
    )
    .bind(workspace)
    .fetch_all(pool)
    .await?;
    assert_eq!(emails.len(), 1, "the first notices did not coalesce");
    let (title, detail) = &emails[0];
    assert_eq!(
        title, "Zatwierdź publikację w społeczności",
        "the digest did not lead with the approval's own title"
    );
    // The earliest-due notice leads: approval 0 expires in seven hours,
    // every show task waits for doors minus two.
    assert!(
        detail.contains("Post 0"),
        "the earliest-due task did not lead the digest: {detail}"
    );
    // Twelve notices — five approvals plus the seven checklist asks the
    // sweep's own door-campaign mint makes live — one primary, eleven
    // named in the tail, from two producers, not one.
    assert!(
        detail.contains("jeszcze 11 zadania"),
        "the digest did not name the other eleven tasks: {detail}"
    );
    let named = detail
        .matches("• Zatwierdź publikację w społeczności")
        .count();
    assert_eq!(named, 4, "the tail did not name each folded task: {detail}");
    assert!(
        detail.contains("• Potwierdź obsadę koncertu"),
        "the tail did not fold the show tasks in: {detail}"
    );

    // A second sweep at the same instant sends nothing more — every approval
    // already owns an assignment, so none re-enters the notice batch.
    let again = repository
        .reconcile_team_handoffs(WorkspaceId::from_uuid(workspace), now)
        .await?;
    let email_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM autopilot_actions action
         JOIN team_assignments assignment
           ON assignment.workspace_id = action.workspace_id
          AND assignment.id = (action.payload->>'assignment_id')::uuid
         WHERE action.workspace_id = $1
           AND action.action_kind = 'team.assignment.email'
           AND assignment.source_kind = 'autopilot_action'",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(email_count, 1, "the same sweep mailed a second time");
    let _ = again;
    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Crew test')")
        .bind(id)
        .bind(format!("crew-init-{}", id.simple()))
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'crew_locale', 'pl')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    // The fixture's member takes twelve handoffs; the workspace-wide weekly
    // ask ceiling defaults to ten and would cap the sweep before the digest
    // assertions ever run. The ceiling is its own tested surface — here it
    // only needs to not interfere.
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'team_weekly_ask_ceiling', '50')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn member(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let member_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
         VALUES ($1, $2, $3, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("{key}-{}@example.test", workspace_id.simple()))
    .bind(format!("Crew {key}"))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, $3, true, ARRAY['approval','operations','social']::text[])",
    )
    .bind(workspace_id)
    .bind(member_id)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(member_id)
}

/// Without an executor advertising `team.email` the sweep queues nothing and
/// the test would pass for the wrong reason.
async fn advertise_team_email(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-crew-test','test','test-manifest',$2,$3)"#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-crew-test','team.email','1',$2,$3)"#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

/// One unanswered approval — the handoff sweep's raw material. `context`
/// mirrors production (`growth_intelligence` raised the flood's batch) and
/// `approval_expires_at` staggers so the digest has an order to pick.
async fn awaiting_approval(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    index: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                 'request_community_engagement',9000,'require_approval','seeded approval',
                 '{}','{}','{}',$5,$6) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("digest-decision-{index}-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(now)
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, approval_expires_at)
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request','target_community',
                 $4,$5,$6,'awaiting_approval',$7)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("digest-action-{index}-{}", Uuid::now_v7()))
    .bind(serde_json::json!({
        "kind": "request_community_engagement",
        "target_id": Uuid::now_v7(),
        "platform": "reddit",
        "subreddit": format!("r/test{index}"),
        "title": format!("Post {index}"),
        "body": "seeded",
        "smart_link": null,
    }))
    // Seven hours out on the nearest one: inside six the ask is already on
    // its last rung, and `first_reminder_at` honestly schedules nothing —
    // this fixture wants ladders, not the boundary.
    .bind(now + time::Duration::hours(7 + index * 12))
    .execute(pool)
    .await?;
    Ok(())
}

/// A show a day out: every checklist ask lands on the same member the
/// approvals do — the sweep mints the door campaign before it lists tasks,
/// so the QR ask fires too; only the post-show one stays quiet (the doors
/// have not closed).
async fn published_show(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,'Crew digest show',$4,'published',now())",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("crew-digest-show-{}", Uuid::now_v7().simple()))
    .bind(now + time::Duration::days(1))
    .execute(pool)
    .await?;
    Ok(())
}
