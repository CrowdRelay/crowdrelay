//! The hand-curated contact import, against a real schema.
//!
//! The band's CRM is where the outreach list actually lives — thousands of
//! radio shows, stations, press outlets and curators gathered by hand. The
//! growth loop could not see any of it: production held 22 press targets, nine
//! with an address. An import that silently dropped rows, or that re-admitted
//! something an operator had discarded, would be worse than no import at all.

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::import_outreach::import_outreach;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use std::io::Write;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_import_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').context("database url has no path")?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{head}/{name}"))
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

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("import-{}", id.simple()))
        .bind("Import test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A CSV in the converter's shape, including rows that must be refused.
fn fixture() -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    writeln!(
        file,
        "display_name,contact_email,target_kind,evidence_url,country_code,fit_score,notes,source"
    )?;
    writeln!(
        file,
        "Radio 357,redakcja@radio357.pl,radio,https://radio357.pl,PL,96,Pitch premiery · Warszawa,crm:media"
    )?;
    writeln!(
        file,
        "Metal Injection,tips@metalinjection.net,press,https://metalinjection.net,US,88,,crm:kontakty"
    )?;
    // Refused: no address. A contact without one is a lead, not a recipient.
    writeln!(
        file,
        "No Address Blog,,press,https://example.test,PL,50,,crm:kontakty"
    )?;
    // Refused: a kind the table's CHECK does not accept.
    writeln!(
        file,
        "Some Venue,book@venue.test,booking_agency,https://venue.test,PL,70,,crm:podmioty"
    )?;
    file.flush()?;
    Ok(file)
}

async fn count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_outreach_targets WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(pool)
            .await?;
    Ok(total)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn usable_rows_land_and_unusable_ones_are_refused() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = import_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn import_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let csv = fixture()?;

    let summary = import_outreach(pool, ws, csv.path()).await?;
    ensure!(summary.written == 2, "expected 2 written, got {summary:?}");
    ensure!(summary.skipped == 2, "expected 2 skipped, got {summary:?}");
    ensure!(count(pool, ws).await? == 2, "only usable rows may land");

    // Everything that landed is reachable — the whole point of the import.
    let unreachable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND (contact_email IS NULL OR contact_email = '')",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(unreachable == 0, "an imported target must have an address");

    // Arrives as `proposed`, exactly as an agent proposal does. A hand-curated
    // list is still a list somebody should look at before the loop acts on it.
    let proposed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_outreach_targets WHERE workspace_id = $1 AND status = 'proposed'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(proposed == 2, "imported targets must arrive as proposed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_second_import_adds_nothing_and_keeps_a_discard_discarded() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = idempotent_inner(&database.pool).await;
    database.drop_database().await;
    result
}

/// Re-running must be safe, and must not undo an operator's decision.
///
/// The sheet is edited and re-exported, so this runs repeatedly. If a second
/// pass re-admitted a target somebody had thrown out, the discard would mean
/// nothing and the same bad contact would keep coming back.
async fn idempotent_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let csv = fixture()?;
    import_outreach(pool, ws, csv.path()).await?;

    sqlx::query(
        "UPDATE agent_outreach_targets SET status = 'discarded' \
         WHERE workspace_id = $1 AND display_name = 'Radio 357'",
    )
    .bind(ws.into_uuid())
    .execute(pool)
    .await?;

    let summary = import_outreach(pool, ws, csv.path()).await?;
    ensure!(summary.written == 2, "the same rows are written again");
    ensure!(
        count(pool, ws).await? == 2,
        "a re-import must not duplicate"
    );

    let status: String = sqlx::query_scalar(
        "SELECT status FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND display_name = 'Radio 357'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        status == "discarded",
        "a discarded target must stay discarded, got {status:?}"
    );
    Ok(())
}
