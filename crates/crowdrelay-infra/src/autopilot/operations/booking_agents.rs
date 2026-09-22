//! Operator-filed booking-agent replies (§4h-10).
//!
//! Kept apart from `record_booking_reply`: a booking target answers for one
//! night, an agent answers for the season. A `declined` here stamps the
//! registry's `refused_until` — the door stays shut for the season — and a
//! `do_not_contact` stamps the flag and the contact governor so every other
//! route honours it too. Any reply also re-confirms the route: an agent who
//! wrote back is an agent whose address was real.

use super::*;

pub(in crate::autopilot) async fn record_booking_agent_reply(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    command: crowdrelay_application::autopilot::RecordBookingAgentReply,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
) -> Result<AutopilotControlMutation, RepositoryError> {
    repo.bounded(async {
        let disposition = command.disposition.as_str();
        let mut tx = repo.pool.begin().await.map_err(map_sqlx)?;
        let operation_id = Uuid::now_v7();
        let details = json!({
            "agent_id": command.agent_id,
            "disposition": disposition,
            "occurred_at": command.occurred_at,
        });
        if let Some(existing) = super::insert_operator_action(
            &mut tx,
            workspace_id,
            operation_id,
            "record_autopilot_booking_agent_reply",
            "booking_agent",
            command.agent_id.into_uuid(),
            "admin_api_key",
            idempotency_key,
            request_id,
            &details,
        )
        .await?
        {
            tx.commit().await.map_err(map_sqlx)?;
            return Ok(AutopilotControlMutation {
                operation_id: existing,
                target_id: command.agent_id.into_uuid(),
                status: "reply_recorded".into(),
                replayed: true,
            });
        }

        // The registry row takes the outcome: a decline closes the season's
        // door from the day they answered (not the day it was filed), a
        // do-not-contact is the wall, and any reply re-proves the route.
        let refused_until = matches!(
            command.disposition,
            crowdrelay_domain::booking_agent::BookingAgentReplyDisposition::Declined
        )
        .then(|| crowdrelay_domain::booking_agent::refusal_until(command.occurred_at));
        let new_version = sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE booking_agents
            SET refused_until = CASE
                    WHEN $3::date IS NULL THEN refused_until
                    ELSE GREATEST(refused_until, $3::date)
                END,
                do_not_contact = do_not_contact OR $4,
                contact_verified_at = CASE
                    WHEN contact_verified_at IS NULL OR contact_verified_at < $5
                    THEN $5 ELSE contact_verified_at
                END,
                version = version + 1
            WHERE workspace_id = $1 AND id = $2
            RETURNING version
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.agent_id.into_uuid())
        .bind(refused_until)
        .bind(matches!(
            command.disposition,
            crowdrelay_domain::booking_agent::BookingAgentReplyDisposition::DoNotContact
        ))
        .bind(command.occurred_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::NotFound)?;

        // A do-not-contact binds every other route to the same address —
        // the governor is the shared ledger the other send paths read.
        if matches!(
            command.disposition,
            crowdrelay_domain::booking_agent::BookingAgentReplyDisposition::DoNotContact
        ) {
            sqlx::query(
                r#"
                INSERT INTO contact_governor (
                    workspace_id, normalized_contact, last_context, last_action_id,
                    last_outbound_at, next_contact_after, do_not_contact
                )
                SELECT $1, lower(btrim(contact_email)), 'booking_agent', NULL, $3, $3, true
                FROM booking_agents
                WHERE workspace_id = $1 AND id = $2
                ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
                SET do_not_contact = true,
                    last_context = EXCLUDED.last_context,
                    next_contact_after = GREATEST(
                        contact_governor.next_contact_after,
                        EXCLUDED.next_contact_after
                    ),
                    updated_at = now()
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.agent_id.into_uuid())
            .bind(command.occurred_at)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }

        sqlx::query(
            r#"
            INSERT INTO booking_agent_interactions
                (workspace_id, agent_id, direction, phase, disposition, source_key,
                 occurred_at, metadata)
            VALUES ($1,$2,'inbound','reply',$3,$4,$5,$6)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.agent_id.into_uuid())
        .bind(disposition)
        .bind(format!("operator:{operation_id}"))
        .bind(command.occurred_at)
        .bind(json!({"agent_version": new_version}))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(AutopilotControlMutation {
            operation_id,
            target_id: command.agent_id.into_uuid(),
            status: "reply_recorded".into(),
            replayed: false,
        })
    })
    .await
}
