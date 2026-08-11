//! Closed-loop executor evidence, release ledger and first-party RUM storage.

use super::*;

const RELEASE_COMPONENTS: [&str; 6] = [
    "crowdrelay-api",
    "crowdrelay-worker",
    "virya-www",
    "synesthesia",
    "virya-signal",
    "n8n",
];

#[async_trait]
impl AutopilotRuntimeRepository for PostgresAutopilotRepository {
    async fn record_execution_report(
        &self,
        workspace_id: WorkspaceId,
        command: RecordExecutionReport,
    ) -> Result<ExecutionReportMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;
            let report_id = Uuid::now_v7();
            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_execution_reports (
                    id, workspace_id, action_id, receipt_key, executor_id, status,
                    provider_reference, error_kind, metadata, occurred_at
                )
                SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10
                WHERE EXISTS (
                    SELECT 1
                    FROM viryaos_autopilot_action_emissions emission
                    WHERE emission.workspace_id=$2 AND emission.action_id=$3
                )
                ON CONFLICT (workspace_id, receipt_key) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(report_id)
            .bind(workspace_id.into_uuid())
            .bind(command.action_id.into_uuid())
            .bind(&command.receipt_key)
            .bind(&command.executor_id)
            .bind(command.status.as_str())
            .bind(&command.provider_reference)
            .bind(&command.error_kind)
            .bind(&command.metadata)
            .bind(command.occurred_at)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if let Some(id) = inserted {
                match command.status {
                    ExecutorReportStatus::Failed => {
                        // A delayed failure receipt may drain after the same action has
                        // already been provider-confirmed. Keep it in the immutable audit
                        // ledger, but never let stale transport ordering reopen the circuit.
                        let provider_already_succeeded = sqlx::query_scalar::<_, bool>(
                            r#"
                            SELECT EXISTS (
                                SELECT 1 FROM viryaos_autopilot_execution_reports report
                                WHERE report.workspace_id=$1 AND report.action_id=$2
                                  AND report.executor_id=$3 AND report.status='succeeded'
                            )
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(command.action_id.into_uuid())
                        .bind(&command.executor_id)
                        .fetch_one(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                        if provider_already_succeeded {
                            transaction.commit().await.map_err(map_sqlx)?;
                            return Ok(ExecutionReportMutation {
                                report_id: id,
                                action_id: command.action_id,
                                status: command.status,
                                replayed: false,
                            });
                        }

                        let reason = command.error_kind.as_deref().unwrap_or("executor_failures");
                        sqlx::query(
                            r#"
                            INSERT INTO viryaos_executor_circuit_breakers (
                                workspace_id, executor_id, failure_count, last_failure_at, reason
                            ) VALUES ($1,$2,1,$3,$4)
                            ON CONFLICT (workspace_id, executor_id) DO UPDATE
                            SET failure_count = CASE
                                    WHEN viryaos_executor_circuit_breakers.last_failure_at >= EXCLUDED.last_failure_at - INTERVAL '15 minutes'
                                    THEN viryaos_executor_circuit_breakers.failure_count + 1 ELSE 1 END,
                                last_failure_at = EXCLUDED.last_failure_at,
                                guarded_until = CASE
                                    WHEN (CASE
                                        WHEN viryaos_executor_circuit_breakers.last_failure_at >= EXCLUDED.last_failure_at - INTERVAL '15 minutes'
                                        THEN viryaos_executor_circuit_breakers.failure_count + 1 ELSE 1 END) >= 3
                                    THEN GREATEST(
                                        COALESCE(viryaos_executor_circuit_breakers.guarded_until, EXCLUDED.last_failure_at),
                                        EXCLUDED.last_failure_at + INTERVAL '15 minutes'
                                    )
                                    WHEN viryaos_executor_circuit_breakers.guarded_until > EXCLUDED.last_failure_at
                                    THEN viryaos_executor_circuit_breakers.guarded_until
                                    ELSE NULL END,
                                reason = CASE
                                    WHEN (CASE
                                        WHEN viryaos_executor_circuit_breakers.last_failure_at >= EXCLUDED.last_failure_at - INTERVAL '15 minutes'
                                        THEN viryaos_executor_circuit_breakers.failure_count + 1 ELSE 1 END) >= 3
                                    THEN EXCLUDED.reason
                                    ELSE viryaos_executor_circuit_breakers.reason END
                            WHERE viryaos_executor_circuit_breakers.last_failure_at IS NULL
                               OR viryaos_executor_circuit_breakers.last_failure_at <= EXCLUDED.last_failure_at
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(&command.executor_id)
                        .bind(command.occurred_at)
                        .bind(reason)
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
                    ExecutorReportStatus::Succeeded => {
                        // Learning and outcome evidence for external actions is
                        // provider-confirmed, never merely outbox-confirmed.
                        let payload_value = sqlx::query_scalar::<_, Value>(
                            "SELECT payload FROM viryaos_autopilot_actions WHERE workspace_id=$1 AND id=$2",
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(command.action_id.into_uuid())
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?
                        .ok_or(RepositoryError::NotFound)?;
                        let payload = serde_json::from_value::<AutopilotActionPayload>(payload_value)
                            .map_err(|_| RepositoryError::Unexpected)?;
                        if payload_requires_executor(&payload) {
                            schedule_effect_measurement(
                                &mut transaction,
                                workspace_id,
                                command.action_id,
                                &payload,
                                command.occurred_at,
                            )
                            .await?;
                            record_execution_outcome(
                                &mut transaction,
                                workspace_id,
                                command.action_id,
                                &payload,
                                command.occurred_at,
                            )
                            .await?;

                            // The provider receipt is also the canonical completion
                            // edge for team-opportunity state. This removes a second
                            // n8n -> CrowdRelay progress callback and its duplicate-send
                            // failure window. Replayed receipts never enter this branch.
                            match &payload {
                                AutopilotActionPayload::ApplyLiveOpportunity { opportunity_id, .. }
                                | AutopilotActionPayload::SubmitFundingApplication { opportunity_id } => {
                                    sqlx::query(
                                        "UPDATE viryaos_team_opportunities \
                                         SET status='submitted', version=version+1 \
                                         WHERE workspace_id=$1 AND id=$2 AND status='submission_requested'",
                                    )
                                    .bind(workspace_id.into_uuid())
                                    .bind((*opportunity_id).into_uuid())
                                    .execute(&mut *transaction)
                                    .await
                                    .map_err(map_sqlx)?;
                                }
                                AutopilotActionPayload::PrepareFundingPackage { opportunity_id } => {
                                    sqlx::query(
                                        "UPDATE viryaos_team_opportunities \
                                         SET package_status='ready', status='prepared', version=version+1 \
                                         WHERE workspace_id=$1 AND id=$2 AND opportunity_kind='funding' \
                                           AND package_status='requested'",
                                    )
                                    .bind(workspace_id.into_uuid())
                                    .bind((*opportunity_id).into_uuid())
                                    .execute(&mut *transaction)
                                    .await
                                    .map_err(map_sqlx)?;
                                }
                                _ => {}
                            }
                        }

                        sqlx::query(
                            r#"
                            UPDATE viryaos_executor_circuit_breakers
                            SET failure_count=0, last_failure_at=NULL, guarded_until=NULL, reason=NULL
                            WHERE workspace_id=$1 AND executor_id=$2
                              AND (last_failure_at IS NULL OR last_failure_at <= $3)
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(&command.executor_id)
                        .bind(command.occurred_at)
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
                    ExecutorReportStatus::Accepted | ExecutorReportStatus::Executing => {}
                }
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(ExecutionReportMutation {
                    report_id: id,
                    action_id: command.action_id,
                    status: command.status,
                    replayed: false,
                });
            }

            let existing = sqlx::query_as::<_, (Uuid, Uuid, String, String)>(
                r#"
                SELECT id, action_id, executor_id, status
                FROM viryaos_autopilot_execution_reports
                WHERE workspace_id=$1 AND receipt_key=$2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.receipt_key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let Some((id, action_id, executor_id, status)) = existing else {
                return Err(RepositoryError::NotFound);
            };
            if action_id != command.action_id.into_uuid()
                || executor_id != command.executor_id
                || status != command.status.as_str()
            {
                return Err(RepositoryError::Conflict);
            }
            Ok(ExecutionReportMutation {
                report_id: id,
                action_id: command.action_id,
                status: command.status,
                replayed: true,
            })
        })
        .await
    }

    async fn record_executor_heartbeat(
        &self,
        workspace_id: WorkspaceId,
        command: RecordExecutorHeartbeat,
    ) -> Result<ExecutorHeartbeatMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;
            let heartbeat_write = sqlx::query(
                r#"
                INSERT INTO viryaos_executor_instances (
                    workspace_id, executor_id, version, manifest_sha,
                    observed_at, expires_at, metadata
                ) VALUES ($1,$2,$3,$4,$5,$6,$7)
                ON CONFLICT (workspace_id, executor_id) DO UPDATE
                SET version=EXCLUDED.version,
                    manifest_sha=EXCLUDED.manifest_sha,
                    observed_at=EXCLUDED.observed_at,
                    expires_at=EXCLUDED.expires_at,
                    metadata=EXCLUDED.metadata
                WHERE viryaos_executor_instances.observed_at <= EXCLUDED.observed_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.executor_id)
            .bind(&command.version)
            .bind(&command.manifest_sha)
            .bind(command.observed_at)
            .bind(command.expires_at)
            .bind(&command.metadata)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if heartbeat_write.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }

            sqlx::query(
                "DELETE FROM viryaos_executor_capabilities WHERE workspace_id=$1 AND executor_id=$2",
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.executor_id)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            for capability in &command.capabilities {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_executor_capabilities (
                        workspace_id, executor_id, capability, capability_version,
                        observed_at, expires_at
                    ) VALUES ($1,$2,$3,$4,$5,$6)
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&command.executor_id)
                .bind(&capability.capability)
                .bind(&capability.version)
                .bind(command.observed_at)
                .bind(command.expires_at)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            sqlx::query(
                r#"
                INSERT INTO viryaos_release_components (
                    workspace_id, component_key, environment, source_sha, version,
                    manifest_sha, observed_at, metadata
                ) VALUES ($1,'n8n','production',$2,$3,$2,$4,$5)
                ON CONFLICT (workspace_id, component_key, environment) DO UPDATE
                SET source_sha=EXCLUDED.source_sha,
                    version=EXCLUDED.version,
                    manifest_sha=EXCLUDED.manifest_sha,
                    observed_at=EXCLUDED.observed_at,
                    metadata=EXCLUDED.metadata
                WHERE viryaos_release_components.observed_at <= EXCLUDED.observed_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.manifest_sha)
            .bind(&command.version)
            .bind(command.observed_at)
            .bind(serde_json::json!({"executor_id": &command.executor_id}))
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ExecutorHeartbeatMutation {
                executor_id: command.executor_id,
                capability_count: command.capabilities.len(),
                expires_at: command.expires_at,
            })
        })
        .await
    }

