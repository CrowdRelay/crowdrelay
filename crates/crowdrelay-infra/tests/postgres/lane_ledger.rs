//! The lane ledger's aggregation against a real schema: four tables, one
//! vocabulary; lanes keyed by platform; the window is when the band *asked*;
//! the oldest unfinished request is reported; another tenant's lanes are not
//! this tenant's.

use crate::common;

use crowdrelay_domain::lane_ledger::{LaneScope, Verdict, verdict};
use crowdrelay_infra::lane_ledger::{autopost_settings, lane_rows};
use uuid::Uuid;

async fn workspace(pool: &sqlx::PgPool, label: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(ws)
        .bind(format!("{label}-{}", ws.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(ws)
}

/// Every post hangs off an autopilot action, which hangs off a decision.
async fn action(pool: &sqlx::PgPool, ws: Uuid) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, trace_id)
         VALUES ($1,$2,$3,'growth_metrics','target_community',$4,'auto_execute',9000,
                 'auto_execute','test','{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision)
    .bind(ws)
    .bind(format!("k-{decision}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    let action = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
              idempotency_key, payload, status, action_class, trace_id, finished_at)
         VALUES ($1,$2,$3,'growth_metrics','agent.content.request','target_community',$4,$5,
                 '{}'::jsonb,'succeeded','third_party',gen_random_uuid(), now())",
    )
    .bind(action)
    .bind(ws)
    .bind(decision)
    .bind(Uuid::now_v7())
    .bind(format!("i-{action}"))
    .execute(pool)
    .await?;
    Ok(action)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn lanes_are_platforms_across_four_tables_and_the_verdict_names_where_each_stops()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "lanes").await?;
    let other = workspace(&pool, "lanes-other").await?;

    // Reddit: fifteen drafts held for a person, four failed, none delivered —
    // the 2026-10-02 production shape. The oldest held one is 50 hours old.
    for i in 0..15 {
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status, created_at)
             VALUES ($1,$2,'reddit',$3,'t','b','awaiting_manual_post', now() - make_interval(hours => $4))",
        )
        .bind(ws)
        .bind(action(&pool, ws).await?)
        .bind(format!("sub{i}"))
        .bind(if i == 0 { 50 } else { 5 })
        .execute(&pool)
        .await?;
    }
    for i in 0..4 {
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status)
             VALUES ($1,$2,'reddit',$3,'t','b','failed')",
        )
        .bind(ws)
        .bind(action(&pool, ws).await?)
        .bind(format!("failed{i}"))
        .execute(&pool)
        .await?;
    }
    // A forum draft shares the table but is its own lane.
    sqlx::query(
        "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status)
         VALUES ($1,$2,'forum','The Black Vault','t','b','awaiting_manual_post')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // A joined/community Telegram can be blocked while the band's own
    // Telegram channel is delivering. Platform name alone must never merge
    // those two authority surfaces.
    sqlx::query(
        "INSERT INTO community_posts
             (workspace_id, action_id, platform, subreddit, title, body, status)
         VALUES ($1,$2,'telegram','metal-room','t','b','awaiting_manual_post')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // Owned Telegram delivers; Instagram delivers with one held behind it.
    for status in ["posted", "posted"] {
        sqlx::query("INSERT INTO telegram_posts (workspace_id, action_id, channel, status) VALUES ($1,$2,'@virya',$3)")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .bind(status)
            .execute(&pool)
            .await?;
    }
    for status in ["posted", "awaiting_manual_post"] {
        sqlx::query("INSERT INTO social_posts (workspace_id, action_id, platform, content, status) VALUES ($1,$2,'instagram','{}'::jsonb,$3)")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .bind(status)
            .execute(&pool)
            .await?;
    }
    // Discord channel: two queued. A post outside the window does not count.
    for _ in 0..2 {
        sqlx::query("INSERT INTO discord_posts (workspace_id, action_id, channel_id, status) VALUES ($1,$2,'123','pending')")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .execute(&pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO telegram_posts (workspace_id, action_id, channel, status, created_at)
         VALUES ($1,$2,'@virya','failed', now() - interval '40 days')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // Another tenant's delivered post is not ours.
    sqlx::query("INSERT INTO telegram_posts (workspace_id, action_id, channel, status) VALUES ($1,$2,'@x','posted')")
        .bind(other)
        .bind(action(&pool, other).await?)
        .execute(&pool)
        .await?;

    let lanes = lane_rows(&pool, ws, 14).await?;
    let find = |scope: LaneScope, name: &str| {
        lanes
            .iter()
            .find(|lane| lane.scope == scope && lane.lane == name)
            .unwrap_or_else(|| panic!("no {scope:?}/{name} lane: {lanes:?}"))
    };
    let reddit = find(LaneScope::Community, "reddit");
    assert_eq!(
        (reddit.counts.held_for_person, reddit.counts.failed),
        (15, 4)
    );
    assert_eq!(verdict(&reddit.counts), Verdict::HeldForPerson);
    assert_eq!(reddit.oldest_unfinished_hours, Some(50));
    assert_eq!(
        verdict(&find(LaneScope::Community, "forum").counts),
        Verdict::HeldForPerson
    );
    let telegram = find(LaneScope::Owned, "telegram");
    assert_eq!(telegram.counts.delivered, 2);
    assert_eq!(
        telegram.counts.failed, 0,
        "the 40-day-old failure is outside the window"
    );
    assert_eq!(verdict(&telegram.counts), Verdict::Delivering);
    assert_eq!(
        verdict(&find(LaneScope::Owned, "instagram").counts),
        Verdict::DeliveringPartly
    );
    assert_eq!(
        verdict(&find(LaneScope::Owned, "discord_channel").counts),
        Verdict::Queued
    );
    assert!(lanes.iter().all(|lane| lane.unknown_statuses == 0));
    assert_eq!(
        verdict(&find(LaneScope::Community, "telegram").counts),
        Verdict::HeldForPerson,
        "community Telegram hold must not poison owned Telegram"
    );
    assert_eq!(
        lanes.len(),
        6,
        "community reddit/forum/telegram plus owned telegram/instagram/discord_channel"
    );

    // A wider window sees the old failure; another tenant sees only its own lane.
    let wide = lane_rows(&pool, ws, 60).await?;
    assert_eq!(
        wide.iter()
            .find(|l| l.scope == LaneScope::Owned && l.lane == "telegram")
            .map(|l| l.counts.failed),
        Some(1)
    );
    let theirs = lane_rows(&pool, other, 14).await?;
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].scope, LaneScope::Owned);
    assert_eq!(theirs[0].counts.delivered, 1);
    // A tenant that asked its lanes nothing has no lanes: quiet, not healthy.
    let empty = workspace(&pool, "lanes-empty").await?;
    assert!(lane_rows(&pool, empty, 14).await?.is_empty());

    // Settings are reported as stored; absent is None, not "off".
    let none = autopost_settings(&pool, ws).await?;
    assert_eq!(
        (none.social_auto_post, none.social_autopost_platforms),
        (None, None)
    );
    sqlx::query("INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1,'social_autopost_platforms','telegram')")
        .bind(ws)
        .execute(&pool)
        .await?;
    assert_eq!(
        autopost_settings(&pool, ws)
            .await?
            .social_autopost_platforms
            .as_deref(),
        Some("telegram")
    );
    Ok(())
}

/// A tenant shaped like production on 2026-10-03: Facebook syncing, Instagram
/// whose latest read failed, and a fresh and a stale video. Some cases add
/// explicit join-ask words; others prove the same source can safely seed Day-0
/// copy. Settings are cached per workspace for a minute, so each stage gets its
/// own tenant.
async fn dayzero_tenant(
    pool: &sqlx::PgPool,
    label: &str,
    settings: &[(&str, &str)],
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = workspace(pool, label).await?;
    sqlx::query(
        "INSERT INTO content_sources (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1,'video','youtube:aaaaaaaaaaa','Fresh', now() - interval '3 days', now() + interval '60 days', '{}'::jsonb),
                ($1,'video','youtube:bbbbbbbbbbb','Stale', now() - interval '200 days', now() + interval '60 days', '{}'::jsonb)",
    )
    .bind(ws)
    .execute(pool)
    .await?;
    for (platform, label, failed) in [("facebook", "Page", false), ("instagram", "IG", true)] {
        sqlx::query(
            "INSERT INTO fanbase_connections
                 (workspace_id, platform, external_account_ref, credential_ref, status, label,
                  last_sync_at, last_sync_failed_at)
             VALUES ($1,$2,$3,'c','connected',$4, now() - interval '1 hour',
                     CASE WHEN $5 THEN now() ELSE NULL END)",
        )
        .bind(ws)
        .bind(platform)
        .bind(format!("ref-{platform}"))
        .bind(label)
        .bind(failed)
        .execute(pool)
        .await?;
    }
    // These fixtures model a worker whose deployment-level Meta publisher
    // is actually on. Individual tests can flip the row to prove the runtime
    // gate dominates tenant authority.
    sqlx::query(
        "INSERT INTO growth_component_state (workspace_id, component, enabled, missing_switch)
         VALUES ($1, 'social_post_executor', true, NULL)",
    )
    .bind(ws)
    .execute(pool)
    .await?;

    // A real fan-acquisition rail ends at an active, confirmed fan. Model a
    // subscribed mail bridge by default; the missing-route regression below
    // deletes it explicitly.
    sqlx::query(
        "INSERT INTO webhook_endpoints
             (workspace_id, name, url, signing_secret_ref, event_types)
         VALUES ($1,$2,'https://mail.example.test/hook','test/confirmation',
                 ARRAY['fan.confirmation_requested']::text[])",
    )
    .bind(ws)
    .bind(format!("confirmation-{label}"))
    .execute(pool)
    .await?;

    for (key, value) in settings {
        sqlx::query("INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1,$2,$3)")
            .bind(ws)
            .bind(key)
            .bind(value)
            .execute(pool)
            .await?;
    }
    Ok(ws)
}

const JOIN_WORDS: (&str, &str) = ("join_ask_variants", "[\"Want the next show first?\"]");
const SITE: (&str, &str) = ("member_site_base_url", "https://band.example");

/// Day-0 readiness against real rows: the prod shape (pages connected and
/// syncing, fresh content, join-ask words, auto-post off with the list set to
/// telegram) is one owner decision from ready, and the read says which one. A
/// failing connection and a stale video are not mistaken for working.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn readiness_names_the_one_decision_between_the_tenant_and_an_autonomous_rail()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;

    // Prod shape: everything works except the owner's standing authority.
    let prod = dayzero_tenant(
        &pool,
        "dz-prod",
        &[SITE, JOIN_WORDS, ("social_autopost_platforms", "telegram")],
    )
    .await?;
    let facts = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, prod).await?;
    assert!(
        facts.fresh_asset,
        "the 3-day-old video counts, the 200-day one does not"
    );
    assert!(facts.join_copy && !facts.social_auto_post);
    let explicit_snapshot = crowdrelay_infra::join_ask::load_join_ask_snapshot(&pool, prod).await?;
    assert_eq!(
        explicit_snapshot.variants,
        vec!["Want the next show first?".to_owned()],
        "explicit tenant wording must win over the source-derived fallback"
    );

    // No copywriter required: the same fresh tenant-owned video title seeds
    // one bounded starter variant. Day-0 can therefore progress to the real
    // owner handoff instead of stopping at NoVariants.
    let grounded = dayzero_tenant(
        &pool,
        "dz-grounded",
        &[SITE, ("social_autopost_platforms", "telegram")],
    )
    .await?;
    let grounded_snapshot =
        crowdrelay_infra::join_ask::load_join_ask_snapshot(&pool, grounded).await?;
    assert_eq!(
        grounded_snapshot.variants,
        vec!["Fresh\n\nJoin for updates.".to_owned()]
    );
    let grounded_facts = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, grounded).await?;
    assert!(grounded_facts.join_copy);
    assert!(
        grounded_facts.facebook_authority_grantable(),
        "fresh grounded source text satisfies the copy prerequisite without granting authority"
    );
    assert_eq!(
        grounded_facts.assess().smallest_missing.map(|m| m.code),
        Some("standing_authority_not_granted")
    );

    let working = |p: &str| {
        facts
            .connections
            .iter()
            .find(|c| c.platform == p)
            .map(|c| c.working)
    };
    assert_eq!(working("facebook"), Some(true));
    assert_eq!(
        working("instagram"),
        Some(false),
        "a connection whose latest read failed is not known to work"
    );
    let readiness = facts.assess();
    assert!(!readiness.ready);
    let missing = readiness.smallest_missing.expect("a blocker is named");
    assert_eq!(missing.code, "standing_authority_not_granted");
    assert!(missing.what.starts_with("facebook"), "{}", missing.what);

    // Tenant authority cannot override a deployment that cannot publish.
    sqlx::query(
        "UPDATE growth_component_state
         SET enabled=false, missing_switch='CROWDRELAY_SOCIAL_AUTO_POST', observed_at=now()
         WHERE workspace_id=$1 AND component='social_post_executor'",
    )
    .bind(prod)
    .execute(&pool)
    .await?;
    let runtime_off_facts = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, prod).await?;
    assert!(
        runtime_off_facts
            .executable_owned_social_platforms()
            .is_empty(),
        "a disabled worker exposes no executable rail to the Brain"
    );
    let runtime_off = runtime_off_facts.assess();
    let blocked = runtime_off.smallest_missing.expect("runtime blocker");
    assert_eq!(blocked.code, "deployment_publish_gate_off");
    assert!(
        !blocked.owner_action,
        "tenant settings cannot repair a worker deployment gate"
    );
    sqlx::query(
        "UPDATE growth_component_state
         SET enabled=true, missing_switch=NULL, observed_at=now()
         WHERE workspace_id=$1 AND component='social_post_executor'",
    )
    .bind(prod)
    .execute(&pool)
    .await?;

    // The explicit Facebook grant is atomic and exact: it must not wake the
    // previously stored Telegram lane alongside Facebook.
    let settings = crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(pool.clone());
    settings.set_facebook_autopost_authority(prod, true).await?;
    let granted_facts = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, prod).await?;
    assert!(granted_facts.social_auto_post);
    assert_eq!(
        granted_facts.autopost_platforms,
        vec!["facebook".to_owned()]
    );
    assert!(granted_facts.assess().ready);
    assert_eq!(
        granted_facts.executable_owned_social_platforms(),
        vec!["facebook".to_owned()],
        "the Brain sees exactly the rail the owner granted"
    );
    settings
        .set_facebook_autopost_authority(prod, false)
        .await?;
    let revoked = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, prod).await?;
    assert!(!revoked.social_auto_post, "revocation must be immediate");
    assert_eq!(
        revoked.autopost_platforms,
        vec!["facebook".to_owned()],
        "revocation does not broaden the remembered authority scope"
    );
    assert!(
        revoked.executable_owned_social_platforms().is_empty(),
        "revoked authority disappears from Brain routing immediately"
    );

    // A publisher without a double-opt-in delivery route is not a usable
    // acquisition rail: it would manufacture pending fans that can never
    // become canonical active fans.
    let no_confirmation = dayzero_tenant(
        &pool,
        "dz-no-confirmation",
        &[
            SITE,
            JOIN_WORDS,
            ("social_auto_post", "true"),
            ("social_autopost_platforms", "facebook"),
        ],
    )
    .await?;
    sqlx::query(
        "DELETE FROM webhook_endpoints
         WHERE workspace_id=$1
           AND event_types @> ARRAY['fan.confirmation_requested']::text[]",
    )
    .bind(no_confirmation)
    .execute(&pool)
    .await?;
    let no_confirmation_facts =
        crowdrelay_infra::lane_ledger::day_zero_facts(&pool, no_confirmation).await?;
    assert!(
        !no_confirmation_facts.confirmation_delivery_route,
        "the route fact must reflect the actual endpoint subscription"
    );
    assert!(
        !no_confirmation_facts.facebook_authority_grantable(),
        "authority cannot be granted into an unconfirmable funnel"
    );
    assert!(
        no_confirmation_facts
            .executable_owned_social_platforms()
            .is_empty(),
        "the Brain must see no executable acquisition rail"
    );
    let no_confirmation_readiness = no_confirmation_facts.assess();
    let blocker = no_confirmation_readiness
        .smallest_missing
        .expect("confirmation route blocker");
    assert_eq!(blocker.code, "confirmation_delivery_unavailable");
    assert!(!blocker.owner_action);

    // No site root: named before any rail, whatever the rails look like.
    let no_site = dayzero_tenant(&pool, "dz-nosite", &[JOIN_WORDS]).await?;
    let no_site_facts = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, no_site).await?;
    assert!(
        no_site_facts.executable_owned_social_platforms().is_empty(),
        "authority-shaped rail cannot bypass a missing first-party destination"
    );
    let no_site = no_site_facts.assess();
    assert_eq!(
        no_site.smallest_missing.map(|m| m.code),
        Some("no_signup_destination")
    );

    // The owner grants it once; the same facts now say ready.
    let granted = dayzero_tenant(
        &pool,
        "dz-granted",
        &[
            SITE,
            JOIN_WORDS,
            ("social_auto_post", "true"),
            ("social_autopost_platforms", "telegram,facebook"),
        ],
    )
    .await?;
    let granted = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, granted)
        .await?
        .assess();
    assert!(granted.ready, "{granted:?}");

    // A tenant with nothing connected is never told it is one switch away.
    let bare = workspace(&pool, "dz-bare").await?;
    let bare = crowdrelay_infra::lane_ledger::day_zero_facts(&pool, bare)
        .await?
        .assess();
    assert!(!bare.ready);
    assert_ne!(
        bare.smallest_missing.map(|m| m.code),
        Some("standing_authority_not_granted")
    );
    Ok(())
}

