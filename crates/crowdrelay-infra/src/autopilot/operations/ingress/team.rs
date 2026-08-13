//! Team opportunity ingress and progress tracking.

use super::*;

#[async_trait]
impl AutopilotTeamStateRepository for PostgresAutopilotRepository {
    async fn upsert_release_plan(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertReleasePlan,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ReleasePlanMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || command.source_key.trim().is_empty()
                || command.title.trim().is_empty()
            {
                return Err(RepositoryError::Unexpected);
            }
            if command.expected_version > 0 && command.release_id.is_none() {
                return Err(RepositoryError::Conflict);
            }

            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            super::configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout)
                .await?;

            let natural = sqlx::query_as::<_, (Uuid, i64)>(
                "SELECT id, version FROM viryaos_release_plans \
                 WHERE workspace_id=$1 AND source_key=$2 FOR UPDATE",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.source_key.trim())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let release_id = match (command.release_id, natural) {
                (Some(requested), Some((persisted, _))) if requested.into_uuid() != persisted => {
                    return Err(RepositoryError::Conflict);
                }
                (Some(requested), _) => requested,
                (None, Some((persisted, _))) => ReleasePlanId::from_uuid(persisted),
                (None, None) => ReleasePlanId::from_uuid(operation_id),
            };

            let details = json!({
                "release_id": release_id,
                "source_key": &command.source_key,
                "title": &command.title,
                "release_at": command.release_at,
                "listen_url": &command.listen_url,
                "active": command.active,
                "assets_ready": command.assets_ready,
                "communication_enabled": command.communication_enabled,
                "press_enabled": command.press_enabled,
                "expected_version": command.expected_version,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "upsert_autopilot_release_plan",
                "release_plan",
                release_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_release_plans WHERE workspace_id=$1 AND id=$2",
                )
                .bind(workspace_id.into_uuid())
                .bind(release_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(ReleasePlanMutation {
                    operation_id: existing,
                    release_id,
                    version,
                    replayed: true,
                });
            }

            let version = if command.expected_version == 0 && natural.is_none() {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_release_plans(
                        id, workspace_id, source_key, title, release_at, listen_url,
                        active, assets_ready, communication_enabled, press_enabled
                    ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                    RETURNING version
                    "#,
                )
                .bind(release_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(command.source_key.trim())
                .bind(command.title.trim())
                .bind(command.release_at)
                .bind(command.listen_url.as_deref())
                .bind(command.active)
                .bind(command.assets_ready)
                .bind(command.communication_enabled)
                .bind(command.press_enabled)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx)?
            } else {
                let expected = if command.expected_version == 0 {
                    natural.map_or(0, |(_, version)| version)
                } else {
                    command.expected_version
                };
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_release_plans
                    SET title=$3,
                        release_at=$4,
                        listen_url=$5,
                        active=$6,
                        assets_ready=$7,
                        communication_enabled=$8,
                        press_enabled=$9,
                        version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$10
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(release_id.into_uuid())
                .bind(command.title.trim())
                .bind(command.release_at)
                .bind(command.listen_url.as_deref())
                .bind(command.active)
                .bind(command.assets_ready)
                .bind(command.communication_enabled)
                .bind(command.press_enabled)
                .bind(expected)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            tx.commit().await.map_err(map_sqlx)?;
            Ok(ReleasePlanMutation {
                operation_id,
                release_id,
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn upsert_team_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTeamOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TeamOpportunityMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || command.source.trim().is_empty()
                || command.external_key.trim().is_empty()
                || command.title.trim().is_empty()
                || command.organization.trim().is_empty()
                || command.fit_basis_points > 10_000
                || command.reputation_basis_points > 10_000
                || !valid_opportunity_currency(&command.currency)
                || command.expected_fee_minor < 0
                || command.estimated_cost_minor < 0
                || command.application_fee_minor < 0
                || command.funding_amount_minor < 0
                || command.own_contribution_minor < 0
                || !command.metadata.is_object()
                || command.country_code.as_ref().is_some_and(|code| {
                    code.len() != 2 || !code.bytes().all(|byte| byte.is_ascii_uppercase())
                })
                || (matches!(command.kind, TeamOpportunityKind::Funding)
                    && command.deadline.is_none())
            {
                return Err(RepositoryError::Unexpected);
            }
            if command.expected_version > 0 && command.opportunity_id.is_none() {
                return Err(RepositoryError::Conflict);
            }

            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            super::configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout).await?;

            let natural = sqlx::query_as::<_, (Uuid, i64)>(
                "SELECT id, version FROM viryaos_team_opportunities \
                 WHERE workspace_id=$1 AND source=$2 AND external_key=$3 FOR UPDATE",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.source.trim())
            .bind(command.external_key.trim())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let opportunity_id = match (command.opportunity_id, natural) {
                (Some(requested), Some((persisted, _))) if requested.into_uuid() != persisted => {
                    return Err(RepositoryError::Conflict);
                }
                (Some(requested), _) => requested,
                (None, Some((persisted, _))) => TeamOpportunityId::from_uuid(persisted),
                (None, None) => TeamOpportunityId::from_uuid(operation_id),
            };

            let details = json!({
                "opportunity_id": opportunity_id,
                "kind": command.kind,
                "source": &command.source,
                "external_key": &command.external_key,
                "title": &command.title,
                "organization": &command.organization,
                "currency": &command.currency,
                "verified_destination": command.verified_destination,
                "fit_basis_points": command.fit_basis_points,
                "reputation_basis_points": command.reputation_basis_points,
                "confidence_basis_points": command.confidence.basis_points(),
                "expected_fee_minor": command.expected_fee_minor,
                "estimated_cost_minor": command.estimated_cost_minor,
                "application_fee_minor": command.application_fee_minor,
                "requires_contract": command.requires_contract,
                "exclusive": command.exclusive,
                "eligible": command.eligible,
                "funding_amount_minor": command.funding_amount_minor,
                "own_contribution_minor": command.own_contribution_minor,
                "deadline": command.deadline,
                "event_starts_at": command.event_starts_at,
                "country_code": command.country_code,
                "travel_band": command.travel_band.map(|band| band.as_str()),
                "expected_version": command.expected_version,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "upsert_autopilot_team_opportunity",
                "team_opportunity",
                opportunity_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_team_opportunities WHERE workspace_id=$1 AND id=$2",
                )
                .bind(workspace_id.into_uuid())
                .bind(opportunity_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(TeamOpportunityMutation {
                    operation_id: existing,
                    opportunity_id,
                    version,
                    replayed: true,
                });
            }

            let version = if command.expected_version == 0 && natural.is_none() {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_team_opportunities(
                        id, workspace_id, opportunity_kind, source, external_key, title,
                        organization, destination_url, contact_email, verified_destination,
                        fit_basis_points, reputation_basis_points, confidence_basis_points,
                        currency, expected_fee_minor, estimated_cost_minor, application_fee_minor,
                        requires_contract, exclusive, eligible, funding_amount_minor,
                        own_contribution_minor, deadline, event_starts_at, country_code,
                        travel_band, metadata
                    ) VALUES(
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
                        $13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,
                        $25,$26,$27
                    )
                    RETURNING version
                    "#,
                )
                .bind(opportunity_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(command.kind.as_str())
                .bind(command.source.trim())
                .bind(command.external_key.trim())
                .bind(command.title.trim())
                .bind(command.organization.trim())
                .bind(command.destination_url.as_deref())
                .bind(command.contact_email.as_deref())
                .bind(command.verified_destination)
                .bind(i32::from(command.fit_basis_points))
                .bind(i32::from(command.reputation_basis_points))
                .bind(i32::from(command.confidence.basis_points()))
                .bind(&command.currency)
                .bind(command.expected_fee_minor)
                .bind(command.estimated_cost_minor)
                .bind(command.application_fee_minor)
                .bind(command.requires_contract)
                .bind(command.exclusive)
                .bind(command.eligible)
                .bind(command.funding_amount_minor)
                .bind(command.own_contribution_minor)
                .bind(command.deadline)
                .bind(command.event_starts_at)
                .bind(command.country_code.as_deref())
                .bind(command.travel_band.map(|band| band.as_str()))
                .bind(&command.metadata)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx)?
            } else {
                let expected = if command.expected_version == 0 {
                    natural.map_or(0, |(_, version)| version)
                } else {
                    command.expected_version
                };
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_team_opportunities
                    SET opportunity_kind=$3,
                        title=$4,
                        organization=$5,
                        destination_url=$6,
                        contact_email=$7,
                        verified_destination=$8,
                        fit_basis_points=$9,
                        reputation_basis_points=$10,
                        confidence_basis_points=$11,
                        currency=$12,
                        expected_fee_minor=$13,
                        estimated_cost_minor=$14,
                        application_fee_minor=$15,
                        requires_contract=$16,
                        exclusive=$17,
                        eligible=$18,
                        funding_amount_minor=$19,
                        own_contribution_minor=$20,
                        deadline=$21,
                        event_starts_at=$22,
                        country_code=$23,
                        travel_band=$24,
                        metadata=$25,
                        version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$26
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(opportunity_id.into_uuid())
                .bind(command.kind.as_str())
                .bind(command.title.trim())
                .bind(command.organization.trim())
                .bind(command.destination_url.as_deref())
                .bind(command.contact_email.as_deref())
                .bind(command.verified_destination)
                .bind(i32::from(command.fit_basis_points))
                .bind(i32::from(command.reputation_basis_points))
                .bind(i32::from(command.confidence.basis_points()))
                .bind(&command.currency)
                .bind(command.expected_fee_minor)
                .bind(command.estimated_cost_minor)
                .bind(command.application_fee_minor)
                .bind(command.requires_contract)
                .bind(command.exclusive)
                .bind(command.eligible)
                .bind(command.funding_amount_minor)
                .bind(command.own_contribution_minor)
                .bind(command.deadline)
                .bind(command.event_starts_at)
                .bind(command.country_code.as_deref())
                .bind(command.travel_band.map(|band| band.as_str()))
                .bind(&command.metadata)
                .bind(expected)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            tx.commit().await.map_err(map_sqlx)?;
            Ok(TeamOpportunityMutation {
                operation_id,
                opportunity_id,
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn record_team_opportunity_progress(
        &self,
        workspace_id: WorkspaceId,
        command: RecordTeamOpportunityProgress,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            super::configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout)
                .await?;
            let operation_id = Uuid::now_v7();
            let progress = match command.progress {
                TeamOpportunityProgress::PackageReady => "package_ready",
                TeamOpportunityProgress::Submitted => "submitted",
                TeamOpportunityProgress::Replied => "replied",
                TeamOpportunityProgress::Won => "won",
                TeamOpportunityProgress::Lost => "lost",
                TeamOpportunityProgress::Dismissed => "dismissed",
            };
            let details = json!({
                "opportunity_id": command.opportunity_id,
                "progress": progress,
                "occurred_at": command.occurred_at,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "record_autopilot_team_opportunity_progress",
                "team_opportunity",
                command.opportunity_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: command.opportunity_id.into_uuid(),
                    status: progress.into(),
                    replayed: true,
                });
            }

            let sql = match command.progress {
                TeamOpportunityProgress::PackageReady => {
                    "UPDATE viryaos_team_opportunities \
                     SET package_status='ready', status='prepared', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND opportunity_kind='funding' \
                       AND package_status='requested'"
                }
                TeamOpportunityProgress::Submitted => {
                    "UPDATE viryaos_team_opportunities SET status='submitted', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND status='submission_requested'"
                }
                TeamOpportunityProgress::Replied => {
                    "UPDATE viryaos_team_opportunities SET status='replied', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND status NOT IN ('won','lost','dismissed')"
                }
                TeamOpportunityProgress::Won => {
                    "UPDATE viryaos_team_opportunities SET status='won', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND status NOT IN ('won','lost','dismissed')"
                }
                TeamOpportunityProgress::Lost => {
                    "UPDATE viryaos_team_opportunities SET status='lost', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND status NOT IN ('won','lost','dismissed')"
                }
                TeamOpportunityProgress::Dismissed => {
                    "UPDATE viryaos_team_opportunities SET status='dismissed', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 AND status NOT IN ('won','lost','dismissed')"
                }
            };
            let changed = sqlx::query(sql)
                .bind(workspace_id.into_uuid())
                .bind(command.opportunity_id.into_uuid())
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            if changed.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }

            tx.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.opportunity_id.into_uuid(),
                status: progress.into(),
                replayed: false,
            })
        })
        .await
    }
}

fn valid_opportunity_currency(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}
