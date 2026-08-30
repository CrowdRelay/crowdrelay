use super::*;
use async_trait::async_trait;
use crowdrelay_application::{
    BeaconReleaseAdminError, BeaconReleaseAdminRepository, CloseReleaseCampaignCommand,
    CloseReleaseCampaignResult, CreateReleaseCampaignCommand, CreateReleaseCampaignResult,
    LaunchReleaseCampaignCommand, LaunchReleaseCampaignResult, UpdateReleaseRecipientCommand,
    UpdateReleaseRecipientResult,
};

#[async_trait]
impl BeaconReleaseAdminRepository for PostgresBeaconReleaseRepository {
    async fn create_release_campaign(
        &self,
        command: &CreateReleaseCampaignCommand,
    ) -> Result<CreateReleaseCampaignResult, BeaconReleaseAdminError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::warn!(%e, "beacon release campaign transaction failed to start");
            BeaconReleaseAdminError::Unavailable
        })?;
        // Idempotency replay check.
        if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
            "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
        )
        .bind(command.workspace_id)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        {
            if action != "create_beacon_release_campaign" {
                return Err(BeaconReleaseAdminError::Conflict);
            }
            tx.commit().await.map_err(|_| BeaconReleaseAdminError::Unavailable)?;
            return Ok(CreateReleaseCampaignResult {
                campaign_id: target_id,
                replayed: true,
            });
        }
        // SKU lookup.
        let variant_id = match sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM merch_variants WHERE workspace_id=$1 AND sku=$2 AND active",
        )
        .bind(command.workspace_id)
        .bind(&command.sku)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconReleaseAdminError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "beacon release SKU lookup failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        };
        let campaign_id = Uuid::now_v7();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_release_campaigns
              (id,workspace_id,slug,title,variant_id,status,claim_deadline)
            VALUES ($1,$2,$3,$4,$5,'draft',$6)
            "#,
        )
        .bind(campaign_id)
        .bind(command.workspace_id)
        .bind(&command.slug)
        .bind(&command.title)
        .bind(variant_id)
        .bind(command.claim_deadline)
        .execute(&mut *tx)
        .await
        {
            if matches!(error, sqlx::Error::Database(ref database) if database.is_unique_violation())
            {
                return Err(BeaconReleaseAdminError::Conflict);
            }
            tracing::warn!(%error, "beacon release campaign insert failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        match record_operator_action(
            &mut tx,
            command.workspace_id,
            OperatorActionRecord {
                action: "create_beacon_release_campaign",
                target_type: "beacon_release_campaign",
                target_id: campaign_id,
                idempotency_key: &command.idempotency_key,
                request_id: command.request_id.as_deref(),
                details: serde_json::json!({
                    "slug": command.slug,
                    "sku": command.sku,
                    "claim_deadline": command.claim_deadline
                }),
            },
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Err(BeaconReleaseAdminError::Conflict),
            Err(error) => {
                tracing::warn!(%error, "beacon release campaign audit failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        }
        tx.commit()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        Ok(CreateReleaseCampaignResult {
            campaign_id,
            replayed: false,
        })
    }

    async fn launch_release_campaign(
        &self,
        command: &LaunchReleaseCampaignCommand,
    ) -> Result<LaunchReleaseCampaignResult, BeaconReleaseAdminError> {
        let workspace_id = command.workspace_id;
        let campaign_id = command.campaign_id;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        // Idempotency replay check.
        if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
            "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
        )
        .bind(workspace_id)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        {
            if action != "launch_beacon_release_campaign" || target_id != campaign_id {
                return Err(BeaconReleaseAdminError::Conflict);
            }
            tx.commit().await.map_err(|_| BeaconReleaseAdminError::Unavailable)?;
            return Ok(LaunchReleaseCampaignResult {
                replayed: true,
                eligible_count: 0,
                reserved_quantity: 0,
                available_before_reservation: 0,
            });
        }
        // Campaign lookup.
        let campaign = match sqlx::query_as::<_, (Uuid, String, String, OffsetDateTime, String)>(
            r#"
            SELECT variant_id,slug,title,claim_deadline,status
            FROM viryaos_beacon_release_campaigns
            WHERE workspace_id=$1 AND id=$2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconReleaseAdminError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "beacon release launch campaign lookup failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        };
        if campaign.4 != "draft" || campaign.3 <= OffsetDateTime::now_utc() {
            return Err(BeaconReleaseAdminError::Conflict);
        }
        // Executor capability check.
        match executor_capability_available_tx(&mut tx, workspace_id, "beacon.release.mail").await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(campaign_id=%campaign_id, "beacon release launch blocked: mail executor capability unavailable");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
            Err(error) => {
                tracing::warn!(%error, "beacon release executor capability lookup failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        }
        // Eligibility snapshot.
        let eligible = match sqlx::query_as::<_, (Uuid, String, String, String)>(
            r#"
            SELECT beacon.id,beacon.display_name,beacon.contact_email,profile.locale
            FROM viryaos_beacon_signal_profiles profile
            JOIN viryaos_beacons beacon
              ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
            WHERE profile.workspace_id=$1 AND profile.status='active'
              AND 'releases'=ANY(profile.topics)
              AND beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
              AND beacon.contact_email IS NOT NULL AND btrim(beacon.contact_email) <> ''
            ORDER BY beacon.id
            FOR UPDATE OF profile,beacon
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "beacon release eligibility snapshot failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        };
        let active_release_count = match sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)::bigint
            FROM viryaos_beacon_signal_profiles profile
            JOIN viryaos_beacons beacon
              ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
            WHERE profile.workspace_id=$1 AND profile.status='active'
              AND 'releases'=ANY(profile.topics)
              AND beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "beacon release active count failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        };
        if active_release_count != eligible.len() as i64 || eligible.is_empty() {
            return Err(BeaconReleaseAdminError::Conflict);
        }
        let Ok(eligible_count) = i32::try_from(eligible.len()) else {
            return Err(BeaconReleaseAdminError::Conflict);
        };
        // Inventory availability check.
        let availability = match inventory_availability_tx(&mut tx, workspace_id, campaign.0).await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconReleaseAdminError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "beacon release inventory lookup failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        };
        let available = availability.on_hand.saturating_sub(availability.reserved);
        if available < i64::from(eligible_count) {
            tracing::info!(campaign_id=%campaign_id, available, eligible_count, "beacon release launch blocked by stock");
            return Err(BeaconReleaseAdminError::Conflict);
        }
        // Reserve stock.
        let reservation_id = Uuid::now_v7();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO inventory_reservations
              (id,workspace_id,reservation_kind,external_reference,request_hash,status,expires_at)
            VALUES ($1,$2,'campaign',$3,$4,'active',NULL)
            "#,
        )
        .bind(reservation_id)
        .bind(workspace_id)
        .bind(format!("beacon-release:{campaign_id}"))
        .bind(request_hash(campaign_id, campaign.0, eligible_count))
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release stock reservation failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        if let Err(error) = sqlx::query(
            "INSERT INTO inventory_reservation_items (workspace_id,reservation_id,variant_id,quantity) VALUES ($1,$2,$3,$4)",
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(campaign.0)
        .bind(eligible_count)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release reservation item failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Create recipients.
        let eligible_ids = eligible.iter().map(|row| row.0).collect::<Vec<_>>();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_release_recipients
              (workspace_id,campaign_id,beacon_id,status)
            SELECT $1,$2,beacon_id,'eligible'
            FROM unnest($3::uuid[]) AS beacon_id
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .bind(&eligible_ids)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release recipient snapshot failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Queue notification emails via outbox.
        let mut mail_beacon_ids = Vec::with_capacity(eligible.len());
        let mut mail_display_names = Vec::with_capacity(eligible.len());
        let mut mail_contact_emails = Vec::with_capacity(eligible.len());
        let mut mail_subjects = Vec::with_capacity(eligible.len());
        let mut mail_texts = Vec::with_capacity(eligible.len());
        let mut mail_request_ids = Vec::with_capacity(eligible.len());
        for (beacon_id, display_name, contact_email, locale) in &eligible {
            let delivery = release_delivery_copy(locale, display_name, &campaign.2, campaign.3);
            mail_beacon_ids.push(*beacon_id);
            mail_display_names.push(display_name.clone());
            mail_contact_emails.push(contact_email.clone());
            mail_subjects.push(delivery.subject);
            mail_texts.push(delivery.text);
            mail_request_ids.push(format!("beacon-release:{campaign_id}:{beacon_id}"));
        }
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO outbox_events
              (workspace_id,event_type,event_version,payload,request_id,max_attempts)
            SELECT $1,'crowdrelay.beacon.release_delivery_confirmation_requested',1,
                   jsonb_build_object(
                     'campaign_id',$2,
                     'campaign_slug',$3,
                     'release_title',$4,
                     'beacon_id',mail.beacon_id,
                     'display_name',mail.display_name,
                     'contact_email',mail.contact_email,
                     'claim_deadline',$5,
                     'member_url',$6,
                     'template_key','beacon_physical_release_confirmation_v1',
                     'subject',mail.subject,
                     'text',mail.body_text
                   ),
                   mail.request_id,12
            FROM unnest(
              $7::uuid[],$8::text[],$9::text[],$10::text[],$11::text[],$12::text[]
            ) AS mail(beacon_id,display_name,contact_email,subject,body_text,request_id)
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .bind(&campaign.1)
        .bind(&campaign.2)
        .bind(campaign.3)
        .bind(RELEASE_MEMBER_URL)
        .bind(&mail_beacon_ids)
        .bind(&mail_display_names)
        .bind(&mail_contact_emails)
        .bind(&mail_subjects)
        .bind(&mail_texts)
        .bind(&mail_request_ids)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release notification bulk outbox insert failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Set campaign status to open.
        if let Err(error) = sqlx::query(
            r#"
            UPDATE viryaos_beacon_release_campaigns
            SET status='open',reservation_id=$3,eligible_count=$4,reserved_quantity=$4,launched_at=now()
            WHERE workspace_id=$1 AND id=$2 AND status='draft'
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .bind(reservation_id)
        .bind(eligible_count)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release campaign launch update failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Audit.
        match record_operator_action(
            &mut tx,
            workspace_id,
            OperatorActionRecord {
                action: "launch_beacon_release_campaign",
                target_type: "beacon_release_campaign",
                target_id: campaign_id,
                idempotency_key: &command.idempotency_key,
                request_id: command.request_id.as_deref(),
                details: serde_json::json!({
                    "eligible_count": eligible_count,
                    "reserved_quantity": eligible_count,
                    "reservation_id": reservation_id,
                    "sku": availability.sku,
                }),
            },
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Err(BeaconReleaseAdminError::Conflict),
            Err(error) => {
                tracing::warn!(%error, "beacon release launch audit failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
        }
        tx.commit()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        Ok(LaunchReleaseCampaignResult {
            replayed: false,
            eligible_count,
            reserved_quantity: eligible_count,
            available_before_reservation: available,
        })
    }

    async fn close_release_campaign(
        &self,
        command: &CloseReleaseCampaignCommand,
    ) -> Result<CloseReleaseCampaignResult, BeaconReleaseAdminError> {
        let workspace_id = command.workspace_id;
        let campaign_id = command.campaign_id;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        // Idempotency replay check.
        if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
            "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
        )
        .bind(workspace_id)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        {
            if action != "close_beacon_release_campaign" || target_id != campaign_id {
                return Err(BeaconReleaseAdminError::Conflict);
            }
            tx.commit().await.map_err(|_| BeaconReleaseAdminError::Unavailable)?;
            return Ok(CloseReleaseCampaignResult {
                already_closed: false,
                replayed: true,
            });
        }
        // Campaign lookup.
        let row = match sqlx::query_as::<_, (String, Option<Uuid>)>(
            "SELECT status,reservation_id FROM viryaos_beacon_release_campaigns WHERE workspace_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconReleaseAdminError::NotFound),
            Err(_) => return Err(BeaconReleaseAdminError::Unavailable),
        };
        if row.0 == "closed" {
            tx.commit()
                .await
                .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
            return Ok(CloseReleaseCampaignResult {
                already_closed: true,
                replayed: false,
            });
        }
        if row.0 != "open" {
            return Err(BeaconReleaseAdminError::Conflict);
        }
        let pending_fulfillment = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM viryaos_beacon_release_recipients WHERE workspace_id=$1 AND campaign_id=$2 AND status IN ('confirmed','prepared')",
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(1);
        if pending_fulfillment > 0 {
            return Err(BeaconReleaseAdminError::Conflict);
        }
        // Release stock reservation.
        if let Some(reservation_id) = row.1
            && let Err(error) = sqlx::query(
                "UPDATE inventory_reservations SET status='released',released_at=now(),release_reason='beacon release campaign closed' WHERE workspace_id=$1 AND id=$2 AND status='active'",
            )
            .bind(workspace_id)
            .bind(reservation_id)
            .execute(&mut *tx)
            .await
        {
            tracing::warn!(%error, "beacon release stock release failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Expire eligible/notified recipients.
        if let Err(error) = sqlx::query(
            r#"
            UPDATE viryaos_beacon_release_recipients
            SET status='expired',expired_at=now(),recipient_name=NULL,recipient_phone=NULL,
                parcel_locker_code=NULL,pii_purged_at=COALESCE(pii_purged_at,now())
            WHERE workspace_id=$1 AND campaign_id=$2 AND status IN ('eligible','notified')
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release recipient expiry failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Close campaign.
        if let Err(error) = sqlx::query(
            "UPDATE viryaos_beacon_release_campaigns SET status='closed',closed_at=now() WHERE workspace_id=$1 AND id=$2 AND status='open'",
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "beacon release close update failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Audit.
        match record_operator_action(
            &mut tx,
            workspace_id,
            OperatorActionRecord {
                action: "close_beacon_release_campaign",
                target_type: "beacon_release_campaign",
                target_id: campaign_id,
                idempotency_key: &command.idempotency_key,
                request_id: command.request_id.as_deref(),
                details: serde_json::json!({"pending_fulfillment": pending_fulfillment}),
            },
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Err(BeaconReleaseAdminError::Conflict),
            Err(_) => return Err(BeaconReleaseAdminError::Unavailable),
        }
        tx.commit()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        Ok(CloseReleaseCampaignResult {
            already_closed: false,
            replayed: false,
        })
    }

    async fn update_release_recipient(
        &self,
        command: &UpdateReleaseRecipientCommand,
    ) -> Result<UpdateReleaseRecipientResult, BeaconReleaseAdminError> {
        let workspace_id = command.workspace_id;
        let campaign_id = command.campaign_id;
        let beacon_id = command.beacon_id;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        // Idempotency replay check.
        if let Ok(Some((action, target_id, details))) = sqlx::query_as::<_, (String, Uuid, serde_json::Value)>(
            "SELECT action,target_id,details FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
        )
        .bind(workspace_id)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        {
            let beacon_id_text = beacon_id.to_string();
            let same_beacon = details.get("beacon_id").and_then(serde_json::Value::as_str)
                == Some(beacon_id_text.as_str());
            let same_status = details.get("to").and_then(serde_json::Value::as_str) == Some(command.status.as_str());
            if action != "update_beacon_release_recipient" || target_id != campaign_id || !same_beacon || !same_status {
                return Err(BeaconReleaseAdminError::Conflict);
            }
            tx.commit().await.map_err(|_| BeaconReleaseAdminError::Unavailable)?;
            return Ok(UpdateReleaseRecipientResult { replayed: true });
        }
        // Recipient + campaign lookup.
        let row = match sqlx::query_as::<_, (String, Uuid, Uuid, String)>(
            r#"
            SELECT recipient.status,campaign.variant_id,campaign.reservation_id,campaign.status
            FROM viryaos_beacon_release_recipients recipient
            JOIN viryaos_beacon_release_campaigns campaign
              ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
            WHERE recipient.workspace_id=$1 AND recipient.campaign_id=$2 AND recipient.beacon_id=$3
              AND campaign.status IN ('open','closed')
            FOR UPDATE OF recipient,campaign
            "#,
        )
        .bind(workspace_id)
        .bind(campaign_id)
        .bind(beacon_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(BeaconReleaseAdminError::NotFound),
            Err(_) => return Err(BeaconReleaseAdminError::Unavailable),
        };
        // Validate transition.
        match crowdrelay_application::validate_beacon_release_recipient_transition(
            row.0.as_str(),
            command.status.as_str(),
            row.3.as_str(),
        ) {
            Ok(_) => {}
            Err(crowdrelay_application::BeaconReleaseTransitionError::InvalidRequestedState) => {
                return Err(BeaconReleaseAdminError::BadRequest);
            }
            Err(_) => return Err(BeaconReleaseAdminError::Conflict),
        }
        // Inventory adjustments for sent/cancelled.
        if command.status == "sent" {
            let remaining = match sqlx::query_scalar::<_, i32>(
                "SELECT quantity FROM inventory_reservation_items WHERE workspace_id=$1 AND reservation_id=$2 AND variant_id=$3 FOR UPDATE",
            )
            .bind(workspace_id)
            .bind(row.2)
            .bind(row.1)
            .fetch_optional(&mut *tx)
            .await
            {
                Ok(Some(value)) if value > 0 => value,
                _ => return Err(BeaconReleaseAdminError::Conflict),
            };
            let ledger_key = format!("beacon-release:{campaign_id}:{beacon_id}:sent");
            if let Err(error) = sqlx::query(
                r#"
                INSERT INTO inventory_ledger
                  (workspace_id,variant_id,delta,movement_kind,idempotency_key,reservation_id,actor_kind,actor_id,reason)
                VALUES ($1,$2,-1,'promotional_issue',$3,$4,'admin','virya-staff','Latarnik physical release')
                ON CONFLICT (workspace_id,idempotency_key) DO NOTHING
                "#,
            )
            .bind(workspace_id)
            .bind(row.1)
            .bind(&ledger_key)
            .bind(row.2)
            .execute(&mut *tx)
            .await
            {
                tracing::warn!(%error, "beacon release promotional issue ledger failed");
                return Err(BeaconReleaseAdminError::Unavailable);
            }
            let reservation_result = if remaining == 1 {
                sqlx::query("DELETE FROM inventory_reservation_items WHERE workspace_id=$1 AND reservation_id=$2 AND variant_id=$3")
                    .bind(workspace_id).bind(row.2).bind(row.1).execute(&mut *tx).await
            } else {
                sqlx::query("UPDATE inventory_reservation_items SET quantity=quantity-1 WHERE workspace_id=$1 AND reservation_id=$2 AND variant_id=$3 AND quantity>1")
                    .bind(workspace_id).bind(row.2).bind(row.1).execute(&mut *tx).await
            };
            match reservation_result {
                Ok(result) if result.rows_affected() == 1 => {}
                Ok(_) => return Err(BeaconReleaseAdminError::Conflict),
                Err(error) => {
                    tracing::warn!(%error, "beacon release reservation decrement failed");
                    return Err(BeaconReleaseAdminError::Unavailable);
                }
            }
        } else if command.status == "cancelled" {
            let decremented = match sqlx::query(
                "UPDATE inventory_reservation_items SET quantity=quantity-1 WHERE workspace_id=$1 AND reservation_id=$2 AND variant_id=$3 AND quantity>1",
            )
            .bind(workspace_id)
            .bind(row.2)
            .bind(row.1)
            .execute(&mut *tx)
            .await
            {
                Ok(result) => result.rows_affected(),
                Err(error) => {
                    tracing::warn!(%error, "beacon release cancellation reservation decrement failed");
                    return Err(BeaconReleaseAdminError::Unavailable);
                }
            };
            if decremented == 0 {
                match sqlx::query(
                    "DELETE FROM inventory_reservation_items WHERE workspace_id=$1 AND reservation_id=$2 AND variant_id=$3 AND quantity=1",
                )
                .bind(workspace_id)
                .bind(row.2)
                .bind(row.1)
                .execute(&mut *tx)
                .await
                {
                    Ok(result) if result.rows_affected() == 1 => {}
                    Ok(_) => return Err(BeaconReleaseAdminError::Conflict),
                    Err(error) => {
                        tracing::warn!(%error, "beacon release cancellation final reservation unit release failed");
                        return Err(BeaconReleaseAdminError::Unavailable);
                    }
                }
            }
        }
        // Update recipient status.
        let activation_due_at = (command.status == "delivered")
            .then(|| OffsetDateTime::now_utc() + time::Duration::days(2));
        let (timestamp_column, purge) = match command.status.as_str() {
            "prepared" => ("prepared_at", false),
            "sent" => ("sent_at", false),
            "delivered" => ("delivered_at", false),
            "cancelled" => ("cancelled_at", true),
            _ => return Err(BeaconReleaseAdminError::BadRequest),
        };
        let sql = format!(
            "UPDATE viryaos_beacon_release_recipients SET status=$4,{timestamp_column}=now(),delivery_details_purge_after=CASE WHEN $4='delivered' THEN now()+interval '30 days' ELSE delivery_details_purge_after END,recipient_name=CASE WHEN $5 THEN NULL ELSE recipient_name END,recipient_phone=CASE WHEN $5 THEN NULL ELSE recipient_phone END,parcel_locker_code=CASE WHEN $5 THEN NULL ELSE parcel_locker_code END,pii_purged_at=CASE WHEN $5 THEN now() ELSE pii_purged_at END,activation_due_at=COALESCE($6,activation_due_at) WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3"
        );
        if let Err(error) = sqlx::query(&sql)
            .bind(workspace_id)
            .bind(campaign_id)
            .bind(beacon_id)
            .bind(&command.status)
            .bind(purge)
            .bind(activation_due_at)
            .execute(&mut *tx)
            .await
        {
            tracing::warn!(%error, "beacon release recipient state update failed");
            return Err(BeaconReleaseAdminError::Unavailable);
        }
        // Audit.
        match record_operator_action(
            &mut tx,
            workspace_id,
            OperatorActionRecord {
                action: "update_beacon_release_recipient",
                target_type: "beacon_release_recipient",
                target_id: campaign_id,
                idempotency_key: &command.idempotency_key,
                request_id: command.request_id.as_deref(),
                details: serde_json::json!({
                    "beacon_id": beacon_id,
                    "from": row.0,
                    "to": command.status,
                    "activation_due_at": activation_due_at,
                }),
            },
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Err(BeaconReleaseAdminError::Conflict),
            Err(_) => return Err(BeaconReleaseAdminError::Unavailable),
        }
        tx.commit()
            .await
            .map_err(|_| BeaconReleaseAdminError::Unavailable)?;
        Ok(UpdateReleaseRecipientResult { replayed: false })
    }
}
