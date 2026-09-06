// ── Action-level attribution forensic tests ──────────────────────────
//
// These tests prove where action identity is lost and recovered in the
// conversion pipeline. The chain is:
//   click_events.smart_link_id → smart_links.slug
//   → community_posts.smart_link = '/l/' || slug → community_posts.action_id
//
// record_community_conversion now propagates action_id through this chain
// via a LEFT JOIN LATERAL. These tests prove the four attribution classes:
//   A: heuristic (last-click among overlapping actions)
//   B: exact (single action)
//   C: unattributable (no community_posts row)
//   D: deterministic selection (duplicate community_posts rows)

/// Shared setup: connect, migrate, return a disposable pool.
async fn attribution_pool() -> Result<(PgPool, DatabaseConfig), Box<dyn std::error::Error>> {
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|e| format!("set CROWDRELAY_TEST_DATABASE_URL: {e}"))?;
    let database_config = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let pool = database::connect(&database_config).await?;
    database::migrate(&pool).await?;
    Ok((pool, database_config))
}

/// Seeds a workspace + city + campaign + two smart_links (with distinct
/// slugs) both tagged to the same community. Returns the IDs.
async fn seed_attribution_scope(
    pool: &PgPool,
    suffix: &str,
) -> Result<
    (
        WorkspaceId,
        WorkspaceSlug,
        CitySlug,
        CampaignId,
        SmartLinkId,
        SmartLinkId,
    ),
    Box<dyn std::error::Error>,
> {
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!("attr-{suffix}"))?;
    let city_slug = CitySlug::parse(format!("attr-city-{suffix}"))?;
    let campaign_id = CampaignId::new();
    let smart_link_a = SmartLinkId::new();
    let smart_link_b = SmartLinkId::new();

    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Attribution test')")
        .bind(workspace_id.into_uuid())
        .bind(workspace_slug.as_str())
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'Attr city', 'PL')")
        .bind(city_slug.as_str())
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO campaigns (id, workspace_id, name) VALUES ($1, $2, 'Attribution')")
        .bind(campaign_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(pool)
        .await?;

    // Two smart links, both tagged to the same community, with distinct slugs.
    let slug_a = format!("link-a-{suffix}");
    let slug_b = format!("link-b-{suffix}");
    sqlx::query(
        r#"INSERT INTO smart_links (id, workspace_id, campaign_id, slug, destination_url, active, channel_source, channel_community)
           VALUES ($1, $2, $3, $4, 'https://example.test/a', true, 'reddit', 'r/attrtest')"#,
    )
    .bind(smart_link_a.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .bind(&slug_a)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO smart_links (id, workspace_id, campaign_id, slug, destination_url, active, channel_source, channel_community)
           VALUES ($1, $2, $3, $4, 'https://example.test/b', true, 'reddit', 'r/attrtest')"#,
    )
    .bind(smart_link_b.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .bind(&slug_b)
    .execute(pool)
    .await?;

    Ok((
        workspace_id,
        workspace_slug,
        city_slug,
        campaign_id,
        smart_link_a,
        smart_link_b,
    ))
}

/// Inserts a decision + action + matching community_posts row.
async fn seed_action_and_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    _smart_link_id: SmartLinkId,
    slug: &str,
    posted: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let action_id = Uuid::now_v7();
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1, $2, $3, 'outreach', 'agent_outcome', $4,
                   'auto_execute', 9000, 'auto_execute', 'attr-test',
                   '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("attr-decision-{action_id}"))
    .bind(action_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1, $2, $3, 'outreach', 'community.engage.request', 'agent_outcome', $1,
                   $4, jsonb_build_object('smart_link', '/l/' || $5), 'succeeded', 'third_party', now())"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(format!("attr-test-{action_id}"))
    .bind(slug)
    .execute(pool)
    .await?;

    let posted_at_sql = if posted { ", posted_at" } else { "" };
    let posted_at_bind = if posted { ", now()" } else { "" };
    let query_str = format!(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, smart_link, status{posted_at_sql})
           VALUES ($1, $2, 'r/attrtest', 'Test post', 'Test body', '/l/' || $3,
                   'posted'{posted_at_bind})"#
    );
    sqlx::query(&query_str)
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(slug)
        .execute(pool)
        .await?;

    Ok(action_id)
}

