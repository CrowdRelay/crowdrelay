//! Reading the human scout file's `META_TARGETS` tab into the beacon
//! roster.
//!
//! `SCOUT.xlsx META_TARGETS` is the scout's target register — festivals,
//! outlets and programmes worth approaching, each with its dedupe key and
//! verification note:
//!
//! ```text
//! Name | Category | Region | Public URL / Username | Public Contact |
//! Relevance | Status | Why useful | Dedupe Key | Checked
//! ```
//!
//! It is a beacon registry in everything but header spelling, so this
//! module parses it into the same [`BeaconSeedReport`] the beacon seed
//! reader produces — one registry, one dedupe rule (`kind` + `city` +
//! `email`/`destination_url`), one write path. The same target the scout
//! calls `euroblast|org` and the GitHub mirror lists under
//! `ORGS`/`beacons` lands on the same beacon row.
//!
//! What this deliberately does NOT claim: `SCOUT AUTO`'s `META_TARGETS`,
//! which is the n8n connector's own config (`Target_ID`, `Platform`,
//! `API_Status`) — that sheet drives the automation, it is not a roster.
//! The pin (`Dedupe Key` + `Category` + the public-route columns) is what
//! tells them apart.

use crate::beacon_seed::{BeaconRefusal, BeaconSeedReport, SeededBeacon, beacon_kind_for};
use crate::beacons::BeaconKind;

/// Header-cell normalisation identical to the sibling readers — `Public
/// URL / Username`, `public_url_username` and `Public-URL-Username` name
/// one column.
fn normalise(cell: &str) -> String {
    cell.trim()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// Whether a header row is the human scout's `META_TARGETS`. The pair
/// `Dedupe Key` + `Category` never appears on the automation's own
/// `META_TARGETS` (it keys on `Target_ID`), and `Name` + a public-route
/// column (`Public URL / Username`, `Public Contact`) separates it from
/// every operational tab.
#[must_use]
pub fn is_scout_meta_targets(header: &[String]) -> bool {
    let has = |needle: &str| header.iter().any(|cell| normalise(cell) == needle);
    has("name")
        && has("category")
        && has("dedupe_key")
        && (has("public_url_username") || has("public_url") || has("public_contact"))
}

/// The scout's `Category` prose → a roster kind. Phrases like
/// `Festival / promoter` fold by containment; a category the map does not
/// cover passes through `beacon_kind_for` verbatim so known spellings still
/// land, and only genuinely unknown kinds refuse.
fn kind_for(raw: &str) -> Option<BeaconKind> {
    let text = raw.trim().to_lowercase();
    let word = if text.contains("festiv") || text.contains("promoter") || text.contains("organizer")
    {
        "promoter"
    } else if text.contains("venue") || text.contains("club") {
        "venue"
    } else if text.contains("radio") {
        "radio"
    } else if text.contains("podcast") {
        "podcast"
    } else if text.contains("press")
        || text.contains("media")
        || text.contains("webzine")
        || text.contains("zine")
        || text.contains("redakcja")
    {
        "local_press"
    } else if text.contains("review") || text.contains("recenzja") {
        "reviewer"
    } else if text.contains("television") || text.contains("tv") {
        "television"
    } else if text.contains("photo") {
        "photographer"
    } else if text.contains("creator") || text.contains("influencer") {
        "creator"
    } else if text.contains("community") || text.contains("forum") {
        "community"
    } else if text.contains("patron") || text.contains("sponsor") {
        "patron"
    } else {
        text.as_str()
    };
    beacon_kind_for(word)
}

/// `Region` → the city half of `Germany / Cologne`. A bare country name
/// (`Polska`, `CEE`) names no city — the row imports global and the raw
/// text travels with it, the same rule the beacon reader applies.
fn city_of(region: &str) -> Option<String> {
    let region = region.trim();
    if region.is_empty() {
        return None;
    }
    region.rsplit_once('/').and_then(|(_, city)| {
        let city = city.trim();
        (!city.is_empty()).then(|| city.to_owned())
    })
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

fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") || value == "-" {
        return None;
    }
    Some(value.to_owned())
}

/// 0–100 sheet score → basis points; a blank or non-numeric cell asserts
/// nothing.
fn score_basis_points(value: &str) -> Option<i32> {
    let value: f64 = value.trim().parse().ok()?;
    Some((value * 100.0).round().clamp(0.0, 10000.0) as i32)
}

