//! Contact extraction from Google Drive tabular files.
//!
//! The Drive connector exports each tabular file as a grid of strings and
//! hands it here. Everything in this module is deterministic: a file with no
//! email column yields zero contacts — that property is the whole noise
//! filter, since the connector scans the whole Drive and no folder boundary
//! exists.
//!
//! Dedup is by normalized email inside the file (last occurrence wins) and
//! by `(workspace_id, normalized_email)` in the staging table downstream.

use crate::values::NormalizedEmail;

/// One extracted contact row, ready for the staging upsert.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractedContact {
    pub email: String,
    pub display_name: Option<String>,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub suggested_kind: Option<String>,
    /// The city the sheet placed this contact in. Free text at this
    /// stage — the booking promote resolves it against the `cities`
    /// catalogue, and a name that matches nothing still travels so the
    /// operator sees what the file said.
    pub city: Option<String>,
    /// The verification sheet's liveness verdict — `active` or `inactive`,
    /// normalised. `None` means the sheet made no claim; an unrecognised
    /// verdict claims nothing either way.
    pub staged_status: Option<String>,
    pub notes: Option<String>,
    /// Every column no role claimed, keyed by its normalised header —
    /// `website`, `social`, `country`, `confidence` and friends carry real
    /// signal a contacts-sheet schema has no field for, so they ride into
    /// `drive_contacts.metadata` rather than being silently dropped.
    /// Empty on header-extracted contacts (mail rows carry no cells).
    pub extras: std::collections::BTreeMap<String, String>,
}

impl ExtractedContact {
    /// Columns no role claimed, folded into the `metadata.intake` envelope
    /// the contact row carries. An empty extras map must produce `{}` — a
    /// literal `{"intake":{}}` would clobber a previous file's extras on
    /// `||` merge.
    pub fn intake_metadata(&self) -> serde_json::Value {
        if self.extras.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "intake": self.extras })
        }
    }
}

/// What `extract_contacts` did, so the sync report is honest about files
/// that produced nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExtractionReport {
    pub contacts: Vec<ExtractedContact>,
    pub rows_read: usize,
    pub rows_without_email: usize,
    /// Header detection failed — no column looked like email. Files that
    /// produce this are simply not contact lists and stay silent.
    pub no_email_column: bool,
}

fn looks_emailish(value: &str) -> bool {
    let value = value.trim();
    value.len() <= 320
        && value
            .bytes()
            .all(|b| b.is_ascii() && !b.is_ascii_whitespace())
        && value.matches('@').count() == 1
        && value.rsplit('.').next().is_some_and(|tld| tld.len() >= 2)
}

/// Exact normalized match wins over substring — "contact" must not alias
/// the name column onto "contact_email", nor "mail" onto "gmail_export".
/// Substring matching stays only as a fallback when no header is exact.
fn header_is(header: &str, patterns: &[&str]) -> bool {
    let normalized = header.trim().to_lowercase().replace([' ', '-'], "_");
    patterns.iter().any(|p| normalized == *p)
}

fn header_contains(header: &str, patterns: &[&str]) -> bool {
    let normalized = header.trim().to_lowercase().replace([' ', '-'], "_");
    patterns.iter().any(|p| normalized.contains(p))
}

/// Token-level contains for the city role: "capacity" and "electricity"
/// both contain the substring "city" while naming nothing of the sort —
/// a venue sheet's Capacity column must never file its 300 as a city.
/// Splitting the normalized header on `_` keeps "venue city" and
/// "home town" matching while a glued word like "hometown" stays out.
fn header_names_city(header: &str, patterns: &[&str]) -> bool {
    let normalized = header.trim().to_lowercase().replace([' ', '-'], "_");
    normalized.split('_').any(|token| patterns.contains(&token))
}

/// Columns that only exist on rows exported *from* the staging registries
/// — `last_seen`, `disappeared`, `refused_until` and friends name the
/// database's own bookkeeping, so a sheet carrying them is a readout kept
/// for context, never an intake list. The list deliberately excludes
/// headers intake sheets legitimately carry (`Source_URL`, `Status`,
/// `Research_Date`, outreach angles) so a seed sheet can never trip it.
const REGISTRY_STATE_HEADERS: &[&str] = &[
    "last_seen",
    "disappeared",
    "disappeared_at",
    "approached_at",
    "refused_until",
    "do_not_contact",
    "relationship_score",
    "verified",
    "accepts_outreach",
    "relevance_pct",
    "confidence_pct",
    "roster_url",
];