    async fn upsert_release_component(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertReleaseComponent,
    ) -> Result<ReleaseComponentMutation, RepositoryError> {
        self.bounded(async {
            sqlx::query(
                r#"
                INSERT INTO viryaos_release_components (
                    workspace_id, component_key, environment, source_sha, artifact_digest,
                    deploy_ref, version, manifest_sha, observed_at, metadata
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                ON CONFLICT (workspace_id, component_key, environment) DO UPDATE
                SET source_sha=EXCLUDED.source_sha,
                    artifact_digest=EXCLUDED.artifact_digest,
                    deploy_ref=EXCLUDED.deploy_ref,
                    version=EXCLUDED.version,
                    manifest_sha=EXCLUDED.manifest_sha,
                    observed_at=EXCLUDED.observed_at,
                    metadata=EXCLUDED.metadata
                WHERE viryaos_release_components.observed_at <= EXCLUDED.observed_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.component_key)
            .bind(&command.environment)
            .bind(&command.source_sha)
            .bind(&command.artifact_digest)
            .bind(&command.deploy_ref)
            .bind(&command.version)
            .bind(&command.manifest_sha)
            .bind(command.observed_at)
            .bind(&command.metadata)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(ReleaseComponentMutation {
                component_key: command.component_key,
                environment: command.environment,
                observed_at: command.observed_at,
            })
        })
        .await
    }

    async fn load_release_ledger(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<ReleaseLedgerOverview, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, Option<String>, Option<String>, OffsetDateTime)>(
                r#"
                SELECT component_key, environment, source_sha, artifact_digest,
                       deploy_ref, version, manifest_sha, observed_at
                FROM viryaos_release_components
                WHERE workspace_id=$1 AND environment='production'
                ORDER BY component_key
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let components = rows
                .into_iter()
                .map(|row| ReleaseComponentSummary {
                    component_key: row.0,
                    environment: row.1,
                    source_sha: row.2,
                    artifact_digest: row.3,
                    deploy_ref: row.4,
                    version: row.5,
                    manifest_sha: row.6,
                    observed_at: row.7,
                    stale: row.7 < now - time::Duration::days(7),
                })
                .collect::<Vec<_>>();
            let present = components.iter().map(|item| item.component_key.as_str()).collect::<std::collections::HashSet<_>>();
            let missing_components = RELEASE_COMPONENTS
                .iter()
                .filter(|key| !present.contains(**key))
                .map(|key| (*key).to_owned())
                .collect::<Vec<_>>();
            let api_sha = components.iter().find(|item| item.component_key == "crowdrelay-api").map(|item| item.source_sha.as_str());
            let worker_sha = components.iter().find(|item| item.component_key == "crowdrelay-worker").map(|item| item.source_sha.as_str());
            let backend_sha_drift = matches!((api_sha, worker_sha), (Some(api), Some(worker)) if api != worker);
            let active_executor_count = sqlx::query_scalar::<_, i64>(
                r#"SELECT count(*)::bigint
                   FROM viryaos_executor_instances executor
                   LEFT JOIN viryaos_executor_circuit_breakers breaker
                     ON breaker.workspace_id=executor.workspace_id AND breaker.executor_id=executor.executor_id
                   WHERE executor.workspace_id=$1 AND executor.expires_at>$2
                     AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=$2)"#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let guarded_executor_count = sqlx::query_scalar::<_, i64>(
                "SELECT count(*)::bigint FROM viryaos_executor_circuit_breakers WHERE workspace_id=$1 AND guarded_until>$2",
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let active_executor_manifest_shas = sqlx::query_scalar::<_, String>(
                r#"SELECT DISTINCT executor.manifest_sha
                   FROM viryaos_executor_instances executor
                   LEFT JOIN viryaos_executor_circuit_breakers breaker
                     ON breaker.workspace_id=executor.workspace_id AND breaker.executor_id=executor.executor_id
                   WHERE executor.workspace_id=$1 AND executor.expires_at>$2
                     AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=$2)
                   ORDER BY executor.manifest_sha"#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(ReleaseLedgerOverview {
                components,
                missing_components,
                backend_sha_drift,
                active_executor_count,
                guarded_executor_count,
                active_executor_manifest_shas,
            })
        })
        .await
    }

    async fn record_rum_sample(
        &self,
        workspace_id: WorkspaceId,
        command: RecordRumSample,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            sqlx::query(
                "DELETE FROM viryaos_rum_samples WHERE workspace_id=$1 AND received_at < now() - INTERVAL '30 days'",
            )
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                INSERT INTO viryaos_rum_samples (
                    workspace_id, surface, metric_key, value, route,
                    device_class, release, metadata, observed_at
                )
                SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9
                WHERE (
                    SELECT count(*)
                    FROM viryaos_rum_samples
                    WHERE workspace_id=$1 AND received_at >= now() - INTERVAL '1 minute'
                ) < 600
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.surface)
            .bind(command.metric_key)
            .bind(command.value)
            .bind(command.route)
            .bind(command.device_class)
            .bind(command.release)
            .bind(command.metadata)
            .bind(command.observed_at)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
    async fn load_rum_summaries(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<RumMetricSummary>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, (String, String, i64, f64, f64)>(
                r#"
                SELECT surface, metric_key, count(*)::bigint,
                       (percentile_cont(0.75) WITHIN GROUP (ORDER BY value))::double precision AS p75,
                       (percentile_cont(0.95) WITHIN GROUP (ORDER BY value))::double precision AS p95
                FROM viryaos_rum_samples
                WHERE workspace_id=$1 AND received_at >= $2 - INTERVAL '24 hours'
                GROUP BY surface, metric_key
                HAVING count(*) >= 3
                ORDER BY surface, metric_key
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(rows
                .into_iter()
                .map(|row| RumMetricSummary {
                    surface: row.0,
                    metric_key: row.1,
                    samples_24h: row.2,
                    p75: row.3,
                    p95: row.4,
                })
                .collect())
        })
        .await
    }
}
