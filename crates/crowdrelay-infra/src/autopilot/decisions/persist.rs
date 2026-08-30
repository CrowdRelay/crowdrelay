/// Result of the shared decision + action persistence primitive.
/// Both `persist_candidate_impl` and `persist_treatment_with_assignment_impl`
/// call `persist_decision_and_action_tx` and match on this outcome.
enum DecisionActionOutcome {
    /// Quota check throttled — no decision or action created.
    Throttled,
    /// Decision already existed (ON CONFLICT) — no new rows.
    DecisionConflict,
    /// Decision created, but disposition doesn't produce an action.
    NoAction,
    /// Decision + action created (or action already existed). The caller
    /// may now add treatment-specific writes (assignment, prediction,
    /// evidence) in the same transaction.
    ActionReady {
        decision_created: bool,
        action_id: Uuid,
        inserted: bool,
    },
}

/// Shared transactional persistence primitive: quota check + decision
/// INSERT + action INSERT + outbox event. Both the non-experiment and
/// treatment paths call this, then add their specific writes.
///
/// One implementation of the transaction semantics — no divergence
/// between the two paths.
async fn persist_decision_and_action_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    candidate: &DecisionCandidate,
    trace: &TraceContext,
) -> Result<DecisionActionOutcome, RepositoryError> {
    // ── Quota check ──
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
        .fetch_optional(&mut **transaction)
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
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if actions_24h >= i64::from(max_actions_24h) {
            return Ok(DecisionActionOutcome::Throttled);
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
            input_snapshot, policy_snapshot, recommendation, trace_id
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
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
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some(decision_id) = inserted_decision else {
        return Ok(DecisionActionOutcome::DecisionConflict);
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
        return Ok(DecisionActionOutcome::NoAction);
    };
    let action_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind,
            subject_kind, subject_id, idempotency_key, payload, status,
            action_class,
            approved_at, approved_by, approval_expires_at,
            trace_id, causation_id
        )
        VALUES (
            $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
            CASE WHEN $10 = 'queued' THEN now() ELSE NULL END,
            CASE WHEN $10 = 'queued' THEN 'policy:bounded_auto' ELSE NULL END,
            CASE WHEN $10 = 'awaiting_approval' THEN now() + INTERVAL '72 hours' ELSE NULL END,
            $12, $13
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
    .bind(trace.trace_id().into_uuid())
    // The action is caused by the decision — causation_id = decision_id.
    .bind(decision_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let real_action_id = inserted.unwrap_or(action_id);
    // ── Outbox event (approval requested) ──
    if inserted.is_some() && status == "awaiting_approval" {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts, trace_id, causation_id, action_id)
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
                    'approval_expires_at', now() + INTERVAL '72 hours',
                    'trace_id', $9::uuid
                ),
                12,
                $9,
                $2,
                $2
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
        .bind(trace.trace_id().into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(DecisionActionOutcome::ActionReady {
        decision_created: true,
        action_id: real_action_id,
        inserted: inserted.is_some(),
    })
}

/// Builds and records the dispatch-time growth evidence + prediction
/// in the same transaction as the action. This is the initial immutable
/// evidence envelope — outcome fields are NULL and filled in when
/// measurements arrive.
///
/// Idempotent: `ON CONFLICT (action_id) DO NOTHING` on the prediction
/// INSERT, and `ON CONFLICT` on the evidence INSERT.
///
/// # Prediction consistency invariant
///
/// The evidence is built from the SAME `prediction` that was used for
/// the decision. This guarantees:
/// `prediction_at_decision == prediction_persisted_in_initial_evidence`.
/// The invariant is enforced structurally — there is no code path that
/// records a prediction without also recording the matching evidence
/// in the same transaction.
///
/// Returns the `GrowthEvidence` so the caller can record the best-effort
/// audit trail (event log + episode upsert) after the transaction commits.
async fn record_prediction_and_evidence_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    prediction: &crowdrelay_brain::DispatchPrediction,
    _candidate: &DecisionCandidate,
    strategy: Option<&str>,
    holdout_probability: f64,
) -> Result<crowdrelay_brain::GrowthEvidence, RepositoryError> {
    // ── Dispatch prediction ──
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
    .bind(action_id)
    .bind(&prediction.template_id)
    .bind(prediction.expected_new_fans)
    .bind(prediction.expected_signal_installs)
    .bind(&pred_context_json)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    // ── Growth evidence ──
    // Build the evidence from the SAME prediction that was used for
    // the decision. This is the prediction consistency invariant:
    // prediction_at_decision == prediction_persisted_in_initial_evidence.
    let target = prediction
        .context
        .subreddit_type
        .clone()
        .unwrap_or_else(|| format!("action:{}", action_id));
    let opportunity_id = crowdrelay_brain::OpportunityId::new(
        &prediction.template_id,
        &target,
        crowdrelay_brain::OpportunityAction::Post,
        &prediction.context,
    );
    let recipient_id = target.clone();
    let channel = prediction
        .context
        .subreddit_type
        .as_deref()
        .map(|s| {
            if s.starts_with("r/") {
                crowdrelay_brain::ReachChannel::RedditPost
            } else {
                crowdrelay_brain::ReachChannel::Other
            }
        })
        .unwrap_or(crowdrelay_brain::ReachChannel::Other);
    let (treatment_propensity, evidence_quality) = if holdout_probability > 0.0 {
        (
            1.0 - holdout_probability,
            crowdrelay_brain::EvidenceQuality::RandomizedHoldout,
        )
    } else {
        (1.0, crowdrelay_brain::EvidenceQuality::Observational)
    };
    let evidence = crowdrelay_brain::GrowthEvidence::at_dispatch(
        workspace_id.into_uuid(),
        Some(action_id),
        Some(opportunity_id.to_string()),
        recipient_id,
        channel,
        1,
        crowdrelay_brain::TreatmentAssignment::Treatment,
        treatment_propensity,
        prediction.expected_new_fans,
        prediction.expected_signal_installs,
        prediction.context.clone(),
        strategy.map(|s| s.to_owned()),
        evidence_quality,
    );
    super::operations::evidence::record_growth_evidence_in_tx(
        transaction,
        workspace_id,
        &evidence,
    )
    .await?;
    Ok(evidence)
}

macro_rules! decision_persist {
    () => {
    async fn persist_candidate_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let result = match outcome {
                DecisionActionOutcome::Throttled => CandidatePersistence {
                    decision_created: false,
                    action_created: false,
                    quota_throttled: true,
                    action_id: None,
                },
                DecisionActionOutcome::DecisionConflict => CandidatePersistence {
                    action_id: None,
                    ..Default::default()
                },
                DecisionActionOutcome::NoAction => CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => CandidatePersistence {
                    decision_created,
                    action_created: inserted,
                    quota_throttled: false,
                    action_id: Some(action_id),
                },
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(result)
        })
        .await
    }

    /// Atomically persists a treatment action AND its experiment assignment
    /// in a single transaction.
    ///
    /// P0-2: ACTION EXISTS, ASSIGNMENT EXISTS, EXECUTION INTENT EXISTS,
    /// PREDICTION EXISTS, INITIAL EVIDENCE EXISTS.
    /// The decision, action, idempotency, outbox, experiment assignment,
    /// dispatch prediction, and initial growth evidence commit as one
    /// state transition. If any INSERT fails, the entire transaction
    /// rolls back.
    ///
    /// The assignment is constructed by the caller with `action_id: None`.
    /// This method fills in the real `action_id` from the inserted action
    /// before recording the assignment, so the linkage is durable.
    #[allow(clippy::too_many_arguments)]
    async fn persist_treatment_with_assignment_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        assignment: &crowdrelay_brain::ExperimentAssignment,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        _holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // ── Shared primitive: quota + decision + action + outbox ──
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let (decision_created, real_action_id, inserted) = match outcome {
                DecisionActionOutcome::Throttled => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: false,
                        action_created: false,
                        quota_throttled: true,
                        action_id: None,
                    });
                }
                DecisionActionOutcome::DecisionConflict => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        action_id: None,
                        ..Default::default()
                    });
                }
                DecisionActionOutcome::NoAction => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: true,
                        action_created: false,
                        quota_throttled: false,
                        action_id: None,
                    });
                }
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => (decision_created, action_id, inserted),
            };
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
            // ── Dispatch prediction + initial growth evidence (same tx) ──
            // The prediction and initial evidence are recorded atomically
            // with the action. The evidence row is the source of truth for
            // the learning loop — outcome fields are NULL and filled in
            // when measurements arrive. Event/episode materialization is
            // best-effort post-commit (audit trail, not source of truth).
            let evidence = record_prediction_and_evidence_tx(
                &mut transaction,
                workspace_id,
                real_action_id,
                prediction,
                candidate,
                strategy,
                _holdout_probability,
            )
            .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            // Best-effort audit trail (event log + episode upsert).
            // These are NOT source of truth — the evidence row is.
            super::operations::evidence::record_evidence_audit_trail(
                self,
                workspace_id,
                &evidence,
            )
            .await;
            Ok(CandidatePersistence {
                decision_created,
                action_created: inserted,
                quota_throttled: false,
                action_id: Some(real_action_id),
            })
        })
        .await
    }

    /// Persists a candidate with dispatch prediction and initial growth
    /// evidence in the same transaction. Used by the non-experiment path
    /// (scanner, strategist) where there is no experiment assignment but
    /// the prediction and evidence still need to be atomic with the action.
    async fn persist_candidate_with_evidence_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // ── Shared primitive: quota + decision + action + outbox ──
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let result = match outcome {
                DecisionActionOutcome::Throttled => CandidatePersistence {
                    decision_created: false,
                    action_created: false,
                    quota_throttled: true,
                    action_id: None,
                },
                DecisionActionOutcome::DecisionConflict => CandidatePersistence {
                    action_id: None,
                    ..Default::default()
                },
                DecisionActionOutcome::NoAction => CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => {
                    // ── Prediction + evidence (same tx) ──
                    let evidence = record_prediction_and_evidence_tx(
                        &mut transaction,
                        workspace_id,
                        action_id,
                        prediction,
                        candidate,
                        strategy,
                        holdout_probability,
                    )
                    .await?;
                    let persistence = CandidatePersistence {
                        decision_created,
                        action_created: inserted,
                        quota_throttled: false,
                        action_id: Some(action_id),
                    };
                    // Commit first, then best-effort audit trail.
                    transaction.commit().await.map_err(map_sqlx)?;
                    super::operations::evidence::record_evidence_audit_trail(
                        self,
                        workspace_id,
                        &evidence,
                    )
                    .await;
                    return Ok(persistence);
                }
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(result)
        })
        .await
    }
    };
}
