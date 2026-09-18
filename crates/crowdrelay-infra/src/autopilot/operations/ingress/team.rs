//! Team opportunity ingress and progress tracking.

use std::collections::BTreeMap;

use crowdrelay_domain::venue_terms::{
    TermsContribution, VenueTermsEvidence, aggregate_venue_terms,
};

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
                "tier": command.tier.map(|tier| tier.as_str()),
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
                "admin_api_key",
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
                        tier, active, assets_ready, communication_enabled, press_enabled
                    ) VALUES($1,$2,$3,$4,$5,$6,COALESCE($7,'track'),$8,$9,$10,$11)
                    RETURNING version
                    "#,
                )
                .bind(release_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(command.source_key.trim())
                .bind(command.title.trim())
                .bind(command.release_at)
                .bind(command.listen_url.as_deref())
                .bind(command.tier.map(|tier| tier.as_str()))
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
                        tier=COALESCE($6, tier),
                        active=$7,
                        assets_ready=$8,
                        communication_enabled=$9,
                        press_enabled=$10,
                        version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$11
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(release_id.into_uuid())
                .bind(command.title.trim())
                .bind(command.release_at)
                .bind(command.listen_url.as_deref())
                .bind(command.tier.map(|tier| tier.as_str()))
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
                || command.strategic_value_basis_points > 10_000
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
                "admin_api_key",
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
                        travel_band, metadata, strategic_value_basis_points,
                        source_observed_at
                    ) VALUES(
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
                        $13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,
                        $25,$26,$27,$28,$29
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
                .bind(i32::from(command.strategic_value_basis_points))
                .bind(command.source_observed_at)
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
                        strategic_value_basis_points=$26,
                        source_observed_at=$28,
                        version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$27
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
                .bind(i32::from(command.strategic_value_basis_points))
                .bind(expected)
                .bind(command.source_observed_at)
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

    async fn record_delivery_fault(
        &self,
        workspace_id: WorkspaceId,
        command: RecordDeliveryFault,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.record_delivery_fault_operator(workspace_id, command, idempotency_key, request_id)
            .await
    }

    async fn complete_editorial_pitch(
        &self,
        workspace_id: WorkspaceId,
        release_id: ReleasePlanId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "complete_autopilot_editorial_pitch",
                "release_plan",
                release_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"release_id": release_id}),
            )
            .await?
            {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: release_id.into_uuid(),
                    status: "submitted".into(),
                    replayed: true,
                });
            }
            // Guarded on it not already being marked: the first person to say
            // so is the record, and a second click is not a second submission.
            let changed = sqlx::query(
                "UPDATE viryaos_release_plans SET editorial_pitch_completed_at=now(), \
                 version=version+1 \
                 WHERE workspace_id=$1 AND id=$2 AND editorial_pitch_completed_at IS NULL",
            )
            .bind(workspace_id.into_uuid())
            .bind(release_id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if changed.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }
            tx.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: release_id.into_uuid(),
                status: "submitted".into(),
                replayed: false,
            })
        })
        .await
    }

    async fn record_playlist_placement(
        &self,
        workspace_id: WorkspaceId,
        command: RecordPlaylistPlacement,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.record_playlist_placement_operator(workspace_id, command, idempotency_key, request_id)
            .await
    }

    async fn record_team_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        command: RecordTeamOpportunityTerms,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        let offered_fee_minor = match command.position {
            PromoterPosition::Offer { fee_minor } => fee_minor,
            PromoterPosition::Withdrawn => 0,
        };
        if offered_fee_minor < 0 || !valid_opportunity_currency(&command.currency) {
            return Err(RepositoryError::Conflict);
        }
        // An offer is priced against the ladder; a withdrawal only settles
        // whatever is live. The reads below exist to build that ladder, so
        // they run for an offer and only for an offer — a withdrawal on an
        // opportunity that has already moved on still reaches the settle.
        let ladder_inputs = if matches!(command.position, PromoterPosition::Offer { .. }) {
            // The ladder needs the show as the agent sees it — costed trip,
            // travel band, how full the year is — so it is read through the
            // same statement the evaluator uses rather than rebuilt from the
            // row. Two ways of costing the same trip is how a floor and a
            // verdict come to disagree.
            let policy = self.live_opportunity_policy(workspace_id).await?;
            let snapshot = self
                .load_live_opportunity_snapshots_for(
                    workspace_id,
                    OffsetDateTime::now_utc(),
                    &["submitted", "replied"],
                )
                .await?
                .into_iter()
                .find(|snapshot| snapshot.opportunity_id == command.opportunity_id)
                .ok_or(RepositoryError::NotFound)?;
            // What this counterparty last agreed to pay, in the currency this
            // negotiation is being conducted in: an accepted terms row's
            // agreed fee is the offer that was accepted — our own unanswered
            // counter on that row was never a deal. The counterparty is the
            // organization, matched by either of its calling cards — the
            // contact email, or the organization name.
            let prior_fee_minor = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT t.offered_fee_minor
                FROM viryaos_team_opportunity_terms t
                JOIN viryaos_team_opportunities o
                  ON o.workspace_id = t.workspace_id AND o.id = t.opportunity_id
                JOIN viryaos_team_opportunities self_o
                  ON self_o.workspace_id = t.workspace_id AND self_o.id = $2
                WHERE t.workspace_id = $1
                  AND t.state = 'accepted'
                  AND t.opportunity_id <> $2
                  AND t.currency = self_o.currency
                  AND (
                       (self_o.contact_email IS NOT NULL AND o.contact_email IS NOT NULL
                        AND lower(o.contact_email) = lower(self_o.contact_email))
                       OR lower(btrim(o.organization)) = lower(btrim(self_o.organization))
                  )
                ORDER BY t.settled_at DESC NULLS LAST, t.updated_at DESC
                LIMIT 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.opportunity_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let market_floor_minor = self
                .market_floor_minor(workspace_id, command.opportunity_id)
                .await?;
            Some((
                terms_ladder(
                    snapshot,
                    policy,
                    snapshot.estimated_cost_minor,
                    prior_fee_minor.unwrap_or(0),
                    market_floor_minor,
                ),
                prior_fee_minor,
                market_floor_minor,
            ))
        } else {
            None
        };

        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let position = match command.position {
                PromoterPosition::Offer { .. } => "offer",
                PromoterPosition::Withdrawn => "withdrawn",
            };
            let details = json!({
                "opportunity_id": command.opportunity_id,
                "position": position,
                "offered_fee_minor": offered_fee_minor,
                "currency": command.currency,
                "responds_by": command.responds_by,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "record_autopilot_team_opportunity_terms",
                "team_opportunity",
                command.opportunity_id.into_uuid(),
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
                    target_id: command.opportunity_id.into_uuid(),
                    status: position.into(),
                    replayed: true,
                });
            }

            let changed = match command.position {
                // A withdrawal settles whatever was live. Nothing is opened:
                // recording that somebody walked away from a conversation that
                // never started would be inventing the conversation.
                PromoterPosition::Withdrawn => sqlx::query(
                    "UPDATE viryaos_team_opportunity_terms \
                     SET state='declined', settled_at=$3, settled_reason='promoter_withdrew', \
                         version=version+1 \
                     WHERE workspace_id=$1 AND opportunity_id=$2 AND settled_at IS NULL",
                )
                .bind(workspace_id.into_uuid())
                .bind(command.opportunity_id.into_uuid())
                .bind(OffsetDateTime::now_utc())
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?,
                // The ladder is written on insert and left alone on update. An
                // improved offer moves the state back to `proposed` so the
                // agent looks again; it does not reset the numbers the last
                // counter was argued from, and it does not reset the round
                // count, which is what stops a promoter nudging their offer up
                // by a złoty to buy another ask.
                PromoterPosition::Offer { .. } => {
                    let (ladder, prior_fee_minor, market_floor_minor) =
                        ladder_inputs.ok_or(RepositoryError::Unexpected)?;
                    sqlx::query(
                        r#"
                        INSERT INTO viryaos_team_opportunity_terms (
                            workspace_id, opportunity_id, state, currency, offered_fee_minor,
                            walk_away_minor, target_minor, opening_ask_minor, floor_basis,
                            prior_fee_minor, market_floor_minor, responds_by
                        ) VALUES ($1,$2,'proposed',$3,$4,$5,$6,$7,$8,$9,$10,$11)
                        ON CONFLICT (workspace_id, opportunity_id) DO UPDATE SET
                            state='proposed',
                            offered_fee_minor=EXCLUDED.offered_fee_minor,
                            responds_by=EXCLUDED.responds_by,
                            version=viryaos_team_opportunity_terms.version+1
                        WHERE viryaos_team_opportunity_terms.settled_at IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(command.opportunity_id.into_uuid())
                    .bind(&command.currency)
                    .bind(offered_fee_minor)
                    .bind(ladder.walk_away_minor)
                    .bind(ladder.target_minor)
                    .bind(ladder.opening_ask_minor)
                    .bind(ladder.floor_basis.as_str())
                    .bind(prior_fee_minor)
                    .bind(market_floor_minor)
                    .bind(command.responds_by)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_sqlx)?
                }
            };
            if changed.rows_affected() != 1 {
                // A settled negotiation is not reopened by another offer. That
                // is an operator deliberately starting a new conversation, and
                // it is theirs to say so.
                return Err(RepositoryError::Conflict);
            }

            tx.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.opportunity_id.into_uuid(),
                status: position.into(),
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
            let operation_id = Uuid::now_v7();
            let progress = match command.progress {
                TeamOpportunityProgress::PackageReady => "package_ready",
                TeamOpportunityProgress::Submitted => "submitted",
                TeamOpportunityProgress::Replied => "replied",
                TeamOpportunityProgress::Won => "won",
                TeamOpportunityProgress::Lost => "lost",
                TeamOpportunityProgress::Dismissed => "dismissed",
            };
            // A refusal is a finding, not a disposal: `lost` and `dismissed`
            // must say why, or the same dead lead is scouted again next cycle.
            // `won` needs none — the result is its own reason.
            let reason = command.reason.as_deref().map(str::trim);
            if matches!(
                command.progress,
                TeamOpportunityProgress::Lost | TeamOpportunityProgress::Dismissed
            ) && reason.is_none_or(|value| value.is_empty() || value.len() > 240)
            {
                return Err(RepositoryError::ConflictBecause(
                    "lost and dismissed require a 1–240 character reason — a row that closes says why",
                ));
            }
            let details = json!({
                "opportunity_id": command.opportunity_id,
                "progress": progress,
                "occurred_at": command.occurred_at,
                "reason": reason,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "record_autopilot_team_opportunity_progress",
                "team_opportunity",
                command.opportunity_id.into_uuid(),
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
                    target_id: command.opportunity_id.into_uuid(),
                    status: progress.into(),
                    replayed: true,
                });
            }

            // Terminal writes carry the reason into `status_reason`; live
            // transitions leave it alone rather than nulling a reason an
            // earlier close recorded.
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
                // Replied/won/lost are thread outcomes — they only exist
                // once a submission was requested or confirmed. Marking a
                // never-sent opportunity 'won' would fabricate a send, a
                // reply and a win in the cross-tenant prior.
                TeamOpportunityProgress::Replied => {
                    "UPDATE viryaos_team_opportunities SET status='replied', version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 \
                       AND status IN ('submission_requested','submitted','replied')"
                }
                TeamOpportunityProgress::Won => {
                    "UPDATE viryaos_team_opportunities SET status='won', status_reason=$3, version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 \
                       AND status IN ('submission_requested','submitted','replied')"
                }
                TeamOpportunityProgress::Lost => {
                    "UPDATE viryaos_team_opportunities SET status='lost', status_reason=$3, version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 \
                       AND status IN ('submission_requested','submitted','replied')"
                }
                // Dismissal is for pre-send cleanup only — once a send is
                // confirmed ('submitted') or answered ('replied'), the
                // thread is evidence and the honest end states are
                // won/lost. Dismissing it would erase the send record.
                TeamOpportunityProgress::Dismissed => {
                    "UPDATE viryaos_team_opportunities SET status='dismissed', status_reason=$3, version=version+1 \
                     WHERE workspace_id=$1 AND id=$2 \
                       AND status IN ('new','prepared','awaiting_approval','submission_requested')"
                }
            };
            let mut query = sqlx::query(sql)
                .bind(workspace_id.into_uuid())
                .bind(command.opportunity_id.into_uuid());
            if matches!(
                command.progress,
                TeamOpportunityProgress::Won
                    | TeamOpportunityProgress::Lost
                    | TeamOpportunityProgress::Dismissed
            ) {
                query = query.bind(reason);
            }
            let changed = query.execute(&mut *tx).await.map_err(map_sqlx)?;
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

