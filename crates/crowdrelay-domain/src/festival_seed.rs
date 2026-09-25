//! Reading the master workbook's `FESTIVAL_PROFILE` tab into edition windows.
//!
//! `festival_editions` ships with an upsert endpoint and the deadline-driven
//! evaluator (`evaluate_festival_window` — deliberately the one booking lane
//! that does not wait for fan demand, because the slot is the demand), and
//! sat empty in production: thirteen festival targets, zero editions, zero
//! asks. The band already keeps the research — `FESTIVAL_PROFILE` rows carry
//! an `Application_Cycle` cell like `status=READY; deadline=2026-10-10` —
//! so this module is the door that data comes through, the same way
//! `venue_seed` and `booking_agent_seed` already read their registries.
//!
//! # What the sheet can and cannot say
//!
//! The cycle cell is a status plus optional dates, not a calendar. A row
//! with no `deadline=` is a festival with no known window — it is *skipped*,
//! not seeded with a guessed date, because a fabricated close is worse than
//! none: it would drive an ask against a window that may not exist. A row
//! whose status is terminal (`REJECTED`, `CLOSED`, `EXPIRED`, `ARCHIVED`)
//! is likewise skipped — that is the band's own bookkeeping about an
//! edition already resolved, not a window to file.
//!
//! Matching to `booking_targets` happens at write time in infra, by
//! normalized name — this module only parses. A row that names no known
//! target is reported unmatched, never guessed into a new target.

use time::Date;

/// The sheet's discriminating columns — the pair that makes a header this
/// sheet's and not some other tab's. Lookups below index headers by their
/// lowercase text, so these names are the claim only.
mod columns {
    pub const ENTITY_NAME: &str = "Entity_Name";
    pub const APPLICATION_CYCLE: &str = "Application_Cycle";
}

/// What `status=…` in an `Application_Cycle` cell means for seeding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FestivalCycleStatus {
    /// A live pipeline state — the window may still be open. Seeded when a
    /// deadline is attached.
    Open,
    /// The edition is resolved — rejected, closed, expired, archived.
    /// Skipped: seeding its deadline would file a window that is not real.
    Terminal,
    /// A status word the vocabulary does not know — treated as open enough
    /// to keep its deadline, but flagged in the report so a typo reads as a
    /// typo rather than silently counting as live.
    Unknown,
}

/// One parsed `FESTIVAL_PROFILE` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FestivalSeedRow {
    /// `ENT-…` — provenance back to the master's entity registry.
    pub entity_id: String,
    /// The festival's own name — the key infra matches a `festival`
    /// booking target on.
    pub entity_name: String,
    /// The edition's application close date, when the cycle cell carried
    /// one — the only field `festival_editions` cannot run without.
    pub application_closes_on: Option<Date>,
    /// `status=…` as classified — drives skip-vs-seed.
    pub cycle_status: FestivalCycleStatus,
    /// The raw cycle cell, kept so an `Unknown` status still reads back
    /// what the sheet actually said.
    pub cycle_raw: String,
    /// `Typical_Month` verbatim — the evaluator ignores it; provenance for
    /// whoever reconciles the import.
    pub typical_month: Option<String>,
    /// The festival's site — lands on `lineup_url` when it is a URL.
    pub website: Option<String>,
}

/// Why a row did not seed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FestivalSeedSkip {
    /// No `Entity_Name` — nothing to match a target on.
    NoName,
    /// `Application_Cycle` empty or without a `deadline=` — no window is
    /// known, and a guessed one is worse than none.
    NoDeadline,
    /// `status=` is `REJECTED`/`CLOSED`/`EXPIRED`/`ARCHIVED` — resolved.
    TerminalStatus,
    /// The `deadline=` value is not a `YYYY-MM-DD` date.
    BadDeadline,
}

