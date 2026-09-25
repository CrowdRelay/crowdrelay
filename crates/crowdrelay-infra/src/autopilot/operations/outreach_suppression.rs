//! The operator saying "don't contact" — or lifting that word again.
//!
//! Deliberately separate from `record_outreach_reply`: a suppression is the
//! target's standing, not a message, so the conversation ledger stays
//! truthful about who spoke last. Kept out of `ingress.rs`, which sits at
//! its size ratchet; the trait method there delegates here.

use super::*;
use crowdrelay_application::autopilot::{AutopilotControlMutation, SuppressOutreachTarget};
use crowdrelay_application::{IdempotencyKey, RequestId};

pub(in crate::autopilot) async fn suppress_outreach_target(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    command: SuppressOutreachTarget,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
) -> Result<AutopilotControlMutation, RepositoryError> {
    repo.bounded(async {
        let mut transaction = repo.pool.begin().await.map_err(map_sqlx)?;
        let operation_id = Uuid::now_v7();
        let details = json!({
            "target_id": command.target_id,
            "do_not_contact": command.do_not_contact,
            "occurred_at": crowdrelay_domain::wire_time::Wire(&command.occurred_at),
        });
        if let Some(existing) = super::insert_operator_action(
            &mut transaction,
            workspace_id,
            operation_id,
            "suppress_autopilot_outreach_target",
            "outreach_target",
            command.target_id.into_uuid(),
            "admin_api_key",
            idempotency_key,
            request_id,
            &details,
        )
        .await?
        {
            // Replay tells the truth the first answer told: a request that
            // found the flag already set is still a no-op, not a recording.
            // The recorded path stamps the operation id into the history
            // snapshot — that row's existence is the original outcome.
            let wrote_change = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM outreach_target_history
                    WHERE workspace_id = $1 AND target_id = $2
                      AND snapshot->>'operation_id' = $3::text
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(existing.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            return Ok(AutopilotControlMutation {
                operation_id: existing,
                target_id: command.target_id.into_uuid(),
                status: if wrote_change {
                    "suppression_recorded"
                } else {
                    "suppression_unchanged"
                }
                .into(),
                replayed: true,
            });
        }

        // Stamping the suppression also closes the consent standing — a
        // contact that may not be written to may not be pitched either.
        // Lifting the suppression never restores `accepts_outreach`: the
        // flag reflects consent the operator cannot re-grant on a contact's
        // behalf, so it stays where the last truthful signal left it.
        let suppressed = sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE outreach_targets
            SET do_not_contact = $3,
                accepts_outreach = CASE WHEN $3 THEN false ELSE accepts_outreach END,
                version = version + 1,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND do_not_contact IS DISTINCT FROM $3
            RETURNING version
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(command.do_not_contact)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx)?;

        let Some(suppressed) = suppressed else {
            // The flag already says what was asked — or the target does not
            // exist. The first is the operator's intent already kept, not a
            // conflict; the second is a not-found.
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM outreach_targets WHERE workspace_id = $1 AND id = $2)",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            transaction.commit().await.map_err(map_sqlx)?;
            return Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.target_id.into_uuid(),
                status: "suppression_unchanged".into(),
                replayed: false,
            });
        };

        sqlx::query(
            r#"
            INSERT INTO outreach_target_history (workspace_id, target_id, version, snapshot)
            SELECT workspace_id, id, version, jsonb_build_object(
                'operation_id', $4::text,
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
        .bind(suppressed)
        .bind(operation_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;

        transaction.commit().await.map_err(map_sqlx)?;
        Ok(AutopilotControlMutation {
            operation_id,
            target_id: command.target_id.into_uuid(),
            status: "suppression_recorded".into(),
            replayed: false,
        })
    })
    .await
}
