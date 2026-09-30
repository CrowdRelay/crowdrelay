//! Sheet intake shared by every file transport (Drive scan, GitHub
//! registry mirror, operator upload): given a file's worksheets, decide
//! what each sheet IS and route its rows to the right registry.
//!
//! Dispatch order is the whole contract:
//!   agent seed → beacon seed → outreach log → festival profile →
//!   opportunity seed → scout meta-targets →
//!   registry dump → venue seed → band seed → contact list
//! The booking-agent sheet is claimed first because its own header carries
//! registry state columns (`Refused_Until`, `Do_Not_Contact`) that would
//! trip the dump guard; the beacon registry sheet is claimed next because
//! its state columns (`Verified`, `Accepts_Outreach`, …) are the same
//! vocabulary the guard rejects on — here they are intake, not a readout;
//! the outreach log is claimed ahead of the dump guard for the same reason
//! (`Status`/`Result`/`Source_System` are the band's send bookkeeping, and
//! its contact column is history, not leads to stage); the festival
//! profile claims on its `Entity_Name`/`Application_Cycle` pair — the
//! `deadline=` cells seed `festival_editions`, which is the only input the
//! festival-window evaluator reads; the scout OPPORTUNITIES grammars
//! (MASTER, SCOUT AUTO, SCOUT_PL, SCOUT/MARCIN_FEED) claim next because
//! the SCOUT_PL shape (`Name` + `City` + `Source_URL`) satisfies the venue
//! pin and would otherwise mint festivals as rooms; the human scout's
//! `META_TARGETS` claim emits the same `BeaconSeedReport` the beacon
//! reader produces and lands in the same roster. The dump guard runs
//! before the venue reader because a registry readout's `Name`/`City`/
//! `Source_URL` satisfies the venue pin and must never mint its rows as
//! rooms. Only a sheet none of the eight claims reaches the contact
//! reader.
//!
//! A workbook may carry a banner above the real header ("venue_seed intake
//! columns — re-importable as-is"), and the scout workbooks stack up to
//! three: a title row, a rule note and a blank row before the header
//! (`SCOUT AUTO` OPPORTUNITIES heads at row 4). A banner hides the sheet
//! from every reader, so when nothing claims the grid as-is the dispatch
//! retries with leading rows dropped, bounded at four. Probing is
//! parse-only — extracting a report does no writes — so trying successive
//! views is safe, and the retry is strictly fallback: a sheet that already
//! parses keeps its first row.

use crowdrelay_domain::beacon_seed::{BeaconSeedReport, extract_beacon_sheet};
use crowdrelay_domain::booking_agent_seed::{AgentSheetReport, extract_agent_sheet};
use crowdrelay_domain::drive_contacts::{
    ExtractedContact, extract_contacts, is_email_header, is_registry_dump,
};
use crowdrelay_domain::festival_seed::{FestivalSeedReport, extract_festival_sheet};
use crowdrelay_domain::opportunity_seed::{OpportunitySeedReport, extract_opportunity_sheet};
use crowdrelay_domain::outreach_log::{OutreachLogReport, extract_outreach_log};
use crowdrelay_domain::peer_act_seed::{
    PeerActSeedReport, extract_seed_sheet as extract_band_sheet,
};
use crowdrelay_domain::scout_targets::extract_scout_meta_targets;
use crowdrelay_domain::venue_seed::{SeedSheetReport, extract_seed_sheet as extract_venue_sheet};
use crowdrelay_infra::{
    beacon_seed::PostgresBeaconSeedRepository, booking_agents::PostgresBookingAgentRepository,
    festival_seed::PostgresFestivalSeedRepository,
    opportunity_seed::PostgresOpportunitySeedRepository,
    outreach_log::PostgresOutreachLogRepository, peer_act_seed::PostgresPeerActSeedRepository,
    venue_seed::PostgresVenueSeedRepository,
};
use sqlx::PgPool;
use uuid::Uuid;

