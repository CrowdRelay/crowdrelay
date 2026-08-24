//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

#[async_trait]
impl AutopilotControlRepository for PostgresAutopilotRepository {
    async fn load_control_overview(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<AutopilotControlOverview, RepositoryError> {
        self.bounded(async {
            let policies = sqlx::query_as::<_, PolicyRow>(
                r#"
                SELECT context, enabled, autonomy_level,
                       minimum_confidence_basis_points, max_actions_24h, config, version,
                       guarded_until, guardrail_reason
                FROM viryaos_autopilot_policies
                WHERE workspace_id = $1
                ORDER BY context
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(policy_summary)
            .collect::<Result<Vec<_>, _>>()?;

            let promotion_budget_guardrails = sqlx::query_as::<_, PromotionBudgetGuardrailRow>(
                r#"
                SELECT currency, maximum_total_daily_budget_minor, maximum_monthly_spend_minor, version
                FROM viryaos_promotion_budget_guardrails
                WHERE workspace_id = $1
                ORDER BY currency
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(|row| PromotionBudgetGuardrailSummary {
                currency: row.currency,
                maximum_total_daily_budget_minor: row.maximum_total_daily_budget_minor,
                maximum_monthly_spend_minor: row.maximum_monthly_spend_minor,
                version: row.version,
            })
            .collect();

            let needs_you = sqlx::query_as::<_, PendingActionRow>(
                r#"
                SELECT action.id, action.context, action.action_kind, action.subject_kind,
                       action.subject_id, action.payload, action.created_at,
                       action.approval_expires_at,
                       assignment.assignee_member_id,
                       profile.member_key AS assignee_member_key,
                       member.display_name AS assignee_display_name,
                       assignment.due_at AS assignment_due_at
                FROM viryaos_autopilot_actions action
                LEFT JOIN viryaos_team_assignments assignment
                  ON assignment.workspace_id=action.workspace_id
                 AND assignment.action_id=action.id
                 AND assignment.status='open'
                LEFT JOIN viryaos_team_profiles profile
                  ON profile.workspace_id=assignment.workspace_id
                 AND profile.member_id=assignment.assignee_member_id
                LEFT JOIN workspace_members member
                  ON member.workspace_id=assignment.workspace_id
                 AND member.id=assignment.assignee_member_id
                WHERE action.workspace_id = $1
                  AND action.status = 'awaiting_approval'
                  AND (action.approval_expires_at IS NULL OR action.approval_expires_at > now())
                ORDER BY action.created_at, action.id
                LIMIT 50
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            // Read once for the whole queue rather than per row: the answer is
            // the same for every action, and asking fifty times would make an
            // exception screen the most expensive query in the cockpit.
            let live_capabilities = sqlx::query_scalar::<_, String>(
                r#"
                SELECT DISTINCT capability_row.capability
                FROM viryaos_executor_capabilities capability_row
                JOIN viryaos_executor_instances executor
                  ON executor.workspace_id=capability_row.workspace_id
                 AND executor.executor_id=capability_row.executor_id
                LEFT JOIN viryaos_executor_circuit_breakers breaker
                  ON breaker.workspace_id=executor.workspace_id
                 AND breaker.executor_id=executor.executor_id
                WHERE capability_row.workspace_id=$1
                  AND capability_row.expires_at>now()
                  AND executor.expires_at>now()
                  AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let needs_you = needs_you
                .into_iter()
                .map(|row| pending_action(row, &live_capabilities))
                .collect::<Result<Vec<_>, _>>()?;

            let available_assignees = sqlx::query_as::<_, (Uuid, String, String)>(
                r#"
                SELECT profile.member_id, profile.member_key, member.display_name
                FROM viryaos_team_profiles profile
                JOIN workspace_members member
                  ON member.workspace_id=profile.workspace_id AND member.id=profile.member_id
                WHERE profile.workspace_id=$1 AND profile.active AND member.status='active'
                ORDER BY member.display_name, profile.member_key
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(|(member_id, member_key, display_name)| TeamAssigneeSummary {
                member_id,
                member_key,
                display_name,
            })
            .collect::<Vec<_>>();

            let recent_decisions = sqlx::query_as::<_, RecentDecisionRow>(
                r#"
                SELECT id, context, decision_kind, confidence_basis_points,
                       disposition, reason, evaluated_at
                FROM viryaos_autopilot_decisions
                WHERE workspace_id = $1
                ORDER BY evaluated_at DESC, id DESC
                LIMIT 50
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(recent_decision)
            .collect::<Result<Vec<_>, _>>()?;

            let recent_actions = sqlx::query_as::<_, RecentActionRow>(
                r#"
                SELECT action.id, action.context, action.action_kind, action.subject_kind, action.subject_id, action.status,
                       action.attempt_count, action.created_at, action.finished_at, action.last_error_kind,
                       latest_report.status AS executor_status,
                       latest_report.executor_id,
                       latest_report.provider_reference,
                       latest_report.occurred_at AS executor_reported_at,
                       latest_report.metadata AS executor_metadata
                FROM viryaos_autopilot_actions action
                LEFT JOIN LATERAL (
                    SELECT report.status, report.executor_id, report.provider_reference, report.occurred_at, report.metadata
                    FROM viryaos_autopilot_execution_reports report
                    WHERE report.workspace_id=action.workspace_id AND report.action_id=action.id
                    ORDER BY report.occurred_at DESC, report.id DESC
                    LIMIT 1
                ) latest_report ON true
                WHERE action.workspace_id = $1
                ORDER BY created_at DESC, id DESC
                LIMIT 50
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(recent_action)
            .collect::<Result<Vec<_>, _>>()?;

            let recent_effects = sqlx::query_as::<_, RecentEffectRow>(
                r#"
                SELECT outcome.measurement_id, outcome.action_id, action.context,
                       measurement.measurement_kind, outcome.effect_assessment,
                       outcome.delta_basis_points, outcome.baseline_value,
                       outcome.observed_value, outcome.observed_at
                FROM viryaos_autopilot_outcomes AS outcome
                JOIN viryaos_autopilot_measurements AS measurement
                  ON measurement.workspace_id = outcome.workspace_id
                 AND measurement.id = outcome.measurement_id
                JOIN viryaos_autopilot_actions AS action
                  ON action.workspace_id = outcome.workspace_id
                 AND action.id = outcome.action_id
                WHERE outcome.workspace_id = $1
                  AND outcome.measurement_id IS NOT NULL
                ORDER BY outcome.observed_at DESC, outcome.id DESC
                LIMIT 20
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(recent_effect)
            .collect::<Result<Vec<_>, _>>()?;

            let stats = sqlx::query_as::<_, ControlStatsRow>(
                r#"
                SELECT
                    count(*) FILTER (WHERE action.status = 'queued') AS queued_actions,
                    count(*) FILTER (WHERE action.status = 'processing') AS processing_actions,
                    count(*) FILTER (
                        WHERE action.status = 'succeeded'
                          AND action.finished_at >= now() - INTERVAL '24 hours'
                          AND (
                              NOT EXISTS (
                                  SELECT 1 FROM viryaos_autopilot_action_emissions emission
                                  WHERE emission.workspace_id=action.workspace_id AND emission.action_id=action.id
                              )
                              OR EXISTS (
                                  SELECT 1 FROM viryaos_autopilot_execution_reports report
                                  WHERE report.workspace_id=action.workspace_id AND report.action_id=action.id
                                    AND report.status='succeeded'
                              )
                          )
                    ) AS succeeded_24h,
                    count(*) FILTER (
                        WHERE action.status = 'failed'
                          AND action.finished_at >= now() - INTERVAL '24 hours'
                    ) AS failed_24h,
                    (SELECT count(DISTINCT report.action_id)::bigint
                     FROM viryaos_autopilot_execution_reports report
                     WHERE report.workspace_id=$1 AND report.status='succeeded'
                       AND report.occurred_at >= now() - INTERVAL '24 hours') AS executor_confirmed_24h,
                    (SELECT count(DISTINCT report.action_id)::bigint
                     FROM viryaos_autopilot_execution_reports report
                     WHERE report.workspace_id=$1 AND report.status='failed'
                       AND report.occurred_at >= now() - INTERVAL '24 hours'
                       AND NOT EXISTS (
                           SELECT 1 FROM viryaos_autopilot_execution_reports success
                           WHERE success.workspace_id=report.workspace_id
                             AND success.action_id=report.action_id AND success.status='succeeded'
                       )) AS executor_failed_24h,
                    (SELECT count(*)::bigint
                     FROM viryaos_autopilot_action_emissions emission
                     JOIN viryaos_autopilot_actions emitted_action
                       ON emitted_action.workspace_id=emission.workspace_id AND emitted_action.id=emission.action_id
                     WHERE emission.workspace_id=$1 AND emitted_action.status='succeeded'
                       AND NOT EXISTS (
                           SELECT 1 FROM viryaos_autopilot_execution_reports report
                           WHERE report.workspace_id=emission.workspace_id AND report.action_id=emission.action_id
                             AND report.status IN ('succeeded','failed')
                       )) AS awaiting_executor
                FROM viryaos_autopilot_actions action
                WHERE action.workspace_id = $1
                  AND (
                      action.status IN ('queued','processing')
                      OR (action.status IN ('succeeded','failed')
                          AND action.finished_at >= now() - INTERVAL '24 hours')
                  )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let runtime_now = OffsetDateTime::now_utc();
            let release_ledger = AutopilotRuntimeRepository::load_release_ledger(
                self,
                workspace_id,
                runtime_now,
            )
            .await?;
            let rum_metrics_24h = AutopilotRuntimeRepository::load_rum_summaries(
                self,
                workspace_id,
                runtime_now,
            )
            .await?;

            Ok(AutopilotControlOverview {
                policies,
                promotion_budget_guardrails,
                needs_you,
                available_assignees,
                recent_decisions,
                recent_actions,
                recent_effects,
                queued_actions: stats.queued_actions,
                processing_actions: stats.processing_actions,
                succeeded_24h: stats.succeeded_24h,
                failed_24h: stats.failed_24h,
                executor_confirmed_24h: stats.executor_confirmed_24h,
                executor_failed_24h: stats.executor_failed_24h,
                awaiting_executor: stats.awaiting_executor,
                release_ledger,
                rum_metrics_24h,
            })
        })
        .await
    }

    async fn load_growth_overview(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AutopilotGrowthOverview, RepositoryError> {
        self.bounded(self.growth_overview(workspace_id, now)).await
    }

    async fn load_chief_of_staff(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<crowdrelay_application::autopilot::AutopilotChiefOfStaff, RepositoryError> {
        self.bounded(operations::load_chief_of_staff(self, workspace_id, now))
            .await
    }

    async fn load_next_best_actions(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<NextBestAction>, RepositoryError> {
        self.bounded(operations::load_next_best_actions(self, workspace_id, now))
            .await
    }

    async fn load_manager_booking_policy(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ManagerBookingPolicySummary, RepositoryError> {
        self.bounded(async {
            let row = sqlx::query_as::<
                _,
                (
                    serde_json::Value,
                    String,
                    Option<String>,
                    i64,
                    OffsetDateTime,
                ),
            >(
                r#"
                SELECT value, source, source_revision, version, synced_at
                FROM viryaos_manager_config
                WHERE workspace_id=$1 AND config_key='booking_policy'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;

            match row {
                Some((value, source, source_revision, version, synced_at)) => {
                    let policy = serde_json::from_value::<BookingManagerPolicy>(value)
                        .map_err(|_| RepositoryError::Unexpected)?;
                    if !policy.is_valid() || version <= 0 {
                        return Err(RepositoryError::Unexpected);
                    }
                    Ok(ManagerBookingPolicySummary {
                        policy,
                        source,
                        source_revision,
                        version,
                        synced_at: Some(synced_at),
                    })
                }
                None => Ok(ManagerBookingPolicySummary {
                    policy: BookingManagerPolicy::default(),
                    source: "database".to_owned(),
                    source_revision: None,
                    version: 0,
                    synced_at: None,
                }),
            }
        })
        .await
    }

    async fn load_acquisition_channels(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AcquisitionChannels, RepositoryError> {
        self.bounded(operations::load_acquisition_channels(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_tour_economics_config(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<TourEconomicsSummary, RepositoryError> {
        self.bounded(async {
            let row = sqlx::query_as::<_, (Value, i64)>(
                r#"
                SELECT to_jsonb(config) - 'workspace_id' - 'version' - 'updated_at', config.version
                FROM viryaos_tour_economics AS config
                WHERE config.workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;

            // A workspace whose row has not been provisioned reads as the timid
            // default at version zero, so an operator can save without first
            // having to discover that nothing exists.
            let Some((columns, version)) = row else {
                return Ok(TourEconomicsSummary {
                    policy: TourEconomicsPolicy::default(),
                    version: 0,
                });
            };
            Ok(TourEconomicsSummary {
                policy: tour_policy_from_columns(&columns),
                version,
            })
        })
        .await
    }

    async fn load_growth_posture(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GrowthPostureView, RepositoryError> {
        self.load_growth_posture_impl(workspace_id).await
    }

    async fn set_growth_posture(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthPosture,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.set_growth_posture_impl(workspace_id, command, idempotency_key, request_id)
            .await
    }

    async fn set_tour_economics(
        &self,
        workspace_id: WorkspaceId,
        command: SetTourEconomics,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TourEconomicsMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0 || !command.policy.is_valid() {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "policy": command.policy,
                "expected_version": command.expected_version,
            });
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_tour_economics",
                "tour_economics",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_tour_economics WHERE workspace_id=$1",
                )
                .bind(workspace_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(TourEconomicsMutation {
                    operation_id: existing,
                    version,
                    replayed: true,
                });
            }

            // Optimistic concurrency: two people editing the van in the same
            // afternoon must not silently overwrite each other's fuel price.
            let version = sqlx::query_scalar::<_, i64>(
                r#"
                UPDATE viryaos_tour_economics SET
                    transport_minor_per_100km_round_trip=$2,
                    transport_rate_covers_vehicles=$3,
                    vehicle_seats=$4,
                    vehicle_cargo_litres=$5,
                    vehicle_fuel_centilitres_per_100km=$6,
                    max_vehicles=$7,
                    crew_size=$8,
                    backline_litres=$9,
                    fuel_price_minor_per_litre=$10,
                    toll_minor_per_km=$11,
                    accommodation_minor_per_room_night=$12,
                    crew_per_room=$13,
                    per_diem_minor_per_person_day=$14,
                    fixed_overhead_minor=$15,
                    overnight_threshold_km=$16,
                    minimum_margin_minor=$17,
                    version=version+1
                WHERE workspace_id=$1 AND version=$18
                RETURNING version
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.policy.transport_minor_per_100km_round_trip)
            .bind(i16::from(command.policy.transport_rate_covers_vehicles))
            .bind(i16::from(command.policy.vehicle.seats))
            .bind(i32::try_from(command.policy.vehicle.cargo_litres).unwrap_or(i32::MAX))
            .bind(
                i32::try_from(command.policy.vehicle.fuel_centilitres_per_100km)
                    .unwrap_or(i32::MAX),
            )
            .bind(i16::from(command.policy.max_vehicles))
            .bind(i16::from(command.policy.crew_size))
            .bind(i32::try_from(command.policy.backline_litres).unwrap_or(i32::MAX))
            .bind(command.policy.fuel_price_minor_per_litre)
            .bind(command.policy.toll_minor_per_km)
            .bind(command.policy.accommodation_minor_per_room_night)
            .bind(i16::from(command.policy.crew_per_room))
            .bind(command.policy.per_diem_minor_per_person_day)
            .bind(command.policy.fixed_overhead_minor)
            .bind(i32::try_from(command.policy.overnight_threshold_km).unwrap_or(i32::MAX))
            .bind(command.policy.minimum_margin_minor)
            .bind(command.expected_version)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(TourEconomicsMutation {
                operation_id,
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn set_manager_booking_policy(
        &self,
        workspace_id: WorkspaceId,
        command: SetManagerBookingPolicy,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ManagerConfigMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || !command.policy.is_valid()
                || command.source_revision.as_ref().is_some_and(|value| value.len() > 200)
            {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "config_key": "booking_policy",
                "policy": command.policy,
                "source": command.source.as_str(),
                "source_revision": command.source_revision,
                "expected_version": command.expected_version,
            });
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_manager_booking_policy",
                "manager_config",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_manager_config WHERE workspace_id=$1 AND config_key='booking_policy'",
                )
                .bind(workspace_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(ManagerConfigMutation {
                    operation_id: existing,
                    config_key: "booking_policy".into(),
                    version,
                    replayed: true,
                });
            }
            let policy_json = serde_json::to_value(&command.policy)
                .map_err(|_| RepositoryError::Unexpected)?;
            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_manager_config(
                        workspace_id,config_key,value,source,source_revision,version,synced_at
                    ) VALUES($1,'booking_policy',$2,$3,$4,1,now())
                    ON CONFLICT (workspace_id,config_key) DO UPDATE SET
                        value=EXCLUDED.value,
                        source=EXCLUDED.source,
                        source_revision=EXCLUDED.source_revision,
                        synced_at=now(),
                        version=viryaos_manager_config.version+1
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(policy_json)
                .bind(command.source.as_str())
                .bind(command.source_revision.as_deref())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_manager_config
                    SET value=$2, source=$3, source_revision=$4,
                        synced_at=now(), version=version+1
                    WHERE workspace_id=$1 AND config_key='booking_policy' AND version=$5
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(policy_json)
                .bind(command.source.as_str())
                .bind(command.source_revision.as_deref())
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ManagerConfigMutation {
                operation_id,
                config_key: "booking_policy".into(),
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn set_growth_envelope(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthEnvelope,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "agent_enabled": command.agent_enabled,
                "dry_run": command.dry_run,
                "weekly_owned_audience_touches": command.weekly_owned_audience_touches,
                "weekly_third_party_touches": command.weekly_third_party_touches,
                "subject_cooldown_hours": command.subject_cooldown_hours,
                "max_recipients_per_step": command.max_recipients_per_step,
                "expected_version": command.expected_version,
            });
            let inserted = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_growth_envelope",
                "growth_envelope",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing) = inserted {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: workspace_id.into_uuid(),
                    status: "envelope_updated".to_owned(),
                    replayed: true,
                });
            }

            let bounded_i32 = |value: u32| -> Result<i32, RepositoryError> {
                i32::try_from(value).map_err(|_| RepositoryError::Unexpected)
            };
            let updated = sqlx::query(
                r#"
                UPDATE viryaos_growth_envelope
                SET agent_enabled = $2,
                    dry_run = $3,
                    weekly_owned_audience_touches = $4,
                    weekly_third_party_touches = $5,
                    subject_cooldown_hours = $6,
                    max_recipients_per_step = $7,
                    version = version + 1,
                    updated_at = now()
                WHERE workspace_id = $1 AND version = $8
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.agent_enabled)
            .bind(command.dry_run)
            .bind(bounded_i32(command.weekly_owned_audience_touches)?)
            .bind(bounded_i32(command.weekly_third_party_touches)?)
            .bind(bounded_i32(command.subject_cooldown_hours)?)
            .bind(bounded_i32(command.max_recipients_per_step)?)
            .bind(command.expected_version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() != 1 {
                // A workspace with no envelope row has never been configured,
                // which is a different problem from losing a version race.
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM viryaos_growth_envelope WHERE workspace_id = $1)",
                )
                .bind(workspace_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return Err(if exists {
                    RepositoryError::Conflict
                } else {
                    RepositoryError::NotFound
                });
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: workspace_id.into_uuid(),
                status: "envelope_updated".to_owned(),
                replayed: false,
            })
        })
        .await
    }

    async fn set_authority(
        &self,
        workspace_id: WorkspaceId,
        command: SetAutopilotAuthority,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "context": command.context.as_str(),
                "enabled": command.enabled,
                "autonomy_level": autonomy_level_str(command.autonomy_level),
                "minimum_confidence_basis_points": command.minimum_confidence.basis_points(),
                "max_actions_24h": command.max_actions_24h,
                "expected_version": command.expected_version,
            });
            let inserted = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_autopilot_authority",
                "autopilot_policy",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing) = inserted {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: workspace_id.into_uuid(),
                    status: "policy_updated".to_owned(),
                    replayed: true,
                });
            }

            let updated = sqlx::query(
                r#"
                UPDATE viryaos_autopilot_policies
                SET enabled = $3,
                    autonomy_level = $4,
                    minimum_confidence_basis_points = $5,
                    max_actions_24h = $6,
                    guarded_until = CASE WHEN $4 <> 'bounded_auto' OR guarded_until <= now() THEN NULL ELSE guarded_until END,
                    guardrail_reason = CASE WHEN $4 <> 'bounded_auto' OR guarded_until <= now() THEN NULL ELSE guardrail_reason END,
                    config = COALESCE($8, config),
                    version = version + 1
                WHERE workspace_id = $1 AND context = $2 AND version = $7
                  AND ($4 <> 'bounded_auto' OR guarded_until IS NULL OR guarded_until <= now())
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.context.as_str())
            .bind(command.enabled)
            .bind(autonomy_level_str(command.autonomy_level))
            .bind(i32::from(command.minimum_confidence.basis_points()))
            .bind(i32::try_from(command.max_actions_24h).map_err(|_| RepositoryError::Unexpected)?)
            .bind(command.expected_version)
            // $8: the knobs. `COALESCE($8, config)` in the statement means a
            // write without them leaves the operator's tuning untouched.
            .bind(command.config.clone())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() != 1 {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM viryaos_autopilot_policies WHERE workspace_id = $1 AND context = $2)",
                )
                .bind(workspace_id.into_uuid())
                .bind(command.context.as_str())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return Err(if exists { RepositoryError::Conflict } else { RepositoryError::NotFound });
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: workspace_id.into_uuid(),
                status: "policy_updated".to_owned(),
                replayed: false,
            })
        })
        .await
    }

    async fn assign_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        member_key: &str,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let member_key = member_key.trim().to_ascii_lowercase();
            if member_key.len() < 2
                || member_key.len() > 48
                || !member_key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'))
            {
                return Err(RepositoryError::Unexpected);
            }

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({"member_key": member_key.clone()});
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "assign_autopilot_action",
                "autopilot_action",
                action_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: action_id.into_uuid(),
                    status: "assignment_updated".to_owned(),
                    replayed: true,
                });
            }

            let action = sqlx::query_as::<_, (String, String, String, Uuid, Option<OffsetDateTime>)>(
                r#"
                SELECT context, action_kind, subject_kind, subject_id, approval_expires_at
                FROM viryaos_autopilot_actions
                WHERE workspace_id=$1 AND id=$2 AND status='awaiting_approval'
                  AND (approval_expires_at IS NULL OR approval_expires_at > now())
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;

            let member = sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                SELECT profile.member_id, profile.member_key, member.display_name, member.normalized_email
                FROM viryaos_team_profiles profile
                JOIN workspace_members member
                  ON member.workspace_id=profile.workspace_id AND member.id=profile.member_id
                WHERE profile.workspace_id=$1 AND profile.member_key=$2
                  AND profile.active AND member.status='active'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&member_key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;

            let need = super::team::assignment_need(&action.0, &action.1);
            let assignment_id = Uuid::now_v7();
            let due_at = action.4;
            let next_reminder_at = super::team::first_reminder_at(OffsetDateTime::now_utc(), due_at);
            let persisted_assignment_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_team_assignments (
                    id, workspace_id, action_id, source_kind, source_id,
                    assignee_member_id, required_skill, due_at, next_reminder_at
                ) VALUES ($1,$2,$3,'autopilot_action',$4,$5,$6,$7,$8)
                ON CONFLICT (workspace_id, action_id) DO UPDATE
                SET assignee_member_id=EXCLUDED.assignee_member_id,
                    required_skill=EXCLUDED.required_skill,
                    status='open', due_at=EXCLUDED.due_at,
                    assigned_at=now(), last_reminded_at=NULL,
                    next_reminder_at=EXCLUDED.next_reminder_at,
                    reminder_count=0, completed_at=NULL
                RETURNING id
                "#,
            )
            .bind(assignment_id)
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .bind(action.3)
            .bind(member.0)
            .bind(need.primary_skill.as_str())
            .bind(due_at)
            .bind(next_reminder_at)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            // An operator reassignment is still a real external handoff. Do not
            // commit it unless the provider-confirmed team-email executor is live.
            super::ensure_executor_capability_strict(&mut transaction, workspace_id, "team.email")
                .await?;
            super::team::queue_team_email_action(
                &mut transaction,
                workspace_id,
                persisted_assignment_id,
                &action.0,
                &member.3,
                &member.2,
                super::team::friendly_action_title(&action.1),
                format!("Wymaga Twojej decyzji w VIRYA OS: {}.", action.1),
                due_at,
                0,
                OffsetDateTime::now_utc(),
            )
            .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: action_id.into_uuid(),
                status: "assignment_updated".to_owned(),
                replayed: false,
            })
        })
        .await
    }

    async fn approve_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.control_action_transition(
            workspace_id,
            action_id,
            idempotency_key,
            request_id,
            "approve_autopilot_action",
            "queued",
        )
        .await
    }

    async fn cancel_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.control_action_transition(
            workspace_id,
            action_id,
            idempotency_key,
            request_id,
            "cancel_autopilot_action",
            "cancelled",
        )
        .await
    }

    async fn approve_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        wave_id: Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.approve_outreach_wave_operator(workspace_id, wave_id, idempotency_key, request_id)
            .await
    }

    async fn mark_decision_handled_externally(
        &self,
        workspace_id: WorkspaceId,
        decision_id: AutopilotDecisionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.mark_decision_handled_operator(workspace_id, decision_id, idempotency_key, request_id)
            .await
    }
}
