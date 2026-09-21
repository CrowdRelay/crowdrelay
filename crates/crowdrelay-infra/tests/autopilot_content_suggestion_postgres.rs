//! Live-Postgres coverage for the content-suggestion lifecycle inside the
//! approval queue: an approved ask commits the beat, a cancelled one records
//! the band's taste, and an unanswered one expires the suggestion with it.

use std::time::Duration;

use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotControlRepository};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
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
    // Deterministic per seed: a replay must present the same key as the
    // original request, which a freshly generated one can never do.
    IdempotencyKey::parse(format!("control-plane-e2e-key-{seed:>03}"))
        .expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn executing_an_approved_suggestion_commits_the_beat() {
    let fixture = fixture("suggestion-approve").await.expect("fixture");
    let suggestion_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_suggestions (
             id, workspace_id, format_key, concept, reason, evidence,
             distribution_promise, status
         ) VALUES ($1,$2,'playthrough','Playthrough','e2e reason',
                   '{\"efe_score\":0.4}','{\"consented_fans\":12}','raised')",
    )
    .bind(suggestion_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("suggestion");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,'e2e-suggestion-approve','content_strategy','content_suggestion',$3,
                   'raise_content_suggestion',9000,'require_approval','e2e','{}','{}','{}',$1)",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(suggestion_id)
    .execute(&fixture.pool)
    .await
    .expect("decision");
    let action_id = Uuid::now_v7();
    let payload = serde_json::json!({
        "kind": "raise_content_suggestion",
        "suggestion_id": suggestion_id,
        "format_key": "playthrough",
        "concept": "Playthrough",
        "reason": "e2e reason",
        "distribution_promise": {"consented_fans": 12},
    });
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-approve-1',$4,'queued', now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(&payload)
    .bind(action_id)
    .execute(&fixture.pool)
    .await
    .expect("queued action");

    // The production path: the claim moves queued → processing, then
    // `execute_action` runs it. The action needs no external executor — the
    // approval is a first-party write.
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("claim");
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the approved suggestion action must be claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await
        .expect("execution");

    let status: String = sqlx::query_scalar("SELECT status FROM content_suggestions WHERE id = $1")
        .bind(suggestion_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("suggestion status");
    assert_eq!(
        status, "approved",
        "approving the queue entry commits the band to the beat"
    );

    // A second queue entry for the same suggestion — the dedup hole this arm
    // exists for — finds the row already approved: the same answer, not a
    // failure.
    let replay_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-approve-2',$4,'queued', now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(&payload)
    .bind(replay_id)
    .execute(&fixture.pool)
    .await
    .expect("replay action");
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("re-claim");
    let replay = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == replay_id)
        .expect("the duplicate entry is still claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, replay, fixture.now)
        .await
        .expect("replay is idempotent");

    // And a suggestion that resolved itself in the meantime cannot be
    // resurrected by a stale queue entry.
    sqlx::query("UPDATE content_suggestions SET status = 'declined' WHERE id = $1")
        .bind(suggestion_id)
        .execute(&fixture.pool)
        .await
        .expect("resolve");
    let stale_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-approve-3',$4,'queued', now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(&payload)
    .bind(stale_id)
    .execute(&fixture.pool)
    .await
    .expect("stale action");
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("stale claim");
    let stale = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == stale_id)
        .expect("the stale entry is claimable");
    let outcome = fixture
        .repository
        .execute_action(fixture.workspace_id, stale, fixture.now)
        .await;
    assert!(
        matches!(outcome, Err(RepositoryError::ConflictBecause(_))),
        "a terminal suggestion must conflict rather than reopen, got {outcome:?}"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn cancelling_a_suggestion_ask_records_the_taste_signal() {
    let fixture = fixture("suggestion-cancel").await.expect("fixture");
    let suggestion_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_suggestions (
             id, workspace_id, format_key, concept, reason, evidence,
             distribution_promise, status
         ) VALUES ($1,$2,'playthrough','Playthrough','e2e reason',
                   '{\"efe_score\":0.4}','{\"consented_fans\":12}','raised')",
    )
    .bind(suggestion_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("suggestion");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,'e2e-suggestion-cancel','content_strategy','content_suggestion',$3,
                   'raise_content_suggestion',9000,'require_approval','e2e','{}','{}','{}',$1)",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(suggestion_id)
    .execute(&fixture.pool)
    .await
    .expect("decision");
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approval_expires_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-cancel-1',$4,'awaiting_approval', now() + interval '7 days')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(serde_json::json!({
        "kind": "raise_content_suggestion",
        "suggestion_id": suggestion_id,
        "format_key": "playthrough",
        "concept": "Playthrough",
        "reason": "e2e reason",
        "distribution_promise": {"consented_fans": 12},
    }))
    .bind(action_id)
    .execute(&fixture.pool)
    .await
    .expect("awaiting action");

    let mutation = fixture
        .repository
        .cancel_action(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotActionId::from_uuid(action_id),
            &key(60),
            None,
        )
        .await
        .expect("cancel");
    assert_eq!(mutation.status, "cancelled");

    // "Not for us" is a first-class signal: the suggestion resolved declined
    // and the outcome row carries it — the open queue's headroom is freed and
    // the engine can learn what the band refuses.
    let status: String = sqlx::query_scalar("SELECT status FROM content_suggestions WHERE id = $1")
        .bind(suggestion_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("suggestion status");
    assert_eq!(status, "declined");

    let (outcome, decided_by): (String, String) = sqlx::query_as(
        "SELECT outcome, decided_by FROM suggestion_outcomes
         WHERE workspace_id = $1 AND suggestion_id = $2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(suggestion_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("outcome row");
    assert_eq!(outcome, "declined");
    assert_eq!(decided_by, "operator:admin_api_key");

    // A second cancel on an already-resolved suggestion conflicts on the
    // action's own state machine and writes no second verdict.
    let second = fixture
        .repository
        .cancel_action(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotActionId::from_uuid(action_id),
            &key(61),
            None,
        )
        .await;
    assert!(
        matches!(second, Err(RepositoryError::ConflictBecause(_))),
        "a cancelled ask must conflict, not re-resolve, got {second:?}"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM suggestion_outcomes WHERE suggestion_id = $1")
            .bind(suggestion_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("outcome count");
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unanswered_ask_expires_the_suggestion_with_it() {
    let fixture = fixture("suggestion-expire").await.expect("fixture");
    let suggestion_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_suggestions (
             id, workspace_id, format_key, concept, reason, evidence,
             distribution_promise, status
         ) VALUES ($1,$2,'playthrough','Playthrough','e2e reason',
                   '{\"efe_score\":0.4}','{\"consented_fans\":12}','raised')",
    )
    .bind(suggestion_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("suggestion");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,'e2e-suggestion-expire','content_strategy','content_suggestion',$3,
                   'raise_content_suggestion',9000,'require_approval','e2e','{}','{}','{}',$1)",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(suggestion_id)
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approval_expires_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-expire-1',$4,'awaiting_approval', now() - interval '1 hour')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(serde_json::json!({
        "kind": "raise_content_suggestion",
        "suggestion_id": suggestion_id,
        "format_key": "playthrough",
        "concept": "Playthrough",
        "reason": "e2e reason",
        "distribution_promise": {"consented_fans": 12},
    }))
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("lapsed ask");

    // The claim sweep reaps the unanswered ask — and the suggestion it asked
    // about. Without the pair the row would hold an open-queue slot forever,
    // invisible to the evaluator and unresolvable by anyone.
    fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await
        .expect("claim sweep");

    let (status, outcome): (String, String) = sqlx::query_as(
        "SELECT s.status,
                (SELECT o.outcome FROM suggestion_outcomes o
                  WHERE o.suggestion_id = s.id) AS outcome
         FROM content_suggestions s WHERE s.id = $1",
    )
    .bind(suggestion_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("suggestion row");
    assert_eq!(
        (status.as_str(), outcome.as_str()),
        ("expired", "expired"),
        "the window closing unanswered resolves the question, not only the ask"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn doing_the_beat_ourselves_counts_as_done() {
    let fixture = fixture("suggestion-handled").await.expect("fixture");
    let suggestion_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_suggestions (
             id, workspace_id, format_key, concept, reason, evidence,
             distribution_promise, status
         ) VALUES ($1,$2,'playthrough','Playthrough','e2e reason',
                   '{\"efe_score\":0.4}','{\"consented_fans\":12}','raised')",
    )
    .bind(suggestion_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("suggestion");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,'e2e-suggestion-handled','content_strategy','content_suggestion',$3,
                   'raise_content_suggestion',9000,'require_approval','e2e','{}','{}','{}',$1)",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(suggestion_id)
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approval_expires_at
         ) VALUES ($5,$1,$2,'content_strategy','content.suggestion.raise','content_suggestion',$3,
                   'e2e-suggestion-handled-1',$4,'awaiting_approval', now() + interval '7 days')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(suggestion_id)
    .bind(serde_json::json!({
        "kind": "raise_content_suggestion",
        "suggestion_id": suggestion_id,
        "format_key": "playthrough",
        "concept": "Playthrough",
        "reason": "e2e reason",
        "distribution_promise": {"consented_fans": 12},
    }))
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("parked ask");

    fixture
        .repository
        .mark_decision_handled_externally(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotDecisionId::from_uuid(decision_id),
            &key(70),
            None,
        )
        .await
        .expect("handled");

    // The band made the beat themselves — the best answer the engine can
    // get. The row resolves `done` with its outcome, not cancelled-and-lost.
    let (status, outcome): (String, String) = sqlx::query_as(
        "SELECT s.status,
                (SELECT o.outcome FROM suggestion_outcomes o
                  WHERE o.suggestion_id = s.id) AS outcome
         FROM content_suggestions s WHERE s.id = $1",
    )
    .bind(suggestion_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("suggestion row");
    assert_eq!(
        (status.as_str(), outcome.as_str()),
        ("done", "done"),
        "a suggestion the band made themselves is a success, not a dismissal"
    );
}
