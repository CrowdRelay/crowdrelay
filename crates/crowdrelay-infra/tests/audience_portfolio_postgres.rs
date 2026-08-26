//! Live-Postgres coverage for the Audience Graph, Label Portfolio,
//! tenant settings and the pilot fan import.
//!
//! These repositories encode the commercial promises of the product — fans
//! never leave home, refusals reopen only through research, caps bind, an
//! empty settings table changes nothing — so they are pinned against a real
//! database rather than mocks. Run via `just test-postgres`.

use crowdrelay_domain::audience_graph::{OutreachStage, PlaceKind};
use crowdrelay_infra::audience_graph::{
    AudienceGraphError, PostgresAudienceGraphRepository, UpsertPlaceInput,
};
use crowdrelay_infra::fan_import::{ImportEntry, PostgresFanImportRepository};
use crowdrelay_domain::portfolio::ConsentStatus;
use crowdrelay_infra::portfolio::{PortfolioError, PostgresPortfolioRepository};
use crowdrelay_infra::tenant_settings::{TenantBrandSettings, TenantSettingsRepository};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_TEST_DATABASE_URL";

async fn pool() -> PgPool {
    let url = std::env::var(TEST_DATABASE_URL_KEY)
        .expect("set CROWDRELAY_TEST_DATABASE_URL to a disposable database");
    PgPool::connect(&url).await.expect("connect to test database")
}

async fn seed_workspace(pool: &PgPool, tag: &str) -> Uuid {
    let id = Uuid::now_v7();
    let slug = format!("{tag}-{}", id.simple());
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(&slug)
        .bind(format!("Test {tag}"))
        .execute(pool)
        .await
        .expect("seed workspace");
    id
}

