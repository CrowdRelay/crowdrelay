//! Beacon operator/provider ingress kept separate from generic outreach ingress.

use super::*;

#[async_trait]
impl AutopilotBeaconStateRepository for PostgresAutopilotRepository {
    async fn upsert_beacon(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBeacon,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<BeaconMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || (command.expected_version > 0 && command.beacon_id.is_none())
                || command.display_name.trim().is_empty()
                || command.display_name.len() > 240
                || command.relationship_score > 100
                || command.relevance_basis_points > 10_000
                || command.confidence.basis_points() > 10_000
                || !command.metadata.is_object()
                || command.contact_email.as_ref().is_some_and(|email| {
                    let trimmed = email.trim();
                    trimmed.is_empty() || trimmed.len() > 320 || !trimmed.contains('@')
                })
                || command.destination_url.as_ref().is_some_and(|value| {
                    let trimmed = value.trim();
                    trimmed.is_empty() || trimmed.len() > 2048
                })
                || command.source_url.as_ref().is_some_and(|value| {
                    let trimmed = value.trim();
                    trimmed.is_empty() || trimmed.len() > 2048
                })
            {
                return Err(RepositoryError::Unexpected);
            }

            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            super::configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout)
                .await?;
            // Match the same identity policy enforced by migration 0053: use
            // email when known, otherwise a public destination URL. Treating
            // NULL email as an identity used to collapse every email-less
            // scene partner/community of one kind in a city into a single row.
            let normalized_email = command.contact_email.as_deref().map(str::trim);
            let normalized_destination = command.destination_url.as_deref().map(str::trim);
            let normalized_source = command.source_url.as_deref().map(str::trim);
            let natural = sqlx::query_as::<_, (Uuid, i64)>(
                r#"
                SELECT id, version
                FROM viryaos_beacons
                WHERE workspace_id=$1 AND beacon_kind=$2
                  AND city_id IS NOT DISTINCT FROM $3
                  AND (
                    ($4::text IS NOT NULL AND contact_email = $4)
                    OR (
                      $4::text IS NULL AND $5::text IS NOT NULL
                      AND destination_url = $5
                    )
                  )
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.kind.as_str())
            .bind(command.city_id.map(CityId::into_uuid))
            .bind(normalized_email)
            .bind(normalized_destination)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let beacon_id = match (command.beacon_id, natural) {
                (Some(requested), Some((persisted, _))) if requested.into_uuid() != persisted => {
                    return Err(RepositoryError::Conflict);
                }
                (Some(requested), _) => requested,
                (None, Some((persisted, _))) => BeaconId::from_uuid(persisted),
                (None, None) => BeaconId::from_uuid(operation_id),
            };
            let details = json!({
                "beacon_id": beacon_id,
                "city_id": command.city_id,
                "kind": command.kind,
                "display_name": command.display_name,
                "verified": command.verified,
                "accepts_outreach": command.accepts_outreach,
                "do_not_contact": command.do_not_contact,
                "relationship_score": command.relationship_score,
                "relevance_basis_points": command.relevance_basis_points,
                "confidence_basis_points": command.confidence.basis_points(),
                "expected_version": command.expected_version,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "upsert_autopilot_beacon",
                "beacon",
                beacon_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_beacons WHERE workspace_id=$1 AND id=$2",
                )
                .bind(workspace_id.into_uuid())
                .bind(beacon_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(BeaconMutation {
                    operation_id: existing,
                    beacon_id,
                    version,
                    replayed: true,
                });
            }

            let version = if command.expected_version == 0 && natural.is_none() {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_beacons(
                        id,workspace_id,city_id,beacon_kind,display_name,contact_email,
                        destination_url,source_url,active,verified,accepts_outreach,
                        do_not_contact,relationship_score,relevance_basis_points,
                        confidence_basis_points,metadata
                    ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
                    RETURNING version
                    "#,
                )
                .bind(beacon_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(command.city_id.map(CityId::into_uuid))
                .bind(command.kind.as_str())
                .bind(command.display_name.trim())
                .bind(normalized_email)
                .bind(normalized_destination)
                .bind(normalized_source)
                .bind(command.active)
                .bind(command.verified)
                .bind(command.accepts_outreach)
                .bind(command.do_not_contact)
                .bind(i32::from(command.relationship_score))
                .bind(i32::from(command.relevance_basis_points))
                .bind(i32::from(command.confidence.basis_points()))
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
                    UPDATE viryaos_beacons
                    SET city_id=$3, beacon_kind=$4, display_name=$5, contact_email=$6,
                        destination_url=$7, source_url=$8, active=$9, verified=$10,
                        accepts_outreach=$11, do_not_contact=$12, relationship_score=$13,
                        relevance_basis_points=$14, confidence_basis_points=$15,
                        metadata=$16, version=version+1
                    WHERE workspace_id=$1 AND id=$2 AND version=$17
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(beacon_id.into_uuid())
                .bind(command.city_id.map(CityId::into_uuid))
                .bind(command.kind.as_str())
                .bind(command.display_name.trim())
                .bind(normalized_email)
                .bind(normalized_destination)
                .bind(normalized_source)
                .bind(command.active)
                .bind(command.verified)
                .bind(command.accepts_outreach)
                .bind(command.do_not_contact)
                .bind(i32::from(command.relationship_score))
                .bind(i32::from(command.relevance_basis_points))
                .bind(i32::from(command.confidence.basis_points()))
                .bind(&command.metadata)
                .bind(expected)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };
            tx.commit().await.map_err(map_sqlx)?;
            Ok(BeaconMutation {
                operation_id,
                beacon_id,
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn record_beacon_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBeaconReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            super::configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout).await?;
            let operation_id = Uuid::now_v7();
            let disposition = beacon_reply_str(command.disposition);
            let details = json!({
                "beacon_id": command.beacon_id,
                "event_id": command.event_id,
                "disposition": disposition,
                "occurred_at": command.occurred_at,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "record_autopilot_beacon_reply",
                "beacon",
                command.beacon_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: command.beacon_id.into_uuid(),
                    status: disposition.into(),
                    replayed: true,
                });
            }
            let status = match command.disposition {
                BeaconReplyDisposition::Interested => "interested",
                BeaconReplyDisposition::Partner => "partner",
                BeaconReplyDisposition::Declined => "declined",
                BeaconReplyDisposition::DoNotContact => "suppressed",
                BeaconReplyDisposition::None | BeaconReplyDisposition::Received => "contacted",
            };
            let changed = sqlx::query(
                r#"
                INSERT INTO viryaos_beacon_campaigns(
                    workspace_id,beacon_id,event_id,status,last_reply_disposition,updated_at
                ) VALUES($1,$2,$3,$4,$5,$6)
                ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
                    status=EXCLUDED.status,
                    last_reply_disposition=EXCLUDED.last_reply_disposition,
                    updated_at=EXCLUDED.updated_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.beacon_id.into_uuid())
            .bind(command.event_id.into_uuid())
            .bind(status)
            .bind(disposition)
            .bind(command.occurred_at)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if changed.rows_affected() == 0 {
                return Err(RepositoryError::Conflict);
            }
            if matches!(command.disposition, BeaconReplyDisposition::DoNotContact) {
                sqlx::query(
                    "UPDATE viryaos_beacons SET do_not_contact=true, accepts_outreach=false, version=version+1 WHERE workspace_id=$1 AND id=$2",
                )
                .bind(workspace_id.into_uuid())
                .bind(command.beacon_id.into_uuid())
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            }
            tx.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.beacon_id.into_uuid(),
                status: disposition.into(),
                replayed: false,
            })
        })
        .await
    }
}

const fn beacon_reply_str(value: BeaconReplyDisposition) -> &'static str {
    match value {
        BeaconReplyDisposition::None => "none",
        BeaconReplyDisposition::Received => "received",
        BeaconReplyDisposition::Interested => "interested",
        BeaconReplyDisposition::Partner => "partner",
        BeaconReplyDisposition::Declined => "declined",
        BeaconReplyDisposition::DoNotContact => "do_not_contact",
    }
}