/// Bound on rows read per file — a contact list beyond that is not a
/// contact list.
pub const MAX_ROWS_PER_FILE: usize = 5000;

/// What a scan *does* with a file lives here — the banner rule, the reader
/// pins, the dump guard — so the revision is defined once and shared by
/// every transport's skip-unchanged marker (`<sha|mtime>#<rev>`). Bump it
/// when the intake rules change or previously-scanned files stay skipped
/// under rules they predate.
pub const SHEET_INTAKE_REVISION: u32 = 8;

/// How many leading rows a banner may occupy before the sheet is left to
/// the contact fallback. `SCOUT AUTO` tabs stack title + rule note + a
/// blank row, so the real header sits at index 3; one more row of slack
/// covers the next banner a sheet will grow.
const MAX_BANNER_ROWS: usize = 4;

/// One worksheet: its tab name when the transport knows it (a CSV has
/// none) and the grid of cell text.
pub struct SheetGrid {
    pub name: Option<String>,
    pub grid: Vec<Vec<String>>,
}

impl From<Vec<Vec<String>>> for SheetGrid {
    /// A nameless sheet — single-grid transports and tests.
    fn from(grid: Vec<Vec<String>>) -> Self {
        Self { name: None, grid }
    }
}

/// What one file's sheets produced: the contacts to stage under the file's
/// own source identity, plus the counts the cycle report folds in.
#[derive(Debug, Default)]
pub struct SheetHarvest {
    /// Contacts from every contact-list sheet and every booking-agent
    /// sheet, merged — the file, not the tab, is the contact source.
    pub contacts: Vec<ExtractedContact>,
    /// At least one sheet yielded staged rows — or was a contact/agent
    /// sheet that claimed to. Mirrors the Drive reader's semantics: the
    /// flag decides whether the file reads as "not a contact list".
    pub saw_email_column: bool,
    pub rows_read: usize,
    pub rows_without_email: usize,
    pub venues_imported: u64,
    pub venue_refusals: usize,
    pub venues_unknown_city: u64,
    /// Venue rows whose write failed — isolated per row so one bad cell
    /// does not abort the sheet.
    pub venues_failed: u64,
    pub peer_acts_imported: u64,
    pub peer_act_refusals: usize,
    pub peer_acts_unresolved_city: u64,
    pub peer_acts_deactivated: u64,
    pub peer_acts_skipped_inactive: u64,
    pub peer_acts_failed: u64,
    /// Sheets a registry readout claimed — counted so the cycle report is
    /// honest that the file was seen but was not intake.
    pub registry_dump_sheets: usize,
    /// Sheets carrying a booking-agent seed that staged contacts.
    pub agent_sheets: usize,
    /// Agent rows the seed upsert filed into `booking_agents` itself —
    /// split new/refreshed; the staging contacts remain the review
    /// path's view of the same rows.
    pub agents_imported: u64,
    pub agents_refreshed: u64,
    /// Agent rows whose own upsert failed — isolated per row.
    pub agents_failed: u64,
    /// Beacon rows newly inserted into `beacons`.
    pub beacons_imported: u64,
    /// Beacon rows the unique identity already knew — refreshed in place.
    pub beacons_refreshed: u64,
    /// Beacon rows the parser refused (no name, no route, unmapped kind).
    pub beacon_refusals: usize,
    /// Beacon rows whose city text matched no unambiguous `cities` entry —
    /// imported global, raw text preserved in metadata.
    pub beacons_unresolved_city: u64,
    /// Beacon rows whose own write failed — isolated per row.
    pub beacons_failed: u64,
    /// Sheets carrying an outreach log (`VIRYA_MASTER`/`PROMO` OUTREACH
    /// tabs or the human SCOUT `OUTREACH_LOG`) that the dedicated reader claimed.
    pub outreach_log_sheets: usize,
    /// Send rows newly written to `outreach_interactions`.
    pub outreach_sends_recorded: u64,
    /// Reply rows newly written.
    pub outreach_replies_recorded: u64,
    /// Log rows already present under their `{book}:{id}` key.
    pub outreach_already_present: u64,
    /// Log rows the sheet marks as never sent (drafts).
    pub outreach_drafts_skipped: u64,
    /// Log rows whose contact matched no `outreach_targets` email.
    pub outreach_unmatched: u64,
    /// Replies whose verdict settled nothing — minted into triage.
    pub outreach_triage_minted: u64,
    /// Log rows whose own write failed — isolated per row.
    pub outreach_failed: u64,
    /// Sheets carrying the master's `FESTIVAL_PROFILE` tab.
    pub festival_sheets: usize,
    /// Edition windows newly written to `festival_editions`.
    pub festival_editions_seeded: u64,
    /// Editions whose `(target, label)` already existed — refreshed.
    pub festival_editions_refreshed: u64,
    /// Festival rows skipped by the parser (terminal status, no deadline).
    pub festival_rows_skipped: usize,
    /// Festival rows naming no `festival` booking target.
    pub festival_unmatched: usize,
    /// Festival rows whose own write failed — isolated per row.
    pub festival_failed: u64,
    /// Sheets carrying a scout `OPPORTUNITIES` tab (any dialect).
    pub opportunity_sheets: usize,
    /// Opportunity rows newly inserted into `team_opportunities`.
    pub opportunities_seeded: u64,
    /// Opportunity rows the `(workspace, scout_sheet, external_key)`
    /// conflict key already knew — refreshed in place.
    pub opportunities_refreshed: u64,
    /// Opportunity rows the parser refused (no title, unmapped kind).
    pub opportunity_refusals: usize,
    /// Opportunity rows whose own write failed — isolated per row.
    pub opportunities_failed: u64,
    /// Sheets carrying the human scout's `META_TARGETS` tab — counted
    /// separately even though the rows land through the beacon import.
    pub scout_meta_sheets: usize,
}

