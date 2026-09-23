//! Booking-agent seed sheets.
//!
//! The shared registry workbook keeps agents in the `booking_agents`
//! table's own shape — `Agency`, `Roster_URL`, `Active` — not in the
//! contact-list shape the generic extractor reads. This reader maps that
//! shape onto [`ExtractedContact`] rows pinned to
//! `suggested_kind = 'booking_agent'`, so they stage like any other
//! contact and the staged-status verdict sync resolves `active` on the
//! agent registry both ways.
//!
//! Columns that are the agent's or the operator's answer —
//! `Approached_At`, `Refused_Until`, `Do_Not_Contact` — are read-back
//! context only: a sheet never writes them.

use crate::drive_contacts::{ExtractedContact, ExtractionReport, status_for};
use crate::values::NormalizedEmail;

mod columns {
    pub const NAME: &str = "name";
    pub const AGENCY: &str = "agency";
    pub const EMAIL: &str = "email";
    pub const ROSTER_URL: &str = "roster_url";
    pub const GENRES: &str = "genres";
    pub const ACTIVE: &str = "active";
    pub const STATUS: &str = "status";
    pub const CITY: &str = "city";
    pub const PHONE: &str = "phone";
    pub const NOTES: &str = "notes";
    pub const SOURCE_URL: &str = "source_url";
    pub const RESEARCH_DATE: &str = "research_date";
}

/// Whether a header row is the booking-agent sheet this module parses.
/// `Agency` + `Roster_URL` is the signature — no ordinary contact list
/// names a roster column, and the venue/band pins never carry either.
#[must_use]
pub fn is_agent_sheet(header: &[String]) -> bool {
    let has = |name: &str| {
        header
            .iter()
            .any(|cell| canonical_column(cell) == Some(name))
    };
    has(columns::AGENCY) && has(columns::ROSTER_URL)
}

/// The column this header cell names, if it names one. Normalised like the
/// sibling readers — case, spaces and hyphens fold to underscores.
#[must_use]
pub fn canonical_column(cell: &str) -> Option<&'static str> {
    let cell = cell
        .trim()
        .to_lowercase()
        .replace([' ', '-'], "_")
        .replace("__", "_");
    match cell.as_str() {
        "name" | "agent" | "nazwa" => Some(columns::NAME),
        "agency" | "company" | "agency_name" | "firma" | "agencja" => Some(columns::AGENCY),
        "email" | "e_mail" | "mail" | "contact_email" => Some(columns::EMAIL),
        "roster_url" | "roster" | "roster_link" => Some(columns::ROSTER_URL),
        "genres" | "genre" | "gatunek" | "gatunki" => Some(columns::GENRES),
        "active" | "is_active" => Some(columns::ACTIVE),
        "status" | "stan" => Some(columns::STATUS),
        "city" | "miasto" => Some(columns::CITY),
        "phone" | "tel" | "mobile" => Some(columns::PHONE),
        "notes" | "note" | "comment" | "notatki" | "uwagi" => Some(columns::NOTES),
        "source_url" | "source" | "evidence_url" | "zrodlo" => Some(columns::SOURCE_URL),
        "research_date" | "researched_on" | "date" | "data" => Some(columns::RESEARCH_DATE),
        _ => None,
    }
}

/// An `Active` cell is a boolean-ish registry flag — `t`/`f` exports from
/// the database itself, `yes`/`no` or `active`/`inactive` from a hand
/// edit. Unknown and empty claim nothing, like every verdict column.
fn active_flag_for(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "1" | "active" | "aktywny" => Some("active"),
        "f" | "false" | "no" | "0" | "inactive" | "nieaktywny" => Some("inactive"),
        _ => None,
    }
}

fn clean(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn capped(value: Option<String>, limit: usize) -> Option<String> {
    value.map(|v| v.chars().take(limit).collect())
}

/// One agent row in the registry table's own terms — what the direct
/// seed upsert writes. `email` is already normalised and is the upsert key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededAgent {
    pub name: String,
    pub agency: Option<String>,
    pub email: String,
    pub roster_url: Option<String>,
    pub genres: Vec<String>,
    /// The sheet's verdict: true unless a Status/Active cell said inactive —
    /// the same rule the staged contact carries into the verdict sync.
    pub active: bool,
    pub source_url: Option<String>,
    pub research_date: Option<String>,
    pub notes: Option<String>,
}

/// What the agent sheet yielded: contacts for the staging/review path and
/// structured rows for the `booking_agents` registry itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentSheetReport {
    pub extraction: ExtractionReport,
    pub agents: Vec<SeededAgent>,
}

