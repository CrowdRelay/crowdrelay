//! Live-Postgres coverage for the content-arc lifecycle inside the approval
//! queue: an approved ask commits the season, a cancelled one retires it as
//! the band's "not this shape", and an unanswered one retires with it so the
//! open-arc slot can never leak.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotControlRepository};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Control plane E2E")
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

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::parse(format!("control-plane-e2e-key-{seed:>03}"))
        .expect("valid idempotency key")
}

/// One proposed arc plus its queue rows — decision, then the ask itself.
async fn seed_arc_ask(
    fixture: &Fixture,
    decision_key: &str,
    action_key: &str,
    status: &str,
) -> Result<(Uuid, Uuid, Uuid), Box<dyn std::error::Error>> {
    let arc_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO arcs (
             id, workspace_id, title, summary, horizon_start, horizon_end,
             spine, evidence, status
         ) VALUES ($1,$2,'Arc Single: release','a 3-beat run',
                   CURRENT_DATE + 1, CURRENT_DATE + 37,
                   '[{\"at\":\"2030-01-01\",\"beat\":\"playthrough\",\"format_key\":\"playthrough\"}]'::jsonb,
                   '{\"anchor\":{\"kind\":\"release\",\"id\":\"rel-e2e\"}}'::jsonb,
                   'proposed')",
    )
    .bind(arc_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'content_strategy','content_arc',$4,
                   'raise_content_arc',9000,'require_approval','e2e','{}','{}','{}',$1)",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_key)
    .bind(arc_id)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at, approved_by, approval_expires_at
         ) VALUES ($1,$2,$3,'content_strategy','content.arc.raise','content_arc',$4,
                   $5,$6,$7, now(), 'operator-e2e', now() + interval '7 days')",
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(arc_id)
    .bind(action_key)
    .bind(serde_json::json!({
        "kind": "raise_content_arc",
        "arc_id": arc_id,
        "title": "Arc Single: release",
        "summary": "a 3-beat run",
        "horizon_start": "2030-01-01",
        "horizon_end": "2030-02-07",
        "beats": 3,
    }))
    .bind(status)
    .execute(&fixture.pool)
    .await?;
    Ok((arc_id, decision_id, action_id))
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn executing_an_approved_arc_commits_the_season() {
    let fixture = fixture("arc-approve").await.expect("fixture");
    let (arc_id, _decision_id, action_id) =
        seed_arc_ask(&fixture, "e2e-arc-approve", "e2e-arc-approve-1", "queued")
            .await
            .expect("arc ask");

    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("claim");
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the approved arc action must be claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await
        .expect("execution");

    let (status, approved_by): (String, Option<String>) =
        sqlx::query_as("SELECT status, approved_by FROM arcs WHERE id = $1")
            .bind(arc_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("arc status");
    assert_eq!(status, "approved", "the band's yes commits the season");
    assert_eq!(
        approved_by.as_deref(),
        Some("operator-e2e"),
        "the approver's name rides the join from the action row"
    );

    // A second queue entry for the same arc — the dedup hole — finds the
    // row already approved: the same answer, not a failure.
    let replay_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($1,$2,$3,'content_strategy','content.arc.raise','content_arc',$4,
                   'e2e-arc-approve-replay',$5,'queued', now())",
    )
    .bind(replay_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(
        sqlx::query_scalar::<_, Uuid>("SELECT decision_id FROM autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("decision id"),
    )
    .bind(arc_id)
    .bind(serde_json::json!({
        "kind": "raise_content_arc",
        "arc_id": arc_id,
        "title": "Arc Single: release",
        "summary": "a 3-beat run",
        "horizon_start": "2030-01-01",
        "horizon_end": "2030-02-07",
        "beats": 3,
    }))
    .execute(&fixture.pool)
    .await
    .expect("replay action");
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("second claim");
    let replay = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == replay_id)
        .expect("the replay must be claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, replay, fixture.now)
        .await
        .expect("replay execution answers the same yes");
    let status: String = sqlx::query_scalar("SELECT status FROM arcs WHERE id = $1")
        .bind(arc_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("arc status");
    assert_eq!(status, "approved");
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn cancelling_an_arc_ask_retires_the_season() {
    let fixture = fixture("arc-cancel").await.expect("fixture");
    let (arc_id, _decision_id, action_id) = seed_arc_ask(
        &fixture,
        "e2e-arc-cancel",
        "e2e-arc-cancel-1",
        "awaiting_approval",
    )
    .await
    .expect("arc ask");

    let mutation = fixture
        .repository
        .cancel_action(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotActionId::from_uuid(action_id),
            &key(70),
            None,
        )
        .await
        .expect("cancel");
    assert_eq!(mutation.status, "cancelled");

    // "Not this season" retires the proposal — the open-arc slot frees and
    // the anchor's cooldown remembers the answer.
    let status: String = sqlx::query_scalar("SELECT status FROM arcs WHERE id = $1")
        .bind(arc_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("arc status");
    assert_eq!(status, "retired");

    let second = fixture
        .repository
        .cancel_action(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotActionId::from_uuid(action_id),
            &key(71),
            None,
        )
        .await;
    assert!(
        matches!(second, Err(RepositoryError::ConflictBecause(_))),
        "a cancelled ask must conflict, not re-resolve, got {second:?}"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unanswered_arc_ask_retires_the_proposal_with_it() {
    let fixture = fixture("arc-expire").await.expect("fixture");
    let (arc_id, _decision_id, _action_id) = seed_arc_ask(
        &fixture,
        "e2e-arc-expire",
        "e2e-arc-expire-1",
        "awaiting_approval",
    )
    .await
    .expect("arc ask");
    // The approval window already closed.
    sqlx::query(
        "UPDATE autopilot_actions
         SET approval_expires_at = now() - interval '1 hour'
         WHERE subject_id = $1 AND subject_kind = 'content_arc'",
    )
    .bind(arc_id)
    .execute(&fixture.pool)
    .await
    .expect("lapse the window");

    // The claim sweep reaps the dead ask — and the proposal goes with it.
    fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("claim");
    let status: String = sqlx::query_scalar("SELECT status FROM arcs WHERE id = $1")
        .bind(arc_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("arc status");
    assert_eq!(
        status, "retired",
        "an ask nobody answered cannot hold the season slot forever"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failed_arc_ask_retires_the_proposal_with_it() {
    let fixture = fixture("arc-failed").await.expect("fixture");
    // The ask already died: queued, claimed, and burned through its
    // attempts. The band never saw the question and the idempotency key is
    // spent — the proposal must not hold the season slot behind it.
    let (arc_id, _decision_id, _action_id) =
        seed_arc_ask(&fixture, "e2e-arc-failed", "e2e-arc-failed-1", "processing")
            .await
            .expect("arc ask");
    sqlx::query(
        "UPDATE autopilot_actions
         SET status = 'failed', finished_at = now(),
             last_error_kind = 'stale_retry_exhausted', attempt_count = 5
         WHERE subject_id = $1 AND subject_kind = 'content_arc'",
    )
    .bind(arc_id)
    .execute(&fixture.pool)
    .await
    .expect("fail the ask");

    fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("claim");
    let status: String = sqlx::query_scalar("SELECT status FROM arcs WHERE id = $1")
        .bind(arc_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("arc status");
    assert_eq!(
        status, "retired",
        "a dead ask frees the season — it does not strand it for a quarter"
    );
}