/// Which structured reader claims a view, if one does. Extraction is
/// parse-only — a report carries no writes — so the dispatch may probe a
/// second view before committing. Order is the contract documented above.
enum Claim {
    Agent(AgentSheetReport),
    Beacon(BeaconSeedReport),
    OutreachLog(OutreachLogReport),
    Festival(FestivalSeedReport),
    Opportunity(OpportunitySeedReport),
    /// The human scout's `META_TARGETS` — a beacon registry in a different
    /// header spelling, emitted as the same report so the same import
    /// path and dedupe keys apply.
    ScoutTargets(BeaconSeedReport),
    Dump,
    Venue(SeedSheetReport),
    Band(PeerActSeedReport),
    Contacts,
}

fn claim(view: &[Vec<String>]) -> Option<Claim> {
    if let Some(report) = extract_agent_sheet(view) {
        // The agent registry's own shape — claimed ahead of the dump guard
        // because its read-back columns (`Approached_At`, `Refused_Until`,
        // `Do_Not_Contact`) are exactly the columns the guard rejects on.
        Some(Claim::Agent(report))
    } else if let Some(report) = extract_beacon_sheet(view) {
        // The beacon registry's own shape — claimed ahead of the dump
        // guard for the same reason as the agent sheet: `Verified`,
        // `Accepts_Outreach` and friends are registry-state columns, but
        // on this sheet they are the intake, not an export of it.
        Some(Claim::Beacon(report))
    } else if let Some(report) = extract_outreach_log(view) {
        // The send log ahead of the dump guard too — `Status`/`Result`/
        // `Source_System` are the band's own bookkeeping columns, and the
        // `Recipient`/`Kontakt` email column must not reach the contact
        // reader (the band already wrote to these people; they are
        // history, not leads to stage).
        Some(Claim::OutreachLog(report))
    } else if let Some(report) = extract_festival_sheet(view) {
        // The master's festival tab — `Application_Cycle` deadlines seed
        // `festival_editions` so the deadline-driven ask lane has windows
        // to read. Ahead of the dump guard for the same reason the agent
        // sheet is: its columns are the band's bookkeeping, not a readout.
        Some(Claim::Festival(report))
    } else if let Some(report) = extract_opportunity_sheet(view) {
        // The scout OPPORTUNITIES grammars — ahead of the dump guard AND
        // the venue reader: the SCOUT_PL shape (`Name` + `City` +
        // `Source_URL`) satisfies the venue pin, and opportunities landing
        // as venues is exactly the misrouting the guard exists to stop.
        Some(Claim::Opportunity(report))
    } else if let Some(report) = extract_scout_meta_targets(view) {
        // The human scout's `META_TARGETS` — target rows that belong in
        // the beacon roster, ahead of the guard and the venue reader for
        // the same reason.
        Some(Claim::ScoutTargets(report))
    } else if view.first().is_some_and(|header| is_registry_dump(header)) {
        // A registry readout is context for a human, not intake — without
        // this its `Name`/`City`/`Source_URL` mints press contacts as venues.
        Some(Claim::Dump)
    } else if let Some(report) = extract_venue_sheet(view) {
        Some(Claim::Venue(report))
    } else {
        extract_band_sheet(view).map(Claim::Band)
    }
}

