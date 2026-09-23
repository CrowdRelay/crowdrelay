//! Reading the registry workbook's beacon sheet into the shared roster.
//!
//! The workbook's `Beacons` tab is the operator's own press/media registry —
//! researched local amplifiers (press, radio, creators, promoters, patrons)
//! with the operator's verification verdicts already on the row. It is not
//! machine research: `Verified`, `Accepts_Outreach` and `Do_Not_Contact`
//! here are a person's call, so unlike `beacon_signal`'s research importer —
//! which files every row unverified — this reader carries the flags through.
//!
//! # Why this is not the registry-dump path
//!
//! The sheet's state columns (`Verified`, `Accepts_Outreach`, …) are exactly
//! the headers `is_registry_dump` guards on, because they look like an export
//! of the database's own bookkeeping. They are not: they are *input* for a
//! table that happens to use the same vocabulary. The claim signature —
//! `Kind` plus at least two beacon-state columns — is what tells this sheet
//! apart from a `drive_contacts` readout (which has `Kind` but none of the
//! state columns) and from a venue or band seed (which never name `Kind`).
//!
//! # Identity routes
//!
//! `beacons` dedupes on two partial unique indexes: email when a row has
//! one, destination URL when it does not. A row with neither cannot be told
//! apart from its own re-import, so it is refused rather than written —
//! `MissingRoute` — keeping a daily sync from minting an unindexed copy of
//! the same row every cycle.

use crate::beacons::BeaconKind;
use crate::values::NormalizedEmail;

/// The workbook's column names. Read through `canonical_column`, so the
/// spellings below also match `Destination URL`, `do-not-contact` and the
/// other normalisation-folded forms a hand edit produces.
pub mod columns {
    pub const NAME: &str = "name";
    pub const KIND: &str = "kind";
    pub const CITY: &str = "city";
    pub const EMAIL: &str = "email";
    pub const DESTINATION_URL: &str = "destination_url";
    pub const SOURCE_URL: &str = "source_url";
    pub const ACTIVE: &str = "active";
    pub const VERIFIED: &str = "verified";
    pub const ACCEPTS_OUTREACH: &str = "accepts_outreach";
    pub const DO_NOT_CONTACT: &str = "do_not_contact";
    pub const RELATIONSHIP_SCORE: &str = "relationship_score";
    pub const RELEVANCE_PCT: &str = "relevance_pct";
    pub const CONFIDENCE_PCT: &str = "confidence_pct";

    /// State columns the claim signature counts — a registry readout's
    /// vocabulary, repurposed here as intake. `CONFIDENCE_PCT` is parsed
    /// but deliberately absent: the band sheet's `Confidence` column is
    /// the same word, so it must not help a foreign header trip the pin.
    pub const STATE: &[&str] = &[
        VERIFIED,
        ACCEPTS_OUTREACH,
        DO_NOT_CONTACT,
        RELATIONSHIP_SCORE,
        RELEVANCE_PCT,
    ];
}

/// One roster row, as the sheet asserted it. Flag fields are `Option`:
/// an empty cell asserts nothing, so an update never erases a flag the
/// sheet stopped carrying — while `INSERT` still applies the table's own
/// defaults (`active`, the rest false).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededBeacon {
    pub display_name: String,
    /// The roster kind after vocabulary mapping — the value the CHECK
    /// constraint and every downstream reader know.
    pub kind: BeaconKind,
    /// What the sheet actually wrote, kept for `metadata.source_kind` when
    /// it differs from `kind` — the mapping stays recoverable.
    pub raw_kind: String,
    /// Free text at this stage; the repository resolves it against the
    /// `cities` catalogue, and a name that matches nothing lands as a
    /// global (NULL-city) beacon with the raw text kept in metadata.
    pub city: Option<String>,
    /// Normalised address when the cell parses; an unparseable address is
    /// treated as absent, not as a reason to lose the row.
    pub email: Option<String>,
    pub destination_url: Option<String>,
    pub source_url: Option<String>,
    pub active: Option<bool>,
    pub verified: Option<bool>,
    pub accepts_outreach: Option<bool>,
    pub do_not_contact: Option<bool>,
    /// The sheet's 0–100 integer, stored as-is.
    pub relationship_score: Option<i32>,
    /// `Relevance_Pct` / `Confidence_Pct` are 0–100 decimals in the sheet;
    /// the columns they land on are basis points, so ×100 with clamping.
    pub relevance_basis_points: Option<i32>,
    pub confidence_basis_points: Option<i32>,
}