/// Whether a header row is a registry readout rather than an intake list.
/// Two registry-state columns are required: a real export always carries
/// several, while a hand-kept outreach list may legitimately name one
/// `do_not_contact` column and must still import.
#[must_use]
pub fn is_registry_dump(header: &[String]) -> bool {
    header
        .iter()
        .filter(|cell| header_is(cell, REGISTRY_STATE_HEADERS))
        .count()
        >= 2
}

/// Whether one cell names the email column — the dispatcher asks this to
/// tell a lone `Email` header (a real one-column list) from a lone banner
/// note (a title row to strip).
#[must_use]
pub fn is_email_header(cell: &str) -> bool {
    header_is(cell, EMAIL_HEADERS)
}

const EMAIL_HEADERS: &[&str] = &["email", "e_mail", "mail"];
const NAME_HEADERS: &[&str] = &["name", "full_name", "contact", "display_name"];
const ORG_HEADERS: &[&str] = &[
    "org",
    "organization",
    "outlet",
    "station",
    "company",
    "publication",
    "blog",
    "venue",
    "channel",
    "show",
    "festival",
];
const TYPE_HEADERS: &[&str] = &["type", "kind", "role", "category"];
/// The verification verdict a researched sheet carries. Exact match only —
/// a contains-fallback would file a "delivery_status" column as a liveness
/// claim on somebody's newsletter export.
const STATUS_HEADERS: &[&str] = &["status", "stan"];
const PHONE_HEADERS: &[&str] = &["phone", "tel", "mobile"];
const NOTES_HEADERS: &[&str] = &["note", "notes", "comment", "comments"];
// "location" rides the contains-fallback: a column literally named
// "location" is a city to a contacts sheet, while "venue location" is
// claimed by the org lookup first and excluded below before city reads.
const CITY_HEADERS: &[&str] = &[
    "city",
    "town",
    "location",
    "miasto",
    "miejscowość",
    "miejscowosc",
];

/// Maps a Status cell onto the staging vocabulary — the same verdicts the
/// venue and band sheets carry. Unknown values claim nothing: "pending"
/// and "unclear" are not findings.
pub(crate) fn status_for(raw: &str) -> Option<&'static str> {
    let v = raw.trim().to_ascii_lowercase();
    let v = v.replace([' ', '-'], "_");
    match v.as_str() {
        "active" | "operating" | "live" | "aktywny" => Some("active"),
        "inactive" | "dead" | "closed" | "gone" | "left" | "retired" | "nieaktywny"
        | "zawieszony" => Some("inactive"),
        _ => None,
    }
}

