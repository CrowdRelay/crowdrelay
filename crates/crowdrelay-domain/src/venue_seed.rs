//! Reading a researched venue sheet into a room the whole platform can share.
//!
//! `booking_discovery` opens by saying the negotiation machinery *"is complete
//! and starves"* — zero venues was a stable state rather than a problem the
//! agent could notice. A hand-built or researched sheet of rooms is the
//! cheapest end to that, and this module is the door it comes through.
//!
//! # Why this is not the contact intake
//!
//! `drive_contacts` keys every row on an email address: a file with no email
//! column yields nothing, and that property is the whole noise filter for a
//! scan of somebody's entire Drive. It is the right rule for a contact list and
//! the wrong one for a venue sheet. In the seed this module was written
//! against, **38 of 233 rooms carry an email**; the rest are an address, a
//! capacity and a source link. A room is a real place whether or not anyone has
//! a booking address for it yet, so the contact is an attribute that may arrive
//! later, never the key.
//!
//! # One row is two different things
//!
//! A researched sheet mixes facts about the world with the researcher's opinion
//! of them, and they do not belong in the same place:
//!
//! - **The room** — name, city, capacity, genre, published route — is the same
//!   for everyone. Two tenants do not disagree about how many people fit in a
//!   club, so this half is tenant-agnostic.
//! - **The view** — is this a good fit for us, what angle would we pitch, how
//!   good is our contact — is one tenant's judgement, and it is exactly the
//!   half a competitor would like to read.
//!
//! A global row is outside `workspace-scope-ratchet`'s reach by construction:
//! that gate guards tables carrying `workspace_id`, because that column *"is
//! the whole of CrowdRelay's tenant isolation"*. So a global table's safety
//! cannot come from the gate — it has to come from holding nothing worth
//! isolating, and splitting the row here is where that is enforced.
//!
//! # What is refused
//!
//! Screening happens on write, the same as `booking_discovery`: a row with no
//! source link, no name or no city is dropped rather than stored as a lead
//! nobody can check. A closed room is kept and marked rather than dropped,
//! because deleting it invites the next sweep to research it again.

use serde::{Deserialize, Serialize};

/// What a public-terms search found, which is not the same as what the terms
/// are.
///
/// The seed's own wording is *"No public booking deal terms found in reviewed
/// public source."* That is a record that somebody looked and there was nothing
/// — worth keeping, because it stops the next sweep repeating the search — and
/// it is emphatically not a fee. Collapsing the two into an empty string would
/// lose the difference between *unknown* and *known absent*, which is the same
/// mistake as rendering a missing number as zero.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum PublicTerms {
    /// Nobody has looked.
    #[default]
    NotSearched,
    /// Somebody looked at a public source and found no terms published.
    SearchedNoneFound,
    /// A term published openly — a hire rate card, a submission page naming a
    /// split. Never a fee somebody was privately offered.
    Published(String),
}

/// The room, as everyone sees it. Nothing here is specific to one tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededRoom {
    pub name: String,
    pub city: String,
    pub country: String,
    pub address: Option<String>,
    pub website: Option<String>,
    /// Free-form tags, split from a sheet's prose. A bias, never a filter —
    /// the same rule `ContentFormatEntry::genre_fit` states for formats.
    pub genre_tags: Vec<String>,
    pub capacity: Option<u32>,
    /// A published booking address. Most rooms in a researched sheet have none.
    pub booking_email: Option<String>,
    pub public_terms: PublicTerms,
    /// Where the claim came from, and when. Required: a room nobody can check
    /// is a rumour with an address.
    pub source_url: String,
    pub researched_on: Option<String>,
    /// Kept and marked rather than dropped, so the next sweep does not spend
    /// its budget rediscovering a room that shut.
    pub closed: bool,
}

/// One tenant's opinion of a room. Never shared, never global.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeededRoomView {
    /// The researcher's fit judgement for *this* act.
    pub target_fit: Option<String>,
    /// How we would pitch it, if we pitched it.
    pub outreach_angle: Option<String>,
    /// What we hold — "Booking email", "Address only". A statement about our
    /// records, not about the room.
    pub contact_quality: Option<String>,
    pub notes: Option<String>,
}

/// A parsed row: the shared half and the private half, already separated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededVenue {
    pub room: SeededRoom,
    pub view: SeededRoomView,
}

/// Why a row was not usable. Each names the column so a sheet can be fixed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeedRefusal {
    MissingName,
    MissingCity,
    /// Link-or-drop, the rule `booking_discovery` and §3.2 already apply: a
    /// candidate with no dated source is not a candidate.
    MissingSource,
}

impl SeedRefusal {
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::MissingName => "a row with no venue name cannot become a room",
            Self::MissingCity => "a room without a city cannot be matched to an audience",
            Self::MissingSource => "a room with no source link is a rumour with an address",
        }
    }
}

fn clean(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") {
        return None;
    }
    Some(value.to_owned())
}

