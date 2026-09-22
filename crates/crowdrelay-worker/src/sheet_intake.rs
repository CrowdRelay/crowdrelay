//! Sheet intake shared by every file transport (Drive scan, GitHub
//! registry mirror, operator upload): given a file's worksheets, decide
//! what each sheet IS and route its rows to the right registry.
//!
//! Dispatch order is the whole contract:
//!   agent seed → registry dump → venue seed → band seed → contact list
//! The booking-agent sheet is claimed first because its own header carries
//! registry state columns (`Refused_Until`, `Do_Not_Contact`) that would
//! trip the dump guard; the dump guard runs before the venue reader
//! because a registry readout's `Name`/`City`/`Source_URL` satisfies the
//! venue pin and must never mint its rows as rooms. Only a sheet none of
//! the four claims reaches the contact reader.
//!
//! A workbook may carry a banner above the real header ("venue_seed intake
//! columns — re-importable as-is"). A banner hides the sheet from every
//! reader, so when nothing claims the grid as-is the dispatch retries with
//! row one dropped. Probing is parse-only — extracting a report does no
//! writes — so trying both views is safe, and the retry is strictly
//! fallback: a sheet that already parses keeps its first row.

use crowdrelay_domain::booking_agent_seed::extract_agent_sheet;
use crowdrelay_domain::drive_contacts::{
    ExtractedContact, ExtractionReport as AgentReport, extract_contacts, is_email_header,
    is_registry_dump,
};
use crowdrelay_domain::peer_act_seed::{
    PeerActSeedReport, extract_seed_sheet as extract_band_sheet,
};
use crowdrelay_domain::venue_seed::{SeedSheetReport, extract_seed_sheet as extract_venue_sheet};
use crowdrelay_infra::{
    peer_act_seed::PostgresPeerActSeedRepository, venue_seed::PostgresVenueSeedRepository,
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
pub const SHEET_INTAKE_REVISION: u32 = 3;

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
}

/// Which structured reader claims a view, if one does. Extraction is
/// parse-only — a report carries no writes — so the dispatch may probe a
/// second view before committing. Order is the contract documented above.
enum Claim {
    Agent(AgentReport),
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

/// The view a contact list is read through. Row 1 wins only when it
/// carries the email header and row 0 doesn't: a banner over a real list
/// loses the banner, while a one-column `Email` sheet keeps its header.
fn contacts_view<'a>(full: &'a [Vec<String>], stripped: &'a [Vec<String>]) -> &'a [Vec<String>] {
    let has_email = |view: &[Vec<String>]| {
        view.first()
            .is_some_and(|row| row.iter().any(|cell| is_email_header(cell)))
    };
    if !has_email(full) && has_email(stripped) {
        stripped
    } else {
        full
    }
}

/// Routes every sheet of one file to its reader and returns what to stage.
/// `file_name` labels refusal logs only — writes carry no file identity.
pub async fn harvest_grids(
    pool: &PgPool,
    workspace_id: Uuid,
    file_name: &str,
    sheets: Vec<Vec<Vec<String>>>,
) -> Result<SheetHarvest, String> {
    let mut harvest = SheetHarvest::default();
    for raw in sheets {
        let grid: Vec<Vec<String>> = raw.into_iter().take(MAX_ROWS_PER_FILE + 1).collect();
        let full: &[Vec<String>] = &grid;
        let stripped: &[Vec<String>] = grid.get(1..).unwrap_or(full);

        // Structured readers first, on the grid as written; a banner row
        // pins nothing, so an unclaimed grid retries on the stripped view.
        // `row_shift` re-anchors a refusal's spreadsheet row number after
        // the strip — the extractors report view-relative rows.
        let (view, claimed, row_shift) = match claim(full) {
            Some(claim) => (full, claim, 0usize),
            None => match claim(stripped) {
                Some(claim) => (stripped, claim, 1usize),
                // Contacts is the terminal fallback — it claims on its own
                // terms (an email column in the header, or email-shaped
                // data below whatever header the sheet carries).
                None => {
                    let view = contacts_view(full, stripped);
                    (view, Claim::Contacts, grid.len() - view.len())
                }
            },
        };

        let mut unclaimed = false;
        match claimed {
            Claim::Agent(report) => {
                harvest.agent_sheets += 1;
                harvest.rows_read += report.rows_read;
                harvest.rows_without_email += report.rows_without_email;
                harvest.contacts.extend(report.contacts);
                harvest.saw_email_column = true;
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
                // stage for operator review.
                let report = extract_contacts(view);
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

/// xlsx → one grid per non-empty worksheet via calamine. A workbook whose
/// cover sheet is a dashboard must not hide its data tabs — the seed
/// readers run per sheet. (The Drive CSV export of a Google Sheet still
/// carries only the first tab — exporting a chosen tab would need the
/// gid-aware export URL, which is a deliberate non-goal here.)
pub fn parse_xlsx_sheets(bytes: &[u8]) -> Result<Vec<Vec<Vec<String>>>, String> {
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
                        Data::DateTime(dt) => dt.to_string(),
                        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
                        Data::Error(_) | Data::Empty => String::new(),
                    })
                    .collect()
            })
            .collect();
        sheets.push(grid);
    }
    Ok(sheets)
}
