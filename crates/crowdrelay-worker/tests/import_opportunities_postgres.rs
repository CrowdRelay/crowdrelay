//! The hand-curated live-opportunity import, against a real schema.
//!
//! `viryaos_team_opportunities` held zero rows in production, so the
//! `booking_opportunity` and `live_opportunity` autopilot contexts had never had
//! a unit to act on — while the band's CRM held 797 festivals and competitions
//! with fit scores, addresses and deadlines.
//!
//! Two properties matter more than the row count. A row with no route must not
//! land, because an action with no destination is a succeeded dispatch that
//! reached nobody — the press-pitch failure. And a re-import must not walk a
//! `submitted` row back to `new`, because that is a second application to the
//! same festival under the band's name.

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::import_opportunities::import_opportunities;
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
        let name = format!("crowdrelay_opps_{}", Uuid::now_v7().simple());
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
        .bind(format!("opps-{}", id.simple()))
        .bind("Opportunity import test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

const HEADER: &str = "external_key,opportunity_kind,title,organization,contact_email,\
destination_url,country_code,city,fit_basis_points,confidence_basis_points,deadline,eligible,\
typical_month,deadline_certainty,priority,genres,next_step,why_fit,verification,\
verified_destination";

/// A CSV in the converter's shape, including rows that must be refused.
fn fixture() -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    writeln!(file, "{HEADER}")?;
    // Reachable by email, with an exact deadline.
    writeln!(
        file,
        "ZG-001,festival,Wacken Metal Battle POLSKA,Wacken Metal Battle POLSKA,\
witek@example.test,,PL,Warszawa,9600,7000,2026-03-15,true,3,Wysoka,A,metal,\
Monitorowac otwarcie rundy,Polski nabor metalowy,Zweryfikowane 2026,true"
    )?;
    // Reachable only by submission form. Still a route, so it lands.
    writeln!(
        file,
        "ZG-002,review_contest,Road to Mystic,Road to Mystic,,\
https://example.test/apply,PL,Krakow,9500,2500,,true,9,Niska,A,metal,,,Historyczny,false"
    )?;
    // Closed edition: lands, but not eligible. The brain's live-calendar index
    // filters on `eligible`, so this is how a dormant annual cycle stays known
    // without being submitted to.
    writeln!(
        file,
        "ZG-003,showcase,Targi Closed 2026,Targi Closed 2026,book@example.test,,\
PL,Lodz,7000,4000,,false,5,Brak,B,,,,,false"
    )?;
    // Refused: no address and no form. Nothing could act on it.
    writeln!(
        file,
        "ZG-004,festival,No Route Festival,No Route Festival,,,PL,Gdansk,9000,2500,,true,7,Niska,C,,,,,false"
    )?;
    // Refused: a kind the table's CHECK does not accept.
    writeln!(
        file,
        "ZG-005,juwenalia,Bad Kind Fest,Bad Kind Fest,x@example.test,,PL,Poznan,8000,2500,,true,5,Niska,C,,,,,false"
    )?;
    file.flush()?;
    Ok(file)
}

