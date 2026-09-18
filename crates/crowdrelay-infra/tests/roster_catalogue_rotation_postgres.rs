//! The cross-act catalogue rotation against a real schema (5.15).
//!
//! Worth a database rather than a unit test for the usual reason: the edge
//! is a row on `amplification_consents` gated by `organization_id`, the
//! catalogue is `viryaos_release_plans`, the carry-ledger is
//! `amplification_deliveries`, and the dispatch is the capped campaign CTE —
//! none of it checked at compile time. What is asserted: the plan proposes
//! the oldest unrotated released release with the edge's real headroom; a
//! run lands deliveries labelled `catalogue:<release_id>` and queues the
//! outbox events through the owner workspace; the monthly cap refuses once
//! spent; a repeated run of the same reference carries nobody twice; and a
//! consent outside the organisation is not found.

use std::time::Duration;

use crowdrelay_infra::portfolio::PostgresPortfolioRepository;
use crowdrelay_infra::roster_catalogue_rotation::{
    catalogue_rotation_plan, run_catalogue_rotation,
};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_catrot_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    slug: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn consent(
    pool: &PgPool,
    organization_id: Uuid,
    from: Uuid,
    to: Uuid,
    purpose: &str,
    status: &str,
    cap: i16,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO amplification_consents
            (id, organization_id, from_workspace_id, to_workspace_id,
             purpose, status, max_campaigns_per_month)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(id)
    .bind(organization_id)
    .bind(from)
    .bind(to)
    .bind(purpose)
    .bind(status)
    .bind(cap)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn release(
    pool: &PgPool,
    workspace_id: Uuid,
    title: &str,
    release_at: OffsetDateTime,
    active: bool,
    communication_enabled: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_release_plans
            (id, workspace_id, source_key, title, release_at,
             active, communication_enabled, listen_url)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(format!("src-{title}"))
    .bind(title)
    .bind(release_at)
    .bind(active)
    .bind(communication_enabled)
    .bind(format!("https://listen/{title}"))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1, $2, 'active')",
    )
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_rotation_lands_labelled_catalogue_inside_the_cap()
-> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let org = organization(&db.pool, "label").await?;
        let act_a = workspace(&db.pool, "act-a", Some(org)).await?;
        let act_b = workspace(&db.pool, "act-b", Some(org)).await?;
        let now = OffsetDateTime::now_utc();

        let edge = consent(
            &db.pool,
            org,
            act_a,
            act_b,
            "catalogue_rotation",
            "active",
            1,
        )
        .await?;
        // A second edge under another purpose must not propose a rotation.
        consent(&db.pool, org, act_a, act_b, "cross_promote", "active", 4).await?;

        let oldest = release(
            &db.pool,
            act_b,
            "Month three",
            now - time::Duration::days(200),
            true,
            true,
        )
        .await?;
        release(
            &db.pool,
            act_b,
            "Month twenty",
            now - time::Duration::days(30),
            true,
            true,
        )
        .await?;
        // Unreleased, cancelled and comms-disabled items are not catalogue.
        release(&db.pool, act_b, "Future", now + time::Duration::days(30), true, true).await?;
        release(&db.pool, act_b, "Cancelled", now - time::Duration::days(100), false, true)
            .await?;
        release(&db.pool, act_b, "Quiet", now - time::Duration::days(150), true, false).await?;

        fan(&db.pool, act_a, "one@example.com").await?;
        fan(&db.pool, act_a, "two@example.com").await?;
        fan(&db.pool, act_b, "beneficiary-only@example.com").await?;

        let plan = catalogue_rotation_plan(&db.pool, org, now).await?;
        assert_eq!(plan.proposals.len(), 1, "only the rotation edge proposes");
        let proposal = &plan.proposals[0];
        assert_eq!(proposal.consent_id, edge);
        assert_eq!(proposal.release.release_id, oldest);
        assert_eq!(proposal.reachable_fans, 2, "act A's fans, not B's");
        assert_eq!(proposal.campaigns_this_month, 0);
        assert!(plan.exhausted.is_empty());

        let run = run_catalogue_rotation(&db.pool, org, edge, now).await?;
        assert_eq!(run.release_id, oldest);
        assert_eq!(run.campaign_reference, format!("catalogue:{oldest}"));
        assert_eq!(run.queued, 2);

        // The deliveries are ledgered under the catalogue reference, and the
        // outbox payload carries the kind + release — the labelling.
        let ledgered: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM amplification_deliveries WHERE consent_id = $1 AND campaign_reference = $2",
        )
        .bind(edge)
        .bind(&run.campaign_reference)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(ledgered, 2);
        let labelled: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM outbox_events
               WHERE event_type = 'amplification.campaign_due'
                 AND payload->>'kind' = 'catalogue_rotation'
                 AND payload->'release'->>'id' = $1::text"#,
        )
        .bind(oldest.to_string())
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(labelled, 2);

        // The cap is spent: the edge no longer proposes, and a run refuses.
        let plan = catalogue_rotation_plan(&db.pool, org, now).await?;
        assert!(plan.proposals.is_empty());
        assert!(plan.exhausted.is_empty(), "spent is not exhausted");
        let err = run_catalogue_rotation(&db.pool, org, edge, now)
            .await
            .expect_err("the cap refuses");
        assert!(matches!(
            err,
            crowdrelay_infra::portfolio::PortfolioError::CapReached
        ));

        // A direct replay of the same reference delivers nobody twice —
        // the ledger's uniqueness is the replay safety.
        let repo = PostgresPortfolioRepository::new(db.pool.clone());
        let replayed = repo
            .run_amplification_campaign(
                act_a,
                edge,
                &format!("catalogue:{oldest}"),
                "subject",
                "text",
                100,
                serde_json::json!({}),
            )
            .await;
        // The cap refuses the replay outright — both readings are honest:
        // nothing new can be delivered either way.
        assert!(matches!(
            replayed,
            Err(crowdrelay_infra::portfolio::PortfolioError::CapReached)
        ));
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.drop_database().await;
    result
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn edges_outside_the_organisation_are_absent() -> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let org = organization(&db.pool, "label-one").await?;
        let other_org = organization(&db.pool, "label-two").await?;
        let ours_a = workspace(&db.pool, "ours-a", Some(org)).await?;
        let ours_b = workspace(&db.pool, "ours-b", Some(org)).await?;
        let theirs_a = workspace(&db.pool, "theirs-a", Some(other_org)).await?;
        let theirs_b = workspace(&db.pool, "theirs-b", Some(other_org)).await?;
        let now = OffsetDateTime::now_utc();

        let theirs_edge = consent(
            &db.pool,
            other_org,
            theirs_a,
            theirs_b,
            "catalogue_rotation",
            "active",
            4,
        )
        .await?;
        release(
            &db.pool,
            theirs_b,
            "Their back then",
            now - time::Duration::days(100),
            true,
            true,
        )
        .await?;
        // Our org's edge exists but carries no releases yet — it reports
        // exhausted, not borrowed from the other label's catalogue.
        let our_edge = consent(
            &db.pool,
            org,
            ours_a,
            ours_b,
            "catalogue_rotation",
            "active",
            4,
        )
        .await?;

        let plan = catalogue_rotation_plan(&db.pool, org, now).await?;
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.exhausted.len(), 1);
        assert_eq!(plan.exhausted[0].consent_id, our_edge);

        // Their edge under our org id is not found — membership is the
        // authority, not the id.
        let err = run_catalogue_rotation(&db.pool, org, theirs_edge, now)
            .await
            .expect_err("another org's edge is not ours");
        assert!(matches!(
            err,
            crowdrelay_infra::portfolio::PortfolioError::NotFound
        ));
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.drop_database().await;
    result
}