/// Why a row was not usable. Each names the problem so the sheet can be
/// fixed — a refusal is a sheet edit away from importing, not a dead end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeaconRefusal {
    /// No `Name` cell: nothing to display, nothing to dedupe on.
    MissingName,
    /// Neither `Email` nor `Destination_URL` parses — no identity route.
    MissingRoute,
    /// A `Kind` the roster vocabulary does not know and the map does not
    /// cover. The value travels with the refusal so the sheet can name a
    /// known kind, or the map can grow deliberately.
    UnmappedKind(String),
}

impl BeaconRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingName => "a row with no name cannot become a beacon".to_owned(),
            Self::MissingRoute => {
                "a beacon with neither email nor destination URL cannot be told apart from its own re-import"
                    .to_owned()
            }
            Self::UnmappedKind(kind) => {
                format!("beacon kind '{kind}' is not one the roster knows — map it or rename it in the sheet")
            }
        }
    }
}

/// What one grid yielded: the beacons that parsed and the rows that did
/// not. Refusals keep their 1-based sheet row number — the number a
/// spreadsheet shows its operator.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BeaconSeedReport {
    pub beacons: Vec<SeededBeacon>,
    pub refusals: Vec<(usize, BeaconRefusal)>,
}

/// Whether a header row is the beacon registry sheet. `Kind` alone is not
/// enough — the contacts readout carries one too — so at least two of the
/// beacon-state columns must also be present. Two vetoes keep the claim
/// honest against lookalikes: a band sheet (pinned by `Name` +
/// `Social`/`Links`/`Genre`) and an unambiguous venue sheet stay with
/// their own readers no matter how many state columns they add later.
#[must_use]
pub fn is_beacon_sheet(header: &[String]) -> bool {
    let has = |name: &str| {
        header
            .iter()
            .any(|cell| canonical_column(cell) == Some(name))
    };
    has(columns::KIND)
        && columns::STATE.iter().filter(|name| has(name)).count() >= 2
        && !crate::peer_act_seed::is_seed_sheet(header)
        && !crate::venue_seed::is_unambiguous_venue_sheet(header)
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
        "name" | "display_name" | "contact" => Some(columns::NAME),
        "kind" | "type" | "category" => Some(columns::KIND),
        "city" | "miasto" | "location" => Some(columns::CITY),
        "email" | "e_mail" | "mail" | "contact_email" => Some(columns::EMAIL),
        "destination_url" | "destination" | "url" | "website" | "link" => {
            Some(columns::DESTINATION_URL)
        }
        "source_url" | "source" | "evidence_url" | "evidence" => Some(columns::SOURCE_URL),
        "active" | "is_active" => Some(columns::ACTIVE),
        "verified" | "is_verified" => Some(columns::VERIFIED),
        "accepts_outreach" | "outreach_ok" | "outreach" => Some(columns::ACCEPTS_OUTREACH),
        "do_not_contact" | "dnc" | "suppressed" => Some(columns::DO_NOT_CONTACT),
        "relationship_score" | "relationship" | "score" => Some(columns::RELATIONSHIP_SCORE),
        "relevance_pct" | "relevance" | "relevance_percent" => Some(columns::RELEVANCE_PCT),
        "confidence_pct" | "confidence" | "confidence_percent" => Some(columns::CONFIDENCE_PCT),
        _ => None,
    }
}