/// Records a click on a smart_link from a visitor.
async fn record_click(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    workspace_slug: &WorkspaceSlug,
    smart_link_id: SmartLinkId,
    slug: &str,
    campaign_id: CampaignId,
    visitor_id: VisitorId,
) -> Result<(), Box<dyn std::error::Error>> {
    let link = ResolvedSmartLink::new(
        smart_link_id,
        workspace_id,
        Some(campaign_id),
        SmartLinkSlug::parse(slug)?,
        DestinationUrl::parse("https://example.test")?,
        1,
    )?;
    let click = ClickEvent::from_link(
        &link,
        Some(visitor_id),
        Some("example.test".to_owned()),
        OffsetDateTime::now_utc(),
    )?;
    let database_config = DatabaseConfig {
        url: std::env::var(TEST_DATABASE_URL_KEY).unwrap_or_default(),
        max_connections: 8,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let repo = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        &database_config,
        false,
        test_sensitive_response_codec(),
    );
    repo.persist_click_batch(std::slice::from_ref(&click))
        .await?;
    Ok(())
}

/// Signs up a fan with the given visitor_id and returns the fan_id.
#[allow(clippy::too_many_arguments)]
async fn signup_fan(
    pool: &PgPool,
    database_config: &DatabaseConfig,
    workspace_id: WorkspaceId,
    workspace_slug: &WorkspaceSlug,
    city_slug: &CitySlug,
    campaign_id: CampaignId,
    visitor_id: VisitorId,
    suffix: &str,
) -> Result<FanId, Box<dyn std::error::Error>> {
    let repo = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        database_config,
        false,
        test_sensitive_response_codec(),
    );
    let email = format!("attr-{suffix}@example.test");
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(&email)?,
        display_name: Some("Attribution test fan".to_owned()),
        city_slug: city_slug.clone(),
        locale: Some("pl-PL".to_owned()),
        campaign_id: Some(campaign_id),
        visitor_id: Some(visitor_id),
        claimed_referral_code: None,
        consent: MarketingConsent::new(true, "privacy-v1", "attr-test")?,
    })?;
    let command = SignupFanCommand::new(
        IdempotencyKey::parse(format!("idem-attr-{suffix}"))?,
        RequestId::parse(format!("req-attr-{suffix}"))?,
        signup,
    );
    let result = repo.persist_fan_signup(&command).await?;
    Ok(result.fan_id)
}

