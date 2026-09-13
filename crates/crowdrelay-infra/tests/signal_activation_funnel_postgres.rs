//! The Signal activation funnel, against a real schema.
//!
//! Migration 0257 says of `signal_installations.fan_id`: "the gap between the
//! count of rows and the count of non-null values here IS the activation
//! funnel." For as long as no code wrote that column the numerator was zero by
//! construction, so the funnel reported 0% no matter how many installs
//! identified themselves. These tests pin the write that closes it, and the
//! three ways it could quietly reopen: a repeat launch erasing the link, a
//! second fan rewriting it, and a workspace reaching across the tenant line.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    config::DatabaseConfig,
    database,
    signal_installations::{link_installation_to_fan, record_installation},
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// The variable every workflow and the local recipe already export. A suite
/// that invents its own name has to be added to the justfile and to both CI
/// jobs, and a forgotten one does not skip — `test_pool` returns an error and
/// the suite fails on a machine that has a database.
const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_TEST_DATABASE_URL";

async fn test_pool() -> Result<PgPool, Box<dyn std::error::Error>> {
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|error| format!("set {TEST_DATABASE_URL_KEY}: {error}"))?;
    let config = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let pool = database::connect(&config).await?;
    database::migrate(&pool).await?;
    Ok(pool)
}

async fn seed_workspace(pool: &PgPool, label: &str) -> Result<WorkspaceId, sqlx::Error> {
    let workspace_id = WorkspaceId::new();
    let slug = format!("{label}-{}", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Signal funnel test')")
        .bind(workspace_id.into_uuid())
        .bind(&slug)
        .execute(pool)
        .await?;
    Ok(workspace_id)
}

async fn seed_fan(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid, sqlx::Error> {
    let fan_id = Uuid::now_v7();
    let email = format!("{}@signal-funnel.test", fan_id.simple());
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(&email)
    .execute(pool)
    .await?;
    Ok(fan_id)
}

async fn linked_fan(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    installation_id: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT fan_id FROM signal_installations WHERE workspace_id = $1 AND installation_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(installation_id)
    .fetch_one(pool)
    .await?;
    row.try_get::<Option<Uuid>, _>("fan_id")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_first_identification_is_what_the_funnel_records()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool, "signal-first").await?;
    let installation_id = format!("install-{}", Uuid::now_v7().simple());

    record_installation(
        &pool,
        workspace_id,
        &installation_id,
        "android",
        Some("1.0.0"),
    )
    .await?;
    assert_eq!(
        linked_fan(&pool, workspace_id, &installation_id).await?,
        None,
        "an install is anonymous until somebody identifies it",
    );

    let first_fan = seed_fan(&pool, workspace_id).await?;
    assert!(
        link_installation_to_fan(&pool, workspace_id, &installation_id, first_fan).await?,
        "the first link reports that it moved the funnel",
    );
    assert_eq!(
        linked_fan(&pool, workspace_id, &installation_id).await?,
        Some(first_fan),
    );

    // A device handed to somebody else must not rewrite who converted it. The
    // column answers "did this install ever convert", and it already has.
    let second_fan = seed_fan(&pool, workspace_id).await?;
    assert!(
        !link_installation_to_fan(&pool, workspace_id, &installation_id, second_fan).await?,
        "a second identification is not a second conversion",
    );
    assert_eq!(
        linked_fan(&pool, workspace_id, &installation_id).await?,
        Some(first_fan),
        "the first fan still owns the conversion",
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_later_launch_does_not_un_identify_the_install() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool, "signal-relaunch").await?;
    let installation_id = format!("install-{}", Uuid::now_v7().simple());
    let fan_id = seed_fan(&pool, workspace_id).await?;

    record_installation(
        &pool,
        workspace_id,
        &installation_id,
        "android",
        Some("1.0.0"),
    )
    .await?;
    assert!(link_installation_to_fan(&pool, workspace_id, &installation_id, fan_id).await?);

    // The app calls `record_installation` on every launch, so its upsert runs
    // far more often than the link does. Its conflict clause must leave
    // `fan_id` alone, or every launch after identification would reset the
    // funnel to anonymous.
    record_installation(
        &pool,
        workspace_id,
        &installation_id,
        "android",
        Some("1.1.0"),
    )
    .await?;
    assert_eq!(
        linked_fan(&pool, workspace_id, &installation_id).await?,
        Some(fan_id),
        "a repeat launch kept the identification",
    );

    let row = sqlx::query(
        "SELECT app_version FROM signal_installations \
         WHERE workspace_id = $1 AND installation_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(&installation_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        row.try_get::<Option<String>, _>("app_version")?,
        Some("1.1.0".to_string()),
        "the launch still recorded the new build",
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_link_never_reaches_outside_its_own_workspace() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let ours = seed_workspace(&pool, "signal-ours").await?;
    let theirs = seed_workspace(&pool, "signal-theirs").await?;

    // The same installation string in two tenants is the shape that catches an
    // unscoped UPDATE: the primary key is (workspace_id, installation_id), so
    // both rows exist and only the where clause separates them.
    let installation_id = format!("install-{}", Uuid::now_v7().simple());
    record_installation(&pool, ours, &installation_id, "android", None).await?;
    record_installation(&pool, theirs, &installation_id, "ios", None).await?;

    let our_fan = seed_fan(&pool, ours).await?;
    assert!(link_installation_to_fan(&pool, ours, &installation_id, our_fan).await?);

    assert_eq!(
        linked_fan(&pool, ours, &installation_id).await?,
        Some(our_fan)
    );
    assert_eq!(
        linked_fan(&pool, theirs, &installation_id).await?,
        None,
        "the other tenant's install of the same id stayed anonymous",
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unknown_installation_is_reported_not_invented() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool, "signal-unknown").await?;
    let fan_id = seed_fan(&pool, workspace_id).await?;

    // A fan may register for push from a client that never reported an install
    // — a browser, or a build older than the install endpoint. The link must
    // report that it changed nothing rather than conjure an install row, which
    // would inflate the funnel's denominator with devices nobody installed.
    let installation_id = format!("never-seen-{}", Uuid::now_v7().simple());
    assert!(!link_installation_to_fan(&pool, workspace_id, &installation_id, fan_id).await?);

    let installs: i64 =
        sqlx::query("SELECT COUNT(*) FROM signal_installations WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(&pool)
            .await?
            .try_get(0)?;
    assert_eq!(installs, 0, "no install row was invented");

    Ok(())
}
