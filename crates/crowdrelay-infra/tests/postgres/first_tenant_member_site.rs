//! Migration 0356 and the release-mail signer: the first tenant keeps what it
//! has always sent, and nobody else inherits it.
//!
//! Everything runs inside a transaction that is rolled back: the `virya`
//! slug is unique, and a shared suite database must not keep a workspace or
//! a setting another test might read.

use crate::common;
use crowdrelay_infra::beacon_signal::beacon_release_signature;
use uuid::Uuid;

/// The migration's own statement, not a restatement of it.
const MIGRATION: &str = include_str!("../../../../migrations/0356_first_tenant_member_site.sql");

async fn site_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id)
    .fetch_optional(&mut **tx)
    .await
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_first_tenant_keeps_its_site_and_nobody_else_inherits_it()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let mut tx = pool.begin().await?;

    let virya = match sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM workspaces WHERE slug = 'virya'",
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(existing) => existing,
        None => sqlx::query_scalar(
            "INSERT INTO workspaces (id, slug, name) VALUES ($1, 'virya', 'Virya') RETURNING id",
        )
        .bind(Uuid::now_v7())
        .fetch_one(&mut *tx)
        .await?,
    };
    let other = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Mgła')")
        .bind(other)
        .bind(common::unique_slug("mgla", other))
        .execute(&mut *tx)
        .await?;

    // The state the migration meets in production: the first tenant reading
    // the old default, with no row of its own.
    sqlx::query(
        "DELETE FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(virya)
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;
    assert_eq!(
        site_row(&mut tx, virya).await?.as_deref(),
        Some("https://virya.music"),
        "the first tenant gets exactly the value it was reading"
    );
    assert_eq!(
        site_row(&mut tx, other).await?,
        None,
        "no other workspace is given the first tenant's site"
    );

    // A value the operator set is never overwritten, however often it runs.
    sqlx::query(
        "UPDATE tenant_settings SET value = 'https://fans.virya.example'
         WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(virya)
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;
    assert_eq!(
        site_row(&mut tx, virya).await?.as_deref(),
        Some("https://fans.virya.example")
    );

    // Release mails: the first tenant signs as it always has; anyone else
    // signs with its own wordmark and is not the first tenant.
    assert_eq!(
        beacon_release_signature(&mut *tx, virya).await?,
        ("Virya".to_owned(), true)
    );
    assert_eq!(
        beacon_release_signature(&mut *tx, other).await?,
        ("Mgła".to_owned(), false)
    );
    // An operator-set wordmark wins, for the first tenant too.
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'brand_wordmark', 'VIRYA')
         ON CONFLICT (workspace_id, key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(virya)
    .execute(&mut *tx)
    .await?;
    assert_eq!(
        beacon_release_signature(&mut *tx, virya).await?,
        ("VIRYA".to_owned(), true)
    );

    tx.rollback().await?;
    Ok(())
}
