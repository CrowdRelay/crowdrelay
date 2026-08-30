macro_rules! decision_persist {
    () => {
    async fn persist_candidate_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            if matches!(
                candidate.disposition,
                PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute
            ) {
                let max_actions_24h = sqlx::query_scalar::<_, i32>(
                    r#"
                    SELECT max_actions_24h
                    FROM viryaos_autopilot_policies
                    WHERE workspace_id = $1 AND context = $2 AND enabled
                    FOR UPDATE
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                let actions_24h = sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT COUNT(*)::bigint
                    FROM viryaos_autopilot_actions
                    WHERE workspace_id = $1
                      AND context = $2
                      AND created_at >= now() - INTERVAL '24 hours'
                      AND status <> 'cancelled'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if actions_24h >= i64::from(max_actions_24h) {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: false,
                        action_created: false,
                        quota_throttled: true,
                        action_id: None,
                    });
                }
            }
            let decision_id = Uuid::now_v7();
            let action_json =
                serde_json::to_value(&candidate.action).map_err(|_| RepositoryError::Unexpected)?;
            let inserted_decision = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_decisions (
                    id, workspace_id, decision_key, context, subject_kind, subject_id,
                    decision_kind, confidence_basis_points, disposition, reason,
                    input_snapshot, policy_snapshot, recommendation
                )
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                ON CONFLICT (workspace_id, decision_key) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(decision_id)
            .bind(workspace_id.into_uuid())
            .bind(&candidate.decision_key)
            .bind(candidate.context.as_str())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(candidate.decision_kind)
            .bind(i32::from(candidate.confidence.basis_points()))
            .bind(disposition_str(candidate.disposition))
            .bind(candidate.reason)
            .bind(&candidate.input_snapshot)
            .bind(&candidate.policy_snapshot)
            .bind(&action_json)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let Some(decision_id) = inserted_decision else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    action_id: None,
                    ..Default::default()
                });
            };

            let status = match candidate.disposition {
                PolicyDisposition::RequireApproval => Some("awaiting_approval"),
                PolicyDisposition::AutoExecute => Some("queued"),
                PolicyDisposition::ObserveOnly
                | PolicyDisposition::RecommendOnly
                | PolicyDisposition::Deny => None,
            };
            let Some(status) = status else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                });
            };

            let action_id = Uuid::now_v7();
            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_actions (
                    id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    action_class,
                    approved_at, approved_by, approval_expires_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                    CASE WHEN $10 = 'queued' THEN now() ELSE NULL END,
                    CASE WHEN $10 = 'queued' THEN 'policy:bounded_auto' ELSE NULL END,
                    CASE WHEN $10 = 'awaiting_approval' THEN now() + INTERVAL '72 hours' ELSE NULL END
                )
                ON CONFLICT DO NOTHING
                RETURNING id
                "#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(candidate.context.as_str())
            .bind(candidate.action.action_kind())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(&candidate.action_idempotency_key)
            .bind(action_json)
            .bind(status)
            // Recorded now rather than derived at read time: this is the class
            // the action was authorised under, which is what an audit needs.
            .bind(candidate.action.action_class().as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if inserted.is_some() && status == "awaiting_approval" {
                sqlx::query(
                    r#"
                    INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts)
                    VALUES (
                        $1, 'crowdrelay.autopilot.approval_requested', 1,
                        jsonb_build_object(
                            'action_id', $2::uuid,
                            'context', $3::text,
                            'action_kind', $4::text,
                            'subject_kind', $5::text,
                            'subject_id', $6::uuid,
                            'reason', $7::text,
                            'confidence_basis_points', $8::integer,
                            'approval_expires_at', now() + INTERVAL '72 hours'
                        ),
                        12
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id)
                .bind(candidate.context.as_str())
                .bind(candidate.action.action_kind())
                .bind(candidate.subject.kind())
                .bind(candidate.subject.uuid())
                .bind(candidate.reason)
                .bind(i32::from(candidate.confidence.basis_points()))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(CandidatePersistence {
                decision_created: true,
                action_created: inserted.is_some(),
                quota_throttled: false,
                action_id: inserted.or(Some(action_id)),
            })
        })
        .await
    }

    /// Atomically persists a treatment action AND its experiment assignment
    /// in a single transaction.
    ///
    /// P0-2: ACTION EXISTS ↔ ASSIGNMENT EXISTS ↔ EXECUTION INTENT EXISTS.
    /// The decision + action + idempotency + outbox + experiment assignment
    /// commit as one state transition. If the assignment INSERT fails, the
    /// entire transaction rolls back — no action without assignment.
    ///
    /// The assignment is constructed by the caller with `action_id: None`.
    /// This method fills in the real `action_id` from the inserted action
    /// before recording the assignment, so the linkage is durable.
    async fn persist_treatment_with_assignment_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        assignment: &crowdrelay_brain::ExperimentAssignment,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        _holdout_probability: f64,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // ── Quota check (same as persist_candidate_impl) ──
            if matches!(
                candidate.disposition,
                PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute
            ) {
                let max_actions_24h = sqlx::query_scalar::<_, i32>(
                    r#"
                    SELECT max_actions_24h
                    FROM viryaos_autopilot_policies
                    WHERE workspace_id = $1 AND context = $2 AND enabled
                    FOR UPDATE
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                let actions_24h = sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT COUNT(*)::bigint
                    FROM viryaos_autopilot_actions
                    WHERE workspace_id = $1
                      AND context = $2
                      AND created_at >= now() - INTERVAL '24 hours'
                      AND status <> 'cancelled'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate.context.as_str())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if actions_24h >= i64::from(max_actions_24h) {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: false,
                        action_created: false,
                        quota_throttled: true,
                        action_id: None,
                    });
                }
            }
            // ── Decision INSERT ──
            let decision_id = Uuid::now_v7();
            let action_json =
                serde_json::to_value(&candidate.action).map_err(|_| RepositoryError::Unexpected)?;
            let inserted_decision = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_decisions (
                    id, workspace_id, decision_key, context, subject_kind, subject_id,
                    decision_kind, confidence_basis_points, disposition, reason,
                    input_snapshot, policy_snapshot, recommendation
                )
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                ON CONFLICT (workspace_id, decision_key) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(decision_id)
            .bind(workspace_id.into_uuid())
            .bind(&candidate.decision_key)
            .bind(candidate.context.as_str())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(candidate.decision_kind)
            .bind(i32::from(candidate.confidence.basis_points()))
            .bind(disposition_str(candidate.disposition))
            .bind(candidate.reason)
            .bind(&candidate.input_snapshot)
            .bind(&candidate.policy_snapshot)
            .bind(&action_json)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let Some(decision_id) = inserted_decision else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    action_id: None,
                    ..Default::default()
                });
            };
            // ── Action INSERT ──
            let status = match candidate.disposition {
                PolicyDisposition::RequireApproval => Some("awaiting_approval"),
                PolicyDisposition::AutoExecute => Some("queued"),
                PolicyDisposition::ObserveOnly
                | PolicyDisposition::RecommendOnly
                | PolicyDisposition::Deny => None,
            };
            let Some(status) = status else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                });
            };
            let action_id = Uuid::now_v7();
            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_actions (
                    id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    action_class,
                    approved_at, approved_by, approval_expires_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                    CASE WHEN $10 = 'queued' THEN now() ELSE NULL END,
                    CASE WHEN $10 = 'queued' THEN 'policy:bounded_auto' ELSE NULL END,
                    CASE WHEN $10 = 'awaiting_approval' THEN now() + INTERVAL '72 hours' ELSE NULL END
                )
                ON CONFLICT DO NOTHING
                RETURNING id
                "#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(candidate.context.as_str())
            .bind(candidate.action.action_kind())
            .bind(candidate.subject.kind())
            .bind(candidate.subject.uuid())
            .bind(&candidate.action_idempotency_key)
            .bind(&action_json)
            .bind(status)
            .bind(candidate.action.action_class().as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let real_action_id = inserted.unwrap_or(action_id);
            // ── Outbox event (approval requested) ──
            if inserted.is_some() && status == "awaiting_approval" {
                sqlx::query(
                    r#"
                    INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts)
                    VALUES (
                        $1, 'crowdrelay.autopilot.approval_requested', 1,
                        jsonb_build_object(
                            'action_id', $2::uuid,
                            'context', $3::text,
                            'action_kind', $4::text,
                            'subject_kind', $5::text,
                            'subject_id', $6::uuid,
                            'reason', $7::text,
                            'confidence_basis_points', $8::integer,
                            'approval_expires_at', now() + INTERVAL '72 hours'
                        ),
                        12
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id)
                .bind(candidate.context.as_str())
                .bind(candidate.action.action_kind())
                .bind(candidate.subject.kind())
                .bind(candidate.subject.uuid())
                .bind(candidate.reason)
                .bind(i32::from(candidate.confidence.basis_points()))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            // ── Experiment assignment INSERT (atomic with action) ──
            // P0-2: The assignment is recorded with the real action_id,
            // inside the same transaction. If this fails, the action is
            // rolled back too — no action without assignment.
            let context_json = serde_json::to_value(&assignment.context)
                .unwrap_or(serde_json::json!({}));
            let prediction_json = serde_json::to_value(prediction)
                .unwrap_or(serde_json::json!({}));
            let kind = assignment.kind();
            // The assignment was constructed with action_id=None (withheld),
            // but this path creates a real action — so execution_status
            // must be Dispatched (durable intent committed), not Withheld.
            // It transitions to Executed only when the external intervention
            // is confirmed by the executor.
            let execution_status = if assignment.arm
                == crowdrelay_brain::TreatmentAssignment::Treatment
            {
                crowdrelay_brain::ExecutionStatus::Dispatched
            } else {
                assignment.execution_status
            };
            sqlx::query(
                r#"
                INSERT INTO viryaos_experiment_assignments
                    (id, workspace_id, unit_id, unit_kind, arm, assigned_at,
                     propensity, intended_holdout_probability, intended_template_id,
                     context, prediction, action_id, strategy, experiment_kind,
                     contamination_estimate, is_interference_controllable,
                     experiment_uuid, assignment_round,
                     eligibility_criteria, selection_context,
                     interference_policy, assignment_time_contamination,
                     experiment_status, execution_status)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                        $17, $18, $19, $20, $21, $22, $23, $24)
                ON CONFLICT (workspace_id, experiment_uuid, assignment_round, unit_id)
                DO NOTHING
                "#,
            )
            .bind(&assignment.assignment_id)
            .bind(workspace_id.into_uuid())
            .bind(&assignment.unit_id)
            .bind(assignment.unit_kind.as_str())
            .bind(assignment.arm.as_str())
            .bind(assignment.assigned_at)
            .bind(assignment.propensity)
            .bind(assignment.intended_holdout_probability)
            .bind(&assignment.intended_template_id)
            .bind(&context_json)
            .bind(&prediction_json)
            .bind(Some(real_action_id))
            .bind(strategy)
            .bind(kind.as_str())
            .bind(assignment.interference_score)
            .bind(assignment.is_interference_controllable)
            .bind(assignment.experiment_uuid)
            .bind(assignment.assignment_round as i32)
            .bind(&assignment.eligibility_criteria)
            .bind(&assignment.selection_context)
            .bind(assignment.interference_policy.as_str())
            .bind(assignment.interference_score)
            .bind(assignment.experiment_status.as_str())
            .bind(execution_status.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // ── Dispatch prediction recording (same tx) ──
            // The prediction is recorded atomically with the action so the
            // causal model can learn from it. The growth evidence row is
            // recorded separately by the caller after the transaction commits
            // — it is measurement infrastructure, not causal bookkeeping.
            let pred_context_json = serde_json::to_value(&prediction.context)
                .unwrap_or(serde_json::json!({}));
            sqlx::query(
                r#"
                INSERT INTO viryaos_dispatch_predictions
                    (workspace_id, action_id, template_id,
                     expected_new_fans, expected_signal_installs, context)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (action_id) DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(real_action_id)
            .bind(&prediction.template_id)
            .bind(prediction.expected_new_fans)
            .bind(prediction.expected_signal_installs)
            .bind(&pred_context_json)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(CandidatePersistence {
                decision_created: true,
                action_created: inserted.is_some(),
                quota_throttled: false,
                action_id: Some(real_action_id),
            })
        })
        .await
    }
    };
}
