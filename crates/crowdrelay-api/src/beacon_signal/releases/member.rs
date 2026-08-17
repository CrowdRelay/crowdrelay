use super::*;

pub async fn my_release_campaigns(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let campaigns = match sqlx::query_as::<_, MemberReleaseCampaignView>(
        r#"
        SELECT campaign.id AS campaign_id,campaign.slug,campaign.title,
               product.name AS product_name,variant.label AS variant_label,
               campaign.status,recipient.status AS recipient_status,campaign.claim_deadline,
               recipient.recipient_name,recipient.recipient_phone,recipient.parcel_locker_code
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacon_release_campaigns campaign
          ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
        JOIN merch_variants variant
          ON variant.workspace_id=campaign.workspace_id AND variant.id=campaign.variant_id
        JOIN merch_products product
          ON product.workspace_id=variant.workspace_id AND product.id=variant.product_id
        WHERE recipient.workspace_id=$1 AND recipient.beacon_id=$2
          AND campaign.status IN ('open','closed')
          AND recipient.status <> 'cancelled'
        ORDER BY campaign.created_at DESC,campaign.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Latarnik release member listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    private_json(StatusCode::OK, MemberReleaseCampaignsResponse { campaigns })
}

pub async fn confirm_release_delivery(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ConfirmReleaseDeliveryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if !principal.topics.iter().any(|topic| topic == "releases") {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let Some(recipient_name) = clean_text(&payload.recipient_name, MAX_NAME_LEN) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Some(recipient_phone) = clean_text(&payload.recipient_phone, MAX_PHONE_LEN) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let Some(parcel_locker_code) = clean_text(&payload.parcel_locker_code, MAX_LOCKER_LEN) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let locker_valid = parcel_locker_code
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    let phone_digits = recipient_phone.bytes().filter(u8::is_ascii_digit).count();
    if !locker_valid || !(7..=15).contains(&phone_digits) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let row = match sqlx::query_as::<_, (String, String, OffsetDateTime)>(
        r#"
        SELECT recipient.status,campaign.title,campaign.claim_deadline
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacon_release_campaigns campaign
          ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
        WHERE recipient.workspace_id=$1 AND recipient.campaign_id=$2 AND recipient.beacon_id=$3
          AND campaign.status='open'
        FOR UPDATE OF recipient,campaign
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(principal.beacon_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if row.2 <= OffsetDateTime::now_utc()
        || !matches!(row.0.as_str(), "eligible" | "notified" | "confirmed")
    {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_release_recipients
        SET status='confirmed',recipient_name=$4,recipient_phone=$5,parcel_locker_code=$6,
            confirmed_at=COALESCE(confirmed_at,now()),pii_purged_at=NULL,
            delivery_details_purge_after=NULL
        WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(principal.beacon_id)
    .bind(&recipient_name)
    .bind(&recipient_phone)
    .bind(&parcel_locker_code)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, campaign_id=%campaign_id, "Latarnik delivery confirmation failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id,max_attempts)
        VALUES ($1,'viryaos.beacon.release_delivery_confirmed',1,$2,$3,12)
        "#,
    )
    .bind(workspace_id)
    .bind(json!({
        "campaign_id": campaign_id,
        "release_title": row.1,
        "beacon_id": principal.beacon_id,
        "display_name": principal.display_name,
    }))
    .bind(format!("beacon-release-confirmed:{campaign_id}:{}", principal.beacon_id))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik delivery confirmation event failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({
            "campaignId": campaign_id,
            "status": "confirmed",
            "message": "Dziękujemy Latarniku — zapisaliśmy Paczkomat dla tego wydania.",
        }),
    )
}

pub async fn decline_release_delivery(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let row = match sqlx::query_as::<_, (String, Uuid, Uuid)>(
        r#"
        SELECT recipient.status,campaign.variant_id,campaign.reservation_id
        FROM viryaos_beacon_release_recipients recipient
        JOIN viryaos_beacon_release_campaigns campaign
          ON campaign.workspace_id=recipient.workspace_id AND campaign.id=recipient.campaign_id
        WHERE recipient.workspace_id=$1 AND recipient.campaign_id=$2 AND recipient.beacon_id=$3
          AND campaign.status='open'
        FOR UPDATE OF recipient,campaign
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(principal.beacon_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if !matches!(row.0.as_str(), "eligible" | "notified" | "confirmed") {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let updated = match sqlx::query(
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
            tracing::warn!(%error, "Latarnik decline reservation decrement failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if updated == 0 {
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
                tracing::warn!(%error, "Latarnik decline final reservation unit release failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_release_recipients
        SET status='declined',declined_at=now(),recipient_name=NULL,recipient_phone=NULL,
            parcel_locker_code=NULL,delivery_details_purge_after=NULL,pii_purged_at=now()
        WHERE workspace_id=$1 AND campaign_id=$2 AND beacon_id=$3
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(principal.beacon_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik release decline update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"campaignId": campaign_id, "status": "declined"}),
    )
}