/// The view a contact list is read through. The earliest header row that
/// carries an email column wins — a banner over a real list loses the
/// banner, while a one-column `Email` sheet keeps its header.
fn contacts_view(grid: &[Vec<String>]) -> &[Vec<String>] {
    let has_email = |view: &[Vec<String>]| {
        view.first()
            .is_some_and(|row| row.iter().any(|cell| is_email_header(cell)))
    };
    for skip in 0..=MAX_BANNER_ROWS {
        let Some(view) = grid.get(skip..) else { break };
        if has_email(view) {
            return view;
        }
    }
    grid
}

/// How far a sheet's structure may be trusted. The registry claims —
/// Beacon flags, agent seeds, venue and band rows — are honoured verbatim
/// for transports the operator owns (their Drive folder, their GitHub
/// mirror). A sheet that arrives attached to inbound mail is not the
/// operator's registry no matter how perfectly it is shaped: anyone who
/// can email the mailbox could otherwise mint `verified`+
/// `accepts_outreach` beacons, flip `booking_agents.active`, or mark real
/// venues closed. Untrusted sheets still stage their contact rows for
/// review — that path exists to be the defang.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SheetTrust {
    RegistryTrusted,
    InboundUntrusted,
}

/// Routes every sheet of one file to its reader and returns what to stage.
/// `file_name` labels refusal logs and the `source` provenance written on
/// outreach-log rows.
pub async fn harvest_grids(
    pool: &PgPool,
    workspace_id: Uuid,
    file_name: &str,
    sheets: Vec<SheetGrid>,
    trust: SheetTrust,
) -> Result<SheetHarvest, String> {
    let mut harvest = SheetHarvest::default();
    for sheet in sheets {
        let grid: Vec<Vec<String>> = sheet.grid.into_iter().take(MAX_ROWS_PER_FILE + 1).collect();
        let full: &[Vec<String>] = &grid;

        // Structured readers first, on the grid as written; a banner row
        // pins nothing, so an unclaimed grid retries on successively
        // stripped views until the banner is gone. `row_shift` re-anchors
        // a refusal's spreadsheet row number after the strip — the
        // extractors report view-relative rows.
        let (view, claimed, row_shift) = {
            let mut found = None;
            for skip in 0..=MAX_BANNER_ROWS {
                let Some(view) = grid.get(skip..) else { break };
                if let Some(claim) = claim(view) {
                    found = Some((view, claim, skip));
                    break;
                }
            }
            match found {
                Some(found) => found,
                // Contacts is the terminal fallback — it claims on its own
                // terms (an email column in the header, or email-shaped
                // data below whatever header the sheet carries).
                None => {
                    let view = contacts_view(full);
                    (view, Claim::Contacts, grid.len() - view.len())
                }
            }
        };

        // An untrusted sheet keeps only its contacts: every registry
        // claim defangs to the review path, where a person decides. The
        // outreach log defangs for the strongest reason of all — an
        // inbound attachment that minted "replied: positive" history
        // would bait re-contact with people nobody actually wrote to.
        // The defanged reader must skip the registry-dump guard — a
        // beacon or agent sheet's own headers trip it, and refusing them
        // would turn "defanged into contacts" into "silently dropped".
        let (claimed, defanged) = match (trust, claimed) {
            (SheetTrust::InboundUntrusted, Claim::Agent(_))
            | (SheetTrust::InboundUntrusted, Claim::Beacon(_))
            | (SheetTrust::InboundUntrusted, Claim::OutreachLog(_))
            | (SheetTrust::InboundUntrusted, Claim::Festival(_))
            | (SheetTrust::InboundUntrusted, Claim::Opportunity(_))
            | (SheetTrust::InboundUntrusted, Claim::ScoutTargets(_))
            | (SheetTrust::InboundUntrusted, Claim::Venue(_))
            | (SheetTrust::InboundUntrusted, Claim::Band(_)) => (Claim::Contacts, true),
            (_, claim) => (claim, false),
        };

        let mut unclaimed = false;
        match claimed {
            Claim::Agent(report) => {
                harvest.agent_sheets += 1;
                harvest.rows_read += report.extraction.rows_read;
                harvest.rows_without_email += report.extraction.rows_without_email;
                harvest.contacts.extend(report.extraction.contacts);
                harvest.saw_email_column = true;
                // The staged contacts keep the review/verdict path alive;
                // the structured rows file the registry itself — this is
                // the write the "EMPTY in production" gap was missing.
                // Each row is its own write: one bad cell must not take
                // the sheet's other agents with it.
                let agents = PostgresBookingAgentRepository::new(pool.clone());
                for agent in &report.agents {
                    match agents.upsert_seed(workspace_id, agent, file_name).await {
                        Ok(true) => harvest.agents_imported += 1,
                        Ok(false) => harvest.agents_refreshed += 1,
                        Err(error) => {
                            harvest.agents_failed += 1;
                            tracing::warn!(
                                %error,
                                file = %file_name,
                                agent = %agent.name,
                                "agent seed row failed to write"
                            );
                        }
                    }
                }
            }
            Claim::Beacon(report) => {
                let summary = PostgresBeaconSeedRepository::new(pool.clone())
                    .import_sheet(workspace_id, file_name, &report)
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "beacon seed rows refused"
                    );
                }
                harvest.rows_read += report.beacons.len() + report.refusals.len();
                harvest.beacons_imported += summary.imported;
                harvest.beacons_refreshed += summary.refreshed;
                harvest.beacon_refusals += report.refusals.len();
                harvest.beacons_unresolved_city += summary.unresolved_city;
                harvest.beacons_failed += summary.failed;
            }
            Claim::OutreachLog(report) => {
                harvest.outreach_log_sheets += 1;
                harvest.rows_read += report.rows_read;
                let sheet_name = sheet.name.as_deref().unwrap_or("sheet");
                let source_label = format!("{file_name}#{sheet_name}");
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        sheet = %sheet_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "outreach log rows refused"
                    );
                }
                let summary = PostgresOutreachLogRepository::new(pool.clone())
                    .import_sheet(workspace_id, &source_label, &report)
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                harvest.outreach_sends_recorded += summary.sends_recorded;
                harvest.outreach_replies_recorded += summary.replies_recorded;
                harvest.outreach_already_present += summary.already_present;
                harvest.outreach_drafts_skipped += summary.drafts_skipped;
                harvest.outreach_unmatched += summary.unmatched;
                harvest.outreach_triage_minted += summary.triage_minted;
                harvest.outreach_failed += summary.failed;
            }
            Claim::Festival(report) => {
                harvest.festival_sheets += 1;
                harvest.rows_read += report.rows.len() + report.skipped.len();
                if !report.skipped.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        skips = ?report
                            .skipped
                            .iter()
                            .map(|(row, skip)| (*row + row_shift, skip.message()))
                            .collect::<Vec<_>>(),
                        "festival profile rows skipped"
                    );
                }
                if !report.unknown_statuses.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        statuses = ?report.unknown_statuses,
                        "festival cycle statuses the map does not know"
                    );
                }
                let summary = PostgresFestivalSeedRepository::new(pool.clone())
                    .import_sheet(
                        workspace_id,
                        &format!("{file_name}#{}", sheet.name.as_deref().unwrap_or("sheet")),
                        &report,
                    )
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                harvest.festival_editions_seeded += summary.seeded;
                harvest.festival_editions_refreshed += summary.refreshed;
                harvest.festival_unmatched += summary.unmatched.len();
                if summary.past_deadline > 0 || !summary.unmatched.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        past_deadline = summary.past_deadline,
                        unmatched = ?summary.unmatched,
                        "festival profile rows with no live window or no target"
                    );
                }
                harvest.festival_failed += summary.failed;
            }
            Claim::Opportunity(report) => {
                harvest.opportunity_sheets += 1;
                harvest.rows_read += report.rows.len() + report.refusals.len();
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "opportunity seed rows refused"
                    );
                }
                // A scout row's contact cell is a lead — stage it beside
                // every other file's contacts so the same address dedupes
                // to one identity no matter which sheet listed it.
                if !report.contacts.is_empty() {
                    harvest.contacts.extend(report.contacts.iter().cloned());
                    harvest.saw_email_column = true;
                }
                let summary = PostgresOpportunitySeedRepository::new(pool.clone())
                    .import_sheet(
                        workspace_id,
                        &format!("{file_name}#{}", sheet.name.as_deref().unwrap_or("sheet")),
                        &report.rows,
                    )
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                harvest.opportunities_seeded += summary.seeded;
                harvest.opportunities_refreshed += summary.refreshed;
                harvest.opportunities_failed += summary.failed;
            }
            Claim::ScoutTargets(report) => {
                // Same import path as the beacon registry sheet — one
                // roster, one dedupe key — with its own counter so the
                // cycle report names which file fed it.
                harvest.scout_meta_sheets += 1;
                let summary = PostgresBeaconSeedRepository::new(pool.clone())
                    .import_sheet(workspace_id, file_name, &report)
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "scout meta-target rows refused"
                    );
                }
                harvest.rows_read += report.beacons.len() + report.refusals.len();
                harvest.beacons_imported += summary.imported;
                harvest.beacons_refreshed += summary.refreshed;
                harvest.beacon_refusals += report.refusals.len();
                harvest.beacons_unresolved_city += summary.unresolved_city;
                harvest.beacons_failed += summary.failed;
            }
            Claim::Dump => {
                harvest.registry_dump_sheets += 1;
                harvest.rows_read += view.len().saturating_sub(1);
            }
            Claim::Venue(report) => {
                // A researched venue sheet is not a contact list: a seed row
                // may carry an Email column and must not stage the room as a
                // contact for it. Its rows import into the shared venue
                // registry as attributed facts instead.
                let summary = PostgresVenueSeedRepository::new(pool.clone())
                    .import_sheet(workspace_id, &report)
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "venue seed rows refused"
                    );
                }
                harvest.rows_read += report.venues.len() + report.refusals.len();
                harvest.venues_imported += summary.imported;
                harvest.venue_refusals += report.refusals.len();
                harvest.venues_unknown_city += summary.unknown_city;
                harvest.venues_failed += summary.failed;
            }
            Claim::Band(report) => {
                // A researched band sheet feeds the shared peer-act registry
                // under the same screening rules — a band row's Email column
                // must not stage the band as a press contact.
                let summary = PostgresPeerActSeedRepository::new(pool.clone())
                    .import_sheet(workspace_id, &report)
                    .await
                    .map_err(|e: sqlx::Error| e.to_string())?;
                if !report.refusals.is_empty() {
                    tracing::info!(
                        file = %file_name,
                        refusals = ?report
                            .refusals
                            .iter()
                            .map(|(row, refusal)| (*row + row_shift, refusal.message()))
                            .collect::<Vec<_>>(),
                        "peer act seed rows refused"
                    );
                }
                harvest.rows_read += report.acts.len() + report.refusals.len();
                harvest.peer_acts_imported += summary.imported;
                harvest.peer_act_refusals += report.refusals.len();
                harvest.peer_acts_unresolved_city += summary.unresolved_city;
                harvest.peer_acts_deactivated += summary.deactivated;
                harvest.peer_acts_skipped_inactive += summary.skipped_inactive;
                harvest.peer_acts_failed += summary.failed;
            }
            Claim::Contacts => {
                // Last resort: a genuine contact list — the file's email rows
                // stage for operator review. A defanged registry sheet reads
                // through the unchecked reader: the dump guard exists to stop
                // staging a trusted registry's own readout, not to drop the
                // contacts an untrusted attachment carried.
                let report = if defanged {
                    crowdrelay_domain::drive_contacts::extract_contacts_unchecked(view)
                } else {
                    extract_contacts(view)
                };
                if !report.no_email_column {
                    harvest.rows_read += report.rows_read;
                    harvest.rows_without_email += report.rows_without_email;
                    harvest.contacts.extend(report.contacts);
                    harvest.saw_email_column = true;
                } else {
                    unclaimed = true;
                }
            }
        }

        if unclaimed || grid.is_empty() {
            // Nothing recognised the sheet — its rows still count toward
            // what the file cost to read.
            harvest.rows_read += grid.len().saturating_sub(1);
        }
    }
    Ok(harvest)
}

