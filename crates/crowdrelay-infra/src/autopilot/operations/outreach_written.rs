//! The act saying it wrote to an outreach contact from its own mailbox.
//!
//! Kept out of `ingress.rs`, which holds the rest of the outreach state
//! writes, only because that file sits at its size ratchet; the trait method
//! there delegates here.

use super::*;
use crowdrelay_application::autopilot::{AutopilotControlMutation, RecordOutreachWritten};
use crowdrelay_application::{IdempotencyKey, RequestId};

pub(in crate::autopilot) async fn record_outreach_written(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    command: RecordOutreachWritten,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
) -> Result<AutopilotControlMutation, RepositoryError> {
    repo.bounded(async {
        let mut transaction = repo.pool.begin().await.map_err(map_sqlx)?;
        let operation_id = Uuid::now_v7();
        let details = json!({
            "target_id": command.target_id,
            "occurred_at": crowdrelay_domain::wire_time::Wire(&command.occurred_at),
        });
        if let Some(existing) = super::insert_operator_action(
            &mut transaction,
            workspace_id,
            operation_id,
            "record_autopilot_outreach_written",
            "outreach_target",
            command.target_id.into_uuid(),
            "admin_api_key",
            idempotency_key,
            request_id,
            &details,
        )
        .await?
        {
            transaction.commit().await.map_err(map_sqlx)?;
            return Ok(AutopilotControlMutation {
                operation_id: existing,
                target_id: command.target_id.into_uuid(),
                status: "written_recorded".into(),
                replayed: true,
            });
        }

        // The target's own clock moves forward only: a message logged
        // late must not make an earlier contact look more recent than it
        // was. A contact marked do-not-contact is refused outright — the
        // console must not record the act writing to someone who asked
        // not to be written to without saying so.
        let written = sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE outreach_targets
            SET last_outreach_at = GREATEST(COALESCE(last_outreach_at, $3), $3),
                version = version + 1,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND NOT do_not_contact
            RETURNING version
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(command.occurred_at)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;

        sqlx::query(
            r#"
            INSERT INTO outreach_target_history (workspace_id, target_id, version, snapshot)
            SELECT workspace_id, id, version, jsonb_build_object(
                'target_kind', target_kind,
                'display_name', display_name,
                'contact_email', contact_email,
                'active', active,
                'verified', verified,
                'accepts_outreach', accepts_outreach,
                'priority', priority,
                'relationship_score', relationship_score,
                'do_not_contact', do_not_contact,
                'last_outreach_at', last_outreach_at,
                'last_reply_at', last_reply_at,
                'last_reply_disposition', last_reply_disposition
            )
            FROM outreach_targets
            WHERE workspace_id = $1 AND id = $2 AND version = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(written)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;

        // First message to this contact on record, or a follow-up.
        sqlx::query(
            r#"
            INSERT INTO outreach_interactions (
                workspace_id, target_id, direction, phase, disposition,
                source_key, occurred_at
            )
            SELECT $1, $2, 'outbound',
                   CASE WHEN EXISTS (
                       SELECT 1 FROM outreach_interactions
                       WHERE workspace_id = $1 AND target_id = $2 AND direction = 'outbound'
                   ) THEN 'followup' ELSE 'initial' END,
                   'none', $3, $4
            ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(format!("operator:{operation_id}"))
        .bind(command.occurred_at)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;

        transaction.commit().await.map_err(map_sqlx)?;
        Ok(AutopilotControlMutation {
            operation_id,
            target_id: command.target_id.into_uuid(),
            status: "written_recorded".into(),
            replayed: false,
        })
    })
    .await
}