/// Reads the conversion provenance event for a fan.
async fn conversion_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<Option<(Option<Uuid>, String, Option<String>, Option<Uuid>)>, Box<dyn std::error::Error>>
{
    let row = sqlx::query_as::<_, (Option<Uuid>, String, Option<String>, Option<Uuid>)>(
        r#"SELECT action_id, attribution_method, community, campaign_id
           FROM fan_provenance_events
           WHERE workspace_id = $1 AND fan_id = $2 AND event_kind = 'conversion'"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

// ── Test A: overlapping actions → last-click action_id recovered ────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn overlapping_actions_attribution_proves_action_identity_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, link_b) =
        seed_attribution_scope(&pool, &suffix).await?;

    // Two actions targeting the same community, each with its own smart link.
    let action_a = seed_action_and_post(
        &pool,
        workspace_id,
        link_a,
        &format!("link-a-{suffix}"),
        true,
    )
    .await?;
    let action_b = seed_action_and_post(
        &pool,
        workspace_id,
        link_b,
        &format!("link-b-{suffix}"),
        true,
    )
    .await?;
    assert_ne!(action_a, action_b, "the two actions must be distinct");

    // Same visitor clicks link_a (earlier) then link_b (later).
    let visitor_id = VisitorId::new();
    let slug_a = format!("link-a-{suffix}");
    let slug_b = format!("link-b-{suffix}");
    record_click(
        &pool,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;
    // Small delay to ensure the second click is strictly later.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    record_click(
        &pool,
        workspace_id,
        &workspace_slug,
        link_b,
        &slug_b,
        campaign_id,
        visitor_id,
    )
    .await?;

    // Fan signs up with this visitor_id.
    let fan_id = signup_fan(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        visitor_id,
        &suffix,
    )
    .await?;

    // The conversion must carry action_b's action_id — the last-click action.
    let row = conversion_row(&pool, workspace_id, fan_id).await?;
    let (action_id, method, community, _campaign) =
        row.ok_or("expected a conversion provenance event")?;
    assert_eq!(
        action_id,
        Some(action_b),
        "last-click attribution must recover action_b's action_id, got {action_id:?} (action_a={action_a}, action_b={action_b})"
    );
    assert_eq!(
        method, "last_community_click",
        "attribution method must be last_community_click (heuristic, not exact)"
    );
    assert_eq!(
        community.as_deref(),
        Some("r/attrtest"),
        "community must be the smart link's channel_community"
    );

    pool.close().await;
    Ok(())
}

// ── Test B: single action → exact action_id ─────────────────────────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn single_action_attribution_is_exact() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, _link_b) =
        seed_attribution_scope(&pool, &suffix).await?;

    // One action, one posted community_post.
    let action_a = seed_action_and_post(
        &pool,
        workspace_id,
        link_a,
        &format!("link-a-{suffix}"),
        true,
    )
    .await?;

    // One click, one signup.
    let visitor_id = VisitorId::new();
    let slug_a = format!("link-a-{suffix}");
    record_click(
        &pool,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;
    let fan_id = signup_fan(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        visitor_id,
        &suffix,
    )
    .await?;

    // The conversion must carry action_a's action_id — exact attribution.
    let row = conversion_row(&pool, workspace_id, fan_id).await?;
    let (action_id, method, _community, _campaign) =
        row.ok_or("expected a conversion provenance event")?;
    assert_eq!(
        action_id,
        Some(action_a),
        "single-action attribution must be exact: action_id must match the only action"
    );
    assert_eq!(method, "last_community_click");

    pool.close().await;
    Ok(())
}

// ── Test C: no community_post → action_id IS NULL ───────────────────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn no_community_post_means_action_id_is_null() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, _link_b) =
        seed_attribution_scope(&pool, &suffix).await?;

    // NO community_posts row for this smart link — the link exists but no
    // action posted it. This is the unattributable case.
    let visitor_id = VisitorId::new();
    let slug_a = format!("link-a-{suffix}");
    record_click(
        &pool,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;
    let fan_id = signup_fan(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        visitor_id,
        &suffix,
    )
    .await?;

    // The conversion must still be written (the click happened), but
    // action_id must be NULL — unattributable, not fabricated.
    let row = conversion_row(&pool, workspace_id, fan_id).await?;
    let (action_id, method, _community, _campaign) =
        row.ok_or("expected a conversion provenance event even without a community_post")?;
    assert_eq!(
        action_id, None,
        "action_id must be NULL when no community_posts row exists — unattributable, not fabricated"
    );
    assert_eq!(method, "last_community_click");

    pool.close().await;
    Ok(())
}

// ── Test D: duplicate community_posts → deterministic selection ─────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn duplicate_community_posts_picks_most_recently_posted()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, _link_b) =
        seed_attribution_scope(&pool, &suffix).await?;

    let slug_a = format!("link-a-{suffix}");

    // Action A: posted (Reddit accepted it, posted_at is set).
    let action_a = seed_action_and_post(&pool, workspace_id, link_a, &slug_a, true).await?;

    // Action B: same smart_link slug, but pending (never reached Reddit).
    // This is a schema-permitted duplicate that normal code paths cannot
    // produce — we insert it directly to prove the selection rule.
    let action_b = Uuid::now_v7();
    let decision_b = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1, $2, $3, 'outreach', 'agent_outcome', $4,
                   'auto_execute', 9000, 'auto_execute', 'attr-test-dup',
                   '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, gen_random_uuid())"#,
    )
    .bind(decision_b)
    .bind(workspace_id.into_uuid())
    .bind(format!("attr-decision-dup-{action_b}"))
    .bind(action_b)
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1, $2, $3, 'outreach', 'community.engage.request', 'agent_outcome', $1,
                   $4, jsonb_build_object('smart_link', '/l/' || $5), 'succeeded', 'third_party', now())"#,
    )
    .bind(action_b)
    .bind(workspace_id.into_uuid())
    .bind(decision_b)
    .bind(format!("attr-test-dup-{action_b}"))
    .bind(&slug_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, smart_link, status)
           VALUES ($1, $2, 'r/attrtest', 'Dup post', 'Dup body', '/l/' || $3, 'pending')"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_b)
    .bind(&slug_a)
    .execute(&pool)
    .await?;
    assert_ne!(action_a, action_b, "the two actions must be distinct");

    // Click the smart link and sign up.
    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;
    let fan_id = signup_fan(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        visitor_id,
        &suffix,
    )
    .await?;

    // The conversion must carry action_a's action_id — the posted row,
    // not the pending row. The selection rule is:
    //   ORDER BY posted_at DESC NULLS LAST, created_at DESC
    // A pending row has posted_at = NULL, so it loses to a posted row.
    let row = conversion_row(&pool, workspace_id, fan_id).await?;
    let (action_id, _method, _community, _campaign) =
        row.ok_or("expected a conversion provenance event")?;
    assert_eq!(
        action_id,
        Some(action_a),
        "duplicate community_posts must select the posted row's action_id, not the pending row's. \
         Got {action_id:?} (action_a={action_a}, action_b={action_b})"
    );

    pool.close().await;
    Ok(())
}