impl FestivalSeedSkip {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoName => "row has no Entity_Name",
            Self::NoDeadline => "Application_Cycle carries no deadline",
            Self::TerminalStatus => "cycle status is terminal",
            Self::BadDeadline => "deadline is not an ISO date",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FestivalSeedReport {
    pub rows: Vec<FestivalSeedRow>,
    /// (1-based sheet row, reason) — the number the spreadsheet shows.
    pub skipped: Vec<(usize, FestivalSeedSkip)>,
    /// Rows carrying a status word the map does not know — they still seed
    /// on their deadline, but the word is reported, not silently trusted.
    pub unknown_statuses: Vec<String>,
}

const TERMINAL_STATUSES: &[&str] = &["rejected", "closed", "expired", "archived"];
const OPEN_STATUSES: &[&str] = &["ready", "waiting", "researching", "discovered"];

/// `status=READY; deadline=2026-10-10; response=2026-11-15` → the status
/// word and the deadline. Cells are `;`-separated `key=value` parts.
fn parse_cycle(raw: &str) -> (FestivalCycleStatus, Option<&str>) {
    let mut status = FestivalCycleStatus::Unknown;
    let mut deadline = None;
    for part in raw.split(';') {
        let part = part.trim();
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "status" => {
                let word = value.trim().to_ascii_lowercase();
                status = if TERMINAL_STATUSES.contains(&word.as_str()) {
                    FestivalCycleStatus::Terminal
                } else if OPEN_STATUSES.contains(&word.as_str()) {
                    FestivalCycleStatus::Open
                } else {
                    FestivalCycleStatus::Unknown
                };
            }
            "deadline" => deadline = Some(value.trim()),
            _ => {}
        }
    }
    (status, deadline)
}

fn is_festival_sheet(header: &[String]) -> bool {
    let has = |needle: &str| {
        header
            .iter()
            .any(|cell| cell.trim().eq_ignore_ascii_case(needle))
    };
    // `Entity_Name` + `Application_Cycle` together are the discriminating
    // pair — a generic contact sheet carries neither, and `Entity_Name`
    // alone also heads master's other profile tabs which carry no cycle.
    has(columns::ENTITY_NAME) && has(columns::APPLICATION_CYCLE)
}

