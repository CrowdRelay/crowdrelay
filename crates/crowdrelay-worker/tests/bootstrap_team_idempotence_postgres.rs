//! A deploy must not put a disabled team member back to work.
//!
//! `bootstrap_team_operations` runs inside `setup`, and `scripts/deploy.sh` runs
//! `setup` on every release before either long-running service starts. That
//! makes it an unattended periodic writer over `workspace_members` and
//! `viryaos_team_profiles`.
//!
//! Its conflict clauses used to set `status = 'active'` and `active = true`
//! unconditionally, so a disablement lasted exactly until the next release.
//! Nothing surfaced the reversal, and nothing else in the codebase writes either
//! column — so the only way to turn somebody off was hand SQL, and the only
//! thing that ever turned them back on was deploying.
//!
//! `admission/support.rs` requires `m.status = 'active'` to operate a gate, and
//! `autopilot/{team,control}.rs` require `profile.active AND
//! member.status = 'active'` to route work. Both came back with the status.

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceSlug;
use crowdrelay_infra::config::{DatabaseConfig, TeamOperationsConfig};
use crowdrelay_worker::bootstrap::bootstrap_team_operations;
use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    url: String,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_team_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').context("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            url,
            admin_url: base,
            name,
        })
    }

    fn database_config(&self) -> DatabaseConfig {
        DatabaseConfig {
            url: self.url.clone(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
        }
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
            ..
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

/// One configured contact. The slot key is what maps to a source-controlled
/// profile, so it has to be one the code knows.
fn one_member(email: &str) -> TeamOperationsConfig {
    TeamOperationsConfig {
        member_1_email: Some(email.to_owned()),
        member_2_email: None,
        member_3_email: None,
        member_4_email: None,
        member_5_email: None,
    }
}

struct MemberState {
    status: String,
    profile_active: bool,
}

async fn member_state(pool: &PgPool, email: &str) -> Result<MemberState> {
    let row = sqlx::query(
        "SELECT member.status, profile.active \
         FROM workspace_members AS member \
         JOIN viryaos_team_profiles AS profile ON profile.member_id = member.id \
         WHERE member.normalized_email = $1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .context("read member state")?;
    Ok(MemberState {
        status: row.try_get("status")?,
        profile_active: row.try_get("active")?,
    })
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_deploy_does_not_re_enable_a_disabled_member() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = disablement_survives(&database).await;
    database.drop_database().await;
    result
}

async fn disablement_survives(database: &DisposableDatabase) -> Result<()> {
    let pool = &database.pool;
    let slug = WorkspaceSlug::parse("team-bootstrap")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Team bootstrap test')")
        .bind(Uuid::now_v7())
        .bind(slug.as_str())
        .execute(pool)
        .await
        .context("insert workspace")?;

    let email = "member-one@team-bootstrap.test";
    let config = one_member(email);
    let db_config = database.database_config();

    // First release: the member is created and is at work.
    bootstrap_team_operations(pool, &slug, &db_config, &config).await?;
    let created = member_state(pool, email).await?;
    ensure!(created.status == "active", "a new member starts active");
    ensure!(created.profile_active, "a new profile starts active");

    // Somebody turns them off. No route does this today, so it is modelled the
    // way it is actually done: by hand.
    sqlx::query("UPDATE workspace_members SET status = 'disabled' WHERE normalized_email = $1")
        .bind(email)
        .execute(pool)
        .await
        .context("disable member")?;
    sqlx::query(
        "UPDATE viryaos_team_profiles SET active = false WHERE member_id = \
         (SELECT id FROM workspace_members WHERE normalized_email = $1)",
    )
    .bind(email)
    .execute(pool)
    .await
    .context("deactivate profile")?;

    // Next release. The contact is still in the deploy secret, because being
    // disabled and being off the team are different statements.
    bootstrap_team_operations(pool, &slug, &db_config, &config).await?;
    let after = member_state(pool, email).await?;
    ensure!(
        after.status == "disabled",
        "a deploy re-enabled a disabled member: status is {}",
        after.status
    );
    ensure!(
        !after.profile_active,
        "a deploy re-activated a deactivated profile"
    );

    // And it is still not a no-op: repeated releases are how skills and the
    // member key stay current, and a disabled member is still a known one.
    let refreshed = sqlx::query(
        "SELECT profile.member_key, cardinality(profile.skills) AS skill_count \
         FROM viryaos_team_profiles AS profile \
         JOIN workspace_members AS member ON member.id = profile.member_id \
         WHERE member.normalized_email = $1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .context("read refreshed profile")?;
    ensure!(
        refreshed.try_get::<String, _>("member_key")? == "member_1",
        "the member key still refreshes"
    );
    ensure!(
        refreshed.try_get::<i32, _>("skill_count")? > 0,
        "skills still refresh"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_invited_member_is_still_promoted_by_a_deploy() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = invitation_is_confirmed(&database).await;
    database.drop_database().await;
    result
}

async fn invitation_is_confirmed(database: &DisposableDatabase) -> Result<()> {
    let pool = &database.pool;
    let slug = WorkspaceSlug::parse("team-invited")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Team invite test')")
        .bind(Uuid::now_v7())
        .bind(slug.as_str())
        .execute(pool)
        .await
        .context("insert workspace")?;

    // An invitation predates the contact reaching the deploy secret. Promoting
    // it is the one activation this function should still perform: a
    // secret-backed contact appearing here is what confirms the invitation.
    let email = "invited@team-bootstrap.test";
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status) \
         VALUES ((SELECT id FROM workspaces WHERE slug = $1), $2, 'staff', 'invited')",
    )
    .bind(slug.as_str())
    .bind(email)
    .execute(pool)
    .await
    .context("insert invited member")?;

    bootstrap_team_operations(pool, &slug, &database.database_config(), &one_member(email)).await?;
    let after = member_state(pool, email).await?;
    ensure!(
        after.status == "active",
        "an invited contact must still be promoted, not left pending: status is {}",
        after.status
    );

    Ok(())
}
