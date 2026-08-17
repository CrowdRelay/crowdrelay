pub async fn bootstrap(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    spec: &BootstrapSpec,
) -> Result<BootstrapResult, BootstrapError> {
    validate_database_timeouts(database)?;
    timeout(
        database.operation_timeout,
        bootstrap_inner(pool, workspace_slug, spec),
    )
    .await
    .map_err(|_| BootstrapError::TimedOut)?
}

async fn bootstrap_inner(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    spec: &BootstrapSpec,
) -> Result<BootstrapResult, BootstrapError> {
    let mut transaction = pool.begin().await.map_err(|_| BootstrapError::Database)?;
    acquire_workspace_lock(&mut transaction, workspace_slug).await?;

    let mut changes = BootstrapChanges::default();
    let (workspace_id, workspace_changed) =
        upsert_workspace(&mut transaction, workspace_slug, &spec.workspace_name).await?;
    changes.workspaces = u64::from(workspace_changed);

    ensure_autopilot_policies(&mut transaction, workspace_id).await?;

    for city in &spec.cities {
        let (city_id, city_changed) = upsert_city(&mut transaction, city).await?;
        changes.cities = changes.cities.saturating_add(u64::from(city_changed));
        let aggregate_changed =
            ensure_city_aggregate(&mut transaction, workspace_id, city_id).await?;
        changes.city_aggregates = changes
            .city_aggregates
            .saturating_add(u64::from(aggregate_changed));
    }

    for campaign in &spec.campaigns {
        let (campaign_id, campaign_changed) =
            upsert_campaign(&mut transaction, workspace_id, campaign).await?;
        changes.campaigns = changes
            .campaigns
            .saturating_add(u64::from(campaign_changed));
        for smart_link in &campaign.smart_links {
            let changed =
                upsert_smart_link(&mut transaction, workspace_id, campaign_id, smart_link).await?;
            changes.smart_links = changes.smart_links.saturating_add(u64::from(changed));
        }
    }

    for endpoint in &spec.webhook_endpoints {
        let changed = upsert_webhook_endpoint(&mut transaction, workspace_id, endpoint).await?;
        changes.webhook_endpoints = changes.webhook_endpoints.saturating_add(u64::from(changed));
    }

    for source in &spec.event_sources {
        let changed = upsert_event_source(&mut transaction, workspace_id, source).await?;
        changes.event_sources = changes.event_sources.saturating_add(u64::from(changed));
    }

    for rule in &spec.reward_rules {
        let changed = upsert_reward_rule(&mut transaction, workspace_id, rule).await?;
        changes.reward_rules = changes.reward_rules.saturating_add(u64::from(changed));
    }

    for event in &spec.events {
        let changed = upsert_event(&mut transaction, workspace_id, event).await?;
        changes.events = changes.events.saturating_add(u64::from(changed));
    }

    for pool in &spec.admission_pools {
        let changed = upsert_admission_pool(&mut transaction, workspace_id, pool).await?;
        changes.admission_pools = changes.admission_pools.saturating_add(u64::from(changed));
    }

    for draw in &spec.reward_draws {
        let changed = upsert_reward_draw(&mut transaction, workspace_id, draw).await?;
        changes.reward_draws = changes.reward_draws.saturating_add(u64::from(changed));
    }

    let audit_recorded = !changes.is_empty();
    if audit_recorded {
        append_service_audit(&mut transaction, workspace_id, changes).await?;
    }

    transaction
        .commit()
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok(BootstrapResult {
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        changes,
        audit_recorded,
    })
}

