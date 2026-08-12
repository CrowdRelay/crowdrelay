pub async fn overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query_as::<_, AudienceOverview>(
        r#"
        SELECT
            (SELECT count(*)::bigint FROM fans f
             WHERE f.workspace_id = $1 AND f.status = 'active') AS active_fans,
            (SELECT count(*)::bigint
             FROM fans f
             WHERE f.workspace_id = $1
               AND f.status = 'active'
               AND EXISTS (
                   SELECT 1
                   FROM fan_consents fc
                   WHERE fc.workspace_id = f.workspace_id
                     AND fc.fan_id = f.id
                     AND fc.purpose = 'marketing'
                     AND fc.granted
                     AND fc.id = (
                         SELECT newest.id
                         FROM fan_consents newest
                         WHERE newest.workspace_id = fc.workspace_id
                           AND newest.fan_id = fc.fan_id
                           AND newest.purpose = fc.purpose
                         ORDER BY newest.recorded_at DESC, newest.id DESC
                         LIMIT 1
                     )
               )) AS marketing_consented_fans,
            (SELECT count(DISTINCT f.id)::bigint
             FROM fans f
             JOIN ticket_orders orders
               ON orders.workspace_id = f.workspace_id
              AND orders.buyer_email = f.normalized_email
             WHERE f.workspace_id = $1
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS ticket_buyers,
            (SELECT count(DISTINCT passes.fan_id)::bigint
             FROM admission_passes passes
             WHERE passes.workspace_id = $1
               AND passes.status = 'redeemed') AS attendees,
            (SELECT count(DISTINCT entries.fan_id)::bigint
             FROM synesthesia_reward_entries entries
             WHERE entries.workspace_id = $1) AS synesthesia_participants,
            (SELECT count(*)::bigint
             FROM referral_attributions referrals
             WHERE referrals.workspace_id = $1
               AND referrals.status = 'qualified') AS qualified_referrals,
            (SELECT count(*)::bigint
             FROM ticket_orders orders
             WHERE orders.workspace_id = $1
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS paid_ticket_orders
        "#,
    )
    .bind(workspace_id)
    .fetch_one(&state.database)
    .await;
    private_json(result, &headers)
}
pub async fn list_fans(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<FanListQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return bad_request(&headers);
    }
    let search = query
        .search
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if search
        .as_ref()
        .is_some_and(|value| value.chars().count() > 160)
    {
        return bad_request(&headers);
    }
    let city_slug = query
        .city_slug
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if city_slug.as_ref().is_some_and(|value| !valid_slug(value)) {
        return bad_request(&headers);
    }

    let result = load_fan_cards(
        &state,
        state.ticketing.workspace_id().into_uuid(),
        search.as_deref(),
        city_slug.as_deref(),
        limit,
    )
    .await;
    private_json(result, &headers)
}