/// Reads one grid as the festival profile, or declines it.
///
/// `None` means the header is not `FESTIVAL_PROFILE`'s — another reader may
/// still claim it. `Some` means the header matched and every non-empty row
/// either parsed or carries a skip reason naming the problem. Pure: no IO,
/// no clock — a deadline is kept verbatim and *past-ness* is decided by the
/// writer, so today's date is never part of parsing.
#[must_use]
pub fn extract_festival_sheet(grid: &[Vec<String>]) -> Option<FestivalSeedReport> {
    let (header, rows) = grid.split_first()?;
    if !is_festival_sheet(header) {
        return None;
    }
    let mut index: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        let key = cell.trim().to_ascii_lowercase();
        if !key.is_empty() {
            index.entry(key).or_insert(i);
        }
    }
    fn cell<'a>(
        index: &std::collections::BTreeMap<String, usize>,
        row: &'a [String],
        name: &str,
    ) -> &'a str {
        index
            .get(name)
            .and_then(|i| row.get(*i))
            .map_or("", String::as_str)
            .trim()
    }
    macro_rules! cell {
        ($row:expr, $name:expr) => {
            cell(&index, $row, $name)
        };
    }

    let mut report = FestivalSeedReport::default();
    for (offset, row) in rows.iter().enumerate() {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let sheet_row = offset + 2;
        let entity_name = cell!(row, "entity_name").to_owned();
        if entity_name.is_empty() {
            report.skipped.push((sheet_row, FestivalSeedSkip::NoName));
            continue;
        }
        let cycle_raw = cell!(row, "application_cycle").to_owned();
        let (status, deadline_raw) = parse_cycle(&cycle_raw);
        if status == FestivalCycleStatus::Terminal {
            report
                .skipped
                .push((sheet_row, FestivalSeedSkip::TerminalStatus));
            continue;
        }
        if status == FestivalCycleStatus::Unknown && !cycle_raw.is_empty() {
            let status_word = cycle_raw
                .split(';')
                .find_map(|part| {
                    part.split_once('=').and_then(|(key, value)| {
                        (key.trim().eq_ignore_ascii_case("status")).then(|| value.trim().to_owned())
                    })
                })
                .unwrap_or_default();
            if !status_word.is_empty() {
                report.unknown_statuses.push(status_word);
            }
        }
        let application_closes_on = match deadline_raw {
            None => {
                report
                    .skipped
                    .push((sheet_row, FestivalSeedSkip::NoDeadline));
                continue;
            }
            Some(raw) => match Date::parse(
                raw,
                &time::macros::format_description!("[year]-[month]-[day]"),
            ) {
                Ok(date) => Some(date),
                Err(_) => {
                    report
                        .skipped
                        .push((sheet_row, FestivalSeedSkip::BadDeadline));
                    continue;
                }
            },
        };
        let website = {
            let raw = cell!(row, "website");
            (raw.starts_with("http://") || raw.starts_with("https://")).then(|| raw.to_owned())
        };
        report.rows.push(FestivalSeedRow {
            entity_id: cell!(row, "entity_id").to_owned(),
            entity_name,
            application_closes_on,
            cycle_status: status,
            cycle_raw,
            typical_month: {
                let month = cell!(row, "typical_month");
                (!month.is_empty()).then(|| month.to_owned())
            },
            website,
        });
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &[&str] = &[
        "Entity_ID",
        "Entity_Name",
        "Festival_Type",
        "Typical_Month",
        "Genres",
        "Territory",
        "Typical_Capacity",
        "Organizer_Entity_ID",
        "Booker_Contact_ID",
        "Application_Cycle",
        "Website",
        "Notes",
    ];

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        let mut grid = vec![HEADER.iter().map(|s| s.to_string()).collect::<Vec<_>>()];
        for row in rows {
            grid.push(row.iter().map(|s| s.to_string()).collect());
        }
        grid
    }

    #[test]
    fn claims_the_festival_profile_header() {
        let report = extract_festival_sheet(&grid(&[])).expect("header claims");
        assert!(report.rows.is_empty());
        // A contact list and a venue sheet do not carry the pair.
        let contacts = vec![
            vec!["Name".to_owned(), "Email".to_owned()],
            vec!["Somebody".to_owned(), "a@b.c".to_owned()],
        ];
        assert!(extract_festival_sheet(&contacts).is_none());
    }

    #[test]
    fn ready_deadline_rows_seed() {
        let report = extract_festival_sheet(&grid(&[&[
            "ENT-000443",
            "Tallinn Music Week",
            "FESTIVAL",
            "April",
            "",
            "",
            "",
            "",
            "",
            "status=READY; deadline=2026-10-26",
            "https://tmw.ee/",
            "",
        ]]))
        .expect("header claims");
        assert_eq!(report.rows.len(), 1);
        assert!(report.skipped.is_empty());
        let row = &report.rows[0];
        assert_eq!(row.entity_id, "ENT-000443");
        assert_eq!(row.entity_name, "Tallinn Music Week");
        assert_eq!(
            row.application_closes_on,
            Some(Date::from_calendar_date(2026, time::Month::October, 26).unwrap())
        );
        assert_eq!(row.cycle_status, FestivalCycleStatus::Open);
        assert_eq!(row.website.as_deref(), Some("https://tmw.ee/"));
    }

    #[test]
    fn terminal_and_undated_rows_skip_with_reasons() {
        let report = extract_festival_sheet(&grid(&[
            &[
                "ENT-1",
                "Rejected Fest",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "status=REJECTED; deadline=2026-08-14",
                "",
                "",
            ],
            &[
                "ENT-2",
                "Waiting Fest",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "status=WAITING",
                "",
                "",
            ],
            &[
                "ENT-3",
                "Bad Date Fest",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "status=READY; deadline=soon",
                "",
                "",
            ],
        ]))
        .expect("header claims");
        assert!(report.rows.is_empty());
        assert_eq!(
            report.skipped,
            vec![
                (2, FestivalSeedSkip::TerminalStatus),
                (3, FestivalSeedSkip::NoDeadline),
                (4, FestivalSeedSkip::BadDeadline),
            ]
        );
    }

    #[test]
    fn unknown_status_words_seed_but_report() {
        let report = extract_festival_sheet(&grid(&[&[
            "ENT-9",
            "Odd Fest",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "status=SHORTHAND; deadline=2027-01-15",
            "",
            "",
        ]]))
        .expect("header claims");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].cycle_status, FestivalCycleStatus::Unknown);
        assert_eq!(report.unknown_statuses, vec!["SHORTHAND".to_owned()]);
    }
}