async fn acquire_workspace_lock(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_slug: &WorkspaceSlug,
) -> Result<(), BootstrapError> {
    let lock_name = format!("{BOOTSTRAP_LOCK_PREFIX}{}", workspace_slug.as_str());
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_name)
        .execute(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

async fn upsert_workspace(
    transaction: &mut Transaction<'_, Postgres>,
    slug: &WorkspaceSlug,
    name: &str,
) -> Result<(Uuid, bool), BootstrapError> {
    let changed_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO workspaces (slug, name)
        VALUES ($1, $2)
        ON CONFLICT (slug) DO UPDATE
        SET name = EXCLUDED.name
        WHERE workspaces.name IS DISTINCT FROM EXCLUDED.name
        RETURNING id
        "#,
    )
    .bind(slug.as_str())
    .bind(name)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(id) = changed_id {
        return Ok((id, true));
    }

    let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR UPDATE")
        .bind(slug.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok((id, false))
}

async fn ensure_autopilot_policies(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<(), BootstrapError> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_policies (workspace_id, context)
        SELECT $1, context.name
        FROM (VALUES
            ('ticket_yield'),
            ('fan_lifecycle'),
            ('campaign_lifecycle'),
            ('merchandising'),
            ('merch_pricing'),
            ('merch_bundle'),
            ('booking_opportunity'),
            ('outreach'),
            ('content_supply'),
            ('promotion_budget'),
            ('experimentation'),
            ('show_operations')
        ) AS context(name)
        ON CONFLICT (workspace_id, context) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

async fn upsert_city(
    transaction: &mut Transaction<'_, Postgres>,
    city: &CitySpec,
) -> Result<(Uuid, bool), BootstrapError> {
    let changed_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO cities (
            slug,
            name,
            country_code,
            region,
            latitude,
            longitude
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (country_code, slug) DO UPDATE
        SET
            name = EXCLUDED.name,
            region = EXCLUDED.region,
            latitude = EXCLUDED.latitude,
            longitude = EXCLUDED.longitude
        WHERE ROW(
            cities.name,
            cities.region,
            cities.latitude,
            cities.longitude
        ) IS DISTINCT FROM ROW(
            EXCLUDED.name,
            EXCLUDED.region,
            EXCLUDED.latitude,
            EXCLUDED.longitude
        )
        RETURNING id
        "#,
    )
    .bind(city.slug.as_str())
    .bind(&city.name)
    .bind(city.country.as_str())
    .bind(&city.region)
    .bind(city.latitude)
    .bind(city.longitude)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(id) = changed_id {
        return Ok((id, true));
    }

    let id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM cities
        WHERE country_code = $1
          AND slug = $2
        FOR UPDATE
        "#,
    )
    .bind(city.country.as_str())
    .bind(city.slug.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok((id, false))
}

async fn ensure_city_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query(
        r#"
        INSERT INTO city_aggregates (workspace_id, city_id)
        VALUES ($1, $2)
        ON CONFLICT (workspace_id, city_id) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .rows_affected()
        == 1;
    Ok(changed)
}

async fn upsert_campaign(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    campaign: &CampaignSpec,
) -> Result<(Uuid, bool), BootstrapError> {
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM campaigns
        WHERE workspace_id = $1
          AND name = $2
        ORDER BY id
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign.name)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;

    match existing.as_slice() {
        [] => {
            let id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO campaigns (workspace_id, name, active)
                VALUES ($1, $2, $3)
                RETURNING id
                "#,
            )
            .bind(workspace_id)
            .bind(&campaign.name)
            .bind(campaign.active)
            .fetch_one(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;
            Ok((id, true))
        }
        [id] => {
            let changed = sqlx::query(
                r#"
                UPDATE campaigns
                SET active = $3
                WHERE workspace_id = $1
                  AND id = $2
                  AND active IS DISTINCT FROM $3
                "#,
            )
            .bind(workspace_id)
            .bind(id)
            .bind(campaign.active)
            .execute(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .rows_affected()
                == 1;
            Ok((*id, changed))
        }
        _ => Err(BootstrapError::CampaignIdentityConflict),
    }
}

async fn upsert_smart_link(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    campaign_id: Uuid,
    smart_link: &SmartLinkSpec,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO smart_links (
            workspace_id,
            campaign_id,
            slug,
            destination_url,
            active
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, slug) DO UPDATE
        SET
            campaign_id = EXCLUDED.campaign_id,
            destination_url = EXCLUDED.destination_url,
            active = EXCLUDED.active
        WHERE ROW(
            smart_links.campaign_id,
            smart_links.destination_url,
            smart_links.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.campaign_id,
            EXCLUDED.destination_url,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(smart_link.slug.as_str())
    .bind(smart_link.destination_url.as_str())
    .bind(smart_link.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_webhook_endpoint(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    endpoint: &WebhookEndpointSpec,
) -> Result<bool, BootstrapError> {
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO webhook_endpoints (
            workspace_id,
            name,
            url,
            signing_secret_ref,
            timeout_ms,
            max_attempts,
            active
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, name) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&endpoint.name)
    .bind(endpoint.url.as_str())
    .bind(&endpoint.signing_secret_ref)
    .bind(i32::try_from(endpoint.timeout_ms).map_err(|_| BootstrapError::Database)?)
    .bind(i32::from(endpoint.max_attempts))
    .bind(endpoint.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if inserted.is_some() {
        return Ok(true);
    }

    let existing = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, signing_secret_ref
        FROM webhook_endpoints
        WHERE workspace_id = $1
          AND name = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&endpoint.name)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .ok_or(BootstrapError::Database)?;
    if existing.1 != endpoint.signing_secret_ref {
        return Err(BootstrapError::WebhookSecretReferenceConflict);
    }

    let changed = sqlx::query(
        r#"
        UPDATE webhook_endpoints
        SET
            url = $3,
            timeout_ms = $4,
            max_attempts = $5,
            active = $6
        WHERE workspace_id = $1
          AND id = $2
          AND ROW(url, timeout_ms, max_attempts, active)
              IS DISTINCT FROM ROW($3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(existing.0)
    .bind(endpoint.url.as_str())
    .bind(i32::try_from(endpoint.timeout_ms).map_err(|_| BootstrapError::Database)?)
    .bind(i32::from(endpoint.max_attempts))
    .bind(endpoint.active)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .rows_affected()
        == 1;
    Ok(changed)
}

async fn upsert_event_source(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    source: &EventSourceSpec,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO event_sources (
            workspace_id, provider, artist_name, app_id, default_country_code,
            timezone, sync_interval_seconds, active, next_sync_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
        ON CONFLICT (workspace_id, provider, artist_name) DO UPDATE SET
            app_id = EXCLUDED.app_id,
            default_country_code = EXCLUDED.default_country_code,
            timezone = EXCLUDED.timezone,
            sync_interval_seconds = EXCLUDED.sync_interval_seconds,
            active = EXCLUDED.active,
            sync_lease_until = NULL,
            sync_lease_owner = NULL,
            next_sync_at = CASE
                WHEN EXCLUDED.active THEN now()
                ELSE event_sources.next_sync_at
            END
        WHERE ROW(
            event_sources.app_id,
            event_sources.default_country_code,
            event_sources.timezone,
            event_sources.sync_interval_seconds,
            event_sources.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.app_id,
            EXCLUDED.default_country_code,
            EXCLUDED.timezone,
            EXCLUDED.sync_interval_seconds,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&source.provider)
    .bind(&source.artist_name)
    .bind(&source.app_id)
    .bind(source.default_country_code.as_str())
    .bind(&source.timezone)
    .bind(i32::try_from(source.sync_interval_seconds).map_err(|_| BootstrapError::Database)?)
    .bind(source.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_event(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event: &EventSpec,
) -> Result<bool, BootstrapError> {
    let city_id = if let Some(city_slug) = &event.city_slug {
        let city_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT cities.id
            FROM cities
            INNER JOIN city_aggregates
                ON city_aggregates.city_id = cities.id
                AND city_aggregates.workspace_id = $1
            WHERE cities.slug = $2
            ORDER BY cities.id
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .bind(city_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?
        .ok_or(BootstrapError::Database)?;
        Some(city_id)
    } else {
        None
    };

    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events (
            workspace_id, city_id, slug, title, description, venue, venue_address,
            timezone, starts_at, doors_at, ends_at, ticket_url, listen_url, image_url,
            trailer_url, external_event_url, status, published_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
            $15, $16, $17, CASE WHEN $17 = 'published' THEN now() ELSE NULL END
        )
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            city_id = EXCLUDED.city_id,
            title = EXCLUDED.title,
            description = EXCLUDED.description,
            venue = EXCLUDED.venue,
            venue_address = EXCLUDED.venue_address,
            timezone = EXCLUDED.timezone,
            starts_at = EXCLUDED.starts_at,
            doors_at = EXCLUDED.doors_at,
            ends_at = EXCLUDED.ends_at,
            ticket_url = EXCLUDED.ticket_url,
            listen_url = EXCLUDED.listen_url,
            image_url = EXCLUDED.image_url,
            trailer_url = EXCLUDED.trailer_url,
            external_event_url = EXCLUDED.external_event_url,
            status = EXCLUDED.status,
            published_at = CASE
                WHEN EXCLUDED.status = 'published' THEN COALESCE(events.published_at, now())
                ELSE events.published_at
            END
        WHERE ROW(
            events.city_id, events.title, events.description, events.venue,
            events.venue_address, events.timezone, events.starts_at, events.doors_at,
            events.ends_at, events.ticket_url, events.listen_url, events.image_url,
            events.trailer_url, events.external_event_url, events.status
        ) IS DISTINCT FROM ROW(
            EXCLUDED.city_id, EXCLUDED.title, EXCLUDED.description, EXCLUDED.venue,
            EXCLUDED.venue_address, EXCLUDED.timezone, EXCLUDED.starts_at, EXCLUDED.doors_at,
            EXCLUDED.ends_at, EXCLUDED.ticket_url, EXCLUDED.listen_url, EXCLUDED.image_url,
            EXCLUDED.trailer_url, EXCLUDED.external_event_url, EXCLUDED.status
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(event.slug.as_str())
    .bind(&event.title)
    .bind(&event.description)
    .bind(&event.venue)
    .bind(&event.venue_address)
    .bind(&event.timezone)
    .bind(event.starts_at)
    .bind(event.doors_at)
    .bind(event.ends_at)
    .bind(event.ticket_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.listen_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.image_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.trailer_url.as_ref().map(DestinationUrl::as_str))
    .bind(
        event
            .external_event_url
            .as_ref()
            .map(DestinationUrl::as_str),
    )
    .bind(&event.status)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_admission_pool(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    pool: &AdmissionPoolSpec,
) -> Result<bool, BootstrapError> {
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM events WHERE workspace_id = $1 AND slug = $2 FOR SHARE",
    )
    .bind(workspace_id)
    .bind(pool.event_slug.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .ok_or(BootstrapError::Database)?;
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO admission_pools (
            workspace_id, event_id, slug, name, capacity, active
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (workspace_id, event_id, slug) DO UPDATE SET
            name = EXCLUDED.name,
            capacity = EXCLUDED.capacity,
            active = EXCLUDED.active
        WHERE ROW(admission_pools.name, admission_pools.capacity, admission_pools.active)
            IS DISTINCT FROM ROW(EXCLUDED.name, EXCLUDED.capacity, EXCLUDED.active)
          AND admission_pools.issued_count + admission_pools.reserved_count <= EXCLUDED.capacity
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(pool.slug.as_str())
    .bind(&pool.name)
    .bind(i32::try_from(pool.capacity).map_err(|_| BootstrapError::Database)?)
    .bind(pool.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_reward_rule(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    rule: &RewardRuleSpec,
) -> Result<bool, BootstrapError> {
    let (reward_type, config) = match &rule.config {
        RewardRuleConfig::MerchDiscount {
            discount_percent,
            code_prefix,
        } => (
            "merch_discount",
            json!({
                "discount_percent": discount_percent,
                "expires_days": rule.expires_days,
                "code_prefix": code_prefix,
            }),
        ),
        RewardRuleConfig::PhysicalItem { item_name, sku } => (
            "physical_item",
            json!({
                "item_name": item_name,
                "sku": sku,
                "expires_days": rule.expires_days,
            }),
        ),
    };
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO reward_rules (
            workspace_id,
            name,
            reward_type,
            threshold,
            config,
            active,
            version
        )
        VALUES ($1, $2, $3, $4, $5, $6, 1)
        ON CONFLICT (workspace_id, name) DO UPDATE
        SET
            reward_type = EXCLUDED.reward_type,
            threshold = EXCLUDED.threshold,
            config = EXCLUDED.config,
            active = EXCLUDED.active,
            version = reward_rules.version + 1
        WHERE ROW(
            reward_rules.reward_type,
            reward_rules.threshold,
            reward_rules.config,
            reward_rules.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.reward_type,
            EXCLUDED.threshold,
            EXCLUDED.config,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&rule.name)
    .bind(reward_type)
    .bind(
        rule.threshold
            .map(i32::try_from)
            .transpose()
            .map_err(|_| BootstrapError::Database)?,
    )
    .bind(config)
    .bind(rule.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_reward_draw(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    draw: &RewardDrawSpec,
) -> Result<bool, BootstrapError> {
    let event_id = if let Some(event_slug) = &draw.event_slug {
        Some(
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM events WHERE workspace_id = $1 AND slug = $2 FOR SHARE",
            )
            .bind(workspace_id)
            .bind(event_slug.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let admission_pool_id = if let Some(pool_slug) = &draw.admission_pool_slug {
        let resolved_event_id = event_id.ok_or(BootstrapError::Database)?;
        Some(
            sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM admission_pools
                WHERE workspace_id = $1 AND event_id = $2 AND slug = $3
                FOR SHARE
                "#,
            )
            .bind(workspace_id)
            .bind(resolved_event_id)
            .bind(pool_slug.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let reward_rule_id = if let Some(rule_name) = &draw.reward_rule_name {
        Some(
            sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM reward_rules
                WHERE workspace_id = $1
                  AND name = $2
                  AND reward_type = 'physical_item'
                  AND active
                FOR SHARE
                "#,
            )
            .bind(workspace_id)
            .bind(rule_name)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO reward_draws (
            workspace_id, slug, name, prize_kind, eligibility_kind, eligibility_ref,
            event_id, admission_pool_id, reward_rule_id, winner_count,
            base_entries, entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, opens_at, closes_at, draw_at, status
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16, $17, $18, $19
        )
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            name = EXCLUDED.name,
            prize_kind = EXCLUDED.prize_kind,
            eligibility_kind = EXCLUDED.eligibility_kind,
            eligibility_ref = EXCLUDED.eligibility_ref,
            event_id = EXCLUDED.event_id,
            admission_pool_id = EXCLUDED.admission_pool_id,
            reward_rule_id = EXCLUDED.reward_rule_id,
            winner_count = EXCLUDED.winner_count,
            base_entries = EXCLUDED.base_entries,
            entries_per_referral = EXCLUDED.entries_per_referral,
            entries_per_checkin = EXCLUDED.entries_per_checkin,
            max_entries = EXCLUDED.max_entries,
            claim_expires_hours = EXCLUDED.claim_expires_hours,
            opens_at = EXCLUDED.opens_at,
            closes_at = EXCLUDED.closes_at,
            draw_at = EXCLUDED.draw_at,
            status = EXCLUDED.status,
            attempts = 0,
            last_error = NULL,
            completed_at = NULL
        WHERE reward_draws.status IN ('draft', 'scheduled')
          AND ROW(
              reward_draws.name,
              reward_draws.prize_kind,
              reward_draws.eligibility_kind,
              reward_draws.eligibility_ref,
              reward_draws.event_id,
              reward_draws.admission_pool_id,
              reward_draws.reward_rule_id,
              reward_draws.winner_count,
              reward_draws.base_entries,
              reward_draws.entries_per_referral,
              reward_draws.entries_per_checkin,
              reward_draws.max_entries,
              reward_draws.claim_expires_hours,
              reward_draws.opens_at,
              reward_draws.closes_at,
              reward_draws.draw_at,
              reward_draws.status
          ) IS DISTINCT FROM ROW(
              EXCLUDED.name,
              EXCLUDED.prize_kind,
              EXCLUDED.eligibility_kind,
              EXCLUDED.eligibility_ref,
              EXCLUDED.event_id,
              EXCLUDED.admission_pool_id,
              EXCLUDED.reward_rule_id,
              EXCLUDED.winner_count,
              EXCLUDED.base_entries,
              EXCLUDED.entries_per_referral,
              EXCLUDED.entries_per_checkin,
              EXCLUDED.max_entries,
              EXCLUDED.claim_expires_hours,
              EXCLUDED.opens_at,
              EXCLUDED.closes_at,
              EXCLUDED.draw_at,
              EXCLUDED.status
          )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(draw.slug.as_str())
    .bind(&draw.name)
    .bind(&draw.prize_kind)
    .bind(&draw.eligibility_kind)
    .bind(draw.eligibility_ref.as_ref().map(crowdrelay_domain::EventSlug::as_str))
    .bind(event_id)
    .bind(admission_pool_id)
    .bind(reward_rule_id)
    .bind(i32::try_from(draw.winner_count).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.base_entries).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.entries_per_referral).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.entries_per_checkin).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.max_entries).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.claim_expires_hours).map_err(|_| BootstrapError::Database)?)
    .bind(draw.opens_at)
    .bind(draw.closes_at)
    .bind(draw.draw_at)
    .bind(&draw.status)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn append_service_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    changes: BootstrapChanges,
) -> Result<(), BootstrapError> {
    let metadata = json!({
        "bootstrap_version": 1,
        "changed_rows": {
            "workspaces": changes.workspaces,
            "cities": changes.cities,
            "city_aggregates": changes.city_aggregates,
            "campaigns": changes.campaigns,
            "smart_links": changes.smart_links,
            "webhook_endpoints": changes.webhook_endpoints,
            "event_sources": changes.event_sources,
            "reward_rules": changes.reward_rules,
            "events": changes.events,
            "admission_pools": changes.admission_pools,
            "reward_draws": changes.reward_draws,
            "total": changes.total(),
        }
    });
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id,
            actor_kind,
            action,
            target_type,
            target_id,
            metadata
        )
        VALUES ($1, 'service', $2, 'workspace', $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(AUDIT_ACTION)
    .bind(workspace_id.to_string())
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok(())
}
