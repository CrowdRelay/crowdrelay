macro_rules! decision_persist {
    () => {
    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout)
                .await?;
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
                return Ok(CandidatePersistence::default());
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
                });
            };

            let action_id = Uuid::now_v7();
            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_actions (
                    id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    approved_at, approved_by, approval_expires_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,
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
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if inserted.is_some() && status == "awaiting_approval" {
                sqlx::query(
                    r#"
                    INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts)
                    VALUES (
                        $1, 'viryaos.autopilot.approval_requested', 1,
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
            })
        })
        .await
    }
    };
}
