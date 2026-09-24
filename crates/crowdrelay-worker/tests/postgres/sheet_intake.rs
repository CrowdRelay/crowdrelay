//! The shared sheet intake against a real schema — the dispatch contract
//! the GitHub registry workbook depends on: banner rows hide headers, dump
//! tabs are context not intake, the agent tab stages verdicts.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::drive_contacts::ExtractedContact;
use crowdrelay_infra::gdrive::PostgresGDriveRepository;
use crowdrelay_worker::sheet_intake::{SheetTrust, harvest_grids};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, label: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("{label}-{}", id.simple()))
        .bind("Sheet intake test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES (gen_random_uuid(), 'wroclaw', 'Wrocław', 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(id)
}

fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect()
}

/// The database.xlsx shape, end to end: a one-cell banner over each real
/// header, two dump tabs that must not import, an agent tab that stages
/// verdicts, and a plain contact list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_registry_workbook_routes_every_tab() -> Result<()> {
    // Its own database: the sheet resolves cities by name against the seeded
    // catalogue, and a sibling suite's second PL "Wrocław" makes that
    // lookup ambiguous — which the intake rightly refuses to guess through.
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL").await?;
    let outcome = the_registry_workbook_routes_every_tab_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn the_registry_workbook_routes_every_tab_on(pool: PgPool) -> Result<()> {
    let workspace_id = workspace(&pool, "workbook").await?;

    let venues = grid(&[
        &["venue_seed intake columns — re-importable as-is"],
        &[
            "Name",
            "City",
            "Country",
            "Address",
            "Website",
            "Audience_Genre",
            "Capacity",
            "Booking_Contact",
            "Public_Financial_Info",
            "Status",
            "Source_URL",
            "Research_Date",
            "Target_Fit",
            "Contact_Quality",
            "Outreach_Angle",
            "Notes",
        ],
        &[
            "Klub Workbook",
            "Wrocław",
            "Poland",
            "Ulica 1",
            "https://klubworkbook.test",
            "rock",
            "300",
            "",
            "",
            "Active",
            "https://klubworkbook.test/about",
            "2026-09-22",
            "High",
            "",
            "",
            "",
        ],
    ]);
    let bands = grid(&[
        &["peer_act_seed intake columns — re-importable as-is"],
        &[
            "Name",
            "Country",
            "City",
            "Genre",
            "Email",
            "Social",
            "Website",
            "Source_URL",
            "Activity",
            "Research_Date",
            "Confidence",
            "Contact_Type",
            "Contact_Source",
            "Outreach_Readiness",
            "Notes",
        ],
        &[
            "Workbook Act",
            "Poland",
            "Wrocław",
            "black metal",
            "act@workbook.test",
            "https://facebook.com/workbookact",
            "https://workbookact.test",
            "https://workbookact.test/shows",
            "Active — toured 2026",
            "2026-09-22",
            "High",
            "",
            "",
            "",
            "",
        ],
    ]);
    let beacons = grid(&[
        &["press/media registry — context only, no sheet intake"],
        &[
            "Name",
            "Kind",
            "City",
            "Email",
            "Destination_URL",
            "Source_URL",
            "Active",
            "Verified",
            "Accepts_Outreach",
            "Do_Not_Contact",
            "Relationship_Score",
            "Relevance_Pct",
            "Confidence_Pct",
        ],
        // A press row carrying Name/City/Source_URL is the venue pin's
        // exact collision — the dump guard is what keeps it from minting
        // a room.
        &[
            "Marcin",
            "promoter",
            "Wrocław",
            "m@beacon.test",
            "https://beacon.test",
            "https://evidence.test",
            "t",
            "t",
            "t",
            "f",
            "100",
            "100.0",
            "100.0",
        ],
    ]);
    let agents = grid(&[
        &["EMPTY in production — the known gap"],
        &[
            "Name",
            "Agency",
            "Email",
            "Roster_URL",
            "Genres",
            "Active",
            "Approached_At",
            "Refused_Until",
            "Do_Not_Contact",
        ],
        &[
            "Agent Live",
            "Agency One",
            "live@agency.test",
            "https://agencyone.test/roster",
            "metal",
            "t",
            "",
            "",
            "",
        ],
        &[
            "Agent Gone",
            "Agency Two",
            "gone@agency.test",
            "https://agencytwo.test",
            "rock",
            "f",
            "",
            "",
            "",
        ],
    ]);
    let contacts_dump = grid(&[
        &["drive_contacts registry — dedupe context"],
        &[
            "Email",
            "Name",
            "Organization",
            "City",
            "Kind",
            "Phone",
            "Notes",
            "Last_Seen",
            "Disappeared",
        ],
        &[
            "staged@contact.test",
            "A Contact",
            "Org",
            "Poznań",
            "press",
            "",
            "",
            "2026-09-22",
            "f",
        ],
    ]);
    let list = grid(&[
        &["Email", "Name", "City", "Website"],
        &[
            "booker@real.test",
            "Real Booker",
            "Wrocław",
            "https://booker.test",
        ],
    ]);

    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "database.xlsx",
        vec![venues, bands, beacons, agents, contacts_dump, list],
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    // The contacts readout was recognised as context, never intake — the
    // beacons tab is intake now, so only one dump remains.
    assert_eq!(
        harvest.registry_dump_sheets, 1,
        "only the contacts readout is a dump"
    );
    assert_eq!(harvest.agent_sheets, 1, "the agents tab is an intake sheet");
    assert_eq!(
        harvest.beacons_imported, 1,
        "the beacons tab did not import"
    );
    assert_eq!(
        harvest.agents_imported, 2,
        "agent rows never reached the registry"
    );
    ensure!(
        !harvest
            .contacts
            .iter()
            .any(|c| c.email == "m@beacon.test" || c.email == "staged@contact.test"),
        "registry readouts staged as contacts"
    );

    // Agents staged as booking agents carrying the Active verdict.
    let live = harvest
        .contacts
        .iter()
        .find(|c| c.email == "live@agency.test")
        .context("live agent row did not stage")?;
    assert_eq!(live.suggested_kind.as_deref(), Some("booking_agent"));
    assert_eq!(live.staged_status.as_deref(), Some("active"));
    assert_eq!(live.organization.as_deref(), Some("Agency One"));
    let gone = harvest
        .contacts
        .iter()
        .find(|c| c.email == "gone@agency.test")
        .context("dead agent row did not stage")?;
    assert_eq!(gone.staged_status.as_deref(), Some("inactive"));

    // The contact list row arrived, venue and band went to their
    // registries rather than the review queue.
    ensure!(
        harvest
            .contacts
            .iter()
            .any(|c| c.email == "booker@real.test"),
        "the plain contact list lost its row"
    );
    assert_eq!(harvest.venues_imported, 1, "venue tab did not import");
    assert_eq!(harvest.peer_acts_imported, 1, "band tab did not import");

    let room: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM place_venues WHERE display_name = 'Klub Workbook'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(room, 1, "the venue row never reached the registry");
    let act: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM place_peer_acts WHERE display_name = 'Workbook Act'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(act, 1, "the band row never reached the registry");

    // The beacon landed with its kind, resolved city and the sheet's own
    // flags — the operator's verdicts, not a research-importer's caution.
    let beacon: (String, bool, bool, bool, i32, i32) = sqlx::query_as(
        "SELECT b.beacon_kind, b.verified, b.accepts_outreach, b.do_not_contact, \
                b.relationship_score, b.relevance_basis_points \
         FROM beacons b \
         JOIN cities c ON c.id = b.city_id \
         WHERE b.workspace_id = $1 AND b.contact_email = 'm@beacon.test' AND c.slug = 'wroclaw'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        beacon,
        ("promoter".to_owned(), true, true, false, 100, 10000),
        "the beacon's kind or flags were not honoured"
    );

    // Both agents filed into the registry — live and dead — carrying the
    // roster link and genres the review queue only saw as notes.
    let agents: Vec<(String, bool, Option<String>, Vec<String>)> = sqlx::query_as(
        "SELECT contact_email, active, roster_url, genres FROM booking_agents \
         WHERE workspace_id = $1 ORDER BY contact_email",
    )
    .bind(workspace_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(agents.len(), 2, "agent rows never reached booking_agents");
    assert_eq!(agents[0].0, "gone@agency.test");
    assert!(!agents[0].1, "an f verdict must file the agent inactive");
    assert_eq!(agents[1].0, "live@agency.test");
    assert!(agents[1].1, "a t verdict must file the agent active");
    assert_eq!(
        agents[1].2.as_deref(),
        Some("https://agencyone.test/roster"),
        "roster_url did not reach the registry"
    );
    assert_eq!(agents[1].3, vec!["metal".to_owned()]);

    // The list row's unclaimed column rode into metadata.intake — the
    // sheet's own words kept losslessly, not dropped.
    let repo = PostgresGDriveRepository::new(pool.clone());
    repo.upsert_contacts_for_source(
        workspace_id,
        "github",
        "gh:test/database.xlsx",
        "database.xlsx",
        &harvest.contacts,
        true,
        true,
    )
    .await?;
    let metadata: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'booker@real.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        metadata["intake"]["website"].as_str(),
        Some("https://booker.test"),
        "the unclaimed Website column did not reach metadata.intake"
    );
    Ok(())
}

