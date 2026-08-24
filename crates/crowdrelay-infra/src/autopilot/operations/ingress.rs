//! Audited operator/provider ingress adapters.

use super::*;

use async_trait::async_trait;
use crowdrelay_application::autopilot::{
    AutopilotBeaconStateRepository, AutopilotContentStateRepository, AutopilotControlMutation,
    AutopilotExperimentStateRepository, AutopilotOutreachStateRepository,
    AutopilotTeamStateRepository, BeaconMutation, ContentSourceMutation, CreateExperiment,
    ExperimentAssignmentSource, ExperimentAssignmentVariant, ExperimentMutation,
    ExperimentObservation, OutreachOpportunityMutation, OutreachTargetMutation, PromoterPosition,
    RecordBeaconReply, RecordDeliveryFault, RecordOutreachReply, RecordPlaylistPlacement,
    RecordTeamOpportunityProgress, RecordTeamOpportunityTerms, ReleasePlanMutation,
    TeamOpportunityKind, TeamOpportunityMutation, TeamOpportunityProgress, UpsertBeacon,
    UpsertContentSource, UpsertOutreachOpportunity, UpsertOutreachTarget, UpsertReleasePlan,
    UpsertTeamOpportunity,
};
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_domain::{
    BeaconId, CityId, ExperimentVariantId, ReleasePlanId, TeamOpportunityId,
    experimentation::ExperimentAllocationSlot, negotiation::terms_ladder,
};

mod beacons;
mod booking_discovery;
mod team;

#[async_trait]
impl AutopilotOutreachStateRepository for PostgresAutopilotRepository {
    async fn upsert_outreach_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachTarget,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachTargetMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || command.priority > 100
                || command.relationship_score > 100
                || command.display_name.trim().is_empty()
                || command.contact_email.trim().is_empty()
            {
                return Err(RepositoryError::Unexpected);
            }
            if command.expected_version > 0 && command.target_id.is_none() {
                return Err(RepositoryError::Conflict);
            }
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let target_id = command.target_id.unwrap_or_else(|| OutreachTargetId::from_uuid(operation_id));
            let details = json!({
                "target_id": command.target_id,
                "target_kind": outreach_target_kind_str(command.kind),
                "display_name": &command.display_name,
                "contact_email": command.contact_email.trim().to_ascii_lowercase(),
                "priority": command.priority,
                "relationship_score": command.relationship_score,
                "active": command.active,
                "verified": command.verified,
                "accepts_outreach": command.accepts_outreach,
                "do_not_contact": command.do_not_contact,
                "expected_version": command.expected_version,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx, workspace_id, operation_id, "upsert_autopilot_outreach_target",
                "outreach_target", target_id.into_uuid(), idempotency_key, request_id, &details,
            ).await? {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachTargetMutation {
                    operation_id: existing,
                    target_id,
                    version: command.expected_version.saturating_add(1).max(1),
                    replayed: true,
                });
            }
            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(r#"
                    INSERT INTO viryaos_outreach_targets(
                        id,workspace_id,target_kind,display_name,contact_email,priority,
                        relationship_score,active,verified,accepts_outreach,do_not_contact
                    ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                    RETURNING version
                "#)
                .bind(target_id.into_uuid()).bind(workspace_id.into_uuid())
                .bind(outreach_target_kind_str(command.kind)).bind(command.display_name.trim())
                .bind(command.contact_email.trim().to_ascii_lowercase()).bind(i32::from(command.priority))
                .bind(i32::from(command.relationship_score)).bind(command.active).bind(command.verified)
                .bind(command.accepts_outreach).bind(command.do_not_contact)
                .fetch_one(&mut *tx).await.map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, i64>(r#"
                    UPDATE viryaos_outreach_targets
                    SET target_kind=$3,display_name=$4,contact_email=$5,priority=$6,
                        relationship_score=$7,active=$8,verified=$9,accepts_outreach=$10,
                        do_not_contact=$11,version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$12
                    RETURNING version
                "#)
                .bind(workspace_id.into_uuid()).bind(target_id.into_uuid())
                .bind(outreach_target_kind_str(command.kind)).bind(command.display_name.trim())
                .bind(command.contact_email.trim().to_ascii_lowercase()).bind(i32::from(command.priority))
                .bind(i32::from(command.relationship_score)).bind(command.active).bind(command.verified)
                .bind(command.accepts_outreach).bind(command.do_not_contact).bind(command.expected_version)
                .fetch_optional(&mut *tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)?
            };
            sqlx::query(r#"
                INSERT INTO viryaos_outreach_target_history(workspace_id,target_id,version,snapshot)
                SELECT workspace_id,id,version,jsonb_build_object(
                    'target_kind',target_kind,'display_name',display_name,'contact_email',contact_email,
                    'priority',priority,'relationship_score',relationship_score,'active',active,
                    'verified',verified,'accepts_outreach',accepts_outreach,'do_not_contact',do_not_contact)
                FROM viryaos_outreach_targets WHERE workspace_id=$1 AND id=$2 AND version=$3
            "#).bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(version)
              .execute(&mut *tx).await.map_err(map_sqlx)?;
            tx.commit().await.map_err(map_sqlx)?;
            Ok(OutreachTargetMutation { operation_id, target_id, version, replayed: false })
        }).await
    }