/// Splits a genres cell into the `text[]` the registry stores — comma,
/// slash and semicolon all separate a hand-written list.
fn genre_list(raw: Option<String>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split([',', '/', ';'])
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Reads one whole grid as a booking-agent sheet, or declines it.
///
/// `None` means the header is not the agent sheet's — the caller tries its
/// other readers. `Some` means the header matched; every non-empty row
/// either produced a staged contact or counted as a row without email.
/// Read-only registry columns are ignored by construction — they never
/// reach an [`ExtractedContact`] field.
#[must_use]
pub fn extract_agent_sheet(grid: &[Vec<String>]) -> Option<AgentSheetReport> {
    let (header, rows) = grid.split_first()?;
    if !is_agent_sheet(header) {
        return None;
    }
    let column = |name: &str| {
        header
            .iter()
            .position(|cell| canonical_column(cell) == Some(name))
    };
    let name_col = column(columns::NAME);
    let agency_col = column(columns::AGENCY);
    let email_col = column(columns::EMAIL);
    let roster_col = column(columns::ROSTER_URL);
    let genres_col = column(columns::GENRES);
    let active_col = column(columns::ACTIVE);
    let status_col = column(columns::STATUS);
    let city_col = column(columns::CITY);
    let phone_col = column(columns::PHONE);
    let notes_col = column(columns::NOTES);
    let source_col = column(columns::SOURCE_URL);
    let research_col = column(columns::RESEARCH_DATE);

    let cell = |row: &[String], column: Option<usize>| {
        column.and_then(|c| row.get(c)).and_then(|v| clean(v))
    };

    let mut report = AgentSheetReport::default();
    let mut by_email: std::collections::HashMap<String, ExtractedContact> =
        std::collections::HashMap::new();
    for row in rows {
        if row.iter().all(|v| v.trim().is_empty()) {
            continue;
        }
        report.extraction.rows_read += 1;
        let Some(raw_email) = cell(row, email_col) else {
            report.extraction.rows_without_email += 1;
            continue;
        };
        let Ok(email) = NormalizedEmail::parse(&raw_email) else {
            report.extraction.rows_without_email += 1;
            continue;
        };
        // `booking_agents.contact_email`'s CHECK wants a dotted domain —
        // `a@localhost` parses but cannot store, so it counts as absent
        // the same way an unparseable cell does.
        if !email
            .as_str()
            .rsplit('@')
            .next()
            .is_some_and(|domain| domain.contains('.'))
        {
            report.extraction.rows_without_email += 1;
            continue;
        }
        // `Status` uses the shared verdict vocabulary; `Active` is the
        // registry flag. A sheet carrying both lets Status win — it is the
        // human-edited one.
        let staged_status = cell(row, status_col)
            .and_then(|v| status_for(&v).map(str::to_owned))
            .or_else(|| cell(row, active_col).and_then(|v| active_flag_for(&v).map(str::to_owned)));
        // Roster and genres are context the review queue reads, not fields
        // of their own — they ride in notes alongside any written note.
        let notes = [
            cell(row, notes_col),
            cell(row, genres_col).map(|g| format!("Genres: {g}")),
            cell(row, roster_col).map(|r| format!("Roster: {r}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        // The registry row keeps the fields the review queue only
        // carries as notes — roster, genres, the row's own provenance.
        // Name falls back the way `promote_beacon_agent` does: agency,
        // then the address itself, so the NOT NULL column never blocks.
        let name = cell(row, name_col)
            .or_else(|| cell(row, agency_col))
            .unwrap_or_else(|| email.as_str().to_owned());
        report.agents.push(SeededAgent {
            name: name.chars().take(200).collect(),
            agency: capped(cell(row, agency_col), 200),
            email: email.as_str().to_owned(),
            // `roster_url`'s CHECK admits `^https?://` only — a cell like
            // `agency.com/roster` or `n/a` would violate it and abort the
            // row's upsert, so the sheet's own words stay in `notes` while
            // the column carries only what it can hold.
            roster_url: cell(row, roster_col).filter(|v| {
                let v = v.trim().to_ascii_lowercase();
                v.starts_with("http://") || v.starts_with("https://")
            }),
            genres: genre_list(cell(row, genres_col)),
            active: staged_status.as_deref() != Some("inactive"),
            source_url: cell(row, source_col),
            research_date: cell(row, research_col),
            notes: cell(row, notes_col),
        });
        by_email.insert(
            email.as_str().to_owned(),
            ExtractedContact {
                email: email.as_str().to_owned(),
                display_name: capped(cell(row, name_col), 200),
                organization: capped(cell(row, agency_col), 200),
                phone: capped(cell(row, phone_col), 40),
                suggested_kind: Some("booking_agent".to_owned()),
                city: capped(cell(row, city_col), 120),
                staged_status,
                notes: capped(clean(&notes), 2000),
                extras: Default::default(),
            },
        );
    }
    report.extraction.contacts = by_email.into_values().collect();
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    const HEADER: &[&str] = &[
        "Name",
        "Agency",
        "Email",
        "Roster_URL",
        "Genres",
        "Active",
        "Approached_At",
        "Refused_Until",
        "Do_Not_Contact",
    ];

    #[test]
    fn the_registry_agent_tab_is_claimed() {
        assert!(is_agent_sheet(
            &HEADER.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        ));
        // A contact list and the sibling seed sheets are not agent sheets.
        for header in [
            vec!["Email", "Name", "City"],
            vec!["Name", "City", "Source_URL"],
            vec!["Name", "Agency", "Email"], // no roster column — not the registry shape
        ] {
            assert!(
                !is_agent_sheet(&header.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                "{header:?} claimed as an agent sheet"
            );
        }
    }

    #[test]
    fn rows_stage_as_agents_carrying_the_active_verdict() {
        let report = extract_agent_sheet(&grid(&[
            HEADER,
            &[
                "Agent Live",
                "Agency One",
                "Live@Agency.test",
                "https://agencyone.test/roster",
                "metal, hardcore",
                "t",
                "2026-06-01",
                "",
                "f",
            ],
            &[
                "Agent Gone",
                "Agency Two",
                "gone@agency.test",
                "https://agencytwo.test",
                "",
                "f",
                "",
                "",
                "",
            ],
            // No email — a lead, not a staged contact.
            &["No Address", "Agency Three", "", "", "", "t", "", "", ""],
        ]))
        .expect("the agent header must claim");
        assert_eq!(report.extraction.rows_read, 3);
        assert_eq!(report.extraction.rows_without_email, 1);
        assert_eq!(report.extraction.contacts.len(), 2);

        let live = report
            .extraction
            .contacts
            .iter()
            .find(|c| c.email == "live@agency.test")
            .expect("live agent missing");
        assert_eq!(live.suggested_kind.as_deref(), Some("booking_agent"));
        assert_eq!(live.staged_status.as_deref(), Some("active"));
        assert_eq!(live.organization.as_deref(), Some("Agency One"));
        assert!(
            live.notes
                .as_deref()
                .is_some_and(|n| n.contains("agencyone.test")),
            "roster url should ride in notes: {:?}",
            live.notes
        );

        let gone = report
            .extraction
            .contacts
            .iter()
            .find(|c| c.email == "gone@agency.test")
            .expect("dead agent missing");
        assert_eq!(gone.staged_status.as_deref(), Some("inactive"));
    }

    #[test]
    fn a_status_column_outranks_active() {
        // Status is the hand-edited verdict; Active is the registry flag.
        // A sheet carrying both lets Status speak.
        let report = extract_agent_sheet(&grid(&[
            &["Name", "Agency", "Email", "Roster_URL", "Active", "Status"],
            &[
                "A",
                "Ag",
                "a@agency.test",
                "https://a.test",
                "t",
                "inactive",
            ],
        ]))
        .expect("agent sheet");
        assert_eq!(
            report.extraction.contacts[0].staged_status.as_deref(),
            Some("inactive")
        );
    }

    #[test]
    fn other_sheets_are_declined() {
        assert!(extract_agent_sheet(&grid(&[&["Email", "Name"], &["a@b.test", "A"],])).is_none());
    }

    #[test]
    fn a_schemeless_roster_url_stays_in_notes_only() {
        // `roster_url`'s CHECK admits `^https?://` — a hand-typed
        // `agency.com/roster` must degrade to NULL rather than abort the
        // row's upsert, and the sheet's words still ride in notes.
        let report = extract_agent_sheet(&grid(&[
            HEADER,
            &[
                "A",
                "Ag",
                "a@agency.test",
                "agency.com/roster",
                "",
                "t",
                "",
                "",
                "",
            ],
        ]))
        .expect("agent sheet");
        assert_eq!(report.agents[0].roster_url, None);
        let contact = &report.extraction.contacts[0];
        assert!(
            contact
                .notes
                .as_deref()
                .is_some_and(|n| n.contains("agency.com/roster")),
            "raw roster cell should ride in notes: {:?}",
            contact.notes
        );
    }

    #[test]
    fn a_dotless_domain_email_counts_as_absent() {
        // `booking_agents.contact_email`'s CHECK demands a dotted domain —
        // `a@localhost` parses yet cannot store, so the row reads as a
        // lead without an address, like an unparseable cell.
        let report = extract_agent_sheet(&grid(&[
            HEADER,
            &["A", "Ag", "a@localhost", "", "", "t", "", "", ""],
        ]))
        .expect("agent sheet");
        assert_eq!(report.extraction.rows_without_email, 1);
        assert!(report.agents.is_empty());
        assert!(report.extraction.contacts.is_empty());
    }
}
