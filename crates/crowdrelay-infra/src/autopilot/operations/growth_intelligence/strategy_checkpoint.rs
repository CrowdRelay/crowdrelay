//! Strategy learning owns its cursor; another model's save cannot consume it.

use super::super::super::*;
use super::evidence_replay::{
    apply_evidence_to_strategy_posterior, record_strategy_posterior_revisions, strategy_observation,
};

const CURSOR_KEY: &str = "_strategy_observation_cursor_micros";

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PosteriorReplay {
    Delta,
    FromScratch,
}

fn decode_cursor(
    value: &serde_json::Value,
    legacy_written_at: OffsetDateTime,
) -> Result<OffsetDateTime, RepositoryError> {
    match value.get(CURSOR_KEY) {
        None => Ok(legacy_written_at),
        Some(value) => value
            .as_i64()
            .and_then(|micros| {
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000).ok()
            })
            .ok_or(RepositoryError::Unexpected),
    }
}

fn observation_time(ev: &crowdrelay_brain::GrowthEvidence) -> Option<OffsetDateTime> {
    let horizon = if ev.observed_incremental_fans.is_some() {
        ev.replayed_14d_at
    } else {
        ev.replayed_3d_at
    };
    horizon.or(ev.resolved_at)
}

pub(super) async fn apply_evidence_to_stored_strategy_posterior(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    evidence: &[crowdrelay_brain::GrowthEvidence],
    replay: PosteriorReplay,
    _causal_checkpoint: Option<OffsetDateTime>,
) {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        refresh(repo, workspace_id, evidence, replay),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "strategy checkpoint failed; its evidence cursor is unchanged and learning will retry")
        }
        Err(_) => {
            tracing::warn!("strategy checkpoint timed out; learning will retry from its own cursor")
        }
    }
}

async fn refresh(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    full_evidence: &[crowdrelay_brain::GrowthEvidence],
    replay: PosteriorReplay,
) -> Result<(), RepositoryError> {
    use crowdrelay_brain::StateConditionedStrategyPosterior;
    // Serialize writers without blocking dispatch when another cycle owns the
    // learner. The compact state and cursor are saved together in one JSON row.
    let mut guard = repo.pool.begin().await.map_err(map_sqlx)?;
    let owns: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1::uuid::text, \
         hashtextextended('crowdrelay:strategy-checkpoint',0)))",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut *guard)
    .await
    .map_err(map_sqlx)?;
    if !owns {
        return Ok(());
    }

    let saved: Option<(serde_json::Value, OffsetDateTime)> = sqlx::query_as(
        "SELECT state, updated_at FROM brain_state \
         WHERE workspace_id=$1 AND module='strategy_posterior'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut *guard)
    .await
    .map_err(map_sqlx)?;
    let (before, own_cursor) = match saved {
        Some((state, written_at)) => {
            let cursor = decode_cursor(&state, written_at)?;
            let posterior = serde_json::from_value::<StateConditionedStrategyPosterior>(state)
                .map_err(|_| RepositoryError::Unexpected)?;
            (posterior, Some(cursor))
        }
        None => (StateConditionedStrategyPosterior::default(), None),
    };
    // Always read a delta from the strategy's own cursor. A causal-model
    // checkpoint may be newer after a failed strategy save, or older after a
    // failed causal save. Neither case can drop or duplicate strategy learning.
    let delta;
    let (rows, cursor, read_cursor, mut after) = match replay {
        PosteriorReplay::Delta => {
            delta = super::super::evidence::load_growth_evidence_on(
                &mut guard,
                workspace_id,
                own_cursor,
            )
            .await?;
            (delta.0.as_slice(), own_cursor, delta.1, before.clone())
        }
        PosteriorReplay::FromScratch => (
            full_evidence,
            None,
            full_evidence
                .iter()
                .flat_map(|ev| {
                    [
                        ev.resolved_at,
                        ev.replayed_3d_at,
                        ev.replayed_14d_at,
                        ev.replayed_30d_at,
                    ]
                    .into_iter()
                    .flatten()
                })
                .max(),
            StateConditionedStrategyPosterior::default(),
        ),
    };
    let accepted: Vec<_> = rows
        .iter()
        .filter(|ev| {
            ev.outcome_basis.teaches_per_action()
                && observation_time(ev).is_some()
                && strategy_observation(ev, cursor).is_some()
        })
        .cloned()
        .collect();
    // Consume newly read horizons even when they do not update this posterior
    // (e.g. Y30 after Y14). Otherwise those rows stay in every future delta.
    // Gate strategy observations against the previous cursor, then advance the
    // scan watermark; a no-op horizon cannot manufacture posterior confidence.
    let learned_through = read_cursor
        .into_iter()
        .chain(cursor)
        .max()
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    if accepted.is_empty()
        && replay == PosteriorReplay::Delta
        && learned_through <= cursor.unwrap_or(OffsetDateTime::UNIX_EPOCH)
    {
        return Ok(());
    }
    apply_evidence_to_strategy_posterior(&mut after, &accepted, cursor);
    let mut state = serde_json::to_value(&after).map_err(|_| RepositoryError::Unexpected)?;
    state
        .as_object_mut()
        .ok_or(RepositoryError::Unexpected)?
        .insert(
            CURSOR_KEY.into(),
            serde_json::json!(
                learned_through.unix_timestamp() * 1_000_000
                    + i64::from(learned_through.microsecond())
            ),
        );
    sqlx::query(
        "INSERT INTO brain_state (workspace_id,module,state,updated_at) \
         VALUES ($1,'strategy_posterior',$2,now()) \
         ON CONFLICT (workspace_id,module) DO UPDATE \
         SET state=EXCLUDED.state,updated_at=EXCLUDED.updated_at",
    )
    .bind(workspace_id.into_uuid())
    .bind(state)
    .execute(&mut *guard)
    .await
    .map_err(map_sqlx)?;
    guard.commit().await.map_err(map_sqlx)?;
    record_strategy_posterior_revisions(repo, workspace_id, &before, &after, &accepted, cursor)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_cursor_survives_a_later_causal_checkpoint() {
        let early = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let late = early + time::Duration::hours(1);
        let state = serde_json::json!({CURSOR_KEY:early.unix_timestamp()*1_000_000});
        assert_eq!(decode_cursor(&state, late).unwrap(), early);
        assert_eq!(decode_cursor(&serde_json::json!({}), late).unwrap(), late);
        assert!(decode_cursor(&serde_json::json!({CURSOR_KEY:"bad"}), late).is_err());
    }
}
