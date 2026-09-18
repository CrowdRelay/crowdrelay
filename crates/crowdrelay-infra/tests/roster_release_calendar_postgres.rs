//! The roster release calendar against a real schema (5.16).
//!
//! Worth a database for the usual reason: the membership boundary is a join
//! on `workspaces.organization_id`, the collision window is a predicate on
//! `viryaos_release_plans`, and the shared-fan citation is an intersection
//! over `fans.normalized_email` — none of it checked at compile time. What
//! is asserted: the calendar lists only the organisation's active releases
//! inside the lookahead; a same-week pair across two member acts produces a
//! collision naming who keeps the week and who moves; the shared-fan count
//! is measured, not assumed; an outsider workspace's release is absent; and
//! an inactive release does not collide.

use std::time::Duration;

use crowdrelay_infra::roster_release_calendar::roster_release_calendar;
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
        let name = format!("crowdrelay_relcal_{}", Uuid::now_v7().simple());
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

async fn release(
    pool: &PgPool,
    workspace_id: Uuid,
    title: &str,
    release_at: OffsetDateTime,
    tier: &str,
    assets_ready: bool,
    active: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_release_plans
            (workspace_id, source_key, title, release_at, tier, assets_ready, active)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(workspace_id)
    .bind(format!("src-{title}"))
    .bind(title)
    .bind(release_at)
    .bind(tier)
    .bind(assets_ready)
    .bind(active)
    .execute(pool)
    .await?;
    Ok(())
}

async fn fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(email)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_same_week_pair_collides_and_the_quieter_release_moves()
-> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let org = organization(&db.pool, "label-one").await?;
        let act_a = workspace(&db.pool, "act-a", Some(org)).await?;
        let act_b = workspace(&db.pool, "act-b", Some(org)).await?;
        let outsider = workspace(&db.pool, "outsider", None).await?;

        let now = OffsetDateTime::now_utc();
        // Same ISO week: act-a's filler and act-b's single.
        let wednesday = now + time::Duration::days(14);
        release(
            &db.pool,
            act_a,
            "Loose track",
            wednesday,
            "filler",
            true,
            true,
        )
        .await?;
        release(
            &db.pool,
            act_b,
            "The single",
            wednesday + time::Duration::days(2),
            "single",
            true,
            true,
        )
        .await?;
        // A clear week for act-a.
        release(
            &db.pool,
            act_a,
            "Follow-up",
            now + time::Duration::days(30),
            "track",
            false,
            true,
        )
        .await?;
        // The outsider's release in the same colliding week must not appear.
        release(
            &db.pool, outsider, "Not ours", wednesday, "single", true, true,
        )
        .await?;
        // An inactive release on a member act does not collide.
        release(
            &db.pool,
            act_b,
            "Cancelled",
            wednesday,
            "track",
            true,
            false,
        )
        .await?;
        // A release beyond the lookahead is not dragged in.
        release(
            &db.pool,
            act_a,
            "Far future",
            now + time::Duration::days(200),
            "single",
            true,
            true,
        )
        .await?;

        // Two shared fans, one exclusive to each side, one unsubscribed
        // (must not count).
        fan(&db.pool, act_a, "shared-one@example.com", "active").await?;
        fan(&db.pool, act_b, "shared-one@example.com", "active").await?;
        fan(&db.pool, act_a, "shared-two@example.com", "active").await?;
        fan(&db.pool, act_b, "shared-two@example.com", "active").await?;
        fan(&db.pool, act_a, "only-a@example.com", "active").await?;
        fan(&db.pool, act_b, "shared-lapsed@example.com", "unsubscribed").await?;
        fan(&db.pool, act_a, "shared-lapsed@example.com", "active").await?;
        // The outsider sharing an email must not inflate the member pair.
        fan(&db.pool, outsider, "shared-one@example.com", "active").await?;

        let calendar = roster_release_calendar(&db.pool, org, now).await?;

        // Three member releases in the window — the outsider's, the
        // cancelled one and the far-future one are all absent.
        assert_eq!(calendar.releases.len(), 3);
        assert!(
            calendar
                .releases
                .iter()
                .all(|r| r.workspace_id.into_uuid() != outsider)
        );

        assert_eq!(calendar.collisions.len(), 1, "one colliding week");
        let collision = &calendar.collisions[0];
        assert_eq!(collision.stays_workspace_id.into_uuid(), act_b);
        assert_eq!(collision.moves_workspace_id.into_uuid(), act_a);
        assert!(
            collision.reason.contains("a Single"),
            "reason cites the deciding rule: {}",
            collision.reason
        );
        // Two shared active fans — the unsubscribed overlap does not count.
        assert_eq!(collision.shared_fans, Some(2));
        assert!(collision.reason.contains("2 active fans"));
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.drop_database().await;
    result
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_empty_roster_gets_an_empty_calendar() -> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let org = organization(&db.pool, "empty-label").await?;
        let calendar = roster_release_calendar(&db.pool, org, OffsetDateTime::now_utc()).await?;
        assert!(calendar.releases.is_empty());
        assert!(calendar.collisions.is_empty());
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.drop_database().await;
    result
}
