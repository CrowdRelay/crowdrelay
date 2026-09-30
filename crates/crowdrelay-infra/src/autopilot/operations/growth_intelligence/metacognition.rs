//! One bounded checkpoint per workspace; previews never call the writer.

use super::*;
use crowdrelay_brain::self_assessment::checkpoint::{
    MetacognitionCheckpoint, MetacognitionObservation,
};

pub(super) async fn load(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> MetacognitionCheckpoint {
    let loaded = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        super::super::evidence::load_brain_state(repo, workspace_id, "metacognition"),
    )
    .await;
    match loaded {
        Ok(Ok(Some((value, _)))) => match serde_json::from_value(value) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                tracing::warn!(%error, "metacognition checkpoint unreadable; using a fresh projection, preserving stored state");
                MetacognitionCheckpoint::empty()
            }
        },
        Ok(Ok(None)) => MetacognitionCheckpoint::empty(),
        Ok(Err(error)) => {
            // This is advisory exploration continuity, not authority or the
            // outcome posterior. Its outage must not stop eligible outreach.
            tracing::warn!(%error, "metacognition checkpoint unavailable; using a fresh projection");
            MetacognitionCheckpoint::empty()
        }
        Err(_) => {
            tracing::warn!("metacognition checkpoint read timed out; using a fresh projection");
            MetacognitionCheckpoint::empty()
        }
    }
}

pub(in crate::autopilot) async fn save(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    value: &serde_json::Value,
) -> Result<(), RepositoryError> {
    let observation: MetacognitionObservation =
        serde_json::from_value(value.clone()).map_err(|_| RepositoryError::Unexpected)?;
    let empty = serde_json::to_value(MetacognitionCheckpoint::empty())
        .map_err(|_| RepositoryError::Unexpected)?;
    let mut tx = repo.pool.begin().await.map_err(map_sqlx)?;
    // A contended advisory checkpoint must not hang a growth evaluation.
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
    sqlx::query(
        "INSERT INTO brain_state (workspace_id, module, state) \
         VALUES ($1, 'metacognition', $2) ON CONFLICT (workspace_id, module) DO NOTHING",
    )
    .bind(workspace_id.into_uuid())
    .bind(empty)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx)?;
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM brain_state \
         WHERE workspace_id=$1 AND module='metacognition' FOR UPDATE",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx)?;
    // Never overwrite a checkpoint we cannot understand. Unlike snapshot
    // projection, persistence cannot safely turn corrupted history into zero.
    let checkpoint: MetacognitionCheckpoint =
        serde_json::from_value(stored).map_err(|_| RepositoryError::Unexpected)?;
    if observation.observed_at_micros > checkpoint.observed_at_micros {
        let next = serde_json::to_value(checkpoint.advance(&observation))
            .map_err(|_| RepositoryError::Unexpected)?;
        sqlx::query(
            "UPDATE brain_state SET state=$2, updated_at=now() \
             WHERE workspace_id=$1 AND module='metacognition'",
        )
        .bind(workspace_id.into_uuid())
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
    }
    tx.commit().await.map_err(map_sqlx)?;
    Ok(())
}