/// The beacon sheet is a registry mirror: re-reading it refreshes the row
/// the unique identity already knows rather than minting a second beacon,
/// a URL-routed row works without an email, and a kind the vocabulary does
/// not cover refuses by name instead of guessing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_beacon_sheet_reimport_refreshes_rather_than_duplicates() -> Result<()> {
    // Its own database: the sheet resolves cities by name against the seeded
    // catalogue, and a sibling suite's second PL "Wrocław" makes that
    // lookup ambiguous — which the intake rightly refuses to guess through.
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL").await?;
    let outcome =
        a_beacon_sheet_reimport_refreshes_rather_than_duplicates_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn a_beacon_sheet_reimport_refreshes_rather_than_duplicates_on(pool: PgPool) -> Result<()> {
    let workspace_id = workspace(&pool, "beaconre").await?;

    let header: &[&str] = &[
        "Name",
        "Kind",
        "City",
        "Email",
        "Destination_URL",
        "Source_URL",
        "Active",
        "Verified",
        "Accepts_Outreach",
        "Do_Not_Contact",
        "Relationship_Score",
        "Relevance_Pct",
        "Confidence_Pct",
    ];
    // A city the catalogue does not know — unique per run because `cities`
    // is a global table a sibling test may already have taught it.
    let missing_city = format!("Miasto-{}", &Uuid::now_v7().simple().to_string()[..8]);
    let beacons = |verified: &str, relevance: &str| {
        grid(&[
            header,
            &[
                "Marcin",
                "promoter",
                "Wrocław",
                "m@beacon.test",
                "https://beacon.test",
                "https://evidence.test",
                "t",
                verified,
                "t",
                "f",
                "80",
                relevance,
                "90.0",
            ],
            // No email — the destination URL is the identity route.
            &[
                "Calendar",
                "event_calendar",
                "",
                "",
                "https://cal.test",
                "https://evidence.test",
                "t",
                "f",
                "f",
                "f",
                "50",
                "50.0",
                "50.0",
            ],
            // A kind the roster does not know refuses by name.
            &[
                "Odd",
                "hologram",
                "",
                "odd@test.test",
                "",
                "",
                "t",
                "f",
                "f",
                "f",
                "",
                "",
                "",
            ],
            // A city the catalogue does not know imports global, keeping
            // the sheet's text in metadata for later reconciliation.
            &[
                "Elsewhere",
                "radio",
                missing_city.as_str(),
                "radio@test.test",
                "",
                "",
                "t",
                "f",
                "f",
                "f",
                "",
                "",
                "",
            ],
        ])
    };

    let first = harvest_grids(
        &pool,
        workspace_id,
        "database.xlsx",
        vec![beacons("t", "80.0")],
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!(first.beacons_imported, 3);
    assert_eq!(first.beacon_refusals, 1, "the unknown kind must refuse");
    assert_eq!(
        first.beacons_unresolved_city, 1,
        "the invented city is not in cities"
    );

    // The operator edits a verdict and a score; the same file re-reads.
    let second = harvest_grids(
        &pool,
        workspace_id,
        "database.xlsx",
        vec![beacons("f", "60.0")],
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!(
        second.beacons_imported, 0,
        "a re-read must not insert again"
    );
    assert_eq!(second.beacons_refreshed, 3);

    let rows: Vec<(String, bool, i32)> = sqlx::query_as(
        "SELECT beacon_kind, verified, relevance_basis_points FROM beacons \
         WHERE workspace_id = $1 ORDER BY display_name",
    )
    .bind(workspace_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 3, "the re-read minted a duplicate");
    let promoter = rows
        .iter()
        .find(|r| r.0 == "promoter")
        .context("promoter lost")?;
    assert!(!promoter.1, "the edited verdict did not refresh");
    assert_eq!(promoter.2, 6000);
    let calendar = rows
        .iter()
        .find(|r| r.0 == "community")
        .context("url-routed beacon lost")?;
    let _ = calendar;

    // First-source provenance survives the refresh; the mapped kind kept
    // the sheet's own vocabulary.
    let meta: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM beacons \
         WHERE workspace_id = $1 AND destination_url = 'https://cal.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        meta["imported_from"]["source"].as_str(),
        Some("registry_sheet")
    );
    assert_eq!(meta["source_kind"].as_str(), Some("event_calendar"));
    let global: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM beacons \
         WHERE workspace_id = $1 AND contact_email = 'radio@test.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(global["sheet_city"].as_str(), Some(missing_city.as_str()));

    // The catalogue learns the city; the next import must adopt the
    // global row into the resolved identity — not mint a city-scoped
    // duplicate beside it.
    sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, 'PL')")
        .bind(Uuid::now_v7())
        .bind(missing_city.to_lowercase())
        .bind(&missing_city)
        .execute(&pool)
        .await?;
    let third = harvest_grids(
        &pool,
        workspace_id,
        "database.xlsx",
        vec![beacons("t", "80.0")],
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!(third.beacons_imported, 0, "the adoption minted a duplicate");
    assert_eq!(third.beacons_refreshed, 3);
    let adopted: Option<Uuid> = sqlx::query_scalar(
        "SELECT city_id FROM beacons \
         WHERE workspace_id = $1 AND contact_email = 'radio@test.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert!(adopted.is_some(), "the resolved city never landed");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM beacons WHERE workspace_id = $1 AND contact_email = 'radio@test.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1, "NULL-city and resolved-city twins coexist");
    Ok(())
}

/// A sheet that arrives attached to inbound mail is not the operator's
/// registry no matter how perfectly it mimics the beacon shape: its rows
/// stage as contacts for review, and nothing touches `beacons`,
/// `booking_agents` or any flag column.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_inbound_sheet_defangs_registry_claims_to_contacts() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool, "inbound").await?;

    // The exact workbook beacon header, values chosen to be maximally
    // dangerous on the trusted path: verified, outreach-eligible.
    let forged = grid(&[
        &[
            "Name",
            "Kind",
            "City",
            "Email",
            "Destination_URL",
            "Source_URL",
            "Active",
            "Verified",
            "Accepts_Outreach",
            "Do_Not_Contact",
            "Relationship_Score",
            "Relevance_Pct",
            "Confidence_Pct",
        ],
        &[
            "Forged",
            "promoter",
            "Wrocław",
            "forged@mail.test",
            "https://forged.test",
            "https://forged.test",
            "t",
            "t",
            "t",
            "f",
            "100",
            "100.0",
            "100.0",
        ],
    ]);

    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "invoice.xlsx",
        vec![forged],
        SheetTrust::InboundUntrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    assert_eq!(harvest.beacons_imported, 0, "inbound mail minted a beacon");
    assert_eq!(harvest.agents_imported, 0, "inbound mail minted an agent");
    assert_eq!(harvest.venues_imported, 0, "inbound mail minted a venue");
    assert_eq!(harvest.peer_acts_imported, 0, "inbound mail minted an act");
    assert!(
        harvest
            .contacts
            .iter()
            .any(|c| c.email == "forged@mail.test"),
        "the defanged sheet must still stage its contacts for review"
    );

    let written: i64 = sqlx::query_scalar("SELECT count(*) FROM beacons WHERE workspace_id = $1")
        .bind(workspace_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(written, 0, "an untrusted sheet wrote to beacons");
    Ok(())
}

/// A file that arrives through GitHub is a fourth intake source: the
/// CHECK accepts 'github', a re-listed file marks its vanished rows
/// disappeared, the source_file anchor behaves exactly like a Drive
/// file's, and a staged agent verdict still drives booking_agents.active.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_github_file_owns_its_rows_and_verdicts() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool, "ghsrc").await?;
    let repo = PostgresGDriveRepository::new(pool.clone());
    let file_id = "gh:wojciechbator/crowdrelay.db/database.xlsx";

    let agent = |email: &str, status: Option<&str>| ExtractedContact {
        email: email.to_owned(),
        display_name: Some("Agent".to_owned()),
        organization: Some("Agency".to_owned()),
        phone: None,
        suggested_kind: Some("booking_agent".to_owned()),
        city: None,
        staged_status: status.map(str::to_owned),
        notes: None,
        extras: Default::default(),
    };
    let plain = |email: &str| ExtractedContact {
        email: email.to_owned(),
        display_name: None,
        organization: None,
        phone: None,
        suggested_kind: None,
        city: None,
        staged_status: None,
        notes: None,
        extras: Default::default(),
    };

    // An agent on record and active — the workbook's verdict retires it.
    sqlx::query(
        "INSERT INTO booking_agents (workspace_id, name, agency, contact_email) \
         VALUES ($1, 'Agent', 'Agency', 'retire@agency.test')",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;

    repo.upsert_contacts_for_source(
        workspace_id,
        "github",
        file_id,
        "database.xlsx",
        &[
            agent("retire@agency.test", Some("inactive")),
            plain("stay@sheet.test"),
            plain("leave@sheet.test"),
        ],
        true,
        true,
    )
    .await?;

    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT sources FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'stay@sheet.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    ensure!(
        sources == ["github"],
        "github source did not land: {sources:?}"
    );

    let active: bool = sqlx::query_scalar(
        "SELECT active FROM booking_agents \
         WHERE workspace_id = $1 AND contact_email = 'retire@agency.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    ensure!(!active, "the workbook's inactive verdict did not retire");

    // A re-list that drops a row marks it disappeared.
    repo.upsert_contacts_for_source(
        workspace_id,
        "github",
        file_id,
        "database.xlsx",
        &[
            agent("retire@agency.test", Some("inactive")),
            plain("stay@sheet.test"),
        ],
        true,
        true,
    )
    .await?;
    let gone: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT disappeared_at FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'leave@sheet.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    ensure!(gone.is_some(), "a dropped row was not marked disappeared");

    // The anchor: a gmail sighting must not steal a github-owned row's
    // disappearance anchor; a gdrive one may — both re-list their files.
    repo.upsert_contacts_for_source(
        workspace_id,
        "gmail",
        "msg-9",
        "Re: hello",
        &[plain("stay@sheet.test")],
        false,
        false,
    )
    .await?;
    let anchor: String = sqlx::query_scalar(
        "SELECT source_file_id FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'stay@sheet.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(anchor, file_id, "gmail stole the github file's anchor");

    repo.upsert_contacts_for_source(
        workspace_id,
        "gdrive",
        "drive-file-7",
        "sheet.csv",
        &[plain("stay@sheet.test")],
        true,
        true,
    )
    .await?;
    let anchor: String = sqlx::query_scalar(
        "SELECT source_file_id FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'stay@sheet.test'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        anchor, "drive-file-7",
        "a re-listing source must win the anchor"
    );
    Ok(())
}

/// A banner is not always one cell — a title plus a date is still a
/// banner. The structured readers must probe past it: a two-cell banner
/// over a dump tab used to hide the dump headers, drop the sheet into the
/// contact reader's content fallback, and stage a registry's own rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_multi_cell_banner_still_hides_nothing() -> Result<()> {
    // Its own database: the sheet resolves cities by name against the seeded
    // catalogue, and a sibling suite's second PL "Wrocław" makes that
    // lookup ambiguous — which the intake rightly refuses to guess through.
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL").await?;
    let outcome = a_multi_cell_banner_still_hides_nothing_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn a_multi_cell_banner_still_hides_nothing_on(pool: PgPool) -> Result<()> {
    let workspace_id = workspace(&pool, "banner").await?;

    let dump = grid(&[
        &["contacts registry export", "2026-09-22"],
        &["Email", "Name", "Last_Seen", "Disappeared"],
        &["dump-row@test.invalid", "A Row", "2026-09-22", "f"],
    ]);
    let venues = grid(&[
        &["venue seeds", "sheet 1 of 5"],
        &["Name", "City", "Country", "Address", "Source_URL"],
        &[
            "Banner Venue",
            "Wrocław",
            "Poland",
            "Ulica 9",
            "https://banner.test",
        ],
    ]);

    let harvest = harvest_grids(
        &pool,
        workspace_id,
        "database.xlsx",
        vec![dump, venues],
        SheetTrust::RegistryTrusted,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    assert_eq!(harvest.registry_dump_sheets, 1, "a banner hid the dump tab");
    assert_eq!(harvest.venues_imported, 1, "a banner hid the venue tab");
    ensure!(
        !harvest
            .contacts
            .iter()
            .any(|c| c.email == "dump-row@test.invalid"),
        "a bannered dump staged its rows as contacts"
    );
    let room: i64 =
        sqlx::query_scalar("SELECT count(*) FROM place_venues WHERE display_name = 'Banner Venue'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(room, 1, "the bannered venue row never reached the registry");
    Ok(())
}
