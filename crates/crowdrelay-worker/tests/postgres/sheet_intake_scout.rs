//! The scout-registry sync's intake, end to end: the GitHub
//! `database_festivals.xlsx` and the Google workbooks' OPPORTUNITIES tabs
//! in their several grammars land through the same reader, dedupe on the
//! destination URL, and preserve what the loop already did. Own file
//! because the parent suite is at its size-ratchet line.

use crate::{common, sheet_intake};

use anyhow::Result;
use crowdrelay_worker::sheet_intake::{SheetTrust, harvest_grids};
use sqlx::PgPool;

use sheet_intake::{grid, workspace};

/// The scout-registry sync's two-file shape, end to end: the GitHub
/// `database_festivals.xlsx` (SCOUT_PL dialect, bannered tabs) and a
/// `SCOUT AUTO` export arrive through the same intake, and the one
/// opportunity both files list — the same destination URL spelled two
/// ways — lands on a single `team_opportunities` row. The scout's
/// META_TARGETS feeds `beacons`, its SUPPORT_TARGETS feeds
/// `place_peer_acts`, a re-run refreshes rather than duplicates, and a
/// status the loop already set survives the re-run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn scout_files_dedupe_across_dialects_and_preserve_status() -> Result<()> {
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL").await?;
    let outcome =
        scout_files_dedupe_across_dialects_and_preserve_status_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn scout_files_dedupe_across_dialects_and_preserve_status_on(pool: PgPool) -> Result<()> {
    let workspace_id = workspace(&pool, "scoutdedupe").await?;

    // database_festivals.xlsx — the SCOUT_PL grammar under its banner row.
    let festivals_opportunities = grid(&[
        &["SCOUT_PL — snapshot dla przebiegu (banner, nie nagłówek)"],
        &[
            "Name",
            "Organizer",
            "Type",
            "Voivodeship",
            "City",
            "Current_Status",
            "Event_Date",
            "Application_Deadline",
            "Next_Cycle",
            "Eligibility",
            "Economics",
            "VIRYA_History",
            "Next_Action",
            "Source_URL",
            "Evidence",
        ],
        &[
            "Euroblast Festival — zgłoszenie",
            "Euroblast",
            "CONTEST / FESTIVAL",
            "",
            "Köln",
            "ACTIVE — call open",
            "2026-09-25",
            "2026-07-31",
            "",
            "Strong fit",
            "",
            "",
            "Apply via site",
            "https://www.euroblast.net/en/contact/",
            "",
        ],
        &[
            "KozyNostra Rock Fest — Przegląd Zespołów",
            "Dom Kultury w Kozach",
            "CONTEST / FESTIVAL",
            "śląskie",
            "Kozy",
            "CONFIRMED_HISTORICAL — 2026 closed",
            "2026-08-22",
            "2026-06-30",
            "",
            "Likely fit",
            "",
            "",
            "Verify next cycle",
            "https://kozynostra.pl/",
            "",
        ],
    ]);
    let meta_targets = grid(&[
        &["SCOUT — cele meta (banner)"],
        &[
            "Name",
            "Category",
            "Region",
            "Public URL / Username",
            "Public Contact",
            "Relevance",
            "Status",
            "Why useful",
            "Dedupe Key",
            "Checked",
        ],
        &[
            "Euroblast Festival",
            "Festival / promoter",
            "Germany / Cologne",
            "https://www.euroblast.net/",
            "su@euroblast.net",
            "95.0",
            "Verified public",
            "Top-tier genre fit",
            "euroblast|org",
            "2026-08-11",
        ],
    ]);
    let support_targets = grid(&[
        &["SCOUT — wsparcia (banner)"],
        &[
            "Band / Artist",
            "Genre",
            "Region",
            "Public URL",
            "Availability",
            "Fit",
            "Status",
            "Dedupe Key",
            "Notes",
        ],
        &[
            "Intake Test Act",
            "metal",
            "Łódź",
            "https://intake-test-act.example/band",
            "2026",
            "High",
            "Active",
            "intake-test-act|support",
            "local scene",
        ],
    ]);

    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "database_festivals.xlsx",
        vec![festivals_opportunities, meta_targets, support_targets]
            .into_iter()
            .map(Into::into)
            .collect(),
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    assert_eq!(harvest.opportunity_sheets, 1, "the PL tab was not claimed");
    assert_eq!(
        harvest.opportunities_seeded, 2,
        "the PL rows did not both seed"
    );
    assert_eq!(harvest.opportunity_refusals, 0);
    assert_eq!(harvest.scout_meta_sheets, 1, "META_TARGETS was not claimed");
    assert_eq!(harvest.beacons_imported, 1, "META_TARGETS did not import");
    assert_eq!(
        harvest.peer_acts_imported, 1,
        "SUPPORT_TARGETS did not reach the band registry"
    );

    // SCOUT AUTO.xlsx — its OPPORTUNITIES tab under banner+note+blank,
    // listing one opportunity the festivals file already asserted (same
    // destination, different spelling) plus one nobody listed.
    let auto_opportunities = grid(&[
        &["SCOUT AUTO — automatyczny raport"],
        &["Generowane przez n8n — nie edytować ręcznie"],
        &[""],
        &[
            "Opportunity_ID",
            "Found_At",
            "Source_Type",
            "Source_Name",
            "Title",
            "Organization",
            "Category",
            "Country",
            "City",
            "Deadline",
            "Event_Date",
            "URL",
            "Contact_Email",
            "Submission_Method",
            "Free_To_Apply",
            "Relevance_Score",
            "Priority",
            "Status",
            "Why_Fit",
            "Red_Flags",
            "Next_Step",
            "Owner",
            "Discord_Status",
            "Dedupe_Key",
            "Raw_Snippet",
        ],
        &[
            "OPP-20260901-001",
            "2026-09-01",
            "scraper",
            "euroblast.net",
            "Euroblast Festival — Band Application",
            "Euroblast",
            "Festival / band application",
            "Germany",
            "Cologne",
            "2026-07-31",
            "2026-09-25",
            "https://euroblast.net/en/contact",
            "application@euroblast.net",
            "Official site",
            "Yes",
            "95.0",
            "A",
            "Monitor",
            "Top-tier genre fit",
            "",
            "Watch deadline",
            "",
            "queued",
            "euroblast|band-application|virya",
            "",
        ],
        &[
            "OPP-20260901-002",
            "2026-09-01",
            "scraper",
            "newfest.example",
            "Newfest — call for bands",
            "Newfest Collective",
            "Festival / showcase",
            "Polska",
            "Poznań",
            "2026-10-15",
            "2026-11-20",
            "https://newfest.example.com/apply",
            "",
            "Official site",
            "Yes",
            "70.0",
            "B",
            "Nowe",
            "Fresh festival",
            "",
            "Verify",
            "",
            "queued",
            "newfest|call|virya",
            "",
        ],
    ]);

    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "SCOUT AUTO.xlsx",
        vec![auto_opportunities]
            .into_iter()
            .map(Into::into)
            .collect(),
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    assert_eq!(
        harvest.opportunity_sheets, 1,
        "the AUTO tab was not claimed"
    );
    assert_eq!(
        harvest.opportunities_seeded, 1,
        "only the genuinely new row should seed"
    );
    assert_eq!(
        harvest.opportunities_refreshed, 1,
        "the cross-file duplicate should refresh, not insert"
    );

    // The later registry sighting enriches the existing opportunity metadata
    // rather than losing its dedupe/evidence keys on the conflict path.
    let dedupe_key: Option<String> = sqlx::query_scalar(
        "SELECT metadata->>'dedupe_key' FROM team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'url:euroblast.net/en/contact'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dedupe_key.as_deref(),
        Some("euroblast|band-application|virya"),
        "registry metadata did not enrich the existing opportunity"
    );

    // Three rows total — the same destination URL asserted by two
    // dialects in two files is one opportunity.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM team_opportunities WHERE workspace_id = $1 AND source = 'scout_sheet'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 3, "cross-dialect dedupe minted a duplicate");

    // What the loop did stays done: a dismissed row must not reopen when
    // a stale snapshot re-asserts it.
    sqlx::query(
        "UPDATE team_opportunities SET status = 'dismissed' \
         WHERE workspace_id = $1 AND external_key = 'url:euroblast.net/en/contact'",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    let auto_opportunities = grid(&[
        &[
            "Opportunity_ID",
            "Found_At",
            "Source_Type",
            "Source_Name",
            "Title",
            "Organization",
            "Category",
            "Country",
            "City",
            "Deadline",
            "Event_Date",
            "URL",
            "Contact_Email",
            "Submission_Method",
            "Free_To_Apply",
            "Relevance_Score",
            "Priority",
            "Status",
            "Why_Fit",
            "Red_Flags",
            "Next_Step",
            "Owner",
            "Discord_Status",
            "Dedupe_Key",
            "Raw_Snippet",
        ],
        &[
            "OPP-20261002-014",
            "2026-10-02",
            "scraper",
            "euroblast.net",
            "Euroblast Festival — Band Application",
            "Euroblast",
            "Festival / band application",
            "Germany",
            "Cologne",
            "2026-07-31",
            "2026-09-25",
            "https://euroblast.net/en/contact",
            "application@euroblast.net",
            "Official site",
            "Yes",
            "95.0",
            "A",
            "Monitor",
            "Top-tier genre fit",
            "",
            "Watch deadline",
            "",
            "queued",
            "euroblast|band-application|virya",
            "",
        ],
    ]);
    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "SCOUT AUTO.xlsx",
        vec![auto_opportunities]
            .into_iter()
            .map(Into::into)
            .collect(),
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!(harvest.opportunities_seeded, 0);
    assert_eq!(harvest.opportunities_refreshed, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM team_opportunities \
         WHERE workspace_id = $1 AND external_key = 'url:euroblast.net/en/contact'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        status, "dismissed",
        "a stale snapshot reopened a dismissed row"
    );
    Ok(())
}
