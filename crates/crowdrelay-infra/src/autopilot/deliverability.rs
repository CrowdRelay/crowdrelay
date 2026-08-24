//! What this workspace's sending looks like from outside.
//!
//! One read and one write, and both are small on purpose. The judgement about
//! what a bounce rate means lives in `crowdrelay_domain::deliverability`; what
//! lives here is counting.
//!
//! The one thing worth stating: the denominator is *sends*, not actions. An
//! action that was queued and never left counts toward nothing, or a workspace
//! whose executor is down looks like a workspace with a bounce problem.

use super::*;
use crowdrelay_domain::OutreachTargetId;

#[derive(sqlx::FromRow)]
struct DeliverabilityRow {
    sent_30d: i64,
    bounces_30d: i64,
    complaints_30d: i64,
    first_sent_at: Option<OffsetDateTime>,
}

impl PostgresAutopilotRepository {
    pub(super) async fn load_deliverability_snapshot_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
        weekly_third_party_ceiling: u32,
    ) -> Result<DeliverabilitySnapshot, RepositoryError> {
        self.bounded(async {
            let row = sqlx::query_as::<_, DeliverabilityRow>(
                r#"
                SELECT
                    (
                        -- Sends, not actions. One that never left the queue is
                        -- not a delivery, and counting it would make a broken
                        -- executor look like a bounce problem.
                        SELECT count(*)::bigint
                        FROM viryaos_autopilot_actions AS action
                        WHERE action.workspace_id = $1
                          AND action.action_class = 'third_party'
                          AND action.status = 'succeeded'
                          AND action.finished_at > $2 - INTERVAL '30 days'
                    ) AS sent_30d,
                    (
                        SELECT count(*)::bigint
                        FROM viryaos_outreach_delivery_faults AS fault
                        WHERE fault.workspace_id = $1
                          AND fault.fault IN ('hard_bounce', 'soft_bounce')
                          AND fault.occurred_at > $2 - INTERVAL '30 days'
                    ) AS bounces_30d,
                    (
                        SELECT count(*)::bigint
                        FROM viryaos_outreach_delivery_faults AS fault
                        WHERE fault.workspace_id = $1
                          AND fault.fault = 'complaint'
                          AND fault.occurred_at > $2 - INTERVAL '30 days'
                    ) AS complaints_30d,
                    (
                        SELECT COALESCE(
                            workspace.first_third_party_send_at,
                            (
                                -- Written by the completion edge since 0101;
                                -- the earliest ledger row answers for history
                                -- older than the column, so a workspace that
                                -- already earned reputation does not ramp as
                                -- if it never sent. Both truths come from the
                                -- same ledger, so they cannot disagree.
                                SELECT min(done.finished_at)
                                FROM viryaos_autopilot_actions AS done
                                WHERE done.workspace_id = $1
                                  AND done.action_class = 'third_party'
                                  AND done.status = 'succeeded'
                                  AND done.finished_at IS NOT NULL
                            )
                        )
                        FROM workspaces AS workspace
                        WHERE workspace.id = $1
                    ) AS first_sent_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(DeliverabilitySnapshot {
                sent_30d: u32::try_from(row.sent_30d).unwrap_or(u32::MAX),
                bounces_30d: u32::try_from(row.bounces_30d).unwrap_or(u32::MAX),
                complaints_30d: u32::try_from(row.complaints_30d).unwrap_or(u32::MAX),
                first_sent_at: row.first_sent_at,
                weekly_third_party_ceiling,
            })
        })
        .await
    }

    pub(super) async fn record_delivery_fault_operator(
        &self,
        workspace_id: WorkspaceId,
        command: RecordDeliveryFault,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // Providers report addresses; the ledger wants a target. Resolving
            // here (inside the same transaction as the write) keeps a webhook
            // about an unknown address a clean NotFound instead of an orphan
            // fault row that suppresses nobody.
            let target_id = match command.subject {
                DeliveryFaultSubject::Target(id) => id,
                DeliveryFaultSubject::ContactEmail(ref email) => {
                    let resolved = sqlx::query_scalar::<_, Uuid>(
                        r#"
                        SELECT id FROM viryaos_outreach_targets
                        WHERE workspace_id = $1
                          AND contact_email = lower(btrim($2))
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(email)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    OutreachTargetId::from_uuid(resolved.ok_or(RepositoryError::NotFound)?)
                }
            };
            let operation_id = Uuid::now_v7();
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "record_autopilot_delivery_fault",
                "outreach_target",
                target_id.into_uuid(),
                idempotency_key,
                request_id,
                &json!({
                    "target_id": target_id,
                    "fault": command.fault.as_str(),
                    "provider_reference": command.provider_reference,
                    "occurred_at": command.occurred_at,
                }),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: target_id.into_uuid(),
                    status: command.fault.as_str().into(),
                    replayed: true,
                });
            }
            // `DO NOTHING` rather than a conflict: a provider retrying its own
            // webhook is normal, and the second delivery of one complaint must
            // not count as two. A zero-row insert *is* the replay, so the
            // caller is told so and the suppression below is skipped — the
            // first delivery already did it.
            let inserted = sqlx::query(
                r#"
                INSERT INTO viryaos_outreach_delivery_faults (
                    workspace_id, target_id, fault, provider_reference, occurred_at
                ) VALUES ($1,$2,$3,$4,$5)
                ON CONFLICT (workspace_id, provider_reference) DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(target_id.into_uuid())
            .bind(command.fault.as_str())
            .bind(command.provider_reference.as_deref())
            .bind(command.occurred_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if inserted.rows_affected() == 0 {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id,
                    target_id: target_id.into_uuid(),
                    status: command.fault.as_str().into(),
                    replayed: true,
                });
            }

            if command.fault.suppresses_target() {
                // The address does not exist. Retrying it is how a sender comes
                // to look like somebody working from a bought list.
                sqlx::query(
                    "UPDATE viryaos_outreach_targets \
                     SET active = false, accepts_outreach = false, version = version + 1 \
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                sqlx::query(
                    "UPDATE viryaos_outreach_opportunities SET active = false \
                     WHERE workspace_id = $1 AND target_id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: target_id.into_uuid(),
                status: command.fault.as_str().into(),
                replayed: false,
            })
        })
        .await
    }
}