    async fn upsert_outreach_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachOpportunityMutation, RepositoryError> {
        self.bounded(async {
            if command.source.trim().is_empty()
                || command.subject_key.trim().is_empty()
                || command.template_key.trim().is_empty()
                || command.expires_at <= command.observed_at
                || command.relevance_basis_points > 10_000
            {
                return Err(RepositoryError::Unexpected);
            }
            if !matches!(command.subject_kind.as_str(), "release" | "event" | "catalogue" | "band") {
                return Err(RepositoryError::Unexpected);
            }

            let source = command.source.trim();
            let subject_kind = command.subject_kind.trim();
            let subject_key = command.subject_key.trim();
            let template_key = command.template_key.trim();
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

            // Resolve the natural-key row before creating the operator audit record.
            // This makes idempotent replays return the actual persisted opportunity id
            // instead of a new UUID generated for the replaying request.
            let natural_row = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
                r#"
                SELECT id, observed_at
                FROM viryaos_outreach_opportunities
                WHERE workspace_id = $1
                  AND source = $2
                  AND target_id = $3
                  AND subject_kind = $4
                  AND subject_key = $5
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(source)
            .bind(command.target_id.into_uuid())
            .bind(subject_kind)
            .bind(subject_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let opportunity_id = match (command.opportunity_id, natural_row.as_ref()) {
                (Some(requested), Some((persisted, _))) if requested.into_uuid() != *persisted => {
                    return Err(RepositoryError::Conflict);
                }
                (Some(requested), _) => requested,
                (None, Some((persisted, _))) => OutreachOpportunityId::from_uuid(*persisted),
                (None, None) => OutreachOpportunityId::from_uuid(operation_id),
            };
            let details = json!({
                "opportunity_id": opportunity_id,
                "target_id": command.target_id,
                "source": source,
                "subject_kind": subject_kind,
                "subject_key": subject_key,
                "template_key": template_key,
                "relevance_basis_points": command.relevance_basis_points,
                "confidence_basis_points": command.confidence.basis_points(),
                "active": command.active,
                "observed_at": command.observed_at,
                "expires_at": command.expires_at,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "upsert_autopilot_outreach_opportunity",
                "outreach_opportunity",
                opportunity_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachOpportunityMutation {
                    operation_id: existing,
                    opportunity_id,
                    replayed: true,
                });
            }

            if natural_row
                .as_ref()
                .is_some_and(|(_, observed_at)| command.observed_at < *observed_at)
            {
                return Err(RepositoryError::Conflict);
            }
            let target_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM viryaos_outreach_targets WHERE workspace_id=$1 AND id=$2 AND active AND verified AND NOT do_not_contact)",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if !target_exists {
                return Err(RepositoryError::Conflict);
            }

            let persisted_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_outreach_opportunities(
                    id,workspace_id,target_id,source,subject_kind,subject_key,template_key,
                    relevance_basis_points,confidence_basis_points,active,observed_at,expires_at)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
                ON CONFLICT(workspace_id,source,target_id,subject_kind,subject_key) DO UPDATE SET
                    template_key=EXCLUDED.template_key,
                    relevance_basis_points=EXCLUDED.relevance_basis_points,
                    confidence_basis_points=EXCLUDED.confidence_basis_points,
                    active=EXCLUDED.active,
                    observed_at=EXCLUDED.observed_at,
                    expires_at=EXCLUDED.expires_at
                WHERE EXCLUDED.observed_at >= viryaos_outreach_opportunities.observed_at
                RETURNING id
                "#,
            )
            .bind(opportunity_id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(source)
            .bind(subject_kind)
            .bind(subject_key)
            .bind(template_key)
            .bind(i32::from(command.relevance_basis_points))
            .bind(i32::from(command.confidence.basis_points()))
            .bind(command.active)
            .bind(command.observed_at)
            .bind(command.expires_at)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;
            if persisted_id != opportunity_id.into_uuid() {
                return Err(RepositoryError::Conflict);
            }

            tx.commit().await.map_err(map_sqlx)?;
            Ok(OutreachOpportunityMutation {
                operation_id,
                opportunity_id,
                replayed: false,
            })
        })
        .await
    }

    async fn record_outreach_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordOutreachReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let disposition = outreach_reply_str(command.disposition);
            let details = json!({
                "target_id": command.target_id,
                "opportunity_id": command.opportunity_id,
                "disposition": disposition,
                "occurred_at": command.occurred_at,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "record_autopilot_outreach_reply",
                "outreach_target",
                command.target_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: command.target_id.into_uuid(),
                    status: "reply_recorded".into(),
                    replayed: true,
                });
            }

            if let Some(opportunity_id) = command.opportunity_id {
                let belongs_to_target = sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS (
                        SELECT 1
                        FROM viryaos_outreach_opportunities
                        WHERE workspace_id = $1 AND id = $2 AND target_id = $3
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(opportunity_id.into_uuid())
                .bind(command.target_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if !belongs_to_target {
                    return Err(RepositoryError::Conflict);
                }
            }

            let relationship_delta = match command.disposition {
                OutreachReplyDisposition::Received | OutreachReplyDisposition::None => 0,
                OutreachReplyDisposition::Positive => 5,
                OutreachReplyDisposition::Declined => -5,
                OutreachReplyDisposition::DoNotContact => -15,
            };
            let new_version = sqlx::query_scalar::<_, i64>(
                r#"
                UPDATE viryaos_outreach_targets
                SET last_reply_at = $3,
                    last_reply_disposition = $4,
                    do_not_contact = CASE WHEN $4 = 'do_not_contact' THEN true ELSE do_not_contact END,
                    accepts_outreach = CASE WHEN $4 = 'do_not_contact' THEN false ELSE accepts_outreach END,
                    relationship_score = GREATEST(0, LEAST(100, relationship_score + $5)),
                    version = version + 1
                WHERE workspace_id = $1 AND id = $2
                RETURNING version
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(command.occurred_at)
            .bind(disposition)
            .bind(relationship_delta)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;

            sqlx::query(
                r#"
                INSERT INTO viryaos_outreach_target_history (
                    workspace_id, target_id, version, snapshot
                )
                SELECT workspace_id, id, version, jsonb_build_object(
                    'target_kind', target_kind,
                    'display_name', display_name,
                    'contact_email', contact_email,
                    'active', active,
                    'verified', verified,
                    'accepts_outreach', accepts_outreach,
                    'priority', priority,
                    'relationship_score', relationship_score,
                    'do_not_contact', do_not_contact,
                    'last_reply_at', last_reply_at,
                    'last_reply_disposition', last_reply_disposition
                )
                FROM viryaos_outreach_targets
                WHERE workspace_id = $1 AND id = $2 AND version = $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(new_version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            if disposition == "do_not_contact" {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_contact_governor (
                    workspace_id, normalized_contact, last_context, last_action_id,
                    last_outbound_at, next_contact_after, do_not_contact
                )
                SELECT $1, lower(btrim(contact_email)), 'outreach', NULL, $3, $3, true
                FROM viryaos_outreach_targets
                WHERE workspace_id=$1 AND id=$2
                ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
                SET do_not_contact=true,
                    last_context=EXCLUDED.last_context,
                    next_contact_after=GREATEST(viryaos_contact_governor.next_contact_after, EXCLUDED.next_contact_after),
                    updated_at=now()
                "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.target_id.into_uuid())
                .bind(command.occurred_at)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            sqlx::query(
                r#"
                INSERT INTO viryaos_outreach_interactions (
                    workspace_id, target_id, opportunity_id, direction, phase,
                    disposition, source_key, occurred_at
                ) VALUES ($1,$2,$3,'inbound','reply',$4,$5,$6)
                ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(command.opportunity_id.map(OutreachOpportunityId::into_uuid))
            .bind(disposition)
            .bind(format!("operator:{operation_id}"))
            .bind(command.occurred_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.target_id.into_uuid(),
                status: "reply_recorded".into(),
                replayed: false,
            })
        })
        .await
    }
}

#[async_trait]
impl AutopilotContentStateRepository for PostgresAutopilotRepository {
    async fn upsert_content_source(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertContentSource,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ContentSourceMutation, RepositoryError> {
        self.bounded(async {
            let invalid = command.expected_version < 0
                || command.source_key.trim().is_empty()
                || command.title.trim().is_empty()
                || command.expires_at <= command.occurred_at
                || !command.metadata.is_object();
            if invalid {
                return Err(RepositoryError::Unexpected);
            }
            if command.expected_version > 0 && command.source_id.is_none() {
                return Err(RepositoryError::Conflict);
            }

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let source_id = command
                .source_id
                .unwrap_or_else(|| ContentSourceId::from_uuid(operation_id));
            let kind = content_source_kind_str(command.kind);
            let details = json!({
                "source_id": command.source_id,
                "source_kind": kind,
                "source_key": &command.source_key,
                "title": &command.title,
                "occurred_at": command.occurred_at,
                "expires_at": command.expires_at,
                "metadata": &command.metadata,
                "expected_version": command.expected_version,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_content_source",
                "content_source",
                source_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(ContentSourceMutation {
                    operation_id: existing,
                    source_id,
                    version: command.expected_version.saturating_add(1).max(1),
                    replayed: true,
                });
            }

            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_content_sources (
                        id, workspace_id, source_kind, source_key, title,
                        occurred_at, expires_at, metadata
                    ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
                    RETURNING version
                    "#,
                )
                .bind(source_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(kind)
                .bind(command.source_key.trim())
                .bind(command.title.trim())
                .bind(command.occurred_at)
                .bind(command.expires_at)
                .bind(&command.metadata)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_content_sources
                    SET source_kind = $3,
                        source_key = $4,
                        title = $5,
                        occurred_at = $6,
                        expires_at = $7,
                        metadata = $8,
                        version = version + 1
                    WHERE workspace_id = $1 AND id = $2 AND version = $9
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(source_id.into_uuid())
                .bind(kind)
                .bind(command.source_key.trim())
                .bind(command.title.trim())
                .bind(command.occurred_at)
                .bind(command.expires_at)
                .bind(&command.metadata)
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            sqlx::query(
                r#"
                INSERT INTO viryaos_content_source_history (
                    workspace_id, source_id, version, snapshot
                )
                SELECT workspace_id, id, version, jsonb_build_object(
                    'source_kind', source_kind,
                    'source_key', source_key,
                    'title', title,
                    'occurred_at', occurred_at,
                    'expires_at', expires_at,
                    'metadata', metadata,
                    'active', active
                )
                FROM viryaos_content_sources
                WHERE workspace_id = $1 AND id = $2 AND version = $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id.into_uuid())
            .bind(version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ContentSourceMutation {
                operation_id,
                source_id,
                version,
                replayed: false,
            })
        })
        .await
    }
}

#[async_trait]
impl AutopilotExperimentStateRepository for PostgresAutopilotRepository {
    async fn create_experiment(
        &self,
        workspace_id: WorkspaceId,
        command: CreateExperiment,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ExperimentMutation, RepositoryError> {
        self.bounded(async {
            let slug = command.slug.trim();
            if slug.is_empty() || !(2..=8).contains(&command.variants.len()) {
                return Err(RepositoryError::Unexpected);
            }
            let total: u32 = command
                .variants
                .iter()
                .map(|variant| u32::from(variant.allocation_basis_points))
                .sum();
            if total != 10_000
                || command
                    .variants
                    .iter()
                    .any(|variant| variant.key.trim().is_empty())
            {
                return Err(RepositoryError::Unexpected);
            }
            let mut keys = std::collections::HashSet::new();
            if command
                .variants
                .iter()
                .any(|variant| !keys.insert(variant.key.trim()))
            {
                return Err(RepositoryError::Unexpected);
            }

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let experiment_id = ExperimentId::from_uuid(operation_id);
            let details = json!({
                "slug": slug,
                "metric": experiment_metric_str(command.metric),
                "variants": command.variants.iter().map(|variant| json!({
                    "key": variant.key,
                    "allocation_basis_points": variant.allocation_basis_points,
                })).collect::<Vec<_>>(),
                "start": command.start,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "create_autopilot_experiment",
                "experiment",
                experiment_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(ExperimentMutation {
                    operation_id: existing,
                    experiment_id: ExperimentId::from_uuid(existing),
                    replayed: true,
                });
            }

            sqlx::query(
                r#"
                INSERT INTO viryaos_experiments (
                    id, workspace_id, slug, metric_kind, status
                ) VALUES ($1,$2,$3,$4,$5)
                "#,
            )
            .bind(experiment_id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(slug)
            .bind(experiment_metric_str(command.metric))
            .bind(if command.start { "running" } else { "draft" })
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            for variant in command.variants {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_experiment_variants (
                        id, workspace_id, experiment_id, variant_key, allocation_basis_points
                    ) VALUES ($1,$2,$3,$4,$5)
                    "#,
                )
                .bind(Uuid::now_v7())
                .bind(workspace_id.into_uuid())
                .bind(experiment_id.into_uuid())
                .bind(variant.key.trim())
                .bind(i32::from(variant.allocation_basis_points))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ExperimentMutation {
                operation_id,
                experiment_id,
                replayed: false,
            })
        })
        .await
    }

    async fn record_experiment_observation(
        &self,
        workspace_id: WorkspaceId,
        command: ExperimentObservation,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            if command.conversions_delta > command.exposures_delta || command.value_minor_delta < 0
            {
                return Err(RepositoryError::Unexpected);
            }

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "experiment_id": command.experiment_id,
                "variant_id": command.variant_id,
                "exposures_delta": command.exposures_delta,
                "conversions_delta": command.conversions_delta,
                "value_minor_delta": command.value_minor_delta,
                "observed_at": command.observed_at,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "record_autopilot_experiment_observation",
                "experiment",
                command.experiment_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: command.experiment_id.into_uuid(),
                    status: "observation_recorded".into(),
                    replayed: true,
                });
            }

            let variant = sqlx::query_as::<_, (i64, i64)>(
                r#"
                SELECT exposures, conversions
                FROM viryaos_experiment_variants
                WHERE workspace_id = $1 AND experiment_id = $2 AND id = $3 AND active
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.experiment_id.into_uuid())
            .bind(command.variant_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;
            let new_exposures = variant
                .0
                .checked_add(i64::from(command.exposures_delta))
                .ok_or(RepositoryError::Unexpected)?;
            let new_conversions = variant
                .1
                .checked_add(i64::from(command.conversions_delta))
                .ok_or(RepositoryError::Unexpected)?;
            if new_conversions > new_exposures {
                return Err(RepositoryError::Unexpected);
            }

            sqlx::query(
                r#"
                UPDATE viryaos_experiment_variants
                SET exposures = $4,
                    conversions = $5,
                    value_minor = value_minor + $6
                WHERE workspace_id = $1 AND experiment_id = $2 AND id = $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.experiment_id.into_uuid())
            .bind(command.variant_id.into_uuid())
            .bind(new_exposures)
            .bind(new_conversions)
            .bind(command.value_minor_delta)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                INSERT INTO viryaos_experiment_observations (
                    workspace_id, experiment_id, variant_id, observation_key,
                    exposures_delta, conversions_delta, value_minor_delta, observed_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.experiment_id.into_uuid())
            .bind(command.variant_id.into_uuid())
            .bind(idempotency_key.as_str())
            .bind(i32::try_from(command.exposures_delta).map_err(|_| RepositoryError::Unexpected)?)
            .bind(
                i32::try_from(command.conversions_delta)
                    .map_err(|_| RepositoryError::Unexpected)?,
            )
            .bind(command.value_minor_delta)
            .bind(command.observed_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.experiment_id.into_uuid(),
                status: "observation_recorded".into(),
                replayed: false,
            })
        })
        .await
    }

    async fn load_experiment_assignment(
        &self,
        workspace_id: WorkspaceId,
        experiment_id: ExperimentId,
    ) -> Result<ExperimentAssignmentSource, RepositoryError> {
        #[derive(Debug, FromRow)]
        struct AssignmentRow {
            experiment_version: i64,
            variant_id: Uuid,
            variant_key: String,
            allocation_basis_points: i32,
            active: bool,
        }

        self.bounded(async {
            let rows = sqlx::query_as::<_, AssignmentRow>(
                r#"
                SELECT
                    experiment.version AS experiment_version,
                    variant.id AS variant_id,
                    variant.variant_key,
                    variant.allocation_basis_points,
                    variant.active
                FROM viryaos_experiments AS experiment
                JOIN viryaos_experiment_variants AS variant
                  ON variant.workspace_id = experiment.workspace_id
                 AND variant.experiment_id = experiment.id
                WHERE experiment.workspace_id = $1
                  AND experiment.id = $2
                  AND experiment.status = 'running'
                ORDER BY variant.id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(experiment_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let version = rows
                .first()
                .map(|row| row.experiment_version)
                .ok_or(RepositoryError::NotFound)?;
            if rows.iter().any(|row| row.experiment_version != version) {
                return Err(RepositoryError::Unexpected);
            }

            let variants = rows
                .into_iter()
                .map(|row| {
                    Ok(ExperimentAssignmentVariant {
                        slot: ExperimentAllocationSlot {
                            variant_id: ExperimentVariantId::from_uuid(row.variant_id),
                            allocation_basis_points: u16::try_from(row.allocation_basis_points)
                                .map_err(|_| RepositoryError::Unexpected)?,
                            active: row.active,
                        },
                        key: row.variant_key,
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?;

            Ok(ExperimentAssignmentSource {
                experiment_id,
                version,
                variants,
            })
        })
        .await
    }
}

const fn outreach_target_kind_str(kind: OutreachTargetKind) -> &'static str {
    match kind {
        OutreachTargetKind::Playlist => "playlist",
        OutreachTargetKind::Radio => "radio",
        OutreachTargetKind::Press => "press",
        OutreachTargetKind::Creator => "creator",
        OutreachTargetKind::SupportSlot => "support_slot",
        OutreachTargetKind::Endorsement => "endorsement",
        OutreachTargetKind::MediaPatronage => "media_patronage",
    }
}

const fn outreach_reply_str(value: OutreachReplyDisposition) -> &'static str {
    match value {
        OutreachReplyDisposition::None => "none",
        OutreachReplyDisposition::Received => "received",
        OutreachReplyDisposition::Positive => "positive",
        OutreachReplyDisposition::Declined => "declined",
        OutreachReplyDisposition::DoNotContact => "do_not_contact",
    }
}

const fn content_source_kind_str(value: ContentSourceKind) -> &'static str {
    match value {
        ContentSourceKind::Event => "event",
        ContentSourceKind::Release => "release",
        ContentSourceKind::ShowCompleted => "show_completed",
    }
}

const fn experiment_metric_str(value: ExperimentMetric) -> &'static str {
    match value {
        ExperimentMetric::Conversion => "conversion",
        ExperimentMetric::RevenuePerExposure => "revenue_per_exposure",
    }
}