impl PostgresAutopilotRepository {
    /// The market floor behind one opportunity's counterparty (§4h-9): the
    /// lowest fee band that holds at every room the workspace's own booking
    /// graph ties them to, in the opportunity's own currency.
    ///
    /// The counterparty is identified by the opportunity row's own fields: a
    /// `promoter` target matches on contact email or display name, and when
    /// the counterparty's organization is itself a room the workspace tracks,
    /// the `venue` target *is* the counterparty — a promoter books rooms, a
    /// venue target names one. A matched target's rooms are the union of its
    /// primary `venue_id` link (0296) and the promoter↔venue edge table
    /// (0313).
    ///
    /// The floor is the MIN of each cleared venue's `fee_p25_minor` — the
    /// band that is true for every room the counterparty works, so it can
    /// never overstate the one this show is at. `None` is the honest answer
    /// when the graph does not know the counterparty, when the matched
    /// targets name no rooms, or when no room clears the contributor floor
    /// in the opportunity's currency — the floor is silent, never zero.
    async fn market_floor_minor(
        &self,
        workspace_id: WorkspaceId,
        opportunity_id: TeamOpportunityId,
    ) -> Result<Option<i64>, RepositoryError> {
        let venue_ids = sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH matched AS (
                SELECT target.id, target.venue_id
                FROM viryaos_booking_targets AS target
                JOIN viryaos_team_opportunities AS opportunity
                  ON opportunity.workspace_id = target.workspace_id
                 AND opportunity.id = $2
                WHERE target.workspace_id = $1
                  AND (
                       (target.target_kind = 'promoter' AND (
                            (opportunity.contact_email IS NOT NULL
                             AND lower(target.contact_email) = lower(opportunity.contact_email))
                            OR lower(btrim(target.display_name)) = lower(btrim(opportunity.organization))
                       ))
                       OR (target.target_kind = 'venue'
                           AND lower(btrim(target.display_name)) = lower(btrim(opportunity.organization)))
                  )
            )
            SELECT venue_id FROM matched WHERE venue_id IS NOT NULL
            UNION
            SELECT edge.venue_id
            FROM viryaos_booking_target_venues AS edge
            JOIN matched ON matched.id = edge.target_id
            WHERE edge.workspace_id = $1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(opportunity_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        if venue_ids.is_empty() {
            return Ok(None);
        }
        // The currency the negotiation is conducted in is the opportunity
        // row's own — the terms contribution vocabulary bands per currency
        // and never converts, so the band must be the same unit the floor is
        // quoted in.
        let currency = sqlx::query_scalar::<_, String>(
            "SELECT currency FROM viryaos_team_opportunities \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(opportunity_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let contributions = crate::night::PostgresNightRepository::new(self.pool.clone())
            .venue_terms_contributions(&venue_ids)
            .await
            .map_err(|error| match error {
                crate::night::NightError::Database(inner) => map_sqlx(inner),
                crate::night::NightError::NotFound => RepositoryError::Unexpected,
            })?;
        let mut by_venue: BTreeMap<Uuid, Vec<TermsContribution>> = BTreeMap::new();
        for row in contributions {
            by_venue.entry(row.venue_id).or_default().push((
                row.workspace_id,
                row.amount_minor,
                row.currency,
                row.contributed_at,
            ));
        }
        let now = OffsetDateTime::now_utc();
        Ok(by_venue
            .values()
            .filter_map(|rows| {
                aggregate_venue_terms(rows, now)
                    .into_iter()
                    .find_map(|evidence| match evidence {
                        VenueTermsEvidence::Band {
                            currency: band_currency,
                            fee_p25_minor,
                            ..
                        } if band_currency == currency => Some(fee_p25_minor),
                        _ => None,
                    })
            })
            .min())
    }
}

fn valid_opportunity_currency(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}