/// Maps the sheet's kind vocabulary onto the roster's. The eleven CHECK
/// values pass through by identity; the workbook's finer-grained channel
/// names fold to their nearest roster kind — `podcast` is audio press like
/// `radio`, a platform-specific creator is a `creator`, and community
/// surfaces (event calendars, music-resource pages, cultural hubs) are
/// `community`. Anything else maps to nothing and the row refuses: a kind
/// the map does not name is a vocabulary decision, not a guess to make.
#[must_use]
pub fn beacon_kind_for(raw: &str) -> Option<BeaconKind> {
    let kind = match raw.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
        "radio" | "independent_radio" | "podcast" => BeaconKind::Radio,
        "local_press" | "press" | "media" | "newspaper" => BeaconKind::LocalPress,
        "television" | "tv" => BeaconKind::Television,
        "reviewer" | "review" | "critic" => BeaconKind::Reviewer,
        "creator" | "local_creator" | "instagram_creator" | "tiktok_creator"
        | "youtube_channel" | "youtube_video" | "soundcloud_artist" | "influencer" | "streamer" => {
            BeaconKind::Creator
        }
        "photographer" | "photo" => BeaconKind::Photographer,
        "promoter" | "gig_promoter" => BeaconKind::Promoter,
        "venue" | "room" => BeaconKind::Venue,
        "scene_partner" | "partner" => BeaconKind::ScenePartner,
        "patron" | "sponsor" | "media_patronage" => BeaconKind::Patron,
        "community"
        | "facebook_page"
        | "event_calendar"
        | "local_music_resource"
        | "cultural_hub"
        | "scene" => BeaconKind::Community,
        _ => return None,
    };
    Some(kind)
}

/// A boolean-ish registry flag — `t`/`f` exports, `yes`/`no` hand edits,
/// `1`/`0` spreadsheets. Empty and unknown claim nothing.
fn flag_for(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "1" | "y" | "tak" => Some(true),
        "f" | "false" | "no" | "0" | "n" | "nie" => Some(false),
        _ => None,
    }
}

/// A 0–100 sheet percentage to 0–10000 basis points. A value outside the
/// range clamps rather than refuses — a hand-typed `110` means "very", not
/// a parse error the row should die on. Non-numeric claims nothing.
fn percent_to_basis_points(raw: Option<&str>) -> Option<i32> {
    let value: f64 = raw?.trim().parse().ok()?;
    Some((value * 100.0).round().clamp(0.0, 10000.0) as i32)
}

/// The 0–100 integer the sheet writes — clamped, not refused.
fn score_for(raw: Option<&str>) -> Option<i32> {
    let value: f64 = raw?.trim().parse().ok()?;
    Some(value.round().clamp(0.0, 100.0) as i32)
}

fn clean(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") {
        return None;
    }
    Some(value.to_owned())
}

fn capped(value: Option<String>, limit: usize) -> Option<String> {
    value.map(|v| v.chars().take(limit).collect())
}

