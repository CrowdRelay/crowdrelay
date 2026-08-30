// ── BeaconSignalRepository adapter ──

use super::*;
use async_trait::async_trait;
use crowdrelay_application::{
    BeaconPreferences, BeaconSignalRepository, BeaconSignalRepositoryError, CreateInviteCommand,
    CreateInviteResult, CreatePressRequestCommand, CreatePressRequestResult, EmitNearbyCommand,
    EmitNearbyResult, ExchangeInviteCommand, ExchangeInviteResult, LeaveCommand, LogoutCommand,
    RecordEngagementCommand, RecordEngagementResult, SubmitCoverageCommand, SubmitCoverageResult,
    UpdatePreferencesCommand,
};

#[async_trait]
impl BeaconSignalRepository for PostgresBeaconReleaseRepository {
    async fn create_invite(
        &self,
        command: &CreateInviteCommand,
    ) -> Result<CreateInviteResult, BeaconSignalRepositoryError> {
        let workspace_id = command.workspace_id;
        let beacon_id = command.beacon_id;
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, %beacon_id, "beacon invite transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Beacon lookup.
        let beacon = sqlx::query_as::<_, (String, bool, bool, bool, bool)>(
            r#"
            SELECT display_name, active, verified, accepts_outreach, do_not_contact
            FROM viryaos_beacons
            WHERE workspace_id = $1 AND id = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .fetch_optional(&mut *tx)
        .await;
        let (display_name, active, verified, accepts_outreach, do_not_contact) = match beacon {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconSignalRepositoryError::NotFound),
            Err(error) => {
                tracing::warn!(%error, %beacon_id, "beacon invite lookup failed");
                return Err(BeaconSignalRepositoryError::Unavailable);
            }
        };
        if !active || !verified || !accepts_outreach || do_not_contact {
            return Err(BeaconSignalRepositoryError::Conflict);
        }
        // Check existing profile status.
        let existing_status = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status
            FROM viryaos_beacon_signal_profiles
            WHERE workspace_id=$1 AND beacon_id=$2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .fetch_optional(&mut *tx)
        .await;
        match existing_status {
            Ok(Some(status)) if status == "active" => {
                return Err(BeaconSignalRepositoryError::Conflict);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, %beacon_id, "beacon signal profile state lookup failed");
                return Err(BeaconSignalRepositoryError::Unavailable);
            }
        }
        // Upsert profile.
        let profile_result = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_signal_profiles (
                workspace_id, beacon_id, status, invite_token_hash, invite_expires_at,
                radius_km, locale, nearby_gigs_enabled, invite_count, last_invited_at
            ) VALUES ($1, $2, 'invited', $3, $4, $5, $6, true, 1, now())
            ON CONFLICT (workspace_id, beacon_id) DO UPDATE SET
                status = 'invited',
                invite_token_hash = EXCLUDED.invite_token_hash,
                invite_expires_at = EXCLUDED.invite_expires_at,
                radius_km = EXCLUDED.radius_km,
                locale = EXCLUDED.locale,
                invite_count = viryaos_beacon_signal_profiles.invite_count + 1,
                last_invited_at = now(), paused_at = NULL, revoked_at = NULL,
                pending_invite_job_id = NULL, updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(&command.invite_token_hash)
        .bind(command.invite_expires_at)
        .bind(command.radius_km)
        .bind(&command.locale)
        .execute(&mut *tx)
        .await;
        if let Err(error) = profile_result {
            tracing::warn!(%error, %beacon_id, "beacon signal invite persistence failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        // Revoke old sessions.
        let revoke_result = sqlx::query(
            r#"
            UPDATE viryaos_beacon_signal_sessions
            SET revoked_at=COALESCE(revoked_at, now())
            WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await;
        if let Err(error) = revoke_result {
            tracing::warn!(%error, %beacon_id, "beacon signal old-session revocation failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        tx.commit().await.map_err(|e| {
            tracing::warn!(%e, %beacon_id, "beacon signal invite transaction failed to commit");
            BeaconSignalRepositoryError::Unavailable
        })?;
        Ok(CreateInviteResult { display_name })
    }

    async fn exchange_invite(
        &self,
        command: &ExchangeInviteCommand,
    ) -> Result<ExchangeInviteResult, BeaconSignalRepositoryError> {
        let workspace_id = command.workspace_id;
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon signal exchange transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                String,
                i32,
                String,
                Vec<String>,
                bool,
                OffsetDateTime,
                Option<Uuid>,
            ),
        >(
            r#"
            SELECT profile.beacon_id, beacon.display_name, beacon.beacon_kind,
                   profile.radius_km, profile.locale, profile.topics,
                   profile.nearby_gigs_enabled, profile.invite_expires_at,
                   profile.pending_invite_job_id
            FROM viryaos_beacon_signal_profiles profile
            JOIN viryaos_beacons beacon
              ON beacon.workspace_id = profile.workspace_id AND beacon.id = profile.beacon_id
            WHERE profile.workspace_id = $1
              AND profile.invite_token_hash = $2
              AND profile.status = 'invited'
              AND beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
            FOR UPDATE OF profile
            "#,
        )
        .bind(workspace_id)
        .bind(&command.invite_token_hash)
        .fetch_optional(&mut *tx)
        .await;
        let (
            beacon_id,
            display_name,
            beacon_kind,
            stored_radius,
            stored_locale,
            stored_topics,
            nearby_gigs_enabled,
            _invite_expires_at,
            source_invite_job_id,
        ) = match row {
            Ok(Some(row)) if row.7 >= OffsetDateTime::now_utc() => row,
            Ok(_) => return Err(BeaconSignalRepositoryError::Unavailable),
            Err(error) => {
                tracing::warn!(%error, "beacon signal invite exchange lookup failed");
                return Err(BeaconSignalRepositoryError::Unavailable);
            }
        };
        let final_radius = command.radius_km.unwrap_or(stored_radius);
        let final_locale = command.locale.clone().unwrap_or(stored_locale);
        let final_topics = command.topics.clone().unwrap_or(stored_topics);
        // Update profile to active.
        let profile_update = sqlx::query(
            r#"
            UPDATE viryaos_beacon_signal_profiles
            SET status='active', invite_token_hash=NULL, invite_expires_at=NULL,
                pending_invite_job_id=NULL, radius_km=$3, locale=$4, topics=$5,
                joined_at=COALESCE(joined_at, now()), last_seen_at=now(), updated_at=now()
            WHERE workspace_id=$1 AND beacon_id=$2
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(final_radius)
        .bind(&final_locale)
        .bind(&final_topics)
        .execute(&mut *tx)
        .await;
        // Insert session.
        let session_insert = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_signal_sessions
                (workspace_id, id, beacon_id, token_hash, expires_at, client_kind, source_invite_job_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(workspace_id)
        .bind(command.session_id)
        .bind(beacon_id)
        .bind(&command.bearer_token_hash)
        .bind(command.session_expires_at)
        .bind(&command.client_kind)
        .bind(source_invite_job_id)
        .execute(&mut *tx)
        .await;
        if profile_update.is_err() || session_insert.is_err() || tx.commit().await.is_err() {
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        Ok(ExchangeInviteResult {
            beacon_id,
            display_name,
            beacon_kind,
            radius_km: final_radius,
            locale: final_locale,
            topics: final_topics,
            nearby_gigs_enabled,
        })
    }

    async fn update_preferences(
        &self,
        command: &UpdatePreferencesCommand,
    ) -> Result<Option<BeaconPreferences>, BeaconSignalRepositoryError> {
        let result = sqlx::query_as::<_, (i32, String, Vec<String>, bool)>(
            r#"
            UPDATE viryaos_beacon_signal_profiles
            SET radius_km=COALESCE($3,radius_km), locale=COALESCE($4,locale),
                topics=COALESCE($5,topics), nearby_gigs_enabled=COALESCE($6,nearby_gigs_enabled),
                updated_at=now()
            WHERE workspace_id=$1 AND beacon_id=$2 AND status='active'
            RETURNING radius_km, locale, topics, nearby_gigs_enabled
            "#,
        )
        .bind(command.workspace_id)
        .bind(command.beacon_id)
        .bind(command.radius_km)
        .bind(&command.locale)
        .bind(&command.topics)
        .bind(command.nearby_gigs_enabled)
        .fetch_optional(&self.pool)
        .await;
        match result {
            Ok(Some((radius_km, locale, topics, nearby_gigs_enabled))) => {
                Ok(Some(BeaconPreferences {
                    radius_km,
                    locale,
                    topics,
                    nearby_gigs_enabled,
                }))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::warn!(%error, beacon_id=%command.beacon_id, "beacon preferences update failed");
                Err(BeaconSignalRepositoryError::Unavailable)
            }
        }
    }

    async fn create_press_request(
        &self,
        command: &CreatePressRequestCommand,
    ) -> Result<CreatePressRequestResult, BeaconSignalRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon press request transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Validate event exists if provided.
        if let Some(event_id) = command.event_id {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
            )
            .bind(command.workspace_id)
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await;
            match exists {
                Ok(true) => {}
                Ok(false) => return Err(BeaconSignalRepositoryError::NotFound),
                Err(error) => {
                    tracing::warn!(%error, %event_id, "beacon press request event lookup failed");
                    return Err(BeaconSignalRepositoryError::Unavailable);
                }
            }
        }
        let request_id = Uuid::now_v7();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_press_requests
                (id,workspace_id,beacon_id,event_id,request_kind,details)
            VALUES ($1,$2,$3,$4,$5,$6)
            "#,
        )
        .bind(request_id)
        .bind(command.workspace_id)
        .bind(command.beacon_id)
        .bind(command.event_id)
        .bind(&command.request_kind)
        .bind(&command.details)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, beacon_id=%command.beacon_id, "beacon press request failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        let event_payload = serde_json::json!({
            "request_id": request_id,
            "beacon_id": command.beacon_id,
            "event_id": command.event_id,
            "request_kind": command.request_kind,
            "details": command.details,
        });
        if let Err(error) = sqlx::query(
            "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.press_request_created',1,$2,$3)",
        )
        .bind(command.workspace_id)
        .bind(event_payload)
        .bind(&command.request_id_header)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, beacon_id=%command.beacon_id, "beacon press request outbox write failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        tx.commit().await.map_err(|e| {
            tracing::warn!(%e, beacon_id=%command.beacon_id, "beacon press request transaction failed to commit");
            BeaconSignalRepositoryError::Unavailable
        })?;
        Ok(CreatePressRequestResult { request_id })
    }

    async fn logout(&self, command: &LogoutCommand) -> Result<(), BeaconSignalRepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| BeaconSignalRepositoryError::Unavailable)?;
        let session = sqlx::query(
            "UPDATE viryaos_beacon_signal_sessions SET revoked_at=now() WHERE workspace_id=$1 AND token_hash=$2 AND revoked_at IS NULL",
        )
        .bind(command.workspace_id)
        .bind(&command.session_hash)
        .execute(&mut *tx)
        .await;
        let endpoint = sqlx::query(
            r#"
            UPDATE fan_push_endpoints
            SET active=false, invalidated_at=now(), updated_at=now()
            WHERE workspace_id=$1 AND audience_kind='beacon' AND principal_hash=$2 AND active
            "#,
        )
        .bind(command.workspace_id)
        .bind(&command.session_hash)
        .execute(&mut *tx)
        .await;
        if session.is_err() || endpoint.is_err() || tx.commit().await.is_err() {
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        Ok(())
    }

    async fn emit_nearby(
        &self,
        command: &EmitNearbyCommand,
    ) -> Result<EmitNearbyResult, BeaconSignalRepositoryError> {
        let result = sqlx::query_as::<_, (i64, i64)>(
            r#"
            WITH candidates AS (
                SELECT beacon.id AS beacon_id,event.id AS event_id,
                       event.title AS event_title,event.starts_at,profile.locale,profile.radius_km,
                       beacon.relationship_score,beacon.relevance_basis_points,
                       engagement.last_notified_at,
                       LEAST(20000,ROUND(
                           6371 * 2 * ASIN(LEAST(1.0,SQRT(
                               POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2),2)
                               + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                               * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2),2)
                           )))
                       )::integer) AS distance_km
                FROM viryaos_beacon_signal_profiles profile
                JOIN viryaos_beacons beacon
                  ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
                JOIN cities home_city ON home_city.id=beacon.city_id
                JOIN events event ON event.workspace_id=profile.workspace_id
                  AND event.status='published' AND event.starts_at > now()
                  AND event.starts_at < now() + ($4::bigint * interval '1 day')
                JOIN cities event_city ON event_city.id=event.city_id
                LEFT JOIN viryaos_beacon_signal_event_engagements engagement
                  ON engagement.workspace_id=profile.workspace_id
                 AND engagement.beacon_id=beacon.id AND engagement.event_id=event.id
                LEFT JOIN viryaos_beacon_campaigns campaign
                  ON campaign.workspace_id=profile.workspace_id
                 AND campaign.beacon_id=beacon.id AND campaign.event_id=event.id
                WHERE profile.workspace_id=$1 AND profile.status='active'
                  AND profile.nearby_gigs_enabled AND 'shows'=ANY(profile.topics)
                  AND beacon.active AND beacon.verified AND beacon.accepts_outreach
                  AND NOT beacon.do_not_contact
                  AND home_city.latitude IS NOT NULL AND home_city.longitude IS NOT NULL
                  AND event_city.latitude IS NOT NULL AND event_city.longitude IS NOT NULL
                  AND COALESCE(engagement.status,'eligible') NOT IN ('completed','declined')
                  AND engagement.last_notified_at IS NULL
                  AND COALESCE(campaign.status,'candidate') NOT IN ('declined','suppressed','closed')
            ), ranked AS (
                SELECT * FROM candidates
                WHERE distance_km <= radius_km
                ORDER BY starts_at,relevance_basis_points DESC,relationship_score DESC,
                         distance_km,beacon_id,event_id
                LIMIT $2
            ), campaign_seed AS (
                INSERT INTO viryaos_beacon_campaigns (workspace_id,beacon_id,event_id,status)
                SELECT $1,beacon_id,event_id,'candidate' FROM ranked
                ON CONFLICT (workspace_id,beacon_id,event_id) DO NOTHING
                RETURNING beacon_id,event_id
            ), engagement_seed AS (
                INSERT INTO viryaos_beacon_signal_event_engagements
                    (workspace_id,beacon_id,event_id,status)
                SELECT $1,beacon_id,event_id,'eligible' FROM ranked
                ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
                    updated_at=viryaos_beacon_signal_event_engagements.updated_at
                RETURNING beacon_id,event_id
            ), push_queued AS (
                INSERT INTO fan_push_deliveries (
                    workspace_id,fan_id,audience_kind,endpoint_id,source_kind,source_id,
                    title,body,target_path,collapse_key
                )
                SELECT $1,NULL,'beacon',endpoint.id,'beacon_nearby_concert',ranked.event_id,
                       CASE WHEN lower(ranked.locale) LIKE 'pl%'
                            THEN 'VIRYA · materiał lokalny' ELSE 'VIRYA · local story' END,
                       CASE WHEN lower(ranked.locale) LIKE 'pl%'
                            THEN ranked.event_title || ' — gramy około ' || ranked.distance_km || ' km od Ciebie. Press room jest gotowy.'
                            ELSE ranked.event_title || ' — we play about ' || ranked.distance_km || ' km from you. The press room is ready.' END,
                       CASE WHEN lower(ranked.locale) LIKE 'pl%'
                            THEN '/pl/latarnik?event_id=' || ranked.event_id::text
                            ELSE '/latarnik?event_id=' || ranked.event_id::text END,
                       'beacon-nearby:' || ranked.event_id::text
                FROM ranked
                JOIN engagement_seed seeded
                  ON seeded.beacon_id=ranked.beacon_id AND seeded.event_id=ranked.event_id
                JOIN viryaos_beacon_signal_sessions session
                  ON session.workspace_id=$1 AND session.beacon_id=ranked.beacon_id
                 AND session.revoked_at IS NULL AND session.expires_at > now()
                JOIN fan_push_endpoints endpoint
                  ON endpoint.workspace_id=$1 AND endpoint.audience_kind='beacon'
                 AND endpoint.principal_hash=session.token_hash
                 AND endpoint.active AND endpoint.invalidated_at IS NULL
                WHERE $3::boolean
                ON CONFLICT (workspace_id,source_kind,source_id,endpoint_id) DO NOTHING
                RETURNING endpoint_id,source_id
            ), notified_pairs AS (
                SELECT DISTINCT session.beacon_id,push_queued.source_id AS event_id
                FROM push_queued
                JOIN fan_push_endpoints endpoint
                  ON endpoint.workspace_id=$1 AND endpoint.id=push_queued.endpoint_id
                JOIN viryaos_beacon_signal_sessions session
                  ON session.workspace_id=$1 AND session.token_hash=endpoint.principal_hash
            ), marked AS (
                UPDATE viryaos_beacon_signal_event_engagements engagement
                SET status=CASE WHEN engagement.status='eligible' THEN 'notified' ELSE engagement.status END,
                    notification_count=engagement.notification_count + 1,
                    first_notified_at=COALESCE(engagement.first_notified_at,now()),
                    last_notified_at=now(),updated_at=now()
                FROM notified_pairs notified
                WHERE engagement.workspace_id=$1
                  AND engagement.beacon_id=notified.beacon_id AND engagement.event_id=notified.event_id
                RETURNING engagement.beacon_id,engagement.event_id
            ), campaign_contacted AS (
                UPDATE viryaos_beacon_campaigns campaign
                SET status=CASE WHEN campaign.status='candidate' THEN 'contacted' ELSE campaign.status END,
                    last_phase='local_push',last_outreach_at=now(),updated_at=now()
                FROM marked
                WHERE campaign.workspace_id=$1
                  AND campaign.beacon_id=marked.beacon_id AND campaign.event_id=marked.event_id
                  AND campaign.status NOT IN ('declined','suppressed','closed')
                RETURNING campaign.beacon_id,campaign.event_id
            )
            SELECT (SELECT count(*)::bigint FROM ranked),
                   (SELECT count(*)::bigint FROM push_queued)
            "#,
        )
        .bind(command.workspace_id)
        .bind(command.limit)
        .bind(command.push_enabled)
        .bind(command.lead_days)
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok((eligible, push_queued)) => Ok(EmitNearbyResult {
                eligible,
                push_queued,
            }),
            Err(error) => {
                tracing::warn!(%error, "beacon nearby notification wave failed");
                Err(BeaconSignalRepositoryError::Unavailable)
            }
        }
    }

    async fn record_event_engagement(
        &self,
        command: &RecordEngagementCommand,
    ) -> Result<RecordEngagementResult, BeaconSignalRepositoryError> {
        let workspace_id = command.workspace_id;
        let beacon_id = command.beacon_id;
        let event_id = command.event_id;
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon engagement transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Eligibility check.
        let allowed = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM events event
                JOIN cities event_city ON event_city.id=event.city_id
                JOIN viryaos_beacons beacon ON beacon.workspace_id=event.workspace_id AND beacon.id=$2
                JOIN cities home_city ON home_city.id=beacon.city_id
                WHERE event.workspace_id=$1 AND event.id=$3
                  AND event.status='published'
                  AND event.starts_at > now() - interval '2 days'
                  AND home_city.latitude IS NOT NULL AND home_city.longitude IS NOT NULL
                  AND event_city.latitude IS NOT NULL AND event_city.longitude IS NOT NULL
                  AND (6371 * 2 * ASIN(LEAST(1.0, SQRT(
                        POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2), 2)
                        + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                        * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2), 2)
                      )))) <= $4
                UNION ALL
                SELECT 1 FROM viryaos_beacon_signal_event_engagements engagement
                WHERE engagement.workspace_id=$1 AND engagement.beacon_id=$2 AND engagement.event_id=$3
            )
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .bind(command.radius_km)
        .fetch_one(&mut *tx)
        .await;
        match allowed {
            Ok(true) => {}
            Ok(false) => return Err(BeaconSignalRepositoryError::NotFound),
            Err(error) => {
                tracing::warn!(%error, %event_id, "beacon engagement eligibility lookup failed");
                return Err(BeaconSignalRepositoryError::Unavailable);
            }
        }
        // Current status.
        let current = sqlx::query_scalar::<_, String>(
            "SELECT status FROM viryaos_beacon_signal_event_engagements WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(%e, %event_id, "beacon engagement current-state lookup failed");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Compute next status (pure logic, kept inline to avoid cross-crate coupling).
        let target = command.action.as_str();
        let next_status = match current.as_deref() {
            None => target,
            Some("completed") => {
                if target == "completed" {
                    "completed"
                } else {
                    return Err(BeaconSignalRepositoryError::Conflict);
                }
            }
            Some("declined") => {
                if target == "declined" {
                    "declined"
                } else {
                    return Err(BeaconSignalRepositoryError::Conflict);
                }
            }
            Some(current) => {
                if target == "declined" {
                    "declined"
                } else if engagement_rank(target) >= engagement_rank(current) {
                    target
                } else {
                    match current {
                        "notified" => "notified",
                        "opened" => "opened",
                        "interested" => "interested",
                        "helping" => "helping",
                        _ => "eligible",
                    }
                }
            }
        };
        // Upsert engagement.
        let upsert = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_signal_event_engagements (
                workspace_id,beacon_id,event_id,status,help_kind,help_details,
                first_opened_at,last_opened_at,interested_at,helping_at,completed_at,declined_at
            ) VALUES (
                $1,$2,$3,$4,$5,$6,
                CASE WHEN $7='opened' THEN now() END,
                CASE WHEN $7='opened' THEN now() END,
                CASE WHEN $7='interested' THEN now() END,
                CASE WHEN $7='helping' THEN now() END,
                CASE WHEN $7='completed' THEN now() END,
                CASE WHEN $7='declined' THEN now() END
            )
            ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
                status=$4,
                help_kind=CASE WHEN $7='helping' THEN $5 ELSE viryaos_beacon_signal_event_engagements.help_kind END,
                help_details=CASE WHEN $7='helping' THEN $6 ELSE viryaos_beacon_signal_event_engagements.help_details END,
                first_opened_at=CASE WHEN $7='opened' THEN COALESCE(viryaos_beacon_signal_event_engagements.first_opened_at,now()) ELSE viryaos_beacon_signal_event_engagements.first_opened_at END,
                last_opened_at=CASE WHEN $7='opened' THEN now() ELSE viryaos_beacon_signal_event_engagements.last_opened_at END,
                interested_at=CASE WHEN $7='interested' THEN COALESCE(viryaos_beacon_signal_event_engagements.interested_at,now()) ELSE viryaos_beacon_signal_event_engagements.interested_at END,
                helping_at=CASE WHEN $7='helping' THEN COALESCE(viryaos_beacon_signal_event_engagements.helping_at,now()) ELSE viryaos_beacon_signal_event_engagements.helping_at END,
                completed_at=CASE WHEN $7='completed' THEN COALESCE(viryaos_beacon_signal_event_engagements.completed_at,now()) ELSE viryaos_beacon_signal_event_engagements.completed_at END,
                declined_at=CASE WHEN $7='declined' THEN COALESCE(viryaos_beacon_signal_event_engagements.declined_at,now()) ELSE viryaos_beacon_signal_event_engagements.declined_at END,
                updated_at=now()
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .bind(next_status)
        .bind(command.help_kind.as_deref())
        .bind(&command.help_details)
        .bind(&command.action)
        .execute(&mut *tx)
        .await;
        if let Err(error) = upsert {
            tracing::warn!(%error, %event_id, "beacon engagement persistence failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        // Sync campaign.
        let (campaign_status, campaign_disposition) = match next_status {
            "eligible" | "notified" | "opened" => ("contacted", "received"),
            "interested" => ("interested", "interested"),
            "helping" => ("partner", "partner"),
            "completed" => ("closed", "partner"),
            "declined" => ("declined", "declined"),
            _ => return Err(BeaconSignalRepositoryError::Conflict),
        };
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_campaigns (
                workspace_id,beacon_id,event_id,status,last_phase,last_reply_disposition,last_outreach_at
            ) VALUES ($1,$2,$3,$4,'local_push',$5,now())
            ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
                status=CASE
                    WHEN $4='closed' THEN 'closed'
                    WHEN $4='declined' THEN 'declined'
                    WHEN $4='partner' THEN 'partner'
                    WHEN viryaos_beacon_campaigns.status='partner' THEN 'partner'
                    WHEN $4='interested' THEN 'interested'
                    WHEN viryaos_beacon_campaigns.status='interested' THEN 'interested'
                    WHEN viryaos_beacon_campaigns.status='declined' THEN 'declined'
                    ELSE 'contacted'
                END,
                last_phase='local_push',
                last_reply_disposition=CASE
                    WHEN $4 IN ('closed','declined','partner','interested') THEN $5
                    WHEN viryaos_beacon_campaigns.status='partner' THEN 'partner'
                    WHEN viryaos_beacon_campaigns.status='interested' THEN 'interested'
                    WHEN viryaos_beacon_campaigns.status='declined' THEN 'declined'
                    ELSE 'received'
                END,
                last_outreach_at=COALESCE(viryaos_beacon_campaigns.last_outreach_at,now()),
                updated_at=now()
            WHERE viryaos_beacon_campaigns.status NOT IN ('suppressed','closed')
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .bind(campaign_status)
        .bind(campaign_disposition)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %event_id, "beacon engagement campaign sync failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        // Outbox.
        let event_payload = serde_json::json!({
            "beacon_id": beacon_id,
            "event_id": event_id,
            "status": next_status,
            "action": command.action,
            "help_kind": command.help_kind,
            "help_details": command.help_details,
        });
        if let Err(error) = sqlx::query(
            "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.signal_engagement_recorded',1,$2,$3)",
        )
        .bind(workspace_id)
        .bind(event_payload)
        .bind(&command.request_id_header)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %event_id, "beacon engagement outbox write failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        tx.commit().await.map_err(|e| {
            tracing::warn!(%e, %event_id, "beacon engagement transaction failed to commit");
            BeaconSignalRepositoryError::Unavailable
        })?;
        Ok(RecordEngagementResult {
            status: next_status.to_owned(),
            help_kind: command.help_kind.clone(),
        })
    }

    async fn submit_coverage(
        &self,
        command: &SubmitCoverageCommand,
    ) -> Result<SubmitCoverageResult, BeaconSignalRepositoryError> {
        let workspace_id = command.workspace_id;
        let beacon_id = command.beacon_id;
        let event_id = command.event_id;
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon coverage transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Check engagement exists and is not declined.
        let engagement_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM viryaos_beacon_signal_event_engagements WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(%e, %event_id, "beacon coverage engagement lookup failed");
            BeaconSignalRepositoryError::Unavailable
        })?;
        match engagement_status {
            Some(status) if status != "declined" => {}
            Some(_) => return Err(BeaconSignalRepositoryError::Conflict),
            None => return Err(BeaconSignalRepositoryError::NotFound),
        }
        // Insert coverage.
        let coverage_id = Uuid::now_v7();
        let coverage_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_beacon_signal_coverage
                (id,workspace_id,beacon_id,event_id,coverage_kind,url,title)
            VALUES ($1,$2,$3,$4,$5,$6,$7)
            ON CONFLICT (workspace_id,beacon_id,event_id,url) DO UPDATE SET
                title=COALESCE(EXCLUDED.title,viryaos_beacon_signal_coverage.title)
            RETURNING id
            "#,
        )
        .bind(coverage_id)
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .bind(&command.coverage_kind)
        .bind(&command.url)
        .bind(&command.title)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            tracing::warn!(%e, %event_id, "beacon coverage persistence failed");
            BeaconSignalRepositoryError::Unavailable
        })?;
        // Mark engagement as completed.
        if let Err(error) = sqlx::query(
            r#"
            UPDATE viryaos_beacon_signal_event_engagements
            SET status='completed',completed_at=COALESCE(completed_at,now()),updated_at=now()
            WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 AND status <> 'declined'
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %event_id, "beacon coverage engagement completion failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        // Close campaign.
        if let Err(error) = sqlx::query(
            r#"
            UPDATE viryaos_beacon_campaigns
            SET status='closed',last_reply_disposition='partner',updated_at=now()
            WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3
              AND status <> 'suppressed'
            "#,
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %event_id, "beacon coverage campaign completion failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        // Outbox.
        let event_payload = serde_json::json!({
            "coverage_id": coverage_id,
            "beacon_id": beacon_id,
            "event_id": event_id,
            "coverage_kind": command.coverage_kind,
            "url": command.url,
            "title": command.title,
        });
        if let Err(error) = sqlx::query(
            "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.coverage_submitted',1,$2,$3)",
        )
        .bind(workspace_id)
        .bind(event_payload)
        .bind(&command.request_id_header)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %event_id, "beacon coverage outbox write failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        tx.commit().await.map_err(|e| {
            tracing::warn!(%e, %event_id, "beacon coverage transaction failed to commit");
            BeaconSignalRepositoryError::Unavailable
        })?;
        Ok(SubmitCoverageResult { coverage_id })
    }

    async fn leave(&self, command: &LeaveCommand) -> Result<(), BeaconSignalRepositoryError> {
        let workspace_id = command.workspace_id;
        let beacon_id = command.beacon_id;
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon leave transaction failed to start");
            BeaconSignalRepositoryError::Unavailable
        })?;
        for result in [
            sqlx::query(
                "UPDATE viryaos_beacon_signal_profiles SET status='revoked',invite_token_hash=NULL,invite_expires_at=NULL,revoked_at=now(),paused_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await,
            sqlx::query(
                "UPDATE viryaos_beacon_signal_sessions SET revoked_at=COALESCE(revoked_at,now()) WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL",
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await,
            sqlx::query(
                r#"
                UPDATE fan_push_endpoints endpoint
                SET active=false,invalidated_at=COALESCE(invalidated_at,now()),updated_at=now()
                WHERE endpoint.workspace_id=$1 AND endpoint.audience_kind='beacon' AND endpoint.active
                  AND endpoint.principal_hash IN (
                      SELECT session.token_hash FROM viryaos_beacon_signal_sessions session
                      WHERE session.workspace_id=$1 AND session.beacon_id=$2
                  )
                "#,
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await,
        ] {
            if let Err(error) = result {
                tracing::warn!(%error, %beacon_id, "beacon leave state mutation failed");
                return Err(BeaconSignalRepositoryError::Unavailable);
            }
        }
        if command.do_not_contact
            && let Err(error) = sqlx::query(
                "UPDATE viryaos_beacons SET accepts_outreach=false,do_not_contact=true,version=version+1,updated_at=now() WHERE workspace_id=$1 AND id=$2",
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await
        {
            tracing::warn!(%error, %beacon_id, "beacon global do-not-contact update failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        let event_payload = serde_json::json!({
            "beacon_id": beacon_id,
            "do_not_contact": command.do_not_contact,
        });
        if let Err(error) = sqlx::query(
            "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.signal_left',1,$2,$3)",
        )
        .bind(workspace_id)
        .bind(event_payload)
        .bind(&command.request_id_header)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, %beacon_id, "beacon leave outbox write failed");
            return Err(BeaconSignalRepositoryError::Unavailable);
        }
        tx.commit().await.map_err(|e| {
            tracing::warn!(%e, %beacon_id, "beacon leave transaction failed to commit");
            BeaconSignalRepositoryError::Unavailable
        })?;
        Ok(())
    }
}

fn engagement_rank(value: &str) -> u8 {
    match value {
        "eligible" => 0,
        "notified" => 1,
        "opened" => 2,
        "interested" => 3,
        "helping" => 4,
        "completed" => 5,
        "declined" => 6,
        _ => 0,
    }
}