fn place_input<'a>(
    workspace_id: Uuid,
    platform: &'a str,
    url: &'a str,
    name: &'a str,
) -> UpsertPlaceInput<'a> {
    UpsertPlaceInput {
        workspace_id,
        place_kind: PlaceKind::Subreddit,
        platform,
        name,
        url,
        country_code: Some("PL"),
        language: Some("pl"),
        genres: &[],
        member_count: Some(1_000),
        activity_bp: Some(7_000),
        notes: None,
    }
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn audience_graph_upsert_advances_and_decays() -> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let repo = PostgresAudienceGraphRepository::new(pool.clone());
    let workspace = seed_workspace(&pool, "ag").await;
    let url = format!("https://reddit.com/r/ag-{}", workspace.simple());

    // Upsert is idempotent per (workspace, platform, url) and seeds the pipeline.
    let first = repo.upsert_place(&place_input(workspace, "reddit", &url, "r/AG")).await?;
    let second = repo.upsert_place(&place_input(workspace, "reddit", &url, "r/AG renamed")).await?;
    assert_eq!(first, second);
    let seeded = repo.place_detail(workspace, first).await?;
    assert_eq!(seeded.stage.as_deref(), Some("discovered"));

    // Domain policy blocks the tempting shortcut straight to contact.
    let illegal = PostgresAudienceGraphRepository::advance_outreach_in_tx(
        &mut pool.begin().await?,
        workspace,
        first,
        OutreachStage::Discovered,
        OutreachStage::Contacted,
        None,
    )
    .await;
    assert!(matches!(illegal, Err(AudienceGraphError::InvalidTransition { .. })));

    // The legal move lands, and rules re-arm the cooldown on the edge.
    repo.attach_rules(
        workspace,
        first,
        &crowdrelay_infra::audience_graph::PlaceRulesInput {
            self_promo_ratio_percent: Some(10),
            contact_channel: Some("modmail"),
            contact_target: Some("mods"),
            requires_approval: false,
            cooldown_days: 30,
            rules_summary: None,
        },
        true,
    )
    .await?;
    let mut tx = pool.begin().await?;
    PostgresAudienceGraphRepository::advance_outreach_in_tx(
        &mut tx,
        workspace,
        first,
        OutreachStage::Discovered,
        OutreachStage::Researched,
        None,
    )
    .await?;
    tx.commit().await?;
    let researched = repo.place_detail(workspace, first).await?;
    assert_eq!(researched.stage.as_deref(), Some("researched"));
    let next_eligible = researched.next_eligible_at.expect("cooldown armed");
    assert!(next_eligible > time::OffsetDateTime::now_utc() + time::Duration::days(20));

    // Decay retires a relationship whose last action is older than the window.
    sqlx::query(
        "UPDATE discovery_outreach SET last_action_at = now() - interval '90 days' WHERE place_id = $1",
    )
    .bind(first)
    .execute(&pool)
    .await?;
    let decayed = repo.decay_dormant(workspace, time::Duration::days(45), 100).await?;
    assert_eq!(decayed, 1);
    let dormant = repo.place_detail(workspace, first).await?;
    assert_eq!(dormant.stage.as_deref(), Some("dormant"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn portfolio_edges_route_only_within_an_organization_and_cap_deliveries()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let repo = PostgresPortfolioRepository::new(pool.clone());
    let owner = seed_workspace(&pool, "pf-owner").await;
    let beneficiary = seed_workspace(&pool, "pf-benefit").await;
    let outsider = seed_workspace(&pool, "pf-outsider").await;

    let org = repo
        .create_organization_for_workspace(owner, &format!("pf-{}", owner.simple()), "PF Label")
        .await?;

    // The second roster member joins the same organization...
    sqlx::query("UPDATE workspaces SET organization_id = $2 WHERE id = $1")
        .bind(beneficiary)
        .bind(org)
        .execute(&pool)
        .await?;

    // ...so the edge between them is allowed; an outsider is not.
    let consent = repo
        .propose_amplification(
            owner,
            beneficiary,
            crowdrelay_domain::portfolio::AmplificationPurpose::ReleaseFeature,
            "all_active",
            1,
            21,
        )
        .await?;
    let cross_org = repo
        .propose_amplification(
            owner,
            outsider,
            crowdrelay_domain::portfolio::AmplificationPurpose::CrossPromote,
            "all_active",
            1,
            21,
        )
        .await;
    assert!(matches!(cross_org, Err(PortfolioError::NotInSameOrganization)));

    repo.decide_amplification(owner, consent, ConsentStatus::Active, Some("op"), None)
        .await?;

    // Two active owner fans; one suppressed address must never be reached.
    for (index, status) in [("a", "active"), ("b", "active"), ("c", "suppressed")] {
        sqlx::query("INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,$4)")
            .bind(Uuid::now_v7())
            .bind(owner)
            .bind(format!("fan-{index}-{}@pf.test", owner.simple()))
            .bind(status)
            .execute(&pool)
            .await?;
    }

    let preview = repo.preview_audience(owner, consent).await?;
    assert_eq!(preview, 2, "suppressed fans never count as reach");

    let queued_first = repo
        .run_amplification_campaign(owner, consent, "pf-camp-1", "Hello", "Body", 100)
        .await?;
    assert_eq!(queued_first, 2);

    // Monthly cap of one campaign: a second distinct reference is refused even
    // though fans exist.
    let capped = repo
        .run_amplification_campaign(owner, consent, "pf-camp-2", "Hello", "Body", 100)
        .await;
    assert!(matches!(capped, Err(PortfolioError::CapReached)));
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn fan_import_lands_pending_and_respects_opt_outs() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let repo = PostgresFanImportRepository::new(pool.clone());
    let workspace = seed_workspace(&pool, "import").await;

    // A pre-existing suppressed address must stay untouched by the import.
    sqlx::query("INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1,'gone@x.test','unsubscribed')")
        .bind(workspace)
        .execute(&pool)
        .await?;

    let entries = vec![
        ImportEntry { email: "new@x.test".into(), display_name: Some("New".into()), locale: Some("pl".into()) },
        ImportEntry { email: "gone@x.test".into(), display_name: None, locale: None },
    ];
    let counts = repo
        .import_batch(workspace, "pilot-batch", &entries, 2, 60)
        .await?;
    assert_eq!(counts.imported_pending, 1);
    assert_eq!(counts.skipped_suppressed, 1);

    let status: String = sqlx::query_scalar(
        "SELECT status FROM fans WHERE workspace_id=$1 AND normalized_email='new@x.test'",
    )
    .bind(workspace)
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "pending");

    // The confirmation email is queued with a real token row behind it.
    let payload: serde_json::Value = sqlx::query_scalar(
        r#"
        SELECT payload FROM outbox_events
        WHERE workspace_id=$1 AND event_type='fan.confirmation_requested'
          AND payload->>'email' = 'new@x.test'
        "#,
    )
    .bind(workspace)
    .fetch_one(&pool)
    .await?;
    assert!(payload.get("confirmation_token").is_some());

    // An immediate re-import hits the resend cooldown instead of double-sending.
    let again = repo
        .import_batch(workspace, "pilot-batch-retry", &entries, 2, 60)
        .await?;
    assert_eq!(again.cooldown_skipped, 1);

    // One audit row names the source.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='fans.imported' AND metadata->>'source'='pilot-batch'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(audited, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn tenant_settings_default_to_the_shipped_constants_then_follow_overrides()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let repo = TenantSettingsRepository::new(pool.clone());
    let workspace = seed_workspace(&pool, "ts").await;

    // Empty table: byte-equal defaults, exactly like before the extraction.
    let before = repo.brand_settings(workspace).await?;
    assert_eq!(*before, TenantBrandSettings::default());

    repo.set_setting(workspace, "member_site_base_url", "https://fans.example.org")
        .await?;
    let after = repo.brand_settings(workspace).await?;
    assert_eq!(after.member_site_base_url, "https://fans.example.org");
    // Untouched keys keep their defaults; overrides are per-key data.
    assert_eq!(after.member_area_path, "pl/latarnik");
    Ok(())
}