/// xlsx → one named grid per non-empty worksheet via calamine. A workbook
/// whose cover sheet is a dashboard must not hide its data tabs — the seed
/// readers run per sheet. The tab name travels with the grid because the
/// outreach-log importer writes `{file}#{sheet}` provenance into metadata.
/// (The Drive CSV export of a Google Sheet still carries only the first
/// tab — exporting a chosen tab would need the gid-aware export URL, which
/// is a deliberate non-goal here.)
pub fn parse_xlsx_sheets(bytes: &[u8]) -> Result<Vec<SheetGrid>, String> {
    use calamine::{Data, Reader, Xlsx, open_workbook_from_rs};
    let mut workbook: Xlsx<std::io::Cursor<&[u8]>> =
        open_workbook_from_rs(std::io::Cursor::new(bytes))
            .map_err(|e| format!("xlsx open failed: {e}"))?;
    let names = workbook.sheet_names().to_owned();
    let mut sheets = Vec::new();
    for name in names {
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        if range.is_empty() {
            continue;
        }
        let grid: Vec<Vec<String>> = range
            .rows()
            .map(|row| {
                row.iter()
                    .map(|cell| match cell {
                        Data::String(s) => s.clone(),
                        Data::Float(f) => {
                            if f.fract() == 0.0 {
                                format!("{f:.0}")
                            } else {
                                f.to_string()
                            }
                        }
                        Data::Int(i) => i.to_string(),
                        Data::Bool(b) => b.to_string(),
                        // calamine's `Display` renders the raw serial
                        // ("45943"), which no downstream date parse can
                        // read — every Research_Date silently collapsed to
                        // `now()`. Render the date part as ISO instead so
                        // the sheet's own clock survives to `observed_at`.
                        Data::DateTime(dt) => {
                            let (y, m, d, _, _, _, _) = dt.to_ymd_hms_milli();
                            format!("{y:04}-{m:02}-{d:02}")
                        }
                        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
                        Data::Error(_) | Data::Empty => String::new(),
                    })
                    .collect()
            })
            .collect();
        sheets.push(SheetGrid {
            name: Some(name),
            grid,
        });
    }
    Ok(sheets)
}
