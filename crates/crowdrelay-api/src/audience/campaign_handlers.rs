// Communication campaign lifecycle handlers, split out of
// engagement_handlers.rs to keep each chunk under the modularity budget.
// Included by audience.rs, so it shares that module's imports.

pub async fn list_campaigns(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let sql =
        campaign_select_sql("WHERE campaign.workspace_id = $1 ORDER BY campaign.created_at DESC");
    let result = sqlx::query_as::<_, CommunicationCampaign>(&sql)
        .bind(state.ticketing.workspace_id().into_uuid())
        .fetch_all(&state.database)
        .await;
    private_json(result, &headers)
}

pub async fn create_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateCommunicationCampaignRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let slug = payload.slug.trim().to_ascii_lowercase();
    let segment_slug = payload.segment_slug.trim().to_ascii_lowercase();
    let channel = payload.channel.trim().to_ascii_lowercase();
    let name = payload.name.trim();
    let template_key = payload.template_key.trim();
    if !valid_slug(&slug)
        || !valid_slug(&segment_slug)
        || name.is_empty()
        || name.chars().count() > 160
        || template_key.is_empty()
        || template_key.chars().count() > 160
        || !matches!(channel.as_str(), "email" | "push" | "in_app")
        || payload
            .subject
            .as_ref()
            .is_some_and(|value| value.chars().count() > 240)
        || !payload.content.is_object()
    {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin communication campaign transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, CommunicationCampaign>(
        r#"
        WITH selected_segment AS (
            SELECT id, slug
            FROM audience_segments
            WHERE workspace_id = $1 AND slug = $2 AND active
        ), inserted AS (
            INSERT INTO communication_campaigns (
                workspace_id, segment_id, slug, name, channel,
                template_key, subject, content
            )
            SELECT $1, selected_segment.id, $3, $4, $5, $6, $7, $8
            FROM selected_segment
            RETURNING *
        )
        SELECT inserted.id, inserted.slug, inserted.name, inserted.channel,
               selected_segment.slug AS segment_slug,
               inserted.template_key, inserted.subject, inserted.content,
               inserted.status, inserted.scheduled_at, inserted.dispatch_event_id,
               inserted.recipient_count, inserted.delivered_count, inserted.failed_count,
               inserted.completed_at, inserted.cancelled_at,
               inserted.created_at, inserted.updated_at
        FROM inserted
        JOIN selected_segment ON selected_segment.id = inserted.segment_id
        "#,
    )
    .bind(workspace_id)
    .bind(&segment_slug)
    .bind(&slug)
    .bind(name)
    .bind(&channel)
    .bind(template_key)
    .bind(payload.subject.as_deref())
    .bind(payload.content)
    .fetch_optional(&mut *transaction)
    .await;
    let campaign = match result {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) if database_conflict(&error) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not create communication campaign");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.created",
        "communication_campaign",
        &campaign.id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({
            "slug": campaign.slug.clone(),
            "channel": campaign.channel.clone(),
            "segment_slug": campaign.segment_slug.clone(),
            "template_key": campaign.template_key.clone(),
        }),
    )
    .await
    {
        tracing::warn!(%error, "could not audit communication campaign creation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit communication campaign transaction");
        return unavailable(&headers);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(campaign),
    )
        .into_response()
}

pub async fn schedule_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ScheduleCampaignRequest>, JsonRejection>,
) -> Response {
    match crate::ecosystem::feature_enabled(&state, "communication_campaigns_enabled").await {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not read communication campaign feature flag");
            return unavailable(&headers);
        }
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let scheduled_at = match OffsetDateTime::parse(payload.scheduled_at.trim(), &Rfc3339) {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    if scheduled_at < OffsetDateTime::now_utc() - time::Duration::minutes(1) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin campaign scheduling transaction");
            return unavailable(&headers);
        }
    };

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Uuid,
            String,
            String,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT campaign.id, campaign.slug, campaign.channel,
               campaign.segment_id, campaign.template_key,
               campaign.status, campaign.scheduled_at
        FROM communication_campaigns campaign
        WHERE campaign.workspace_id = $1 AND campaign.id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (id, slug, channel, segment_id, template_key, status, existing_schedule) = match row {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not lock communication campaign");
            return unavailable(&headers);
        }
    };
    if status == "scheduled" && existing_schedule == Some(scheduled_at) {
        drop(transaction);
        return campaign_response(&state, workspace_id, campaign_id, &headers).await;
    }
    if status != "draft" {
        return Problem::conflict(request_id(&headers))
            .private()
            .into_response();
    }

    let event_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, available_at, request_id
        )
        VALUES (
            $1,
            'communication.campaign_due',
            1,
            jsonb_build_object(
                'campaign_id', $2::uuid,
                'campaign_slug', $3::text,
                'channel', $4::text,
                'segment_id', $5::uuid,
                'template_key', $6::text
            ),
            $7,
            $8
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(id)
    .bind(&slug)
    .bind(&channel)
    .bind(segment_id)
    .bind(&template_key)
    .bind(scheduled_at)
    .bind(request_id_value.as_deref())
    .fetch_one(&mut *transaction)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not enqueue communication campaign");
            return unavailable(&headers);
        }
    };

    if let Err(error) = sqlx::query(
        r#"
        UPDATE communication_campaigns
        SET status = 'scheduled', scheduled_at = $3, dispatch_event_id = $4
        WHERE workspace_id = $1 AND id = $2 AND status = 'draft'
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(scheduled_at)
    .bind(event_id)
    .execute(&mut *transaction)
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not persist campaign schedule");
        return unavailable(&headers);
    }

    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.scheduled",
        "communication_campaign",
        &campaign_id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "dispatch_event_id": event_id, "scheduled_at": scheduled_at }),
    )
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not audit campaign schedule");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %campaign_id, "could not commit campaign schedule");
        return unavailable(&headers);
    }
    campaign_response(&state, workspace_id, campaign_id, &headers).await
}

pub async fn cancel_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not begin campaign cancellation transaction");
            return unavailable(&headers);
        }
    };
    let cancelled = match sqlx::query_as::<_, (String, Option<Uuid>)>(
        r#"
        UPDATE communication_campaigns
        SET status = 'cancelled', cancelled_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status IN ('draft', 'scheduled')
        RETURNING slug, dispatch_event_id
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *transaction)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM communication_campaigns WHERE workspace_id = $1 AND id = $2)",
            )
            .bind(workspace_id)
            .bind(campaign_id)
            .fetch_one(&mut *transaction)
            .await
            .unwrap_or(false);
            return if exists {
                Problem::conflict(request_id_value)
                    .private()
                    .into_response()
            } else {
                Problem::not_found(request_id_value)
                    .private()
                    .into_response()
            };
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not cancel communication campaign");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.cancelled",
        "communication_campaign",
        &campaign_id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "slug": cancelled.0, "dispatch_event_id": cancelled.1 }),
    )
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not audit campaign cancellation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %campaign_id, "could not commit campaign cancellation");
        return unavailable(&headers);
    }
    campaign_response(&state, workspace_id, campaign_id, &headers).await
}