/// Reads one grid as the scout `META_TARGETS` tab, or declines it.
///
/// `None` means the header is not this sheet's — the caller tries its
/// other readers. `Some` means the pin matched and every non-empty row
/// either parsed into a [`SeededBeacon`] or refused with the same refusal
/// vocabulary the beacon registry sheet uses. Pure: no IO, no clock.
#[must_use]
pub fn extract_scout_meta_targets(grid: &[Vec<String>]) -> Option<BeaconSeedReport> {
    let (header, rows) = grid.split_first()?;
    if !is_scout_meta_targets(header) {
        return None;
    }
    let mut index: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        let key = normalise(cell);
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

    let mut report = BeaconSeedReport::default();
    for (offset, row) in rows.iter().enumerate() {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let sheet_row = offset + 2;
        let name = cell!(row, "name");
        if name.is_empty() {
            report
                .refusals
                .push((sheet_row, BeaconRefusal::MissingName));
            continue;
        }
        let raw_category = cell!(row, "category");
        let Some(kind) = kind_for(raw_category) else {
            report.refusals.push((
                sheet_row,
                BeaconRefusal::UnmappedKind(raw_category.to_owned()),
            ));
            continue;
        };
        let email = cell!(row, "public_contact");
        let email = looks_emailish(email).then(|| email.to_owned());
        let url_raw = cell!(row, "public_url_username");
        let url_raw = if url_raw.is_empty() {
            cell!(row, "public_url")
        } else {
            url_raw
        };
        let destination_url =
            clean(url_raw).filter(|v| v.starts_with("http://") || v.starts_with("https://"));
        if email.is_none() && destination_url.is_none() {
            report
                .refusals
                .push((sheet_row, BeaconRefusal::MissingRoute));
            continue;
        }
        let status = cell!(row, "status");
        report.beacons.push(SeededBeacon {
            display_name: name.to_owned(),
            kind,
            raw_kind: raw_category.to_owned(),
            city: city_of(cell!(row, "region")),
            email,
            destination_url,
            source_url: None,
            active: None,
            // Only a verification verdict asserts the row was checked —
            // "Verified public" marks it; workflow statuses claim nothing.
            verified: status.to_lowercase().contains("verified").then_some(true),
            accepts_outreach: None,
            do_not_contact: None,
            relationship_score: None,
            relevance_basis_points: score_basis_points(cell!(row, "relevance")),
            confidence_basis_points: None,
        });
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &[&str] = &[
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
    ];

    fn grid(header: &[&str], rows: &[&[&str]]) -> Vec<Vec<String>> {
        let mut grid = vec![header.iter().map(|s| s.to_string()).collect::<Vec<_>>()];
        for row in rows {
            grid.push(row.iter().map(|s| s.to_string()).collect());
        }
        grid
    }

    #[test]
    fn claims_the_scout_meta_targets_header_only() {
        assert!(extract_scout_meta_targets(&grid(HEADER, &[])).is_some());
        // The automation's own META_TARGETS is connector config, not a roster.
        let auto = grid(
            &[
                "Target_ID",
                "Active",
                "Platform",
                "Entity_Type",
                "Name",
                "Object_ID_or_Username",
                "Profile_URL",
                "API_Status",
            ],
            &[],
        );
        assert!(extract_scout_meta_targets(&auto).is_none());
        // A contact list claims nothing either.
        let contacts = grid(&["Name", "Email"], &[&["Somebody", "a@b.c"]]);
        assert!(extract_scout_meta_targets(&contacts).is_none());
    }

    #[test]
    fn festival_promoter_rows_become_promoter_beacons() {
        let report = extract_scout_meta_targets(&grid(
            HEADER,
            &[&[
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
            ]],
        ))
        .expect("header claims");
        assert_eq!(report.beacons.len(), 1);
        let beacon = &report.beacons[0];
        assert_eq!(beacon.kind, BeaconKind::Promoter);
        assert_eq!(beacon.raw_kind, "Festival / promoter");
        assert_eq!(beacon.city.as_deref(), Some("Cologne"));
        assert_eq!(beacon.email.as_deref(), Some("su@euroblast.net"));
        assert_eq!(
            beacon.destination_url.as_deref(),
            Some("https://www.euroblast.net/")
        );
        assert_eq!(beacon.verified, Some(true));
        assert_eq!(beacon.relevance_basis_points, Some(9_500));
    }

    #[test]
    fn rows_with_no_public_route_are_refused() {
        let report = extract_scout_meta_targets(&grid(
            HEADER,
            &[&[
                "Mystery Fest",
                "Festival / promoter",
                "Polska",
                "not a url",
                "no contact",
                "",
                "",
                "",
                "mystery|org",
                "",
            ]],
        ))
        .expect("header claims");
        assert!(report.beacons.is_empty());
        assert_eq!(report.refusals, vec![(2, BeaconRefusal::MissingRoute)]);
    }

    #[test]
    fn unknown_categories_refuse_with_the_raw_value() {
        let report = extract_scout_meta_targets(&grid(
            HEADER,
            &[&[
                "Odd Thing",
                "quantum gathering",
                "",
                "https://example.com/",
                "",
                "",
                "",
                "",
                "odd|org",
                "",
            ]],
        ))
        .expect("header claims");
        assert_eq!(
            report.refusals,
            vec![(
                2,
                BeaconRefusal::UnmappedKind("quantum gathering".to_owned())
            )]
        );
    }
}