/// What the platform said about the publish token reaches readiness: a token
/// that reads but cannot publish is named and blocks the rail, a confirmed one
/// is clean, an answer older than two days stops counting, and a tenant nobody
/// has checked carries a caveat instead of a block.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_recorded_publish_scope_check_decides_whether_the_rail_is_ready()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_domain::day_zero::PublishPermission;
    use crowdrelay_infra::lane_ledger::{day_zero_facts, record_publish_scopes};
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let granted_settings = [
        SITE,
        JOIN_WORDS,
        ("social_auto_post", "true"),
        ("social_autopost_platforms", "facebook"),
    ];
    let permission = |facts: &crowdrelay_infra::lane_ledger::DayZeroFacts, p: &str| {
        facts
            .connections
            .iter()
            .find(|c| c.platform == p)
            .map(|c| c.publish)
    };

    // Nobody has checked: unverified, and a caveat, not a block.
    let unchecked = dayzero_tenant(&pool, "dz-scope-none", &granted_settings).await?;
    let facts = day_zero_facts(&pool, unchecked).await?;
    assert_eq!(
        permission(&facts, "facebook"),
        Some(PublishPermission::Unverified)
    );

    // Reads work but the platform says the token cannot publish.
    let read_only = dayzero_tenant(&pool, "dz-scope-read", &granted_settings).await?;
    record_publish_scopes(&pool, read_only, &["pages_read_engagement".to_owned()]).await?;
    let facts = day_zero_facts(&pool, read_only).await?;
    assert_eq!(
        permission(&facts, "facebook"),
        Some(PublishPermission::Missing)
    );
    assert_eq!(
        permission(&facts, "instagram"),
        Some(PublishPermission::Missing),
        "each platform needs its own scope"
    );
    assert_eq!(
        facts.assess().smallest_missing.map(|m| m.code),
        Some("publish_permission_missing")
    );

    // Confirmed: clean.
    let confirmed = dayzero_tenant(&pool, "dz-scope-ok", &granted_settings).await?;
    record_publish_scopes(
        &pool,
        confirmed,
        &[
            "pages_manage_posts".to_owned(),
            "instagram_content_publish".to_owned(),
        ],
    )
    .await?;
    let facts = day_zero_facts(&pool, confirmed).await?;
    assert_eq!(
        permission(&facts, "facebook"),
        Some(PublishPermission::Verified)
    );

    // Re-recording overwrites (one row per key), it does not accumulate.
    record_publish_scopes(&pool, confirmed, &["pages_read_engagement".to_owned()]).await?;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_settings WHERE workspace_id = $1 AND key LIKE 'meta_publish_scopes%'",
    )
    .bind(confirmed)
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 2);

    // An answer older than two days is not trusted as current.
    sqlx::query(
        "UPDATE tenant_settings SET value = to_char(now() - interval '3 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         WHERE workspace_id = $1 AND key = 'meta_publish_scopes_checked_at'",
    )
    .bind(read_only)
    .execute(&pool)
    .await?;
    let stale = day_zero_facts(&pool, read_only).await?;
    assert_eq!(
        permission(&stale, "facebook"),
        Some(PublishPermission::Unverified)
    );
    Ok(())
}

