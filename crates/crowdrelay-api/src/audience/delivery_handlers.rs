const DELIVERY_CLAIM_TTL_MINUTES: i64 = 15;

pub async fn delivery_plan(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<DeliveryPlanQuery>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let limit = query.limit.unwrap_or(250);
    if !(1..=MAX_DELIVERY_PLAN_LIMIT).contains(&limit) {
        return bad_request(&headers);
    }
    match crate::ecosystem::feature_enabled(&state, "communication_campaigns_enabled").await {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not read communication campaign feature flag");
            return unavailable(&headers);
        }
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let campaign = match load_campaign(&state, workspace_id, campaign_id).await {
        Ok(Some(value)) if value.status == "scheduled" => value,
        Ok(Some(_)) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not load campaign delivery plan");
            return unavailable(&headers);
        }
    };
    if campaign
        .scheduled_at
        .is_none_or(|value| value > OffsetDateTime::now_utc() + time::Duration::minutes(1))
    {
        return Problem::conflict(request_id(&headers))
            .private()
            .into_response();
    }
    if campaign.channel == "email" {
        match crate::ecosystem::feature_enabled(&state, "mailer_enabled").await {
            Ok(true) => {}
            Ok(false) => {
                return Problem::conflict(request_id(&headers))
                    .private()
                    .into_response();
            }
            Err(error) => {
                tracing::warn!(%error, %campaign_id, "could not read mailer feature flag");
                return unavailable(&headers);
            }
        }
    }
    let segment = match load_segment(&state, workspace_id, &campaign.segment_slug).await {
        Ok(Some(value)) if value.active => value,
        Ok(_) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not load campaign segment");
            return unavailable(&headers);
        }
    };
    let filter = match serde_json::from_value::<AudienceFilter>(segment.filter) {
        Ok(value) if value.validate() => value,
        _ => return unavailable(&headers),
    };
    match ensure_recipient_snapshot(
        &state,
        workspace_id,
        campaign_id,
        &filter,
        campaign.channel.as_str(),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not snapshot campaign recipients");
            return unavailable(&headers);
        }
    }
    if let Err(error) = expire_stale_delivery_claims(&state, workspace_id, campaign_id).await {
        tracing::warn!(%error, %campaign_id, "could not expire stale campaign delivery claims");
        return unavailable(&headers);
    }
    let mut recipients = match delivery_recipients(
        &state,
        workspace_id,
        campaign_id,
        campaign.channel.as_str(),
        query.after_fan_id,
        limit + 1,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not resolve campaign recipients");
            return unavailable(&headers);
        }
    };
    let next_after_fan_id = if i64::try_from(recipients.len()).unwrap_or(i64::MAX) > limit {
        recipients.pop();
        recipients.last().map(|recipient| recipient.fan_id)
    } else {
        None
    };
    let delivery = match delivery_progress(
        &state,
        workspace_id,
        campaign_id,
        campaign.channel.as_str(),
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not read campaign delivery progress");
            return unavailable(&headers);
        }
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(DeliveryPlan {
            campaign,
            recipients,
            next_after_fan_id,
            delivery,
        }),
    )
        .into_response()
}

