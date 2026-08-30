use super::*;
use crowdrelay_application::BeaconReleaseAdminRepository;

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
    let command = crowdrelay_application::CreateReleaseCampaignCommand {
        workspace_id,
        idempotency_key,
        request_id: request_id_value.clone(),
        slug,
        title,
        sku,
        claim_deadline: payload.claim_deadline,
    };
    match state.beacon_release.create_release_campaign(&command).await {
        Ok(result) => {
            if result.replayed {
                private_json(
                    StatusCode::OK,
                    json!({"campaignId": result.campaign_id, "replayed": true}),
                )
            } else {
                private_json(
                    StatusCode::CREATED,
                    json!({"campaignId": result.campaign_id, "status": "draft"}),
                )
            }
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::NotFound) => {
            BeaconSignalError::NotFound.response(request_id_value)
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::Conflict) => {
            BeaconSignalError::Conflict.response(request_id_value)
        }
        Err(_) => BeaconSignalError::Unavailable.response(request_id_value),
    }
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
    let command = crowdrelay_application::LaunchReleaseCampaignCommand {
        workspace_id,
        campaign_id,
        idempotency_key,
        request_id: request_id_value.clone(),
    };
    match state.beacon_release.launch_release_campaign(&command).await {
        Ok(result) => {
            if result.replayed {
                private_json(
                    StatusCode::OK,
                    json!({"campaignId": campaign_id, "replayed": true}),
                )
            } else {
                private_json(
                    StatusCode::OK,
                    json!({
                        "campaignId": campaign_id,
                        "status": "open",
                        "eligibleCount": result.eligible_count,
                        "reservedQuantity": result.reserved_quantity,
                        "availableBeforeReservation": result.available_before_reservation,
                    }),
                )
            }
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::NotFound) => {
            BeaconSignalError::NotFound.response(request_id_value)
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::Conflict) => {
            BeaconSignalError::Conflict.response(request_id_value)
        }
        Err(_) => BeaconSignalError::Unavailable.response(request_id_value),
    }
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
    let command = crowdrelay_application::CloseReleaseCampaignCommand {
        workspace_id,
        campaign_id,
        idempotency_key,
        request_id: request_id_value.clone(),
    };
    match state.beacon_release.close_release_campaign(&command).await {
        Ok(_result) => private_json(
            StatusCode::OK,
            json!({"campaignId": campaign_id, "status": "closed"}),
        ),
        Err(crowdrelay_application::BeaconReleaseAdminError::NotFound) => {
            BeaconSignalError::NotFound.response(request_id_value)
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::Conflict) => {
            BeaconSignalError::Conflict.response(request_id_value)
        }
        Err(_) => BeaconSignalError::Unavailable.response(request_id_value),
    }
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
    let command = crowdrelay_application::UpdateReleaseRecipientCommand {
        workspace_id,
        campaign_id,
        beacon_id,
        status: payload.status.clone(),
        idempotency_key,
        request_id: request_id_value.clone(),
    };
    match state
        .beacon_release
        .update_release_recipient(&command)
        .await
    {
        Ok(result) => {
            if result.replayed {
                private_json(
                    StatusCode::OK,
                    json!({
                        "campaignId": campaign_id, "beaconId": beacon_id, "status": payload.status, "replayed": true
                    }),
                )
            } else {
                private_json(
                    StatusCode::OK,
                    json!({
                        "campaignId": campaign_id, "beaconId": beacon_id, "status": payload.status
                    }),
                )
            }
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::NotFound) => {
            BeaconSignalError::NotFound.response(request_id_value)
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::Conflict) => {
            BeaconSignalError::Conflict.response(request_id_value)
        }
        Err(crowdrelay_application::BeaconReleaseAdminError::BadRequest) => {
            BeaconSignalError::BadRequest.response(request_id_value)
        }
        Err(_) => BeaconSignalError::Unavailable.response(request_id_value),
    }
}