/// Where joins came from: tagged vs untagged, behaviour next to the tag, and
/// only action-linked conversions counted as system-attributed. Old joins fall
/// out of the window, closed accounts and other tenants never appear.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn signup_channels_split_joins_by_tag_and_never_guess_a_channel()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::signup_channels::signup_channels;
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "channels").await?;
    let other = workspace(&pool, "channels-other").await?;

    async fn fan(
        pool: &sqlx::PgPool,
        ws: Uuid,
        email: &str,
        days_old: i32,
        status: &str,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
             VALUES ($1,$2,$3,$4, now() - make_interval(days => $5))",
        )
        .bind(id)
        .bind(ws)
        .bind(email)
        .bind(status)
        .bind(days_old)
        .execute(pool)
        .await?;
        Ok(id)
    }
    async fn tag(
        pool: &sqlx::PgPool,
        ws: Uuid,
        fan: Uuid,
        source: &str,
        medium: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        sqlx::query(
            "INSERT INTO fan_ad_attribution (workspace_id, fan_id, utm_source, utm_medium)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(ws)
        .bind(fan)
        .bind(source)
        .bind(medium)
        .execute(pool)
        .await?;
        Ok(())
    }

    // Two joined through the owned-link landing, one of whom came back.
    let a = fan(&pool, ws, "a@x.test", 3, "active").await?;
    let b = fan(&pool, ws, "b@x.test", 5, "active").await?;
    tag(&pool, ws, a, "owned", "watch").await?;
    tag(&pool, ws, b, "owned", "watch").await?;
    sqlx::query(
        "INSERT INTO fan_sessions (id, workspace_id, fan_id, session_token_hash, created_at, expires_at, last_seen_at)
         SELECT gen_random_uuid(), $1, $2, gen_random_bytes(32), now() - interval '3 days', now() + interval '30 days', now() - interval '2 days'",
    )
    .bind(ws)
    .bind(a)
    .execute(&pool)
    .await?;
    // One untagged join that a CrowdRelay action's tracked link converted.
    let c = fan(&pool, ws, "c@x.test", 4, "active").await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events
             (workspace_id, fan_id, event_kind, channel, action_id, attribution_method, occurred_at)
         VALUES ($1,$2,'conversion','reddit',gen_random_uuid(),'last_tracked_click', now())",
    )
    .bind(ws)
    .bind(c)
    .execute(&pool)
    .await?;
    // A conversion with no action is not the system's, and neither is a join
    // before the window, a closed account, or another tenant's fan.
    let d = fan(&pool, ws, "d@x.test", 2, "active").await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events (workspace_id, fan_id, event_kind, channel, attribution_method, occurred_at)
         VALUES ($1,$2,'conversion','smart_link','direct_arrival', now())",
    )
    .bind(ws)
    .bind(d)
    .execute(&pool)
    .await?;
    fan(&pool, ws, "old@x.test", 200, "active").await?;
    fan(&pool, ws, "closed@x.test", 2, "suppressed").await?;
    let stranger = fan(&pool, other, "s@y.test", 2, "active").await?;
    tag(&pool, other, stranger, "instagram", "bio").await?;

    let rows = signup_channels(&pool, ws, 90).await?;
    let find = |source: &str| rows.iter().find(|r| r.source == source);
    let owned = find("owned").expect("the owned-link joins are their own channel");
    assert_eq!(
        (
            owned.medium.as_str(),
            owned.joined,
            owned.opened_signal_30d,
            owned.system_attributed
        ),
        ("watch", 2, 1, 0),
        "two joined, one opened Signal, neither is the system's"
    );
    let untagged = find("(untagged)").expect("untagged joins are reported as untagged");
    assert_eq!(
        (untagged.joined, untagged.system_attributed),
        (2, 1),
        "c and d are untagged; only c's conversion carries an action"
    );
    assert!(
        find("instagram").is_none(),
        "another tenant's tag never appears"
    );
    assert_eq!(
        rows.iter().map(|r| r.joined).sum::<i64>(),
        4,
        "old and closed excluded"
    );

    // A wider window brings the old join in as one more untagged fan.
    let wide = signup_channels(&pool, ws, 365).await?;
    assert_eq!(
        wide.iter()
            .find(|r| r.source == "(untagged)")
            .map(|r| r.joined),
        Some(3)
    );
    Ok(())
}