async fn count(pool: &PgPool, ws: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_team_opportunities WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn usable_rows_land_and_routeless_ones_are_refused() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = import_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn import_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let csv = fixture()?;

    let summary = import_opportunities(pool, ws, csv.path()).await?;
    ensure!(summary.written == 3, "expected 3 written, got {summary:?}");
    ensure!(summary.skipped == 2, "expected 2 skipped, got {summary:?}");
    ensure!(
        summary.with_deadline == 1,
        "only the exact-dated row has a deadline, got {summary:?}"
    );
    ensure!(count(pool, ws).await? == 3, "only usable rows may land");

    // Every row that landed is reachable, by address or by form. This is the
    // property that separates this import from the press-pitch path, where a
    // drafted action had no destination at all.
    let routeless: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND contact_email IS NULL AND destination_url IS NULL",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(routeless == 0, "an imported opportunity must have a route");

    // Only the row whose verification names an official source claims a verified
    // destination. This gate holds an opportunity in `evaluate_live_opportunity`
    // before its score is even read, and `execution.rs` selects on it, so claiming
    // it for a row nobody checked would send an application to an address nobody
    // checked. Automated discovery sets it false on purpose; this carries only the
    // assertion the band already recorded.
    let verified: Vec<(String, bool)> = sqlx::query_as(
        "SELECT external_key, verified_destination FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 ORDER BY external_key",
    )
    .bind(ws.into_uuid())
    .fetch_all(pool)
    .await?;
    ensure!(
        verified
            == [
                ("ZG-001".to_owned(), true),
                ("ZG-002".to_owned(), false),
                ("ZG-003".to_owned(), false),
            ],
        "only the officially verified row may claim a destination, got {verified:?}"
    );

    // Arrives as `new`, the status the booking autopilot picks up. A
    // hand-curated list is still a list the loop should screen.
    let fresh: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND status = 'new'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(fresh == 3, "imported opportunities start as new");

    // The sheet's A–D priority is the only strategic signal the import has;
    // it must land as `strategic_value_basis_points` or the row can never
    // reach the live-opportunity score bar. A maps above the policy's 8_500
    // Landmark floor, B above the 6_000 Notable floor.
    let strategic: Vec<(String, i32)> = sqlx::query_as(
        "SELECT external_key, strategic_value_basis_points FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 ORDER BY external_key",
    )
    .bind(ws.into_uuid())
    .fetch_all(pool)
    .await?;
    ensure!(
        strategic
            == [
                ("ZG-001".to_owned(), 9_000),
                ("ZG-002".to_owned(), 9_000),
                ("ZG-003".to_owned(), 6_500),
            ],
        "priority must land as strategic value, got {strategic:?}"
    );

    // A closed edition lands ineligible, so the live-calendar index skips it.
    let eligible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND eligible",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(eligible == 2, "the closed edition must not be eligible");

    // The month estimate is metadata, never a deadline. 429 of the sheet's rows
    // date themselves by the month the event happens in, at low certainty, and
    // the brain cannot tell an estimate from a fact once it is a timestamp.
    let estimated: Option<String> = sqlx::query_scalar(
        "SELECT metadata->>'typical_deadline_month' FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-002'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        estimated.as_deref() == Some("9"),
        "the month estimate belongs in metadata, got {estimated:?}"
    );
    let no_deadline: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT deadline FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-002'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        no_deadline.is_none(),
        "an estimated month must not become a deadline"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn reimport_refreshes_contacts_without_resetting_progress() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = reimport_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn reimport_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let csv = fixture()?;
    import_opportunities(pool, ws, csv.path()).await?;

    // The loop applies to the festival and an operator dismisses the showcase.
    sqlx::query(
        "UPDATE viryaos_team_opportunities SET status = 'submitted' \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE viryaos_team_opportunities SET status = 'dismissed', eligible = false \
         WHERE workspace_id = $1 AND external_key = 'ZG-003'",
    )
    .bind(ws.into_uuid())
    .execute(pool)
    .await?;

    // The sheet is re-exported with a corrected address for the festival.
    let mut second = tempfile::NamedTempFile::new()?;
    writeln!(second, "{HEADER}")?;
    writeln!(
        second,
        "ZG-001,festival,Wacken Metal Battle POLSKA,Wacken Metal Battle POLSKA,\
newaddress@example.test,,PL,Warszawa,9700,7000,2026-03-20,true,3,Wysoka,A,metal,,,Zweryfikowane 2026,false"
    )?;
    writeln!(
        second,
        "ZG-003,showcase,Targi Closed 2026,Targi Closed 2026,book@example.test,,\
PL,Lodz,7000,4000,,true,5,Brak,B,,,,,false"
    )?;
    second.flush()?;
    import_opportunities(pool, ws, second.path()).await?;

    ensure!(
        count(pool, ws).await? == 3,
        "a re-import must not duplicate"
    );

    // Contact details and the deadline refresh: that is what a re-import is for.
    let (email, fit): (Option<String>, i32) = sqlx::query_as(
        "SELECT contact_email, fit_basis_points FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        email.as_deref() == Some("newaddress@example.test"),
        "a corrected address must land, got {email:?}"
    );
    ensure!(fit == 9700, "a corrected fit score must land, got {fit}");

    // Strategic value fills a blank but never overwrites: a nonzero value may
    // be operator judgement or loop learning the sheet does not know about.
    sqlx::query(
        "UPDATE viryaos_team_opportunities SET strategic_value_basis_points = 8_800 \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .execute(pool)
    .await?;
    import_opportunities(pool, ws, csv.path()).await?;
    let kept: i32 = sqlx::query_scalar(
        "SELECT strategic_value_basis_points FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        kept == 8_800,
        "an enriched strategic value must survive a re-import, got {kept}"
    );

    // Status does not move. Walking `submitted` back to `new` would put the band
    // in front of the same festival twice under its own name.
    let status: String = sqlx::query_scalar(
        "SELECT status FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        status == "submitted",
        "status must survive a re-import, got {status}"
    );

    // A verification is never taken away. The second export carries
    // `verified_destination=false` for ZG-001, and the row keeps the true it had:
    // an operator who verified a destination inside CrowdRelay must not lose it to
    // a re-export that happens not to carry the note.
    let still_verified: bool = sqlx::query_scalar(
        "SELECT verified_destination FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-001'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        still_verified,
        "a re-import must not withdraw a verified destination"
    );

    // And a dismissal survives, even though the sheet now says eligible.
    let (status, eligible): (String, bool) = sqlx::query_as(
        "SELECT status, eligible FROM viryaos_team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'ZG-003'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        status == "dismissed" && !eligible,
        "a dismissal must not be reopened by a re-import, got {status} eligible={eligible}"
    );

    Ok(())
}