pub async fn fan_detail(
    State(state): State<crate::AppState>,
    Path(fan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let fan = load_fan_card(&state, workspace_id, fan_id).await;
    let fan = match fan {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not load fan 360 card");
            return unavailable(&headers);
        }
    };

    let email = fan.email.clone();
    let (acquisitions, event_interests, attendance, ticket_purchases, rewards, synesthesia, tags) = tokio::join!(
        sqlx::query_as::<_, AcquisitionTouch>(
            r#"
            SELECT acquisition.source, campaign.name AS campaign_name, acquisition.occurred_at
            FROM fan_acquisition_events acquisition
            LEFT JOIN campaigns campaign
              ON campaign.workspace_id = acquisition.workspace_id
             AND campaign.id = acquisition.campaign_id
            WHERE acquisition.workspace_id = $1 AND acquisition.fan_id = $2
            ORDER BY acquisition.occurred_at DESC, acquisition.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, EventInterestTouch>(
            r#"
            SELECT event.slug AS event_slug, event.title AS event_title, interest.created_at
            FROM event_interests interest
            JOIN events event
              ON event.workspace_id = interest.workspace_id
             AND event.id = interest.event_id
            WHERE interest.workspace_id = $1 AND interest.fan_id = $2
            ORDER BY interest.created_at DESC, event.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, AttendanceTouch>(
            r#"
            SELECT event.slug AS event_slug, event.title AS event_title,
                   pass.status, pass.redeemed_at
            FROM admission_passes pass
            JOIN events event
              ON event.workspace_id = pass.workspace_id
             AND event.id = pass.event_id
            WHERE pass.workspace_id = $1 AND pass.fan_id = $2
            ORDER BY COALESCE(pass.redeemed_at, pass.issued_at) DESC, pass.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, TicketPurchase>(
            r#"
            SELECT orders.public_reference AS order_reference,
                   event.slug AS event_slug,
                   event.title AS event_title,
                   orders.status,
                   orders.currency::text AS currency,
                   orders.amount_gross_minor,
                   orders.amount_refunded_minor,
                   orders.paid_at
            FROM ticket_orders orders
            JOIN ticket_sales sale
              ON sale.workspace_id = orders.workspace_id
             AND sale.id = orders.ticket_sale_id
            JOIN events event
              ON event.workspace_id = sale.workspace_id
             AND event.id = sale.event_id
            WHERE orders.workspace_id = $1 AND orders.buyer_email = $2
            ORDER BY orders.created_at DESC, orders.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(&email)
        .fetch_all(&state.database),
        sqlx::query_as::<_, RewardTouch>(
            r#"
            SELECT rule.name AS reward_name, rule.reward_type, grant.status, grant.created_at
            FROM reward_grants grant
            JOIN reward_rules rule
              ON rule.workspace_id = grant.workspace_id
             AND rule.id = grant.reward_rule_id
            WHERE grant.workspace_id = $1 AND grant.fan_id = $2
            ORDER BY grant.created_at DESC, grant.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, SynesthesiaTouch>(
            r#"
            SELECT entry.campaign_slug, entry.entered_at, run.completed_at, run.client_total_elapsed_ms
            FROM synesthesia_reward_entries entry
            JOIN synesthesia_runs run
              ON run.workspace_id = entry.workspace_id
             AND run.id = entry.run_id
            WHERE entry.workspace_id = $1 AND entry.fan_id = $2
            ORDER BY entry.entered_at DESC, entry.id DESC
            LIMIT 50
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT tag
            FROM fan_audience_tags
            WHERE workspace_id = $1 AND fan_id = $2
            ORDER BY tag
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
    );

    let detail = match (
        acquisitions,
        event_interests,
        attendance,
        ticket_purchases,
        rewards,
        synesthesia,
        tags,
    ) {
        (Ok(a), Ok(i), Ok(att), Ok(t), Ok(r), Ok(synesthesia), Ok(tags)) => FanDetail {
            fan,
            acquisitions: a,
            event_interests: i,
            attendance: att,
            ticket_purchases: t,
            rewards: r,
            synesthesia,
            tags,
        },
        _ => return unavailable(&headers),
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(detail),
    )
        .into_response()
}

pub async fn add_tag(
    State(state): State<crate::AppState>,
    Path(fan_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<TagRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let tag = payload.tag.trim().to_ascii_lowercase();
    if !valid_tag(&tag) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not begin fan tag transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, (bool, bool)>(
        r#"
        WITH target AS (
            SELECT id
            FROM fans
            WHERE workspace_id = $1 AND id = $2
        ), inserted AS (
            INSERT INTO fan_audience_tags (workspace_id, fan_id, tag, source)
            SELECT $1, target.id, $3, 'operator'
            FROM target
            ON CONFLICT (workspace_id, fan_id, tag) DO NOTHING
            RETURNING 1
        )
        SELECT EXISTS (SELECT 1 FROM target), EXISTS (SELECT 1 FROM inserted)
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(&tag)
    .fetch_one(&mut *transaction)
    .await;
    let (exists, inserted) = match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not tag fan");
            return unavailable(&headers);
        }
    };
    if !exists {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }
    if inserted
        && append_audit(
            &mut transaction,
            workspace_id,
            "audience.tag.added",
            "fan",
            &fan_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({ "tag": tag.clone() }),
        )
        .await
        .is_err()
    {
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %fan_id, "could not commit fan tag transaction");
        return unavailable(&headers);
    }
    (
        if inserted {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(serde_json::json!({ "fan_id": fan_id, "tag": tag })),
    )
        .into_response()
}

pub async fn remove_tag(
    State(state): State<crate::AppState>,
    Path((fan_id, tag)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> Response {
    let tag = tag.trim().to_ascii_lowercase();
    if !valid_tag(&tag) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not begin fan untag transaction");
            return unavailable(&headers);
        }
    };
    let deleted = match sqlx::query(
        "DELETE FROM fan_audience_tags WHERE workspace_id = $1 AND fan_id = $2 AND tag = $3",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(&tag)
    .execute(&mut *transaction)
    .await
    {
        Ok(value) => value.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not untag fan");
            return unavailable(&headers);
        }
    };
    if deleted
        && append_audit(
            &mut transaction,
            workspace_id,
            "audience.tag.removed",
            "fan",
            &fan_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({ "tag": tag }),
        )
        .await
        .is_err()
    {
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %fan_id, "could not commit fan untag transaction");
        return unavailable(&headers);
    }
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response()
}

pub async fn list_segments(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, AudienceSegment>(
        r#"
        SELECT id, slug, name, description, filter, active, created_at, updated_at
        FROM audience_segments
        WHERE workspace_id = $1
        ORDER BY active DESC, name, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

pub async fn create_segment(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateSegmentRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let slug = payload.slug.trim().to_ascii_lowercase();
    let name = payload.name.trim();
    if !valid_slug(&slug)
        || name.is_empty()
        || name.chars().count() > 160
        || payload
            .description
            .as_ref()
            .is_some_and(|value| value.chars().count() > 1000)
        || !payload.filter.validate()
    {
        return bad_request(&headers);
    }
    let filter = match serde_json::to_value(&payload.filter) {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin audience segment transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, AudienceSegment>(
        r#"
        INSERT INTO audience_segments (workspace_id, slug, name, description, filter)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, slug, name, description, filter, active, created_at, updated_at
        "#,
    )
    .bind(workspace_id)
    .bind(&slug)
    .bind(name)
    .bind(payload.description.as_deref())
    .bind(filter)
    .fetch_one(&mut *transaction)
    .await;
    let segment = match result {
        Ok(value) => value,
        Err(error) if database_conflict(&error) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not create audience segment");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "audience.segment.created",
        "audience_segment",
        &segment.id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "slug": segment.slug.clone(), "name": segment.name.clone() }),
    )
    .await
    {
        tracing::warn!(%error, "could not audit audience segment creation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit audience segment transaction");
        return unavailable(&headers);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(segment),
    )
        .into_response()
}

pub async fn preview_segment(
    State(state): State<crate::AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PreviewQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(25);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) || !valid_slug(&slug) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let segment = match load_segment(&state, workspace_id, &slug).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, slug, "could not load audience segment");
            return unavailable(&headers);
        }
    };
    let filter = match serde_json::from_value::<AudienceFilter>(segment.filter.clone()) {
        Ok(value) if value.validate() => value,
        _ => return unavailable(&headers),
    };
    let result = match segment_members(&state, workspace_id, &filter, limit).await {
        Ok((total, sample)) => SegmentPreview {
            segment,
            total,
            sample,
        },
        Err(error) => {
            tracing::warn!(%error, slug, "could not preview audience segment");
            return unavailable(&headers);
        }
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(result),
    )
        .into_response()
}

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
