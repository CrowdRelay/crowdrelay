// ── Edge tests: every live channel attributes ──────────────────────
//
// The conversion gate used to require `channel_community IS NOT NULL`,
// which only the Reddit path ever set — Instagram, Telegram and Discord
// clicks were recorded and never attributed. These tests prove the
// relaxed gate: any channel-labelled link attributes at channel level,
// community only where the channel honestly has one.

/// A smart link on an arbitrary channel — the multi-channel variant of
/// the reddit-tagged pair `seed_attribution_scope` builds.
async fn seed_smart_link(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    campaign_id: CampaignId,
    slug: &str,
    channel_source: Option<&str>,
    channel_community: Option<&str>,
) -> Result<SmartLinkId, Box<dyn std::error::Error>> {
    let id = SmartLinkId::new();
    sqlx::query(
        r#"INSERT INTO smart_links
           (id, workspace_id, campaign_id, slug, destination_url, active,
            channel_source, channel_community)
           VALUES ($1, $2, $3, $4, 'https://example.test/d', true, $5, $6)"#,
    )
    .bind(id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .bind(slug)
    .bind(channel_source)
    .bind(channel_community)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Decision + action rows with no post row — the shared half of
/// `seed_action_and_post` for post tables other than community_posts.
async fn seed_action_only(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &str,
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
           VALUES ($1, $2, $3, 'outreach', 'social.post.request', 'agent_outcome', $1,
                   $4, jsonb_build_object('smart_link', '/l/' || $5), 'succeeded', 'third_party', now())"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(format!("attr-test-{action_id}"))
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(action_id)
}

async fn seed_social_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    slug: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO social_posts
           (workspace_id, action_id, platform, content, smart_link, status, posted_at)
           VALUES ($1, $2, 'instagram', '{}'::jsonb, '/l/' || $3, 'posted', now())"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_telegram_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    slug: &str,
    channel: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO telegram_posts
           (workspace_id, action_id, channel, smart_link, status, posted_at)
           VALUES ($1, $2, $3, '/l/' || $4, 'posted', now())"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(channel)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_discord_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    slug: &str,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO discord_posts
           (workspace_id, action_id, channel_id, smart_link, status, posted_at)
           VALUES ($1, $2, $3, '/l/' || $4, 'posted', now())"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(channel_id)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(())
}

/// All conversion rows for a fan — the referral path can legitimately
/// write two, so the single-row `conversion_row` helper is not enough.
type ConversionRow = (String, String, Option<String>, Option<Uuid>, Option<String>);

async fn conversion_rows(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<Vec<ConversionRow>, Box<dyn std::error::Error>> {
    let rows = sqlx::query_as::<_, ConversionRow>(
        r#"SELECT channel, attribution_method, community, action_id, source_target
           FROM fan_provenance_events
           WHERE workspace_id = $1 AND fan_id = $2 AND event_kind = 'conversion'
           ORDER BY channel"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The interaction rows a visitor's clicks wrote — `(fan_id, channel,
/// action_id)`, fan_id NULL until signup links the history.
async fn interaction_rows(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    visitor_id: VisitorId,
) -> Result<Vec<(Option<Uuid>, String, Option<Uuid>)>, Box<dyn std::error::Error>> {
    let rows = sqlx::query_as::<_, (Option<Uuid>, String, Option<Uuid>)>(
        r#"SELECT fan_id, channel, action_id
           FROM fan_provenance_events
           WHERE workspace_id = $1 AND anonymous_visitor_id = $2
             AND event_kind = 'interaction'
           ORDER BY occurred_at"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(Into::<Uuid>::into(visitor_id))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── Test E: an Instagram link with no community still attributes ────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn social_post_without_community_attributes_at_channel_level()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    let slug = format!("ig-{suffix}");
    let link = seed_smart_link(
        &pool,
        workspace_id,
        campaign_id,
        &slug,
        Some("instagram"),
        None,
    )
    .await?;
    let action = seed_action_only(&pool, workspace_id, &slug).await?;
    seed_social_post(&pool, workspace_id, action, &slug).await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link,
        &slug,
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

    let rows = conversion_rows(&pool, workspace_id, fan_id).await?;
    let [(channel, method, community, action_id, _target)] = rows.as_slice() else {
        panic!("expected exactly one conversion row, got {rows:?}");
    };
    assert_eq!(channel, "instagram", "channel is the link's channel_source");
    assert_eq!(method, "last_tracked_click");
    assert_eq!(
        community, &None,
        "an Instagram link carries no community — channel-level attribution, honestly"
    );
    assert_eq!(
        action_id,
        &Some(action),
        "the UNION must resolve action_id through social_posts, not only community_posts"
    );

    pool.close().await;
    Ok(())
}

// ── Test F: a Telegram post attributes with its channel as community ─

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn telegram_post_attributes_with_channel_as_community()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    let slug = format!("tg-{suffix}");
    let link = seed_smart_link(
        &pool,
        workspace_id,
        campaign_id,
        &slug,
        Some("telegram"),
        Some("@bandchat"),
    )
    .await?;
    let action = seed_action_only(&pool, workspace_id, &slug).await?;
    seed_telegram_post(&pool, workspace_id, action, &slug, "@bandchat").await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link,
        &slug,
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

    let rows = conversion_rows(&pool, workspace_id, fan_id).await?;
    let [(channel, method, community, action_id, _target)] = rows.as_slice() else {
        panic!("expected exactly one conversion row, got {rows:?}");
    };
    assert_eq!(channel, "telegram");
    assert_eq!(method, "last_tracked_click");
    assert_eq!(
        community.as_deref(),
        Some("@bandchat"),
        "a telegram channel IS the community the post reached"
    );
    assert_eq!(action_id, &Some(action));

    pool.close().await;
    Ok(())
}

// ── Test G: a Discord post attributes through the same chain ────────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn discord_post_attributes_through_same_chain() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    let slug = format!("dc-{suffix}");
    let link = seed_smart_link(
        &pool,
        workspace_id,
        campaign_id,
        &slug,
        Some("discord"),
        Some("9988776655"),
    )
    .await?;
    let action = seed_action_only(&pool, workspace_id, &slug).await?;
    seed_discord_post(&pool, workspace_id, action, &slug, "9988776655").await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link,
        &slug,
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

    let rows = conversion_rows(&pool, workspace_id, fan_id).await?;
    let [(channel, _method, community, action_id, _target)] = rows.as_slice() else {
        panic!("expected exactly one conversion row, got {rows:?}");
    };
    assert_eq!(channel, "discord");
    assert_eq!(community.as_deref(), Some("9988776655"));
    assert_eq!(action_id, &Some(action));

    pool.close().await;
    Ok(())
}

// ── Test H: a click writes an anonymous interaction; signup links it ─

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn click_writes_interaction_and_signup_links_it_to_the_fan()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;
    let slug_a = format!("link-a-{suffix}");
    let action_a = seed_action_and_post(&pool, workspace_id, link_a, &slug_a, true).await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;

    // Before any signup the click is an anonymous interaction row —
    // fan_id NULL, visitor identity carried for the later link.
    let before = interaction_rows(&pool, workspace_id, visitor_id).await?;
    let [(fan, channel, action_id)] = before.as_slice() else {
        panic!("expected exactly one interaction row before signup, got {before:?}");
    };
    assert_eq!(fan, &None, "an anonymous click must not name a fan");
    assert_eq!(channel, "reddit");
    assert_eq!(
        action_id,
        &Some(action_a),
        "the interaction row resolves the post's action through the same UNION"
    );

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

    // Signup links the anonymous history: the interaction row now names
    // the fan, and the conversion row exists beside it.
    let after = interaction_rows(&pool, workspace_id, visitor_id).await?;
    let [(fan, _channel, _action)] = after.as_slice() else {
        panic!("expected the interaction row to persist after signup, got {after:?}");
    };
    assert_eq!(
        fan,
        &Some(fan_id.into_uuid()),
        "signup must link the visitor's anonymous history to the fan"
    );
    assert!(
        conversion_row(&pool, workspace_id, fan_id).await?.is_some(),
        "the conversion row exists beside the linked interaction"
    );

    pool.close().await;
    Ok(())
}

// ── Test I: an unlabelled link records a click, never a conversion ──

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn unlabelled_link_records_interaction_but_never_converts()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    // channel_source NULL — a link nobody labelled. The click is real
    // and recorded; the conversion gate honestly refuses it.
    let slug = format!("plain-{suffix}");
    let link = seed_smart_link(&pool, workspace_id, campaign_id, &slug, None, None).await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link,
        &slug,
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

    let interactions = interaction_rows(&pool, workspace_id, visitor_id).await?;
    assert_eq!(
        interactions.len(),
        1,
        "the click is still an interaction — it happened"
    );
    assert_eq!(
        interactions.first().map(|(_, channel, _)| channel.as_str()),
        Some("smart_link"),
        "an unlabelled link falls back to the generic channel"
    );
    assert!(
        conversion_rows(&pool, workspace_id, fan_id)
            .await?
            .is_empty(),
        "an unlabelled link writes no conversion — unattributable, not zero-attributed"
    );

    pool.close().await;
    Ok(())
}

// ── Test J: a claimed referral adds a referral conversion row ────────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn claimed_referral_adds_a_referral_row_beside_the_click()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, link_a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;
    let slug_a = format!("link-a-{suffix}");
    let action_a = seed_action_and_post(&pool, workspace_id, link_a, &slug_a, true).await?;

    // The referrer: an existing fan holding an active code.
    let referrer_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO fans (id, workspace_id, normalized_email, display_name, locale, status)
           VALUES ($1, $2, $3, 'Referrer', 'pl-PL', 'active')"#,
    )
    .bind(referrer_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("attr-referrer-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    let code = ReferralCode::parse(format!("refcode-{suffix}"))?;
    sqlx::query("INSERT INTO referral_codes (workspace_id, fan_id, code) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(referrer_id)
        .bind(code.as_str())
        .execute(&pool)
        .await?;

    // The new fan clicked a community link AND claimed the referral.
    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link_a,
        &slug_a,
        campaign_id,
        visitor_id,
    )
    .await?;
    let fan_id = signup_fan_opts(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        visitor_id,
        &suffix,
        Some(code),
    )
    .await?;

    // Both attributions are honest and both are written: the channel
    // that produced the click, and the fan who sent them.
    let rows = conversion_rows(&pool, workspace_id, fan_id).await?;
    assert_eq!(
        rows.len(),
        2,
        "a click-attributed referral carries two conversion rows, got {rows:?}"
    );
    let click_row = rows
        .iter()
        .find(|(channel, _, _, _, _)| channel == "reddit")
        .expect("the click conversion row must exist");
    assert_eq!(click_row.1, "last_tracked_click");
    assert_eq!(click_row.3, Some(action_a));
    let referral_row = rows
        .iter()
        .find(|(channel, _, _, _, _)| channel == "referral")
        .expect("the referral conversion row must exist");
    assert_eq!(referral_row.1, "referral_code");
    assert_eq!(
        referral_row.4.as_deref(),
        Some(format!("fan:{referrer_id}").as_str()),
        "source_target names the referrer, not the code"
    );

    pool.close().await;
    Ok(())
}

// ── Test K: record_fan_arrival writes the provenance row ─────────────

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn fan_arrival_writes_provenance_not_only_acquisition()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, _database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, _workspace_slug, _city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    // A fan who arrived at the merch-table QR — created directly, as the
    // check-in path does, rather than through the public signup.
    let fan_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO fans (id, workspace_id, normalized_email, display_name, locale, status)
           VALUES ($1, $2, $3, 'QR fan', 'pl-PL', 'active')"#,
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("attr-qr-{suffix}@example.test"))
    .execute(&pool)
    .await?;

    let mut tx = pool.begin().await?;
    crowdrelay_infra::acquisition::record_fan_arrival(
        &mut tx,
        workspace_id,
        FanId::from_uuid(fan_id),
        "concert_qr",
        &format!("req-qr-{suffix}"),
        &crowdrelay_infra::acquisition::ArrivalContext {
            source_target: Some(format!("krakow-{suffix}")),
            campaign_id: Some(campaign_id.into_uuid()),
        },
    )
    .await?;
    tx.commit().await?;

    let rows = conversion_rows(&pool, workspace_id, FanId::from_uuid(fan_id)).await?;
    let [(channel, method, community, action_id, target)] = rows.as_slice() else {
        panic!("expected exactly one conversion row, got {rows:?}");
    };
    assert_eq!(channel, "concert_qr");
    assert_eq!(method, "direct_arrival");
    assert_eq!(
        community, &None,
        "a QR code is not a community — the row carries the channel only"
    );
    assert_eq!(action_id, &None);
    assert_eq!(
        target.as_deref(),
        Some(format!("krakow-{suffix}").as_str()),
        "the arrival context's source_target names the event"
    );

    pool.close().await;
    Ok(())
}

