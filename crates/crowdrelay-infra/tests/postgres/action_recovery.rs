//! Recovery must preserve one action identity without trusting an old worker.

use crate::common;
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::outward_link_gate::{
    advertise, city, emitted_for, outreach_action, repository, target, workspace,
};

async fn fixture() -> Result<(PgPool, String, WorkspaceId, Uuid), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("recovery-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    // The clock advances through all five retries in these tests.
    for table in ["executor_instances", "executor_capabilities"] {
        sqlx::query(&format!(
            "UPDATE {table} SET expires_at = now() + INTERVAL '2 days' WHERE workspace_id = $1"
        ))
        .bind(workspace_id)
        .execute(&pool)
        .await?;
    }
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "An approved letter without a URL.",
    )
    .await?;
    Ok((pool, url, WorkspaceId::from_uuid(workspace_id), action_id))
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn recovered_claim_fences_old_execution_and_failure() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, url, workspace_id, action_id) = fixture().await?;
    let repo = repository(&pool, &url);
    let now = OffsetDateTime::now_utc();
    let first = repo
        .claim_due_actions(workspace_id, 1, now)
        .await?
        .remove(0);
    let recovered_at = now + time::Duration::minutes(16);
    let second = repo
        .claim_due_actions(workspace_id, 1, recovered_at)
        .await?
        .remove(0);
    assert_eq!(first.id, second.id);
    assert_eq!(second.attempt_number, 2);
    assert!(matches!(
        repo.execute_action(workspace_id, &first, recovered_at)
            .await,
        Err(RepositoryError::Conflict)
    ));
    repo.fail_action(
        workspace_id,
        first.id,
        first.attempt_number,
        "late_failure",
        false,
        recovered_at,
    )
    .await?;
    assert_eq!(
        emitted_for(&pool, workspace_id.into_uuid(), action_id).await,
        0
    );
    let state: (String, i32) = sqlx::query_as(
        "SELECT status, attempt_count FROM autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, ("processing".to_owned(), 2));
    let interrupted: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt_number, error_kind FROM autopilot_action_attempts
         WHERE workspace_id=$1 AND action_id=$2 AND outcome='failed'",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(interrupted, vec![(1, "stale_claim_recovered".to_owned())]);

    // A different tenant cannot execute or fail even the current attempt.
    let foreign = WorkspaceId::from_uuid(workspace(&pool).await?);
    assert!(matches!(
        repo.execute_action(foreign, &second, recovered_at).await,
        Err(RepositoryError::Conflict)
    ));
    repo.fail_action(
        foreign,
        second.id,
        second.attempt_number,
        "foreign_failure",
        false,
        recovered_at,
    )
    .await?;
    repo.execute_action(workspace_id, &second, recovered_at)
        .await?;
    assert!(matches!(
        repo.execute_action(workspace_id, &second, recovered_at)
            .await,
        Err(RepositoryError::Conflict)
    ));
    repo.fail_action(
        workspace_id,
        second.id,
        second.attempt_number,
        "late_failure",
        false,
        recovered_at,
    )
    .await?;
    assert_eq!(
        emitted_for(&pool, workspace_id.into_uuid(), action_id).await,
        1
    );
    let terminals: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt_number, outcome FROM autopilot_action_attempts
         WHERE workspace_id=$1 AND action_id=$2 AND outcome IN ('failed','succeeded')
         ORDER BY attempt_number",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        terminals,
        vec![(1, "failed".to_owned()), (2, "succeeded".to_owned())]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn retry_backoff_is_bounded_and_terminal_failures_stay_terminal()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url, workspace_id, action_id) = fixture().await?;
    let repo = repository(&pool, &url);
    // Postgres stores timestamptz at microsecond precision, so a nanosecond
    // clock reading never round-trips equal against a stored available_at —
    // and the rounded second has to sit after the fixture's own now().
    let mut now = OffsetDateTime::now_utc().replace_nanosecond(0)? + time::Duration::seconds(1);
    for (index, delay) in [5_i64, 10, 20, 40, 0].into_iter().enumerate() {
        let action = repo
            .claim_due_actions(workspace_id, 1, now)
            .await?
            .remove(0);
        assert_eq!(action.attempt_number, u32::try_from(index + 1)?);
        repo.fail_action(
            workspace_id,
            action.id,
            action.attempt_number,
            "repository_unavailable",
            true,
            now,
        )
        .await?;
        // A duplicate failure cannot move the due time or append another terminal.
        repo.fail_action(
            workspace_id,
            action.id,
            action.attempt_number,
            "duplicate",
            true,
            now,
        )
        .await?;
        let (status, due, error): (String, OffsetDateTime, String) = sqlx::query_as(
            "SELECT status, available_at, last_error_kind FROM autopilot_actions
             WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(error, "repository_unavailable");
        if delay > 0 {
            assert_eq!(status, "queued");
            assert_eq!(due, now + time::Duration::minutes(delay));
            assert!(
                repo.claim_due_actions(workspace_id, 1, due - time::Duration::seconds(1))
                    .await?
                    .is_empty()
            );
            now = due;
        } else {
            assert_eq!(status, "failed");
        }
    }
    assert!(
        repo.claim_due_actions(workspace_id, 1, now + time::Duration::days(1))
            .await?
            .is_empty()
    );
    let failed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM autopilot_action_attempts
         WHERE workspace_id=$1 AND action_id=$2 AND outcome='failed'",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(failed, 5);
    assert_eq!(
        emitted_for(&pool, workspace_id.into_uuid(), action_id).await,
        0
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn repeated_worker_loss_closes_every_attempt_and_exhausts_the_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url, workspace_id, action_id) = fixture().await?;
    let repo = repository(&pool, &url);
    let mut now = OffsetDateTime::now_utc();
    for attempt in 1..=5 {
        let action = repo
            .claim_due_actions(workspace_id, 1, now)
            .await?
            .remove(0);
        assert_eq!(action.attempt_number, attempt);
        assert!(
            repo.claim_due_actions(workspace_id, 1, now)
                .await?
                .is_empty()
        );
        now += time::Duration::minutes(16);
    }
    assert!(
        repo.claim_due_actions(workspace_id, 1, now)
            .await?
            .is_empty()
    );
    assert!(
        repo.claim_due_actions(workspace_id, 1, now)
            .await?
            .is_empty()
    );
    let state: (String, String) = sqlx::query_as(
        "SELECT status, last_error_kind FROM autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        state,
        ("failed".to_owned(), "stale_retry_exhausted".to_owned())
    );
    let terminals: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt_number, error_kind FROM autopilot_action_attempts
         WHERE workspace_id=$1 AND action_id=$2 AND outcome='failed' ORDER BY attempt_number",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(terminals.len(), 5);
    for (index, (attempt, error)) in terminals.iter().enumerate() {
        assert_eq!(*attempt, i32::try_from(index + 1)?);
        assert_eq!(
            error,
            if index == 4 {
                "stale_retry_exhausted"
            } else {
                "stale_claim_recovered"
            }
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn permanent_refusal_is_not_requeued() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, url, workspace_id, action_id) = fixture().await?;
    let repo = repository(&pool, &url);
    let now = OffsetDateTime::now_utc();
    let action = repo
        .claim_due_actions(workspace_id, 1, now)
        .await?
        .remove(0);
    repo.fail_action(
        workspace_id,
        action.id,
        action.attempt_number,
        "state_changed",
        false,
        now,
    )
    .await?;
    assert!(
        repo.claim_due_actions(workspace_id, 1, now + time::Duration::days(1))
            .await?
            .is_empty()
    );
    assert!(matches!(
        repo.execute_action(workspace_id, &action, now).await,
        Err(RepositoryError::Conflict)
    ));
    assert_eq!(
        emitted_for(&pool, workspace_id.into_uuid(), action_id).await,
        0
    );
    Ok(())
}
