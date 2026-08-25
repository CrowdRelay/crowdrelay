use super::*;

pub async fn admin_list_release_campaigns(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let pool = match pool_summary(state.ticketing.pool(), workspace_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release pool summary failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let campaigns = match load_campaigns(state.ticketing.pool(), workspace_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release campaign listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let recipients = match sqlx::query_as::<_, AdminReleaseRecipientView>(
        r#"
        SELECT recipient.campaign_id,recipient.beacon_id,beacon.display_name,beacon.beacon_kind,city.name AS city,
               recipient.status,recipient.recipient_name,recipient.recipient_phone,
               recipient.parcel_locker_code,recipient.confirmed_at,recipient.prepared_at,
               recipient.sent_at,recipient.delivered_at,recipient.activation_due_at,
               recipient.activation_queued_at,recipient.activation_suppressed_at
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=recipient.workspace_id AND beacon.id=recipient.beacon_id
        JOIN viryaos_beacon_release_campaigns campaign
          ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
        LEFT JOIN cities city ON city.id=beacon.city_id
        WHERE recipient.workspace_id=$1
          AND (campaign.status='open' OR recipient.status IN ('confirmed','prepared','sent','delivered'))
        ORDER BY campaign.created_at DESC,recipient.campaign_id,
          CASE recipient.status WHEN 'confirmed' THEN 0 WHEN 'prepared' THEN 1 WHEN 'sent' THEN 2 WHEN 'notified' THEN 3 ELSE 4 END,
          beacon.display_name,beacon.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    // Read one past the bound so truncation is detectable without a second
    // count query over the same growing table.
    .bind(MAX_ADMIN_RELEASE_RECIPIENTS + 1)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release open recipient listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let mut recipients = recipients;
    let limit = usize::try_from(MAX_ADMIN_RELEASE_RECIPIENTS).unwrap_or(usize::MAX);
    let recipients_truncated = recipients.len() > limit;
    if recipients_truncated {
        recipients.truncate(limit);
        tracing::warn!(
            limit = MAX_ADMIN_RELEASE_RECIPIENTS,
            "Latarnik release recipient roster truncated"
        );
    }
    private_json(
        StatusCode::OK,
        AdminReleaseCampaignsResponse {
            pool,
            campaigns,
            recipients,
            recipients_truncated,
        },
    )
}

pub async fn admin_create_release_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateReleaseCampaignRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let Some(slug) = clean_slug(&payload.slug) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Some(title) = clean_text(&payload.title, MAX_TITLE_LEN) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Some(sku) = clean_text(&payload.sku, MAX_SKU_LEN) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    if payload.claim_deadline <= OffsetDateTime::now_utc() {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release campaign transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };

    if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        if action != "create_beacon_release_campaign" {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({"campaignId": target_id, "replayed": true}));
    }

    let variant_id = match sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM merch_variants WHERE workspace_id=$1 AND sku=$2 AND active",
    )
    .bind(workspace_id)
    .bind(&sku)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik release SKU lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
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
    .bind(workspace_id)
    .bind(&slug)
    .bind(&title)
    .bind(variant_id)
    .bind(payload.claim_deadline)
    .execute(&mut *tx)
    .await
    {
        if matches!(error, sqlx::Error::Database(ref database) if database.is_unique_violation()) {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        tracing::warn!(%error, "Latarnik release campaign insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "create_beacon_release_campaign",
            target_type: "beacon_release_campaign",
            target_id: campaign_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({"slug": slug, "sku": sku, "claim_deadline": payload.claim_deadline}),
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik release campaign audit failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::CREATED,
        json!({"campaignId": campaign_id, "status": "draft"}),
    )
}

pub async fn admin_launch_release_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release launch transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };

    if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        if action != "launch_beacon_release_campaign" || target_id != campaign_id {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({"campaignId": campaign_id, "replayed": true}));
    }

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
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik release launch campaign lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if campaign.4 != "draft" || campaign.3 <= OffsetDateTime::now_utc() {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    match executor_capability_available_tx(&mut tx, workspace_id, "beacon.release.mail").await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(campaign_id=%campaign_id, "Latarnik release launch blocked: mail executor capability unavailable");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        Err(error) => {
            tracing::warn!(%error, "Latarnik release executor capability lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }

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
            tracing::warn!(%error, "Latarnik release eligibility snapshot failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
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
            tracing::warn!(%error, "Latarnik release active count failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if active_release_count != eligible.len() as i64 || eligible.is_empty() {
        // Every active release Latarnik must be contactable before a campaign can
        // promise a copy. Fix the relationship record instead of silently skipping.
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let Ok(eligible_count) = i32::try_from(eligible.len()) else {
        return BeaconSignalError::Conflict.response(request_id_value);
    };

    let availability = match inventory_availability_tx(&mut tx, workspace_id, campaign.0).await {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik release inventory lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let available = availability.on_hand.saturating_sub(availability.reserved);
    if available < i64::from(eligible_count) {
        tracing::info!(campaign_id=%campaign_id, available, eligible_count, "Latarnik release launch blocked by stock");
        return BeaconSignalError::Conflict.response(request_id_value);
    }

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
        tracing::warn!(%error, "Latarnik release stock reservation failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
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
        tracing::warn!(%error, "Latarnik release reservation item failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }

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
        tracing::warn!(%error, "Latarnik release recipient snapshot failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }

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
        tracing::warn!(%error, "Latarnik release notification bulk outbox insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }

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
        tracing::warn!(%error, "Latarnik release campaign launch update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "launch_beacon_release_campaign",
            target_type: "beacon_release_campaign",
            target_id: campaign_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({
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
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik release launch audit failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({
            "campaignId": campaign_id,
            "status": "open",
            "eligibleCount": eligible_count,
            "reservedQuantity": eligible_count,
            "availableBeforeReservation": available,
        }),
    )
}

pub async fn admin_close_release_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let row = match sqlx::query_as::<_, (String, Option<Uuid>)>(
        "SELECT status,reservation_id FROM viryaos_beacon_release_campaigns WHERE workspace_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if row.0 == "closed" {
        return private_json(
            StatusCode::OK,
            json!({"campaignId": campaign_id, "status": "closed"}),
        );
    }
    if row.0 != "open" {
        return BeaconSignalError::Conflict.response(request_id_value);
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
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    if let Some(reservation_id) = row.1
        && let Err(error) = sqlx::query(
            "UPDATE inventory_reservations SET status='released',released_at=now(),release_reason='beacon release campaign closed' WHERE workspace_id=$1 AND id=$2 AND status='active'",
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(%error, "Latarnik release stock release failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
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
        tracing::warn!(%error, "Latarnik release recipient expiry failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        "UPDATE viryaos_beacon_release_campaigns SET status='closed',closed_at=now() WHERE workspace_id=$1 AND id=$2 AND status='open'",
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik release close update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "close_beacon_release_campaign",
            target_type: "beacon_release_campaign",
            target_id: campaign_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({"pending_fulfillment": pending_fulfillment}),
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"campaignId": campaign_id, "status": "closed"}),
    )
}

pub async fn admin_list_release_recipients(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let exists = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM viryaos_beacon_release_campaigns WHERE workspace_id=$1 AND id=$2)",
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_one(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release recipient campaign lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if !exists {
        return BeaconSignalError::NotFound.response(request_id_value);
    }
    let recipients = match sqlx::query_as::<_, AdminReleaseRecipientView>(
        r#"
        SELECT recipient.campaign_id,recipient.beacon_id,beacon.display_name,beacon.beacon_kind,city.name AS city,
               recipient.status,recipient.recipient_name,recipient.recipient_phone,
               recipient.parcel_locker_code,recipient.confirmed_at,recipient.prepared_at,
               recipient.sent_at,recipient.delivered_at,recipient.activation_due_at,
               recipient.activation_queued_at,recipient.activation_suppressed_at
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=recipient.workspace_id AND beacon.id=recipient.beacon_id
        LEFT JOIN cities city ON city.id=beacon.city_id
        WHERE recipient.workspace_id=$1 AND recipient.campaign_id=$2
        ORDER BY CASE recipient.status
          WHEN 'confirmed' THEN 0 WHEN 'prepared' THEN 1 WHEN 'sent' THEN 2
          WHEN 'notified' THEN 3 WHEN 'delivered' THEN 4 ELSE 5 END,
          beacon.display_name,beacon.id
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik release recipient listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    private_json(
        StatusCode::OK,
        AdminReleaseRecipientsResponse {
            campaign_id,
            recipients,
        },
    )
}

pub async fn admin_update_release_recipient(
    State(state): State<crate::AppState>,
    Path((campaign_id, beacon_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<UpdateReleaseRecipientRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !matches!(
        payload.status.as_str(),
        "prepared" | "sent" | "delivered" | "cancelled"
    ) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if let Ok(Some((action, target_id, details))) = sqlx::query_as::<_, (String, Uuid, serde_json::Value)>(
        "SELECT action,target_id,details FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        let beacon_id_text = beacon_id.to_string();
        let same_beacon = details.get("beacon_id").and_then(serde_json::Value::as_str)
            == Some(beacon_id_text.as_str());
        let same_status = details.get("to").and_then(serde_json::Value::as_str) == Some(payload.status.as_str());
        if action != "update_beacon_release_recipient" || target_id != campaign_id || !same_beacon || !same_status {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({
            "campaignId": campaign_id, "beaconId": beacon_id, "status": payload.status, "replayed": true
        }));
    }
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
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    match crowdrelay_application::validate_beacon_release_recipient_transition(
        row.0.as_str(),
        payload.status.as_str(),
        row.3.as_str(),
    ) {
        Ok(_) => {}
        Err(crowdrelay_application::BeaconReleaseTransitionError::InvalidRequestedState) => {
            return BeaconSignalError::BadRequest.response(request_id_value);
        }
        Err(_) => return BeaconSignalError::Conflict.response(request_id_value),
    }

    if payload.status == "sent" {
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
            _ => return BeaconSignalError::Conflict.response(request_id_value),
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
            tracing::warn!(%error, "Latarnik release promotional issue ledger failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
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
            Ok(_) => return BeaconSignalError::Conflict.response(request_id_value),
            Err(error) => {
                tracing::warn!(%error, "Latarnik release reservation decrement failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    } else if payload.status == "cancelled" {
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
                tracing::warn!(%error, "Latarnik release cancellation reservation decrement failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
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
                Ok(_) => return BeaconSignalError::Conflict.response(request_id_value),
                Err(error) => {
                    tracing::warn!(%error, "Latarnik release cancellation final reservation unit release failed");
                    return BeaconSignalError::Unavailable.response(request_id_value);
                }
            }
        }
    }

    let activation_due_at = (payload.status == "delivered")
        .then(|| OffsetDateTime::now_utc() + time::Duration::days(2));

    let (timestamp_column, purge) = match payload.status.as_str() {
        "prepared" => ("prepared_at", false),
        "sent" => ("sent_at", false),
        "delivered" => ("delivered_at", false),
        "cancelled" => ("cancelled_at", true),
        _ => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    // Column choice comes from the closed enum above, never from user input.
    let sql = format!(
        "UPDATE viryaos_beacon_release_recipients SET status=$4,{timestamp_column}=now(),delivery_details_purge_after=CASE WHEN $4='delivered' THEN now()+interval '30 days' ELSE delivery_details_purge_after END,recipient_name=CASE WHEN $5 THEN NULL ELSE recipient_name END,recipient_phone=CASE WHEN $5 THEN NULL ELSE recipient_phone END,parcel_locker_code=CASE WHEN $5 THEN NULL ELSE parcel_locker_code END,pii_purged_at=CASE WHEN $5 THEN now() ELSE pii_purged_at END,activation_due_at=COALESCE($6,activation_due_at) WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3"
    );
    if let Err(error) = sqlx::query(&sql)
        .bind(workspace_id)
        .bind(campaign_id)
        .bind(beacon_id)
        .bind(&payload.status)
        .bind(purge)
        .bind(activation_due_at)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(%error, "Latarnik release recipient state update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "update_beacon_release_recipient",
            target_type: "beacon_release_recipient",
            target_id: campaign_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({
                "beacon_id": beacon_id,
                "from": row.0,
                "to": payload.status,
                "activation_due_at": activation_due_at,
            }),
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"campaignId": campaign_id, "beaconId": beacon_id, "status": payload.status}),
    )
}