/// Maps a free-form type/role value onto the staging vocabulary. Unknown
/// values yield None — the operator decides on promote rather than the
/// extractor guessing.
fn kind_for(raw: &str) -> Option<&'static str> {
    let v = raw.trim().to_ascii_lowercase();
    let v = v.replace([' ', '-'], "_");
    match v.as_str() {
        "fan" | "subscriber" | "mailing_list" | "listener" | "follower" => Some("fan"),
        "press" | "journalist" | "media" | "local_media" | "editor" | "writer" | "blog"
        | "blogger" => Some("press"),
        "radio" | "radio_show" | "radio_station" | "independent_radio" | "dj" | "airplay" => {
            Some("radio")
        }
        "playlist" | "curator" | "playlist_curator" | "dsp" => Some("playlist"),
        "media_patronage" | "patronage" | "patron" | "sponsor" | "partner" => {
            Some("media_patronage")
        }
        "endorsement" | "endorser" => Some("endorsement"),
        "creator" | "influencer" | "youtuber" | "streamer" | "tiktok" => Some("creator"),
        // The event organiser — a festival programme, a band contest, a
        // cultural centre — pitched to *play*, not to review. Before this
        // kind existed the promote fell back to "press" and organisers were
        // sent a review request for the album.
        "organiser" | "organizer" | "organizator" | "organizatorzy" | "event_organiser"
        | "event_organizer" | "festival_organiser" | "festival_organizer" | "contest"
        | "competition" | "band_contest" | "konkurs" => Some("organiser"),
        // Booking supply, not outreach: the kind names which queue the
        // promote lands in (booking_candidates), never the press
        // outreach vocabulary.
        // "club" stays unmapped on purpose: a fan club typed in a role
        // column must not cross into booking supply.
        "promoter" | "booking" | "booker" => Some("promoter"),
        // §12-5 entity 4: an agent represents the band — the opposite
        // direction from a promoter, who books one room for one night.
        // "agent" was left unmapped while the only landing zones were
        // promoter (wrong direction) or press; booking_agents is the
        // home that makes the mapping safe.
        "booking_agent" | "talent_buyer" | "agent" | "booking_agency" | "agency" => {
            Some("booking_agent")
        }
        "venue" | "room" | "hall" | "live_venue" | "concert_venue" | "music_venue" => Some("venue"),
        "festival" | "fest" | "festiwal" => Some("festival"),
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

/// The staging table caps every field (display_name 200, organization 200,
/// phone 40, notes 2000, source_file_name 500). Truncating here keeps one
/// overlong cell from failing the CHECK and poisoning the whole file's
/// upsert — a tail of a name is worth more than no contact at all.
fn capped(value: Option<String>, limit: usize) -> Option<String> {
    value.map(|v| v.chars().take(limit).collect())
}

/// Picks the column index for a role: explicit header match wins; the email
/// column falls back to the column with the most email-shaped values when
/// at least half its non-empty values look like addresses.
fn find_column(
    headers: &[String],
    rows: &[Vec<String>],
    patterns: &[&str],
    contains: fn(&str, &[&str]) -> bool,
    email_fallback: bool,
    exclude: &[usize],
) -> Option<usize> {
    if let Some(index) = headers
        .iter()
        .enumerate()
        .position(|(i, h)| !exclude.contains(&i) && header_is(h, patterns))
        .or_else(|| {
            headers
                .iter()
                .enumerate()
                .position(|(i, h)| !exclude.contains(&i) && contains(h, patterns))
        })
    {
        return Some(index);
    }
    if !email_fallback || rows.is_empty() {
        return None;
    }
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    (0..columns)
        .filter(|column| {
            let mut non_empty = 0usize;
            let mut emailish = 0usize;
            for row in rows.iter().take(200) {
                if let Some(value) = row.get(*column).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                    non_empty += 1;
                    if looks_emailish(value) {
                        emailish += 1;
                    }
                }
            }
            non_empty > 0 && emailish * 2 >= non_empty
        })
        .max_by_key(|column| {
            rows.iter()
                .take(200)
                .filter(|row| row.get(*column).is_some_and(|v| looks_emailish(v)))
                .count()
        })
}

/// Extracts contacts from one grid of cells. `grid[0]` is treated as the
/// header row; a grid with no usable header row yields `no_email_column`.
pub fn extract_contacts(grid: &[Vec<String>]) -> ExtractionReport {
    let mut report = ExtractionReport::default();
    let Some((headers, rows)) = grid.split_first() else {
        report.no_email_column = true;
        return report;
    };
    // A registry readout reached the contact reader — dispatch normally
    // turns it away first, but the guard is intrinsic so no path (upload,
    // a future transport) can stage the database's own rows back.
    if is_registry_dump(headers) {
        report.no_email_column = true;
        report.rows_read = rows.len();
        return report;
    }
    extract_contacts_unchecked(grid)
}