// ── Test L: a post bound by smart_link_id alone still attributes ────
//
// The UNION that resolves `post.action_id` used to match only the text
// form `smart_link = '/l/' || slug`. The executors also write
// `smart_link_id` — and a row whose text form is NULL or stale would have
// been invisible to the join, dropping the action attribution while the
// channel rollup still counted the fan.
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn a_post_bound_by_link_id_alone_still_attributes()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_config) = attribution_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let (workspace_id, workspace_slug, city_slug, campaign_id, _a, _b) =
        seed_attribution_scope(&pool, &suffix).await?;

    let slug = format!("tg-idonly-{suffix}");
    let link = seed_smart_link(
        &pool,
        workspace_id,
        campaign_id,
        &slug,
        Some("telegram"),
        Some("@bandchat"),
    )
    .await?;
    let action = seed_action_only(&pool, workspace_id, &slug).await?;
    // The text column stays empty — only the FK binds the post to the link.
    sqlx::query(
        r#"INSERT INTO telegram_posts
           (workspace_id, action_id, channel, smart_link, smart_link_id, status, posted_at)
           VALUES ($1, $2, '@bandchat', NULL, $3, 'posted', now())"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action)
    .bind(link.into_uuid())
    .execute(&pool)
    .await?;

    let visitor_id = VisitorId::new();
    record_click(
        &pool,
        &database_config,
        workspace_id,
        &workspace_slug,
        link,
        &slug,
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

    let rows = conversion_rows(&pool, workspace_id, fan_id).await?;
    let [(channel, _method, _community, action_id, _target)] = rows.as_slice() else {
        panic!("expected exactly one conversion row, got {rows:?}");
    };
    assert_eq!(channel, "telegram");
    assert_eq!(
        action_id,
        &Some(action),
        "the UNION must resolve action_id through smart_link_id when the text column is NULL"
    );

    pool.close().await;
    Ok(())
}
