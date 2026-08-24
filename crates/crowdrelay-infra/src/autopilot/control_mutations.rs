//! The control plane's operator mutations, split out of `control.rs`.
//!
//! Same repository, same transactions, one seam: everything here records an
//! operator decision about a parked action or a finding, under the usual
//! idempotency ledger. Reads live in `control.rs`.

use super::*;

impl PostgresAutopilotRepository {
    pub(super) async fn control_action_transition(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
        operator_action: &'static str,
        target_status: &'static str,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({"requested_status": target_status});
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                operator_action,
                "autopilot_action",
                action_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing) = replay {
                let status = sqlx::query_scalar::<_, String>(
                    "SELECT status FROM viryaos_autopilot_actions WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::NotFound)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: action_id.into_uuid(),
                    status,
                    replayed: true,
                });
            }

            let updated = if target_status == "queued" {
                sqlx::query_scalar::<_, String>(
                    r#"
                    UPDATE viryaos_autopilot_actions
                    SET status = 'queued', approved_at = now(), approved_by = 'operator:admin_api_key'
                    WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'
                      AND (approval_expires_at IS NULL OR approval_expires_at > now())
                    RETURNING status
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, String>(
                    r#"
                    UPDATE viryaos_autopilot_actions
                    SET status = 'cancelled', finished_at = now()
                    WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'
                    RETURNING status
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            };
            // The transition matched nothing, and the operator deserves to know
            // which nothing. Collapsing all three into `Conflict` made a wrong
            // action id, an already-approved action and an expired approval
            // read identically in the cockpit, so a stale queue looked like a
            // broken button.
            let status = match updated {
                Some(status) => status,
                None => {
                    let existing = sqlx::query_scalar::<_, String>(
                        "SELECT status FROM viryaos_autopilot_actions
                         WHERE workspace_id = $1 AND id = $2",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(action_id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    return Err(existing.map_or(RepositoryError::NotFound, |_| {
                        RepositoryError::Conflict
                    }));
                }
            };
            if target_status == "queued" {
                sqlx::query(
                    r#"
                    UPDATE viryaos_team_assignments
                    SET status='done', completed_at=now(), next_reminder_at=NULL
                    WHERE workspace_id=$1 AND action_id=$2 AND status='open'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            } else {
                sqlx::query(
                    r#"
                    UPDATE viryaos_team_assignments
                    SET status='cancelled', completed_at=NULL, next_reminder_at=NULL
                    WHERE workspace_id=$1 AND action_id=$2 AND status='open'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: action_id.into_uuid(),
                status,
                replayed: false,
            })
        })
        .await
    }

    /// Records "we did this ourselves" about one finding.
    ///
    /// A first-class outcome, not a dismissal: the ledger row says a human
    /// took the opportunity, which is a success the measured record can read
    /// as one. The decision leaves every read model through that row, and any
    /// action of it still parked is withdrawn in the same transaction so a
    /// handled finding cannot go out anyway hours later.
    pub(super) async fn mark_decision_handled_operator(
        &self,
        workspace_id: WorkspaceId,
        decision_id: AutopilotDecisionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // The finding must exist before anything records having handled
            // it; otherwise a stale board click writes a suppression row for
            // a decision nobody ever saw.
            let exists = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM viryaos_autopilot_decisions
                    WHERE workspace_id = $1 AND id = $2
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let operation_id = Uuid::now_v7();
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "handle_autopilot_decision_externally",
                "autopilot_decision",
                decision_id.into_uuid(),
                idempotency_key,
                request_id,
                &json!({"decision_id": decision_id, "outcome": "handled_by_human"}),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: decision_id.into_uuid(),
                    status: "handled_externally".into(),
                    replayed: true,
                });
            }
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'cancelled', finished_at = now()
                WHERE workspace_id = $1 AND decision_id = $2 AND status = 'awaiting_approval'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: decision_id.into_uuid(),
                status: "handled_externally".into(),
                replayed: false,
            })
        })
        .await
    }
}