/// `extract_contacts` without the registry-dump guard. The one caller that
/// needs it is intake's untrusted path, where a registry-shaped sheet that
/// arrived over inbound mail is deliberately defanged into a contact list —
/// its rows still stage for review even though its registry claims are
/// refused. Trusted transports keep the guard: a registry readout must
/// never stage the database's own rows back as new contacts.
pub fn extract_contacts_unchecked(grid: &[Vec<String>]) -> ExtractionReport {
    let mut report = ExtractionReport::default();
    let Some((headers, rows)) = grid.split_first() else {
        report.no_email_column = true;
        return report;
    };
    let Some(email_col) = find_column(headers, rows, EMAIL_HEADERS, header_contains, true, &[])
    else {
        report.no_email_column = true;
        report.rows_read = rows.len();
        return report;
    };
    // Each lookup excludes every column already claimed — a header like
    // "Venue city" contains patterns for two roles, and serving both from
    // one column files the venue's name as its city.
    let mut claimed: Vec<usize> = vec![email_col];
    let name_col = find_column(
        headers,
        rows,
        NAME_HEADERS,
        header_contains,
        false,
        &claimed,
    );
    claimed.extend(name_col);
    let org_col = find_column(headers, rows, ORG_HEADERS, header_contains, false, &claimed);
    claimed.extend(org_col);
    let type_col = find_column(
        headers,
        rows,
        TYPE_HEADERS,
        header_contains,
        false,
        &claimed,
    );
    claimed.extend(type_col);
    let status_col = find_column(headers, rows, STATUS_HEADERS, header_is, false, &claimed);
    claimed.extend(status_col);
    let phone_col = find_column(
        headers,
        rows,
        PHONE_HEADERS,
        header_contains,
        false,
        &claimed,
    );
    claimed.extend(phone_col);
    let notes_col = find_column(
        headers,
        rows,
        NOTES_HEADERS,
        header_contains,
        false,
        &claimed,
    );
    claimed.extend(notes_col);
    let city_col = find_column(
        headers,
        rows,
        CITY_HEADERS,
        header_names_city,
        false,
        &claimed,
    );
    claimed.extend(city_col);

    // Everything no role claimed, kept by normalised header name — the
    // sheet's own words for fields the staging table does not have.
    let extra_columns: Vec<(usize, String)> = headers
        .iter()
        .enumerate()
        .filter(|(i, _)| !claimed.contains(i))
        .filter_map(|(i, h)| {
            let name = h.trim().to_lowercase().replace([' ', '-'], "_");
            (!name.is_empty()).then_some((i, name))
        })
        .collect();

    let cell = |row: &[String], column: Option<usize>| {
        column.and_then(|c| row.get(c)).and_then(|v| clean(v))
    };

    // Last occurrence of an address wins the row's fields — a duplicated
    // line in one sheet is an edit, not two contacts.
    let mut by_email: std::collections::HashMap<String, ExtractedContact> =
        std::collections::HashMap::new();
    for row in rows {
        report.rows_read += 1;
        let Some(raw_email) = row
            .get(email_col)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        else {
            report.rows_without_email += 1;
            continue;
        };
        let Ok(email) = NormalizedEmail::parse(raw_email) else {
            report.rows_without_email += 1;
            continue;
        };
        let extras: std::collections::BTreeMap<String, String> = extra_columns
            .iter()
            .filter_map(|(i, name)| {
                let value = row.get(*i).map(|v| v.trim()).filter(|v| !v.is_empty())?;
                Some((name.clone(), value.chars().take(500).collect()))
            })
            .collect();
        by_email.insert(
            email.as_str().to_owned(),
            ExtractedContact {
                email: email.as_str().to_owned(),
                display_name: capped(cell(row, name_col), 200),
                organization: capped(cell(row, org_col), 200),
                phone: capped(cell(row, phone_col), 40),
                suggested_kind: cell(row, type_col).and_then(|v| kind_for(&v).map(str::to_owned)),
                city: capped(cell(row, city_col), 120),
                staged_status: cell(row, status_col)
                    .and_then(|v| status_for(&v).map(str::to_owned)),
                notes: capped(cell(row, notes_col), 2000),
                extras,
            },
        );
    }
    report.contacts = by_email.into_values().collect();
    report
}

/// One address harvested from a Gmail message's From/To/Cc headers.
/// `display_name` comes from the `"Name <addr>"` form when present.
/// No body content is ever read upstream — this module only ever sees
/// header strings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderContact {
    pub email: String,
    pub display_name: Option<String>,
}