pub async fn claim_campaign_delivery(
    State(state): State<crate::AppState>,
    Path((campaign_id, fan_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<ClaimCampaignDeliveryRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let attempt_key = payload.attempt_key.trim();
    if attempt_key.is_empty() || attempt_key.len() > 160 || !attempt_key.is_ascii() {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let inserted = sqlx::query_as::<_, CampaignDeliveryState>(
        r#"
        INSERT INTO communication_campaign_deliveries (
            workspace_id, campaign_id, fan_id, attempt_key, status
        )
        SELECT $1, $2, $3, $4, 'claimed'
        FROM communication_campaign_recipients snapshot
        JOIN communication_campaigns campaign
          ON campaign.workspace_id = snapshot.workspace_id
         AND campaign.id = snapshot.campaign_id
        JOIN fans fan
          ON fan.workspace_id = snapshot.workspace_id
         AND fan.id = snapshot.fan_id
        WHERE snapshot.workspace_id = $1
          AND snapshot.campaign_id = $2
          AND snapshot.fan_id = $3
          AND campaign.status = 'scheduled'
          AND fan.status = 'active'
          AND (
              campaign.channel NOT IN ('email', 'push')
              OR EXISTS (
                  SELECT 1
                  FROM fan_consents consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
                    AND consent.id = (
                        SELECT newest.id
                        FROM fan_consents newest
                        WHERE newest.workspace_id = consent.workspace_id
                          AND newest.fan_id = consent.fan_id
                          AND newest.purpose = consent.purpose
                        ORDER BY newest.recorded_at DESC, newest.id DESC
                        LIMIT 1
                    )
              )
          )
        ON CONFLICT (workspace_id, campaign_id, fan_id) DO NOTHING
        RETURNING fan_id, attempt_key, status, provider_reference, error_code,
                  claimed_at, completed_at
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(fan_id)
    .bind(attempt_key)
    .fetch_optional(&state.database)
    .await;
    match inserted {
        Ok(Some(delivery)) => {
            return (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(CampaignDeliveryClaim {
                    delivery,
                    send_allowed: true,
                }),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, %campaign_id, %fan_id, "could not claim campaign delivery");
            return unavailable(&headers);
        }
    }
    let existing = sqlx::query_as::<_, CampaignDeliveryState>(
        r#"
        SELECT fan_id, attempt_key, status, provider_reference, error_code,
               claimed_at, completed_at
        FROM communication_campaign_deliveries
        WHERE workspace_id = $1 AND campaign_id = $2 AND fan_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(fan_id)
    .fetch_optional(&state.database)
    .await;
    match existing {
        Ok(Some(delivery)) if delivery.attempt_key == attempt_key => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(CampaignDeliveryClaim {
                delivery,
                // A replay is ambiguous: fail closed and never auto-send twice.
                send_allowed: false,
            }),
        )
            .into_response(),
        Ok(Some(_)) | Ok(None) => Problem::conflict(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %campaign_id, %fan_id, "could not verify campaign delivery claim");
            unavailable(&headers)
        }
    }
}

pub async fn report_campaign_delivery(
    State(state): State<crate::AppState>,
    Path((campaign_id, fan_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<ReportCampaignDeliveryRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let attempt_key = payload.attempt_key.trim();
    let status = payload.status.trim();
    let provider_reference = payload
        .provider_reference
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let error_code = payload
        .error_code
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if attempt_key.is_empty()
        || attempt_key.len() > 160
        || !attempt_key.is_ascii()
        || !matches!(status, "delivered" | "failed")
        || provider_reference.is_some_and(|value| value.len() > 240 || !value.is_ascii())
        || error_code.is_some_and(|value| value.len() > 120 || !value.is_ascii())
    {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let updated = sqlx::query_as::<_, CampaignDeliveryState>(
        r#"
        UPDATE communication_campaign_deliveries
        SET status = $5,
            provider_reference = $6,
            error_code = $7,
            completed_at = now(),
            updated_at = now()
        WHERE workspace_id = $1
          AND campaign_id = $2
          AND fan_id = $3
          AND attempt_key = $4
          AND status = 'claimed'
        RETURNING fan_id, attempt_key, status, provider_reference, error_code,
                  claimed_at, completed_at
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(fan_id)
    .bind(attempt_key)
    .bind(status)
    .bind(provider_reference)
    .bind(if status == "failed" { error_code } else { None })
    .fetch_optional(&state.database)
    .await;
    match updated {
        Ok(Some(delivery)) => {
            return (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(delivery),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, %campaign_id, %fan_id, "could not report campaign delivery");
            return unavailable(&headers);
        }
    }
    let existing = sqlx::query_as::<_, CampaignDeliveryState>(
        r#"
        SELECT fan_id, attempt_key, status, provider_reference, error_code,
               claimed_at, completed_at
        FROM communication_campaign_deliveries
        WHERE workspace_id = $1 AND campaign_id = $2 AND fan_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(fan_id)
    .fetch_optional(&state.database)
    .await;
    match existing {
        Ok(Some(delivery))
            if delivery.attempt_key == attempt_key && delivery.status == status =>
        {
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(delivery),
            )
                .into_response()
        }
        Ok(Some(_)) | Ok(None) => Problem::conflict(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %campaign_id, %fan_id, "could not verify campaign delivery report replay");
            unavailable(&headers)
        }
    }
}

pub async fn complete_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CompleteCampaignRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    if !(0..=10_000_000).contains(&payload.recipient_count)
        || !(0..=10_000_000).contains(&payload.delivered_count)
        || !(0..=10_000_000).contains(&payload.failed_count)
        || i64::from(payload.delivered_count) + i64::from(payload.failed_count)
            != i64::from(payload.recipient_count)
    {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let campaign = match load_campaign(&state, workspace_id, campaign_id).await {
        Ok(Some(value)) if matches!(value.status.as_str(), "scheduled" | "completed") => value,
        Ok(Some(_)) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not load communication campaign for completion");
            return unavailable(&headers);
        }
    };
    if campaign.status == "scheduled" {
        if let Err(error) = expire_stale_delivery_claims(&state, workspace_id, campaign_id).await {
            tracing::warn!(%error, %campaign_id, "could not expire stale delivery claims before completion");
            return unavailable(&headers);
        }
        let progress = match delivery_progress(
            &state,
            workspace_id,
            campaign_id,
            campaign.channel.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, %campaign_id, "could not verify communication campaign delivery progress");
                return unavailable(&headers);
            }
        };
        if progress.pending_count != 0 || progress.claimed_count != 0 {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        let authoritative = (
            i32::try_from(progress.eligible_count),
            i32::try_from(progress.delivered_count),
            i32::try_from(progress.failed_count),
        );
        let (Ok(recipient_count), Ok(delivered_count), Ok(failed_count)) = authoritative else {
            return unavailable(&headers);
        };
        if payload.recipient_count != recipient_count
            || payload.delivered_count != delivered_count
            || payload.failed_count != failed_count
        {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
    }
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not begin campaign completion transaction");
            return unavailable(&headers);
        }
    };
    let updated = match sqlx::query(
        r#"
        UPDATE communication_campaigns
        SET status = 'completed',
            recipient_count = $3,
            delivered_count = $4,
            failed_count = $5,
            completed_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'scheduled'
          AND recipient_snapshot_at IS NOT NULL
          AND recipient_snapshot_count IS NOT NULL
          AND $3 <= recipient_snapshot_count
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(payload.recipient_count)
    .bind(payload.delivered_count)
    .bind(payload.failed_count)
    .execute(&mut *transaction)
    .await
    {
        Ok(value) => value.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not complete communication campaign");
            return unavailable(&headers);
        }
    };
    if updated {
        if let Err(error) = append_audit(
            &mut transaction,
            workspace_id,
            "communication.campaign.completed",
            "communication_campaign",
            &campaign_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({
                "recipient_count": payload.recipient_count,
                "delivered_count": payload.delivered_count,
                "failed_count": payload.failed_count,
            }),
        )
        .await
        {
            tracing::warn!(%error, %campaign_id, "could not audit campaign completion");
            return unavailable(&headers);
        }
        if let Err(error) = transaction.commit().await {
            tracing::warn!(%error, %campaign_id, "could not commit campaign completion");
            return unavailable(&headers);
        }
        return campaign_response(&state, workspace_id, campaign_id, &headers).await;
    }
    drop(transaction);
    match load_campaign(&state, workspace_id, campaign_id).await {
        Ok(Some(existing))
            if existing.status == "completed"
                && existing.recipient_count == Some(payload.recipient_count)
                && existing.delivered_count == Some(payload.delivered_count)
                && existing.failed_count == Some(payload.failed_count) =>
        {
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(existing),
            )
                .into_response()
        }
        Ok(Some(_)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(None) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not verify campaign completion replay");
            unavailable(&headers)
        }
    }
}

async fn expire_stale_delivery_claims(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE communication_campaign_deliveries
        SET status = 'failed',
            error_code = COALESCE(error_code, 'claim_expired_unknown'),
            completed_at = now(),
            updated_at = now()
        WHERE workspace_id = $1
          AND campaign_id = $2
          AND status = 'claimed'
          AND claimed_at < now() - ($3::bigint * interval '1 minute')
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(DELIVERY_CLAIM_TTL_MINUTES)
    .execute(&state.database)
    .await?;
    Ok(())
}

async fn delivery_progress(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
    channel: &str,
) -> Result<DeliveryProgress, sqlx::Error> {
    let require_marketing = matches!(channel, "email" | "push");
    sqlx::query_as::<_, DeliveryProgress>(
        r#"
        SELECT count(*) FILTER (
                   WHERE delivery.fan_id IS NOT NULL OR (
                   fan.status = 'active'
                   AND ($3::boolean = false OR EXISTS (
                       SELECT 1
                       FROM fan_consents consent
                       WHERE consent.workspace_id = fan.workspace_id
                         AND consent.fan_id = fan.id
                         AND consent.purpose = 'marketing'
                         AND consent.granted
                         AND consent.id = (
                             SELECT newest.id
                             FROM fan_consents newest
                             WHERE newest.workspace_id = consent.workspace_id
                               AND newest.fan_id = consent.fan_id
                               AND newest.purpose = consent.purpose
                             ORDER BY newest.recorded_at DESC, newest.id DESC
                             LIMIT 1
                         )
                   ))
               )
               )::bigint AS eligible_count,
               count(*) FILTER (
                   WHERE delivery.fan_id IS NULL AND (
                   fan.status = 'active'
                   AND ($3::boolean = false OR EXISTS (
                       SELECT 1
                       FROM fan_consents consent
                       WHERE consent.workspace_id = fan.workspace_id
                         AND consent.fan_id = fan.id
                         AND consent.purpose = 'marketing'
                         AND consent.granted
                         AND consent.id = (
                             SELECT newest.id
                             FROM fan_consents newest
                             WHERE newest.workspace_id = consent.workspace_id
                               AND newest.fan_id = consent.fan_id
                               AND newest.purpose = consent.purpose
                             ORDER BY newest.recorded_at DESC, newest.id DESC
                             LIMIT 1
                         )
                   ))
               )
               )::bigint AS pending_count,
               count(*) FILTER (WHERE delivery.status = 'claimed')::bigint AS claimed_count,
               count(*) FILTER (WHERE delivery.status = 'delivered')::bigint AS delivered_count,
               count(*) FILTER (WHERE delivery.status = 'failed')::bigint AS failed_count
        FROM communication_campaign_recipients snapshot
        JOIN fans fan
          ON fan.workspace_id = snapshot.workspace_id
         AND fan.id = snapshot.fan_id
        LEFT JOIN communication_campaign_deliveries delivery
          ON delivery.workspace_id = snapshot.workspace_id
         AND delivery.campaign_id = snapshot.campaign_id
         AND delivery.fan_id = snapshot.fan_id
        WHERE snapshot.workspace_id = $1
          AND snapshot.campaign_id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(require_marketing)
    .fetch_one(&state.database)
    .await
}
