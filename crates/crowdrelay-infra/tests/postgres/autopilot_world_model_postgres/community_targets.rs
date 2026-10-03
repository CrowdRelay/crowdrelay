
/// The community pool must rank by what a community produced, not by how
/// big it is.
///
/// `load_community_targets` used to order by `member_count DESC` — the
/// biggest subreddit first, whatever it had ever returned. A community that
/// converted a fan, and then one that produced clicks, must now lead a
/// community that only offers reach: the pool is the control surface the
/// attribution ledger feeds, and ordering it by audience size would make the
/// evidence it just started collecting irrelevant.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_community_that_converted_outranks_a_bigger_quiet_one()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("target-order-{suffix}"))
        .bind("Target order")
        .execute(&pool)
        .await?;

    // Three promoted community targets, largest-first by member count — the
    // order the old query would have returned them in.
    for (subreddit, members) in [
        ("bigsilent", 900_000i32),
        ("clicked", 50_000),
        ("converts", 100),
    ] {
        let place_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO discovery_places
               (id, workspace_id, place_kind, platform, name, url, member_count)
             VALUES ($1, $2, 'subreddit', 'reddit', $3, $4, $5)",
        )
        .bind(place_id)
        .bind(workspace_id.into_uuid())
        .bind(format!("r/{subreddit}"))
        .bind(format!("https://www.reddit.com/r/{subreddit}"))
        .bind(members)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO agent_outreach_targets
               (id, workspace_id, target_kind, display_name, status,
                subreddit, place_id)
             VALUES ($1, $2, 'community', $3, 'promoted', $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("r/{subreddit}"))
        .bind(subreddit)
        .bind(place_id)
        .execute(&pool)
        .await?;
    }

    // A fan whose arrival is attributed to r/converts, and two anonymous
    // clickers through r/clicked links. r/bigsilent produced nothing.
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("target-order-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events
           (workspace_id, fan_id, event_kind, channel, community,
            attribution_method, attribution_confidence, occurred_at)
         VALUES ($1, $2, 'conversion', 'reddit', 'r/converts',
                 'last_tracked_click', 1.0, now())",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .execute(&pool)
    .await?;
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO fan_provenance_events
               (workspace_id, event_kind, channel, community,
                anonymous_visitor_id, attribution_method,
                attribution_confidence, occurred_at)
             VALUES ($1, 'interaction', 'reddit', 'r/clicked', $2,
                     'tracked_click', 1.0, now())",
        )
        .bind(workspace_id.into_uuid())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await?;
    }

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let engager = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .ok_or("no community-engager snapshot was produced")?;

    let names: Vec<&str> = engager
        .unengaged_targets
        .iter()
        .map(|target| target.subreddit.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["converts", "clicked", "bigsilent"],
        "the pool must lead with the community that produced a fan, then the \
         one that produced clicks, and only then the largest unmeasured one"
    );
    let converted = &engager.unengaged_targets[0];
    assert_eq!(converted.converted_fans_90d, 1);
    assert_eq!(converted.interactions_90d, 0);
    // The fan never consented to marketing nor acted since: a conversion,
    // not yet a fan who stayed.
    assert_eq!(converted.durable_fans_90d, 0);
    let clicked = &engager.unengaged_targets[1];
    assert_eq!(clicked.converted_fans_90d, 0);
    assert_eq!(clicked.interactions_90d, 2);

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn community_yield_counts_one_canonical_person_and_latest_consent()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    let suffix = ws.simple().to_string();
    sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'canonical community yield')")
        .bind(ws)
        .bind(format!("canonical-yield-{suffix}"))
        .execute(&pool)
        .await?;

    let place_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO discovery_places
           (id,workspace_id,place_kind,platform,name,url,member_count)
         VALUES($1,$2,'subreddit','reddit','r/canonicalfans',
                'https://www.reddit.com/r/canonicalfans',5000)",
    )
    .bind(place_id)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_outreach_targets
           (workspace_id,target_kind,display_name,status,subreddit,place_id,screening_verdict)
         VALUES($1,'community','r/canonicalfans','promoted','canonicalfans',$2,'admitted')",
    )
    .bind(ws)
    .bind(place_id)
    .execute(&pool)
    .await?;

    let root = Uuid::now_v7();
    let historical = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans(id,workspace_id,normalized_email,status)
         VALUES($1,$3,$4,'active'),($2,$3,$5,'merged')",
    )
    .bind(root)
    .bind(historical)
    .bind(ws)
    .bind(format!("canonical-root-{suffix}@example.test"))
    .bind(format!("canonical-old-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE fans SET merged_into_fan_id=$1 WHERE workspace_id=$2 AND id=$3",
    )
    .bind(root)
    .bind(ws)
    .bind(historical)
    .execute(&pool)
    .await?;

    // Both historical IDs received conversion rows before identity resolution.
    // They are one person now and must contribute exactly one conversion.
    for fan_id in [historical, root] {
        sqlx::query(
            "INSERT INTO fan_provenance_events
               (workspace_id,fan_id,event_kind,channel,community,
                attribution_method,attribution_confidence,occurred_at)
             VALUES($1,$2,'conversion','reddit','r/canonicalfans',
                    'last_tracked_click',1.0,now())",
        )
        .bind(ws)
        .bind(fan_id)
        .execute(&pool)
        .await?;
    }

    // A real recent first-party action exists on the survivor.
    sqlx::query(
        "INSERT INTO fan_sessions(
             workspace_id,fan_id,session_token_hash,last_seen_at,expires_at
         ) VALUES($1,$2,$3,now(),now()+interval '30 days')",
    )
    .bind(ws)
    .bind(root)
    .bind(root.as_bytes().to_vec())
    .execute(&pool)
    .await?;

    // Same timestamp, opposite decisions. id is the deterministic tie-breaker:
    // the later inserted revoke must win and durable_fans must stay zero.
    let at = OffsetDateTime::now_utc() - time::Duration::hours(1);
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let (grant_id, revoke_id) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    sqlx::query(
        "INSERT INTO fan_consents(
             id,workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at
         ) VALUES($1,$3,$4,'marketing',true,'v1','test',$5),
                 ($2,$3,$4,'marketing',false,'v2','test',$5)",
    )
    .bind(grant_id)
    .bind(revoke_id)
    .bind(ws)
    .bind(root)
    .bind(at)
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let load = || repository.load_growth_intelligence_snapshots(
        workspace_id,
        OffsetDateTime::now_utc(),
    );

    let snapshots = load().await?;
    let target = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .and_then(|snapshot| {
            snapshot
                .unengaged_targets
                .iter()
                .find(|target| target.subreddit == "canonicalfans")
        })
        .ok_or("canonical target missing")?;
    assert_eq!(target.converted_fans_90d, 1);
    assert_eq!(
        target.durable_fans_90d, 0,
        "latest revoke wins even when grant/revoke timestamps tie"
    );

    sqlx::query(
        "INSERT INTO fan_consents(
             workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at
         ) VALUES($1,$2,'marketing',true,'v3','test',now())",
    )
    .bind(ws)
    .bind(root)
    .execute(&pool)
    .await?;
    let snapshots = load().await?;
    let target = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .and_then(|snapshot| {
            snapshot
                .unengaged_targets
                .iter()
                .find(|target| target.subreddit == "canonicalfans")
        })
        .ok_or("canonical target missing after grant")?;
    assert_eq!(target.converted_fans_90d, 1);
    assert_eq!(
        target.durable_fans_90d, 1,
        "the one canonical person becomes durable after the latest explicit grant"
    );
    Ok(())
}