/// Local-parts and whole domains that are never a person worth reviewing.
const SKIP_LOCAL: &[&str] = &[
    "noreply",
    "no-reply",
    "no_reply",
    "donotreply",
    "do-not-reply",
    "mailer-daemon",
    "postmaster",
    "bounce",
    "bounces",
];
const SKIP_DOMAINS: &[&str] = &[
    "bounces.google.com",
    "calendar-notification.bounces.google.com",
];

/// Splits an RFC-5322-style address list (`"Jane <jane@x.com>", bob@y.com`)
/// into validated contacts. The tenant's own mailbox (`self_email`) and
/// automated senders are excluded — an operator never promotes the tenant
/// to their own fan list, and noreply addresses are not people.
///
/// Dedup is by normalized email; the first non-empty display name wins.
pub fn extract_header_contacts(header_values: &[String], self_email: &str) -> Vec<HeaderContact> {
    let self_norm = self_email.trim().to_ascii_lowercase();
    let mut seen: std::collections::HashMap<String, HeaderContact> =
        std::collections::HashMap::new();
    for header in header_values {
        for entry in header.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (name, addr) = match entry.rsplit_once('<') {
                Some((name, rest)) => {
                    let addr = rest.trim_end_matches('>').trim();
                    (name.trim().trim_matches('"'), addr)
                }
                None => ("", entry.trim_matches(|c| c == '<' || c == '>')),
            };
            let Ok(email) = NormalizedEmail::parse(addr) else {
                continue;
            };
            let email = email.as_str();
            if email == self_norm {
                continue;
            }
            let (local, domain) = email.rsplit_once('@').unwrap_or(("", ""));
            let local_base = local.split('+').next().unwrap_or(local);
            if SKIP_LOCAL.contains(&local_base) || SKIP_DOMAINS.contains(&domain) {
                continue;
            }
            let name = capped(clean(name), 200);
            seen.entry(email.to_owned())
                .and_modify(|c| {
                    if c.display_name.is_none() {
                        c.display_name = name.clone();
                    }
                })
                .or_insert(HeaderContact {
                    email: email.to_owned(),
                    display_name: name,
                });
        }
    }
    seen.into_values().collect()
}

