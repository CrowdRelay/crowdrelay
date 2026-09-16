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
    pub notes: Option<String>,
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

/// Maps a free-form type/role value onto the staging vocabulary. Unknown
/// values yield None — the operator decides on promote rather than the
/// extractor guessing.
fn kind_for(raw: &str) -> Option<&'static str> {
    let v = raw.trim().to_ascii_lowercase();
    let v = v.replace([' ', '-'], "_");
    match v.as_str() {
        "fan" | "subscriber" | "mailing_list" | "listener" | "follower" => Some("fan"),
        "press" | "journalist" | "media" | "editor" | "writer" | "blog" | "blogger" => {
            Some("press")
        }
        "radio" | "radio_show" | "radio_station" | "dj" | "airplay" => Some("radio"),
        "playlist" | "curator" | "playlist_curator" | "dsp" => Some("playlist"),
        "media_patronage" | "patronage" | "patron" | "sponsor" | "partner" => {
            Some("media_patronage")
        }
        "endorsement" | "endorser" => Some("endorsement"),
        "creator" | "influencer" | "youtuber" | "streamer" | "tiktok" => Some("creator"),
        // Booking supply, not outreach: the kind names which queue the
        // promote lands in (viryaos_booking_candidates), never the press
        // outreach vocabulary.
        // "agent" and "club" stay unmapped on purpose: a press agent or a
        // fan club typed in a role column must not cross into booking supply.
        "promoter" | "booking" | "booker" | "booking_agent" | "talent_buyer" => Some("promoter"),
        "venue" | "room" | "hall" | "live_venue" | "concert_venue" | "music_venue" => Some("venue"),
        "festival" | "fest" | "festival_organizer" | "festival_organiser" => Some("festival"),
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
        by_email.insert(
            email.as_str().to_owned(),
            ExtractedContact {
                email: email.as_str().to_owned(),
                display_name: capped(cell(row, name_col), 200),
                organization: capped(cell(row, org_col), 200),
                phone: capped(cell(row, phone_col), 40),
                suggested_kind: cell(row, type_col).and_then(|v| kind_for(&v).map(str::to_owned)),
                city: capped(cell(row, city_col), 120),
                notes: capped(cell(row, notes_col), 2000),
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
    fn polish_city_headers_land() {
        let report = extract_contacts(&grid(&[
            &["Email", "Miasto"],
            &["promoter@agency.pl", "Warszawa"],
        ]));
        assert_eq!(report.contacts[0].city.as_deref(), Some("Warszawa"));
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
    fn headers_dedup_and_keep_first_name() {
        let contacts = extract_header_contacts(
            &["a@b.com".to_string(), "\"Alice\" <a@b.com>".to_string()],
            "x@y.com",
        );
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].display_name.as_deref(), Some("Alice"));
    }
}