/// A Telegram chat named `deathcore` must not credit r/deathcore.
///
/// Telegram and Discord links write their own `channel_community` — a chat
/// or server name — into the same provenance column a Reddit link fills with
/// a subreddit. The community-target join normalized on the name alone, so a
/// telegram conversion in a chat called 'deathcore' would have ranked the
/// subreddit for fans it never produced. The channel predicate is what keeps
/// the vocabularies apart.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_telegram_channel_named_like_a_subreddit_credits_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("chan-collision-{suffix}"))
        .bind("Channel collision")
        .execute(&pool)
        .await?;

    // Two promoted community targets — the colliding name and a control.
    for (subreddit, members) in [("deathcore", 900_000i32), ("quietreal", 100)] {
        let place_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO discovery_places
               (id, workspace_id, place_kind, platform, name, url, member_count)
             VALUES ($1, $2, 'subreddit', 'reddit', $3, $4, $5)",
        )
        .bind(place_id)
        .bind(workspace_id.into_uuid())
        .bind(format!("r/{subreddit}"))
        .bind(format!("https://www.reddit.com/r/{subreddit}"))
        .bind(members)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO agent_outreach_targets
               (id, workspace_id, target_kind, display_name, status,
                subreddit, place_id)
             VALUES ($1, $2, 'community', $3, 'promoted', $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("r/{subreddit}"))
        .bind(subreddit)
        .bind(place_id)
        .execute(&pool)
        .await?;
    }

    // A fan converted through a TELEGRAM chat called 'deathcore', and a real
    // reddit conversion for the control. Plus a telegram interaction on the
    // colliding name.
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("chan-collision-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events
           (workspace_id, fan_id, event_kind, channel, community,
            attribution_method, attribution_confidence, occurred_at)
         VALUES ($1, $2, 'conversion', 'telegram', 'deathcore',
                 'last_tracked_click', 1.0, now())",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .execute(&pool)
    .await?;
    let control_fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind(control_fan)
    .bind(workspace_id.into_uuid())
    .bind(format!("chan-control-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events
           (workspace_id, fan_id, event_kind, channel, community,
            attribution_method, attribution_confidence, occurred_at)
         VALUES ($1, $2, 'conversion', 'reddit', 'r/quietreal',
                 'last_tracked_click', 1.0, now())",
    )
    .bind(workspace_id.into_uuid())
    .bind(control_fan)
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let engager = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .ok_or("no community-engager snapshot was produced")?;

    let deathcore = engager
        .unengaged_targets
        .iter()
        .find(|target| target.subreddit == "deathcore")
        .expect("the deathcore target is in the pool");
    assert_eq!(
        deathcore.converted_fans_90d, 0,
        "a telegram chat's conversions must not credit the subreddit"
    );
    assert_eq!(deathcore.interactions_90d, 0);
    let quietreal = engager
        .unengaged_targets
        .iter()
        .find(|target| target.subreddit == "quietreal")
        .expect("the control target is in the pool");
    assert_eq!(
        quietreal.converted_fans_90d, 1,
        "the reddit-channel conversion still counts — the predicate narrows, it does not remove"
    );

    Ok(())
}