/// Delimited text → the grid `extract_contacts` reads. A header row is
/// required by the extractor downstream; blank trailing rows are dropped
/// here. Lives beside the extractor because both sides of the upload — the
/// Drive connector and the operator's file — parse through the same path.
pub fn parse_delimited(bytes: &[u8], delimiter: u8) -> Result<Vec<Vec<String>>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut grid: Vec<Vec<String>> = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| format!("delimited parse failed: {e}"))?;
        grid.push(record.iter().map(str::to_owned).collect());
    }
    while grid
        .last()
        .is_some_and(|row| row.iter().all(|c| c.trim().is_empty()))
    {
        grid.pop();
    }
    Ok(grid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn extracts_by_header_names() {
        let report = extract_contacts(&grid(&[
            &["Name", "Email", "Outlet", "Type", "City"],
            &[
                "Jane Doe",
                "Jane@Example.com",
                "Radio Z",
                "press",
                "Wrocław",
            ],
        ]));
        assert_eq!(report.contacts.len(), 1);
        let contact = &report.contacts[0];
        assert_eq!(contact.email, "jane@example.com");
        assert_eq!(contact.display_name.as_deref(), Some("Jane Doe"));
        assert_eq!(contact.organization.as_deref(), Some("Radio Z"));
        assert_eq!(contact.suggested_kind.as_deref(), Some("press"));
        assert_eq!(contact.city.as_deref(), Some("Wrocław"));
    }

    #[test]
    fn a_claimed_column_cannot_serve_two_roles() {
        // "Venue city" contains patterns for both organisation and city —
        // the earlier lookup claims it, and the city reads as absent
        // rather than as the venue's name.
        let report = extract_contacts(&grid(&[
            &["Email", "Venue city"],
            &["bookings@klubx.pl", "Klub X"],
        ]));
        let contact = &report.contacts[0];
        assert_eq!(contact.organization.as_deref(), Some("Klub X"));
        assert_eq!(contact.city, None);
    }

    #[test]
    fn agent_vocabulary_lands_as_booking_agent() {
        // §12-5 entity 4: the agent is its own entity, not a promoter — the
        // intake's three spellings must all file `booking_agent`, which the
        // promote routes to booking_agents. A promoter row stays a
        // promoter, and a fan club still files nothing.
        for (typed, expected) in [
            ("booking_agent", Some("booking_agent")),
            ("Booking Agent", Some("booking_agent")),
            ("talent buyer", Some("booking_agent")),
            ("agent", Some("booking_agent")),
            ("promoter", Some("promoter")),
            ("club", None),
        ] {
            let report = extract_contacts(&grid(&[
                &["Email", "Type"],
                &["route@agency.example", typed],
            ]));
            assert_eq!(
                report.contacts[0].suggested_kind.as_deref(),
                expected,
                "typed {typed:?}"
            );
        }
    }

    #[test]
    fn organiser_vocabulary_files_organiser_not_press() {
        // A sheet cell reading "organizator" used to land as no suggestion,
        // and the promote's blank fallback filed the contact as press — the
        // festival organiser then got an album-review request.
        for (typed, expected) in [
            ("organizator", Some("organiser")),
            ("Organiser", Some("organiser")),
            ("organizer", Some("organiser")),
            ("event organizer", Some("organiser")),
            ("band contest", Some("organiser")),
            ("konkurs", Some("organiser")),
            // Booking directions stay booking — a festival the act plays
            // for is a room to book, an organiser is a programme to join.
            ("festival", Some("festival")),
        ] {
            let report = extract_contacts(&grid(&[
                &["Email", "Type"],
                &["route@agency.example", typed],
            ]));
            assert_eq!(
                report.contacts[0].suggested_kind.as_deref(),
                expected,
                "typed {typed:?}"
            );
        }
    }

    #[test]
    fn polish_city_headers_land() {
        let report = extract_contacts(&grid(&[
            &["Email", "Miasto"],
            &["promoter@agency.pl", "Warszawa"],
        ]));
        assert_eq!(report.contacts[0].city.as_deref(), Some("Warszawa"));
    }

    #[test]
    fn a_status_column_stages_the_liveness_verdict() {
        // The verification sheet's claim travels with the contact — the
        // sync retires a dead agent on it. Only the two verdicts land; an
        // unrecognised word claims nothing, and a contacts sheet with no
        // Status column stages nothing either.
        for (typed, expected) in [
            ("Active", Some("active")),
            ("inactive", Some("inactive")),
            ("left the agency", None),
        ] {
            let report = extract_contacts(&grid(&[
                &["Email", "Type", "Status"],
                &["route@agency.example", "agent", typed],
            ]));
            assert_eq!(
                report.contacts[0].staged_status.as_deref(),
                expected,
                "status {typed:?}"
            );
        }
        let report = extract_contacts(&grid(&[
            &["Email", "Type"],
            &["route@agency.example", "agent"],
        ]));
        assert_eq!(report.contacts[0].staged_status, None);
    }

    #[test]
    fn a_capacity_column_is_not_a_city() {
        // "capacity" and "electricity" contain the substring "city" while
        // naming nothing of the sort — a venue sheet's capacity column
        // must never file its 300 as the contact's city.
        let report = extract_contacts(&grid(&[
            &["Email", "Capacity"],
            &["bookings@klubx.pl", "300"],
        ]));
        assert_eq!(report.contacts[0].city, None);
    }

    #[test]
    fn a_multi_word_city_header_still_matches() {
        let report = extract_contacts(&grid(&[
            &["Email", "Home Town"],
            &["fan@list.pl", "Gdańsk"],
        ]));
        assert_eq!(report.contacts[0].city.as_deref(), Some("Gdańsk"));
    }

    #[test]
    fn a_registry_readout_is_not_a_contact_list() {
        // The workbook's mirror tabs carry the staging table's own
        // bookkeeping columns — `Last_Seen`, `Disappeared`, `Do_Not_Contact`
        // — so their rows must never re-stage no matter how the grid
        // arrives.
        for internals in [
            ["Last_Seen", "Disappeared"],
            ["Do_Not_Contact", "approached_at"],
            ["refused_until", "Verified"],
        ] {
            let report = extract_contacts(&grid(&[
                &["Email", "Name", internals[0], internals[1]],
                &["someone@x.com", "A Row", "2026-09-22", "t"],
            ]));
            assert!(
                report.no_email_column,
                "a sheet carrying {internals:?} staged anyway"
            );
            assert!(report.contacts.is_empty());
        }
        // And an ordinary contact list still reads fine.
        let report = extract_contacts(&grid(&[&["Email", "Name"], &["someone@x.com", "A Row"]]));
        assert!(!report.no_email_column);
        assert_eq!(report.contacts.len(), 1);
    }

    #[test]
    fn email_column_detected_without_header() {
        let report = extract_contacts(&grid(&[
            &["col a", "col b"],
            &["someone@x.com", "note"],
            &["other@y.com", ""],
        ]));
        assert!(!report.no_email_column);
        assert_eq!(report.contacts.len(), 2);
    }

    #[test]
    fn file_without_emails_yields_nothing() {
        let report = extract_contacts(&grid(&[&["Title", "Amount"], &["Merch run", "120"]]));
        assert!(report.no_email_column);
        assert!(report.contacts.is_empty());
    }

    #[test]
    fn duplicate_email_last_wins() {
        let report = extract_contacts(&grid(&[
            &["Email", "Name"],
            &["a@b.com", "Old"],
            &["a@b.com", "New"],
        ]));
        assert_eq!(report.contacts.len(), 1);
        assert_eq!(report.contacts[0].display_name.as_deref(), Some("New"));
    }

    #[test]
    fn invalid_addresses_are_counted_not_admitted() {
        let report = extract_contacts(&grid(&[&["Email"], &["not-an-email"], &["good@ok.com"]]));
        assert_eq!(report.contacts.len(), 1);
        assert_eq!(report.rows_without_email, 1);
    }

    #[test]
    fn headers_yield_named_contacts() {
        let contacts = extract_header_contacts(
            &[
                "\"Jane Doe\" <Jane@Press.com>, bob@radio.fm".to_string(),
                "me <tenant@band.com>, noreply@service.io".to_string(),
            ],
            "Tenant@Band.com",
        );
        assert_eq!(contacts.len(), 2);
        let jane = contacts
            .iter()
            .find(|c| c.email == "jane@press.com")
            .unwrap();
        assert_eq!(jane.display_name.as_deref(), Some("Jane Doe"));
        // Self and noreply are out.
        assert!(!contacts.iter().any(|c| c.email.contains("tenant@")));
        assert!(!contacts.iter().any(|c| c.email.contains("noreply")));
    }

    #[test]
    fn a_single_state_column_does_not_make_a_dump() {
        // A hand-kept outreach list may name a `do_not_contact` column —
        // one registry-state header alone must not refuse the sheet.
        let header: Vec<String> = ["Name", "Email", "Do_Not_Contact"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!is_registry_dump(&header));
        // Two is the export's signature — a contacts registry readout.
        let dump: Vec<String> = ["Email", "Name", "Last_Seen", "Disappeared"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_registry_dump(&dump));
    }

    #[test]
    fn headers_dedup_and_keep_first_name() {
        let contacts = extract_header_contacts(
            &["a@b.com".to_string(), "\"Alice\" <a@b.com>".to_string()],
            "x@y.com",
        );
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn unclaimed_columns_ride_into_extras() {
        // Website, Country and Confidence name no staging field — they
        // land in extras keyed by their normalised header rather than
        // being silently dropped.
        let report = extract_contacts(&grid(&[
            &["Email", "Name", "Website", "Country", "Confidence"],
            &["jane@press.com", "Jane", "https://jane.test", "PL", "High"],
        ]));
        let contact = &report.contacts[0];
        assert_eq!(
            contact.extras.get("website").map(String::as_str),
            Some("https://jane.test")
        );
        assert_eq!(
            contact.extras.get("country").map(String::as_str),
            Some("PL")
        );
        assert_eq!(
            contact.extras.get("confidence").map(String::as_str),
            Some("High")
        );
        // Claimed columns do not double-land.
        assert!(!contact.extras.contains_key("email"));
        assert!(!contact.extras.contains_key("name"));
    }
}