/// Reads one whole grid as the beacon registry sheet, or declines it.
///
/// `None` means the header is not this sheet's — the caller tries its other
/// readers. `Some` means the header matched; every non-empty row below it
/// either parsed into a [`SeededBeacon`] or refused with a reason naming
/// the problem. Pure: no IO, no clock.
#[must_use]
pub fn extract_beacon_sheet(grid: &[Vec<String>]) -> Option<BeaconSeedReport> {
    let (header, rows) = grid.split_first()?;
    if !is_beacon_sheet(header) {
        return None;
    }
    let mut index: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        if let Some(name) = canonical_column(cell) {
            index.entry(name).or_insert(i);
        }
    }
    fn cell_at<'a>(
        index: &std::collections::BTreeMap<&str, usize>,
        row: &'a [String],
        name: &str,
    ) -> Option<&'a str> {
        index
            .get(name)
            .and_then(|i| row.get(*i))
            .map(String::as_str)
    }

    let mut report = BeaconSeedReport::default();
    for (offset, row) in rows.iter().enumerate() {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let row_number = offset + 2;
        let Some(display_name) = capped(clean(cell_at(&index, row, columns::NAME)), 240) else {
            report
                .refusals
                .push((row_number, BeaconRefusal::MissingName));
            continue;
        };
        let raw_kind = cell_at(&index, row, columns::KIND)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let Some(kind) = beacon_kind_for(&raw_kind) else {
            report
                .refusals
                .push((row_number, BeaconRefusal::UnmappedKind(raw_kind.clone())));
            continue;
        };
        let email = cell_at(&index, row, columns::EMAIL)
            .and_then(|raw| NormalizedEmail::parse(raw).ok())
            .map(|email| email.as_str().to_owned())
            // `beacons.contact_email`'s CHECK wants a dotted domain —
            // `a@localhost` parses as an email but cannot store, so it
            // counts as absent, exactly like an unparseable cell does.
            .filter(|email| {
                email
                    .rsplit('@')
                    .next()
                    .is_some_and(|domain| domain.contains('.'))
            });
        let destination_url = capped(clean(cell_at(&index, row, columns::DESTINATION_URL)), 2048);
        if email.is_none() && destination_url.is_none() {
            report
                .refusals
                .push((row_number, BeaconRefusal::MissingRoute));
            continue;
        }
        report.beacons.push(SeededBeacon {
            display_name,
            kind,
            raw_kind,
            city: capped(clean(cell_at(&index, row, columns::CITY)), 120),
            email,
            destination_url,
            source_url: capped(clean(cell_at(&index, row, columns::SOURCE_URL)), 2048),
            active: flag_for(cell_at(&index, row, columns::ACTIVE)),
            verified: flag_for(cell_at(&index, row, columns::VERIFIED)),
            accepts_outreach: flag_for(cell_at(&index, row, columns::ACCEPTS_OUTREACH)),
            do_not_contact: flag_for(cell_at(&index, row, columns::DO_NOT_CONTACT)),
            relationship_score: score_for(cell_at(&index, row, columns::RELATIONSHIP_SCORE)),
            relevance_basis_points: percent_to_basis_points(cell_at(
                &index,
                row,
                columns::RELEVANCE_PCT,
            )),
            confidence_basis_points: percent_to_basis_points(cell_at(
                &index,
                row,
                columns::CONFIDENCE_PCT,
            )),
        });
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Vec<String> {
        [
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
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn the_workbook_beacon_header_claims_the_sheet() {
        assert!(is_beacon_sheet(&header()));
    }

    #[test]
    fn a_contacts_readout_is_not_a_beacon_sheet() {
        let contacts: Vec<String> = [
            "Email",
            "Name",
            "Organization",
            "City",
            "Kind",
            "Phone",
            "Notes",
            "Last_Seen",
            "Disappeared",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(!is_beacon_sheet(&contacts));
    }

    #[test]
    fn a_kind_column_without_state_columns_is_not_enough() {
        let header: Vec<String> = ["Name", "Kind", "City", "Email"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!is_beacon_sheet(&header));
    }

    #[test]
    fn every_workbook_kind_maps_or_refuses_by_name() {
        for (raw, want) in [
            ("radio", BeaconKind::Radio),
            ("independent_radio", BeaconKind::Radio),
            ("podcast", BeaconKind::Radio),
            ("local_press", BeaconKind::LocalPress),
            ("reviewer", BeaconKind::Reviewer),
            ("creator", BeaconKind::Creator),
            ("instagram_creator", BeaconKind::Creator),
            ("tiktok_creator", BeaconKind::Creator),
            ("youtube_channel", BeaconKind::Creator),
            ("soundcloud_artist", BeaconKind::Creator),
            ("promoter", BeaconKind::Promoter),
            ("patron", BeaconKind::Patron),
            ("facebook_page", BeaconKind::Community),
            ("event_calendar", BeaconKind::Community),
            ("local_music_resource", BeaconKind::Community),
            ("cultural_hub", BeaconKind::Community),
        ] {
            assert_eq!(beacon_kind_for(raw), Some(want), "kind {raw} mis-mapped");
        }
        assert_eq!(beacon_kind_for("hologram"), None);
    }

    #[test]
    fn a_full_workbook_row_parses() {
        let grid = vec![
            header(),
            vec![
                "Marcin".to_owned(),
                "promoter".to_owned(),
                "Wrocław".to_owned(),
                "m@beacon.test".to_owned(),
                "https://beacon.test".to_owned(),
                "https://evidence.test".to_owned(),
                "t".to_owned(),
                "t".to_owned(),
                "t".to_owned(),
                "f".to_owned(),
                "100".to_owned(),
                "87.5".to_owned(),
                "90.0".to_owned(),
            ],
        ];
        let report = extract_beacon_sheet(&grid).expect("beacon sheet must claim");
        assert_eq!(report.beacons.len(), 1);
        let beacon = &report.beacons[0];
        assert_eq!(beacon.kind, BeaconKind::Promoter);
        assert_eq!(beacon.raw_kind, "promoter");
        assert_eq!(beacon.city.as_deref(), Some("Wrocław"));
        assert_eq!(beacon.email.as_deref(), Some("m@beacon.test"));
        assert_eq!(beacon.verified, Some(true));
        assert_eq!(beacon.do_not_contact, Some(false));
        assert_eq!(beacon.relationship_score, Some(100));
        assert_eq!(beacon.relevance_basis_points, Some(8750));
        assert_eq!(beacon.confidence_basis_points, Some(9000));
    }

    #[test]
    fn a_row_with_neither_route_refuses() {
        let mut row = vec![String::new(); 13];
        row[0] = "No Route".to_owned();
        row[1] = "local_press".to_owned();
        let report = extract_beacon_sheet(&[header(), row]).unwrap();
        assert_eq!(
            report.refusals,
            vec![(2, BeaconRefusal::MissingRoute)],
            "email-less, url-less rows must refuse rather than mint unindexed duplicates"
        );
    }

    #[test]
    fn an_unknown_kind_refuses_with_the_value_named() {
        let mut row = vec![String::new(); 13];
        row[0] = "Odd".to_owned();
        row[1] = "hologram".to_owned();
        row[3] = "odd@test.test".to_owned();
        let report = extract_beacon_sheet(&[header(), row]).unwrap();
        assert_eq!(
            report.refusals,
            vec![(2, BeaconRefusal::UnmappedKind("hologram".to_owned()))]
        );
    }

    #[test]
    fn empty_flags_claim_nothing() {
        let mut row = vec![String::new(); 13];
        row[0] = "Sparse".to_owned();
        row[1] = "radio".to_owned();
        row[4] = "https://radio.test".to_owned();
        let report = extract_beacon_sheet(&[header(), row]).unwrap();
        let beacon = &report.beacons[0];
        assert_eq!(beacon.active, None);
        assert_eq!(beacon.verified, None);
        assert_eq!(beacon.relationship_score, None);
        assert_eq!(beacon.relevance_basis_points, None);
    }

    #[test]
    fn percentages_clamp_and_round() {
        assert_eq!(percent_to_basis_points(Some("100.0")), Some(10000));
        assert_eq!(percent_to_basis_points(Some("87.5")), Some(8750));
        assert_eq!(percent_to_basis_points(Some("110")), Some(10000));
        assert_eq!(percent_to_basis_points(Some("-5")), Some(0));
        assert_eq!(percent_to_basis_points(Some("high")), None);
        assert_eq!(percent_to_basis_points(None), None);
    }

    #[test]
    fn a_band_sheet_with_lookalike_columns_stays_a_band_sheet() {
        // A peer-act sheet that gains a `Type` column plus beacon-flavoured
        // flags still pins `Name` + `Genre` — the veto keeps it with the
        // band reader rather than refusing every row `UnmappedKind`.
        let header: Vec<String> = [
            "Name",
            "Type",
            "City",
            "Country",
            "Genre",
            "Email",
            "Verified",
            "Do_Not_Contact",
            "Confidence",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(crate::peer_act_seed::is_seed_sheet(&header));
        assert!(!is_beacon_sheet(&header));
    }

    #[test]
    fn an_unambiguous_venue_sheet_stays_a_venue_sheet() {
        // Same veto on the venue side: `Address` makes the venue pin
        // unambiguous, so added state columns cannot pull it into beacons.
        let header: Vec<String> = [
            "Name",
            "Kind",
            "City",
            "Address",
            "Source_URL",
            "Verified",
            "Do_Not_Contact",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(crate::venue_seed::is_unambiguous_venue_sheet(&header));
        assert!(!is_beacon_sheet(&header));
    }

    #[test]
    fn a_dotless_domain_email_counts_as_absent() {
        // `beacons.contact_email`'s CHECK demands a dotted domain — a cell
        // like `a@localhost` parses as an email yet cannot store, so the
        // row must find its identity in the URL or refuse.
        let mut row = vec![String::new(); 13];
        row[0] = "Local".to_owned();
        row[1] = "radio".to_owned();
        row[3] = "a@localhost".to_owned();
        row[4] = "https://radio.test".to_owned();
        let report = extract_beacon_sheet(&[header(), row]).unwrap();
        assert_eq!(report.beacons[0].email, None);
        assert_eq!(
            report.beacons[0].destination_url.as_deref(),
            Some("https://radio.test")
        );
    }
}