/// Splits a sheet's genre prose into tags.
///
/// The seed writes "Rock / metal / alternative" and "Electronic /
/// experimental". Splitting on the separators rather than parsing into an enum
/// keeps this a bias rather than a filter, and means a genre nobody anticipated
/// survives the import instead of being rounded to the nearest known one.
#[must_use]
pub fn genre_tags(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = clean(raw) else {
        return Vec::new();
    };
    let mut tags: Vec<String> = raw
        .split(['/', ',', ';'])
        .filter_map(|part| clean(Some(part)))
        .map(|part| part.to_lowercase())
        .collect();
    tags.dedup();
    tags
}

/// Reads the public-terms cell.
///
/// A sheet that says it looked and found nothing is recorded as having looked.
/// Anything else with text in it is a published term; an empty cell is nobody
/// having checked.
#[must_use]
pub fn public_terms(raw: Option<&str>) -> PublicTerms {
    let Some(raw) = clean(raw) else {
        return PublicTerms::NotSearched;
    };
    let lowered = raw.to_lowercase();
    let looked_and_found_nothing = lowered.contains("no public")
        || lowered.contains("none found")
        || lowered.contains("not found")
        || lowered.contains("no terms");
    if looked_and_found_nothing {
        PublicTerms::SearchedNoneFound
    } else {
        PublicTerms::Published(raw)
    }
}

fn capacity(raw: Option<&str>) -> Option<u32> {
    let raw = clean(raw)?;
    // Sheets write "600", "ca. 600", "600 (standing)". Take the first run of
    // digits and ignore the rest rather than refusing the row over prose.
    let digits: String = raw
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|value| *value > 0)
}

/// One field set from a sheet row, keyed by the sheet's own column names.
pub type SeedRow<'a> = std::collections::BTreeMap<&'a str, &'a str>;

