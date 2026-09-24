//! `crowdrelay_workspace_wordmark` — the one name a workspace's fan-facing
//! messages carry.
//!
//! Everything runs inside a transaction that is rolled back: the `virya` slug
//! is unique, and a shared suite database must not keep a workspace another
//! test might need.

use crate::common;
use uuid::Uuid;

async fn workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    slug: &str,
    name: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3) RETURNING id")
        .bind(Uuid::now_v7())
        .bind(slug)
        .bind(name)
        .fetch_one(&mut **tx)
        .await
}

async fn wordmark(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT crowdrelay_workspace_wordmark($1)")
        .bind(workspace_id)
        .fetch_one(&mut **tx)
        .await
}

async fn set_wordmark(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'brand_wordmark', $2)
         ON CONFLICT (workspace_id, key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(workspace_id)
    .bind(value)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn each_workspace_speaks_as_itself() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, _) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let mut tx = pool.begin().await?;
    let suffix = Uuid::now_v7().simple().to_string();

    // The first tenant keeps the wordmark every message it ever sent carried,
    // whatever its workspace row happens to be called.
    let virya =
        match sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = 'virya'")
            .fetch_optional(&mut *tx)
            .await?
        {
            Some(existing) => existing,
            None => workspace(&mut tx, "virya", "Virya workspace").await?,
        };
    assert_eq!(wordmark(&mut tx, virya).await?, "VIRYA");

    // Any other act is its own name — never the first tenant's.
    let act = workspace(&mut tx, &format!("mgla-{suffix}"), "Mgła").await?;
    assert_eq!(wordmark(&mut tx, act).await?, "Mgła");

    // An explicit setting wins, for the first tenant too; a blank one is
    // ignored rather than silencing the name.
    set_wordmark(&mut tx, act, "MGŁA").await?;
    assert_eq!(wordmark(&mut tx, act).await?, "MGŁA");
    set_wordmark(&mut tx, act, "   ").await?;
    assert_eq!(wordmark(&mut tx, act).await?, "Mgła");
    set_wordmark(&mut tx, virya, "Virya").await?;
    assert_eq!(wordmark(&mut tx, virya).await?, "Virya");

    // Ticket and pass references: the first tenant's stay 'VIRYA'; anyone
    // else's are its slug, ASCII only, so a code can be read out at a door.
    let prefix = |id: Uuid| {
        sqlx::query_scalar::<_, String>("SELECT crowdrelay_workspace_reference_prefix($1)").bind(id)
    };
    assert_eq!(prefix(virya).fetch_one(&mut *tx).await?, "VIRYA");
    // Separators dropped, capped at twelve: `mgla-<32 hex>` → MGLA + 8 hex.
    assert_eq!(
        prefix(act).fetch_one(&mut *tx).await?,
        format!("MGLA{}", &suffix[..8]).to_uppercase()
    );
    let spaced = workspace(&mut tx, &format!("the_b-{}", &suffix[..4]), "The B").await?;
    assert_eq!(
        prefix(spaced).fetch_one(&mut *tx).await?,
        format!("THEB{}", &suffix[..4]).to_uppercase()
    );

    tx.rollback().await?;
    Ok(())
}
