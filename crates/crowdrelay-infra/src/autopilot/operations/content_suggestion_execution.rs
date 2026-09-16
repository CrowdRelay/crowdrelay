//! Execution boundary for content suggestions.
//!
//! Approving a suggestion ask commits the band to the beat — it does not
//! publish, draft, or dispatch anything. Production and distribution are
//! later lifecycle steps with their own surfaces; the only write here is the
//! `raised → approved` transition nobody else may make.

use super::*;

pub(in crate::autopilot) async fn approve_content_suggestion(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    suggestion_id: crowdrelay_domain::ContentSuggestionId,
) -> Result<(), RepositoryError> {
    // Approval commits the band to the beat: raised → approved is the
    // transition nobody else may write, and the row stays open until the band
    // reports an outcome or the beat's day passes unreported — the suggestion
    // sweep resolves an overdue commitment `expired` then. A lapsed suggestion
    // is a dead ask — the window it argued for has passed, and committing the
    // band to it now would be a lie.
    let changed = sqlx::query(
        r#"
        UPDATE viryaos_content_suggestions
        SET status = 'approved', updated_at = now()
        WHERE workspace_id = $1 AND id = $2 AND status = 'raised'
          AND (expires_at IS NULL OR expires_at > now())
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(suggestion_id.into_uuid())
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .rows_affected();
    if changed == 0 {
        // A replay finds the row already approved — that is the answer, not
        // a failure. A terminal row means the queue entry outlived its
        // question: stale, and the conflict says so rather than resurrecting
        // it.
        let row = sqlx::query_as::<_, (String, bool)>(
            r#"
            SELECT status,
                   expires_at IS NOT NULL AND expires_at <= now() AS lapsed
            FROM viryaos_content_suggestions
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(suggestion_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        match row
            .as_ref()
            .map(|(status, lapsed)| (status.as_str(), *lapsed))
        {
            Some(("approved", _)) => Ok(()),
            Some((_, true)) => Err(RepositoryError::ConflictBecause(
                "This suggestion's window has passed — approving it now would commit the band to a beat that is already gone.",
            )),
            Some((_, false)) => Err(RepositoryError::ConflictBecause(
                "This suggestion was already resolved — approving it now would reopen a closed question.",
            )),
            None => Err(RepositoryError::NotFound),
        }
    } else {
        Ok(())
    }
}