/// Parses one researched row into its shared and private halves.
///
/// # Errors
///
/// Refuses a row missing a name, a city, or a source link.
pub fn parse_seed_row(row: &SeedRow<'_>) -> Result<SeededVenue, SeedRefusal> {
    let get = |key: &str| clean(row.get(key).copied());

    let name = get("Name").ok_or(SeedRefusal::MissingName)?;
    let city = get("City").ok_or(SeedRefusal::MissingCity)?;
    let source_url = get("Source_URL").ok_or(SeedRefusal::MissingSource)?;

    // "Booking_Contact" is the column a researcher fills when the address is
    // specifically for booking; "Email" is whatever general address was found.
    // Prefer the booking one and fall back, because pitching a general inbox is
    // better than pitching nobody.
    let booking_email = get("Booking_Contact").or_else(|| get("Email"));

    let closed = get("Status")
        .map(|status| status.eq_ignore_ascii_case("closed"))
        .unwrap_or(false);

    Ok(SeededVenue {
        room: SeededRoom {
            name,
            city,
            country: get("Country").unwrap_or_default(),
            address: get("Address"),
            website: get("Website"),
            genre_tags: genre_tags(row.get("Audience_Genre").copied()),
            capacity: capacity(row.get("Capacity").copied()),
            booking_email,
            public_terms: public_terms(row.get("Public_Financial_Info").copied()),
            source_url,
            researched_on: get("Research_Date"),
            closed,
        },
        view: SeededRoomView {
            target_fit: get("Target_Fit"),
            outreach_angle: get("Outreach_Angle"),
            contact_quality: get("Contact_Quality"),
            notes: get("Notes"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row taken verbatim from the seed this module was written against.
    fn progresja() -> SeedRow<'static> {
        SeedRow::from([
            ("Name", "Progresja"),
            ("Organisation", "Progresja"),
            ("Email", "klub@progresja.com"),
            ("Phone", "+48 22 460 57 60"),
            ("Type", "venue"),
            (
                "Notes",
                "Strong rock focus; three stages; also operates Hala Koło.",
            ),
            ("Country", "Poland"),
            ("City", "Warsaw"),
            ("Address", "ul. Fort Wola 22, Warsaw"),
            ("Website", "https://www.progresja.com"),
            ("Audience_Genre", "Rock / metal / alternative"),
            ("Booking_Contact", "klub@progresja.com"),
            (
                "Public_Financial_Info",
                "No public booking deal terms found in reviewed public source.",
            ),
            ("Status", "Active"),
            ("Source_URL", "https://goout.net/en/progresja/vzxueb/"),
            ("Research_Date", "2026-09-16"),
            ("Target_Fit", "High"),
            ("Contact_Quality", "Booking email"),
            ("Outreach_Angle", "Metal/rock club pitch"),
        ])
    }

    /// The row that proves the point: 188 of 233 rooms in the seed are
    /// "Address only", and the contact intake would have dropped every one.
    fn stodola() -> SeedRow<'static> {
        SeedRow::from([
            ("Name", "Stodoła"),
            ("Country", "Poland"),
            ("City", "Warsaw"),
            ("Address", "ul. Batorego 10, Warsaw"),
            ("Website", "https://stodola.pl"),
            ("Audience_Genre", "Rock / punk / guitar music"),
            (
                "Public_Financial_Info",
                "No public booking deal terms found in reviewed public source.",
            ),
            ("Status", "Active"),
            ("Source_URL", "https://goout.net/en/stodola/vzgueb/"),
            ("Contact_Quality", "Address only"),
        ])
    }

    #[test]
    fn a_room_with_no_email_is_still_a_room() {
        let parsed = parse_seed_row(&stodola()).expect("address-only rooms are the majority");
        assert_eq!(parsed.room.name, "Stodoła");
        assert_eq!(parsed.room.booking_email, None);
        assert_eq!(parsed.room.city, "Warsaw");
    }

    #[test]
    fn the_researchers_opinion_does_not_reach_the_shared_room() {
        let parsed = parse_seed_row(&progresja()).expect("row parses");
        // Everything the world can check.
        assert_eq!(parsed.room.city, "Warsaw");
        assert_eq!(
            parsed.room.website.as_deref(),
            Some("https://www.progresja.com")
        );
        // Everything that is one tenant's judgement.
        assert_eq!(parsed.view.target_fit.as_deref(), Some("High"));
        assert_eq!(
            parsed.view.outreach_angle.as_deref(),
            Some("Metal/rock club pitch")
        );
        assert_eq!(
            parsed.view.contact_quality.as_deref(),
            Some("Booking email")
        );
    }

    #[test]
    fn a_booking_address_is_preferred_over_a_general_one() {
        let mut row = progresja();
        row.insert("Email", "info@progresja.com");
        row.insert("Booking_Contact", "booking@progresja.com");
        let parsed = parse_seed_row(&row).expect("row parses");
        assert_eq!(
            parsed.room.booking_email.as_deref(),
            Some("booking@progresja.com")
        );
    }

    #[test]
    fn genre_prose_becomes_tags_not_an_enum() {
        assert_eq!(
            genre_tags(Some("Rock / metal / alternative")),
            vec!["rock", "metal", "alternative"]
        );
        assert_eq!(
            genre_tags(Some("Electronic / experimental")),
            vec!["electronic", "experimental"]
        );
        // A genre nobody anticipated survives rather than being rounded off.
        assert_eq!(genre_tags(Some("Vaporwave")), vec!["vaporwave"]);
        assert!(genre_tags(None).is_empty());
    }

    /// "We looked and found nothing" is information. "Nobody looked" is not the
    /// same thing, and an empty string would lose the difference.
    #[test]
    fn a_terms_search_that_found_nothing_is_recorded_as_a_search() {
        assert_eq!(
            public_terms(Some(
                "No public booking deal terms found in reviewed public source."
            )),
            PublicTerms::SearchedNoneFound
        );
        assert_eq!(public_terms(None), PublicTerms::NotSearched);
        assert_eq!(
            public_terms(Some("Hire rate card published: 1200 EUR")),
            PublicTerms::Published("Hire rate card published: 1200 EUR".to_owned())
        );
    }

    #[test]
    fn a_room_with_no_source_is_refused() {
        let mut row = progresja();
        row.remove("Source_URL");
        assert_eq!(parse_seed_row(&row), Err(SeedRefusal::MissingSource));
    }

    #[test]
    fn a_row_with_no_name_or_no_city_is_refused() {
        let mut no_name = progresja();
        no_name.remove("Name");
        assert_eq!(parse_seed_row(&no_name), Err(SeedRefusal::MissingName));

        let mut no_city = progresja();
        no_city.remove("City");
        assert_eq!(parse_seed_row(&no_city), Err(SeedRefusal::MissingCity));
    }

    /// Dropping a closed room invites the next sweep to research it again.
    #[test]
    fn a_closed_room_is_marked_not_dropped() {
        let mut row = progresja();
        row.insert("Status", "Closed");
        let parsed = parse_seed_row(&row).expect("closed rooms are kept");
        assert!(parsed.room.closed);
    }

    #[test]
    fn capacity_survives_the_prose_around_it() {
        let mut row = progresja();
        for (written, expected) in [
            ("600", Some(600)),
            ("ca. 600", Some(600)),
            ("600 (standing)", Some(600)),
            ("unknown", None),
            ("0", None),
        ] {
            row.insert("Capacity", written);
            assert_eq!(
                parse_seed_row(&row).expect("row parses").room.capacity,
                expected,
                "capacity {written:?}"
            );
        }
    }

    #[test]
    fn every_refusal_names_the_column_problem() {
        for refusal in [
            SeedRefusal::MissingName,
            SeedRefusal::MissingCity,
            SeedRefusal::MissingSource,
        ] {
            assert!(refusal.message().len() > 20, "{:?}", refusal);
        }
    }
}
