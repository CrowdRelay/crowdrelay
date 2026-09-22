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

mod common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceSlug;
use crowdrelay_infra::config::{DatabaseConfig, TeamMemberSpec, TeamOperationsConfig};
use crowdrelay_worker::bootstrap::bootstrap_team_operations;
use sqlx::{PgPool, Row};
use std::time::Duration;
use uuid::Uuid;

/// One configured contact. The member key has to satisfy the
/// `viryaos_team_profiles.member_key` CHECK constraint.
fn one_member(email: &str) -> TeamOperationsConfig {
    TeamOperationsConfig {
        members: vec![TeamMemberSpec {
            member_key: "member_1".to_owned(),
            email: email.to_owned(),
            display_name: "Team Member 1".to_owned(),
            skills: vec!["general".to_owned(), "operations".to_owned()],
        }],
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

fn db_config() -> DatabaseConfig {
    DatabaseConfig {
        url: std::env::var("CROWDRELAY_TEST_DATABASE_URL").expect("suite database url"),
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_deploy_does_not_re_enable_a_disabled_member() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    disablement_survives(&database).await
}

async fn disablement_survives(database: &PgPool) -> Result<()> {
    let pool = database;
    let slug = WorkspaceSlug::parse("team-bootstrap")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Team bootstrap test')")
        .bind(Uuid::now_v7())
        .bind(slug.as_str())
        .execute(pool)
        .await
        .context("insert workspace")?;

    let email = "member-one@team-bootstrap.test";
    let config = one_member(email);
    let db_config = db_config();

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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    invitation_is_confirmed(&database).await
}

async fn invitation_is_confirmed(database: &PgPool) -> Result<()> {
    let pool = database;
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

    bootstrap_team_operations(pool, &slug, &db_config(), &one_member(email)).await?;
    let after = member_state(pool, email).await?;
    ensure!(
        after.status == "active",
        "an invited contact must still be promoted, not left pending: status is {}",
        after.status
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_elastic_roster_bootstraps_member_keys_beyond_the_legacy_slots() -> Result<()> {
    // `CROWDRELAY_TEAM_MEMBERS_JSON` members carry their own key, name and
    // skills — a crew is however many people the tenant has, not five slots.
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let slug = WorkspaceSlug::parse("team-elastic")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Elastic roster test')")
        .bind(Uuid::now_v7())
        .bind(slug.as_str())
        .execute(pool)
        .await
        .context("insert workspace")?;

    let config = TeamOperationsConfig {
        members: vec![
            TeamMemberSpec {
                member_key: "ops_lead".to_owned(),
                email: "ops@team-elastic.test".to_owned(),
                display_name: "Ops Lead".to_owned(),
                skills: vec!["operations".to_owned(), "booking".to_owned()],
            },
            TeamMemberSpec {
                member_key: "social_1".to_owned(),
                email: "social@team-elastic.test".to_owned(),
                display_name: "Social One".to_owned(),
                skills: vec!["social".to_owned(), "visual".to_owned()],
            },
            TeamMemberSpec {
                member_key: "social_2".to_owned(),
                email: "social2@team-elastic.test".to_owned(),
                display_name: "Social Two".to_owned(),
                skills: vec!["social".to_owned()],
            },
        ],
    };

    bootstrap_team_operations(pool, &slug, &db_config(), &config).await?;

    let rows = sqlx::query(
        "SELECT member.normalized_email, member.display_name, member.status, \
                profile.member_key, profile.skills, profile.active \
         FROM viryaos_team_profiles AS profile \
         JOIN workspace_members AS member ON member.id = profile.member_id \
         ORDER BY profile.member_key",
    )
    .fetch_all(pool)
    .await
    .context("read roster")?;
    ensure!(
        rows.len() == 3,
        "all three members bootstrap: {}",
        rows.len()
    );
    let ops = &rows[0];
    ensure!(ops.try_get::<String, _>("member_key")? == "ops_lead");
    ensure!(ops.try_get::<String, _>("display_name")? == "Ops Lead");
    ensure!(ops.try_get::<String, _>("status")? == "active");
    ensure!(ops.try_get::<bool, _>("active")?);
    ensure!(
        ops.try_get::<Vec<String>, _>("skills")? == ["operations".to_owned(), "booking".to_owned()],
        "JSON-declared skills land verbatim"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_member_key_repointed_at_a_new_email_rebinds_its_profile() -> Result<()> {
    // Crews churn: an elastic roster makes it normal for `ops_lead` to stop
    // being Ada and start being Ben. The member_key is the routing identity,
    // so the profile follows the key to the new member row — a UNIQUE
    // (workspace_id, member_key) violation on every deploy would be the bug.
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let slug = WorkspaceSlug::parse("team-repoint")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Key repoint test')")
        .bind(Uuid::now_v7())
        .bind(slug.as_str())
        .execute(pool)
        .await
        .context("insert workspace")?;

    let db_config = db_config();
    let ada = TeamOperationsConfig {
        members: vec![TeamMemberSpec {
            member_key: "ops_lead".to_owned(),
            email: "ada@team-repoint.test".to_owned(),
            display_name: "Ada Ops".to_owned(),
            skills: vec!["operations".to_owned()],
        }],
    };
    bootstrap_team_operations(pool, &slug, &db_config, &ada).await?;
    let ada_member_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT member_id FROM viryaos_team_profiles WHERE member_key = 'ops_lead'",
    )
    .fetch_one(pool)
    .await
    .context("read first member_id")?;

    let ben = TeamOperationsConfig {
        members: vec![TeamMemberSpec {
            member_key: "ops_lead".to_owned(),
            email: "ben@team-repoint.test".to_owned(),
            display_name: "Ben Ops".to_owned(),
            skills: vec!["operations".to_owned(), "booking".to_owned()],
        }],
    };
    bootstrap_team_operations(pool, &slug, &db_config, &ben).await?;

    let row = sqlx::query(
        "SELECT profile.member_id, profile.skills, member.normalized_email \
         FROM viryaos_team_profiles AS profile \
         JOIN workspace_members AS member ON member.id = profile.member_id \
         WHERE profile.member_key = 'ops_lead'",
    )
    .fetch_one(pool)
    .await
    .context("read repointed profile")?;
    let ben_member_id = row.try_get::<Uuid, _>("member_id")?;
    ensure!(
        ben_member_id != ada_member_id,
        "the profile re-binds to the new member row"
    );
    ensure!(
        row.try_get::<String, _>("normalized_email")? == "ben@team-repoint.test",
        "the routing identity now reaches Ben"
    );
    ensure!(
        row.try_get::<Vec<String>, _>("skills")? == ["operations".to_owned(), "booking".to_owned()],
        "skills refresh on the re-bind"
    );
    ensure!(
        member_state(pool, "ada@team-repoint.test").await.is_err(),
        "Ada's member row keeps no routing profile"
    );

    Ok(())
}