impl PostgresAutopilotRepository {
    pub(super) async fn load_growth_posture_impl(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GrowthPostureView, RepositoryError> {
        self.bounded(async {
            let row = sqlx::query_as::<_, (Option<String>, i64, Option<OffsetDateTime>)>(
                r#"
                SELECT posture, expected_version, set_at
                FROM viryaos_growth_posture
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(match row {
                Some((posture, version, set_at)) => GrowthPostureView {
                    // A posture this build cannot parse is not a reason to
                    // guess permissively; it reads as unset and the safe
                    // defaults hold.
                    posture: posture.as_deref().and_then(GrowthPosture::parse),
                    expected_version: version,
                    set_at,
                },
                None => GrowthPostureView {
                    posture: None,
                    expected_version: 1,
                    set_at: None,
                },
            })
        })
        .await
    }

    pub(super) async fn set_growth_posture_impl(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthPosture,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_growth_autonomy_posture",
                "growth_posture",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &json!({
                    "posture": command.posture.as_str(),
                    "expected_version": command.expected_version,
                }),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: workspace_id.into_uuid(),
                    status: format!("posture_{}", command.posture.as_str()),
                    replayed: true,
                });
            }

            // Optimistic concurrency on the posture row itself. A missing row
            // is version 1, so a first application from a fresh workspace only
            // succeeds when nobody else raced one in.
            let current: Option<i64> = sqlx::query_scalar(
                r#"
                SELECT expected_version FROM viryaos_growth_posture
                WHERE workspace_id = $1 FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let current_version = current.unwrap_or(1);
            if current_version != command.expected_version {
                return Err(RepositoryError::Conflict);
            }
            let next_version = current_version + 1;

            // One: every context level. The mapping lives in the application
            // layer where the context list lives; this loop applies it verbatim.
            for context in AutopilotContext::ALL {
                sqlx::query(
                    r#"
                    UPDATE viryaos_autopilot_policies
                    SET enabled = true, autonomy_level = $3
                    WHERE workspace_id = $1 AND context = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(context.as_str())
                .bind(autonomy_level_str(command.posture.context_level(context)))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            // Two: the four class ceilings, with the posture named as why.
            for class in [
                ActionClass::FirstPartyReversible,
                ActionClass::OwnedAudience,
                ActionClass::ThirdParty,
                ActionClass::Paid,
            ] {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_growth_autonomy (
                        workspace_id, action_class, ceiling, rationale
                    ) VALUES ($1, $2, $3, $4)
                    ON CONFLICT (workspace_id, action_class) DO UPDATE
                    SET ceiling = EXCLUDED.ceiling,
                        rationale = EXCLUDED.rationale,
                        version = viryaos_growth_autonomy.version + 1
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(class.as_str())
                .bind(autonomy_level_str(command.posture.ceiling(class)))
                .bind(command.posture.ceiling_rationale())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            // Three: the envelope switches only. Budgets, cooldowns and blast
            // radius are the operator's tuned numbers; a posture flip that
            // silently reset them would be a regression wearing a feature's
            // clothes.
            let (agent_enabled, dry_run) = command.posture.envelope();
            sqlx::query(
                r#"
                INSERT INTO viryaos_growth_envelope (workspace_id, agent_enabled, dry_run)
                VALUES ($1, $2, $3)
                ON CONFLICT (workspace_id) DO UPDATE
                SET agent_enabled = EXCLUDED.agent_enabled,
                    dry_run = EXCLUDED.dry_run,
                    version = viryaos_growth_envelope.version + 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(agent_enabled)
            .bind(dry_run)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            sqlx::query(
                r#"
                INSERT INTO viryaos_growth_posture (
                    workspace_id, posture, expected_version, set_at
                ) VALUES ($1, $2, $3, now())
                ON CONFLICT (workspace_id) DO UPDATE
                SET posture = EXCLUDED.posture,
                    expected_version = EXCLUDED.expected_version,
                    set_at = now()
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.posture.as_str())
            .bind(next_version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: workspace_id.into_uuid(),
                status: format!("posture_{}", command.posture.as_str()),
                replayed: false,
            })
        })
        .await
    }
}
