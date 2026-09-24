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

/// The sheet's column names, in header order.
///
/// The research brief (4G.6) tells the operator's tool to answer with exactly
/// this header, and `parse_seed_row` reads through these same names — so a
/// column the prompt asks for is always a column the intake reads, and a
/// rename fails in a test rather than in somebody's Drive.
pub mod columns {
    pub const NAME: &str = "Name";
    pub const CITY: &str = "City";
    pub const COUNTRY: &str = "Country";
    pub const ADDRESS: &str = "Address";
    pub const WEBSITE: &str = "Website";
    pub const AUDIENCE_GENRE: &str = "Audience_Genre";
    pub const CAPACITY: &str = "Capacity";
    pub const BOOKING_CONTACT: &str = "Booking_Contact";
    pub const EMAIL: &str = "Email";
    pub const PUBLIC_FINANCIAL_INFO: &str = "Public_Financial_Info";
    pub const STATUS: &str = "Status";
    pub const SOURCE_URL: &str = "Source_URL";
    pub const RESEARCH_DATE: &str = "Research_Date";
    pub const TARGET_FIT: &str = "Target_Fit";
    pub const CONTACT_QUALITY: &str = "Contact_Quality";
    pub const OUTREACH_ANGLE: &str = "Outreach_Angle";
    pub const NOTES: &str = "Notes";

    /// The header row, in the order the seed itself was written in.
    pub const ALL: &[&str] = &[
        NAME,
        CITY,
        COUNTRY,
        ADDRESS,
        WEBSITE,
        AUDIENCE_GENRE,
        CAPACITY,
        BOOKING_CONTACT,
        EMAIL,
        PUBLIC_FINANCIAL_INFO,
        STATUS,
        SOURCE_URL,
        RESEARCH_DATE,
        TARGET_FIT,
        CONTACT_QUALITY,
        OUTREACH_ANGLE,
        NOTES,
    ];
}

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
    /// The sheet's liveness claim, normalised: `closed` marks a dead room so
    /// the next sweep does not research it again; `active` is written too —
    /// a reopened room's fresh `active` fact outranks its stale `closed` on
    /// `observed_at` within the same provenance. Any other word is not a
    /// status claim and parses as `None`.
    pub status: Option<String>,
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

    let name = get(columns::NAME).ok_or(SeedRefusal::MissingName)?;
    let city = get(columns::CITY).ok_or(SeedRefusal::MissingCity)?;
    let source_url = get(columns::SOURCE_URL).ok_or(SeedRefusal::MissingSource)?;

    // "Booking_Contact" is the column a researcher fills when the address is
    // specifically for booking; "Email" is whatever general address was found.
    // Prefer the booking one and fall back, because pitching a general inbox is
    // better than pitching nobody.
    let booking_email = get(columns::BOOKING_CONTACT).or_else(|| get(columns::EMAIL));

    // The Status vocabulary is two words — "closed" and "active" — and the
    // intake writes whichever the sheet claims. Anything else ("unknown",
    // "seasonal", a note) is no claim at all rather than a parsed guess.
    let status = get(columns::STATUS).and_then(|status| {
        let lowered = status.to_ascii_lowercase();
        (lowered == "closed" || lowered == "active").then_some(lowered)
    });

    Ok(SeededVenue {
        room: SeededRoom {
            name,
            city,
            country: get(columns::COUNTRY).unwrap_or_default(),
            address: get(columns::ADDRESS),
            website: get(columns::WEBSITE),
            genre_tags: genre_tags(row.get(columns::AUDIENCE_GENRE).copied()),
            capacity: capacity(row.get(columns::CAPACITY).copied()),
            booking_email,
            public_terms: public_terms(row.get(columns::PUBLIC_FINANCIAL_INFO).copied()),
            source_url,
            researched_on: get(columns::RESEARCH_DATE),
            status,
        },
        view: SeededRoomView {
            target_fit: get(columns::TARGET_FIT),
            outreach_angle: get(columns::OUTREACH_ANGLE),
            contact_quality: get(columns::CONTACT_QUALITY),
            notes: get(columns::NOTES),
        },
    })
}

/// Whether a header row is the researched-venue sheet this module parses.
///
/// The brief pins the header, so detection is the brief's own contract: a
/// sheet carrying `Name`, `City` and `Source_URL` is a venue sheet no matter
/// what else it carries. That matters in both directions — a contact list
/// never has a `Source_URL` column, and a venue sheet that *also* has an
/// `Email` column is still a venue sheet (the seed's own rows prove a room
/// needs no email to be real).
///
/// The one collision is the band sheet: its pinned header also carries
/// `Name`, `City` and `Source_URL`, so the venue pin alone would swallow it
/// and mint bands as rooms. The disambiguation runs both ways — a band
/// sheet always carries an identity column (`Social`, `Links`, `Genre`)
/// and never a venue-only one (`Address`, `Capacity`, `Audience_Genre`,
/// `Booking_Contact`, `Target_Fit`…), so a header matching both predicates
/// is a venue sheet with a stray column, not a band sheet.
#[must_use]
pub fn is_seed_sheet(header: &[String]) -> bool {
    venue_pin(header) && (venue_specific(header) || !crate::peer_act_seed::is_seed_sheet(header))
}

/// `is_seed_sheet` without its band-fallback arm — true only when a
/// venue-exclusive column is also present. Exported so an overlapping
/// claim signature (the beacon sheet's) can let an unambiguous venue
/// sheet through without being swallowed by the looser full pin.
#[must_use]
pub fn is_unambiguous_venue_sheet(header: &[String]) -> bool {
    venue_pin(header) && venue_specific(header)
}

fn venue_pin(header: &[String]) -> bool {
    has_column(header, columns::NAME)
        && has_column(header, columns::CITY)
        && has_column(header, columns::SOURCE_URL)
}

fn venue_specific(header: &[String]) -> bool {
    [
        columns::ADDRESS,
        columns::CAPACITY,
        columns::AUDIENCE_GENRE,
        columns::BOOKING_CONTACT,
        columns::PUBLIC_FINANCIAL_INFO,
        columns::TARGET_FIT,
        columns::CONTACT_QUALITY,
        columns::OUTREACH_ANGLE,
    ]
    .iter()
    .any(|name| has_column(header, name))
}

fn has_column(header: &[String], name: &str) -> bool {
    header
        .iter()
        .any(|cell| canonical_column(cell) == Some(name))
}

/// The column this header cell names, if it names one.
///
/// Matched on a normalised form — trimmed, lowercased, spaces and hyphens read
/// as underscores — so `Source_URL`, `Source URL` and `source url` are one
/// column. The brief pins the exact header, and a sheet that came back through
/// a spreadsheet's own export, a translation round-trip or somebody's retyping
/// is still the sheet the operator was asked for. Read strictly, such a file
/// falls through to the contact reader and every room in it is staged as a
/// press contact — a misfile somebody then has to undo by hand.
#[must_use]
pub fn canonical_column(cell: &str) -> Option<&'static str> {
    let normalise = |value: &str| {
        value
            .trim()
            .to_lowercase()
            .replace([' ', '-'], "_")
            .replace("__", "_")
    };
    let cell = normalise(cell);
    columns::ALL
        .iter()
        .copied()
        .find(|name| normalise(name) == cell)
}

/// What one grid yielded: the venues that parsed and the rows that did not.
///
/// Refusals keep their 1-based sheet row number — the number a spreadsheet
/// shows its operator — so a refusal reads "row 17 has no source link", not
/// "record 15 failed".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeedSheetReport {
    pub venues: Vec<SeededVenue>,
    /// (1-based sheet row number, refusal) so the operator can fix the sheet.
    pub refusals: Vec<(usize, SeedRefusal)>,
}

/// Reads one whole grid as a venue sheet, or declines it.
///
/// `None` means the header is not the seed sheet's — the file is somebody
/// else's table and the caller should try its other readers. `Some` means
/// the header matched, and every non-empty row below it was either parsed
/// into a venue or refused with a reason that names the column problem.
/// Pure: no IO, no clock — what the sheet said is the whole input.
#[must_use]
pub fn extract_seed_sheet(grid: &[Vec<String>]) -> Option<SeedSheetReport> {
    let (header, rows) = grid.split_first()?;
    if !is_seed_sheet(header) {
        return None;
    }
    // Column name → cell index, read off the sheet's own header so a sheet
    // with extra or reordered columns still parses. The first occurrence of
    // a name wins — a sheet that repeats a header is ambiguous, not richer.
    let mut index: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        // Keyed by the canonical column name, not the sheet's spelling, so a
        // row lookup asks for `Source_URL` however the header wrote it. A cell
        // naming no known column is dropped rather than carried: the parser
        // reads by name, and an unknown name has no reader.
        if let Some(name) = canonical_column(cell) {
            index.entry(name).or_insert(i);
        }
    }
    let mut report = SeedSheetReport::default();
    for (offset, row) in rows.iter().enumerate() {
        // A row of only whitespace is the tail of an edited sheet, not a
        // venue missing everything.
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let seed_row: SeedRow<'_> = index
            .iter()
            .filter_map(|(name, i)| row.get(*i).map(|cell| (*name, cell.as_str())))
            .collect();
        match parse_seed_row(&seed_row) {
            Ok(venue) => report.venues.push(venue),
            // offset counts from the first data row; +2 lands on the sheet's
            // own row number (header is row 1, first data row is row 2).
            Err(refusal) => report.refusals.push((offset + 2, refusal)),
        }
    }
    Some(report)
}

/// What a gig-plan refusal can ask somebody to go and find (4G.6).
///
/// The operator already keeps research in Drive sheets and already pays for
/// (or freely uses) an AI assistant. When the planner refuses for want of a
/// room or a route, the honest next step is not "wait for the system to grow
/// a researcher" — it is handing the operator a question that is already
/// correctly specified, so the answer lands back through the intake without
/// editing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResearchSubject {
    /// No room on record in this city: go and find the rooms.
    Rooms,
    /// A room is on record but nobody who books it is reachable: go and find
    /// the booking contact for *this* room, not rooms in general.
    BookingContact { venue: String },
}

/// A prompt the operator can paste into whatever AI they already use.
///
/// The column list is rendered from [`columns::ALL`] — the same names the
/// parser reads — so a sheet produced from this prompt parses without the
/// operator editing it, which is the whole point of the delegation.
///
/// `genre` is the band's own word for what they play (the listing's
/// `genre_tags`). `None` leaves the genre to the operator rather than
/// inventing one, because a prompt that names a genre the band does not play
/// returns a sheet of rooms that were never going to fit.
///
/// `capacity_range` bounds the search to rooms the band could plausibly fill.
/// `None` asks without a range rather than guessing one.
#[must_use]
pub fn research_brief(
    subject: &ResearchSubject,
    city: &str,
    genre: Option<&str>,
    capacity_range: Option<(u32, u32)>,
) -> String {
    let genre_clause = match genre {
        Some(genre) => format!(" that book {genre} acts"),
        None => " that book acts in the band's genre".to_owned(),
    };
    let capacity_clause = match capacity_range {
        Some((low, high)) if low == high => {
            format!(" and hold roughly {low} people")
        }
        Some((low, high)) => format!(" and hold roughly {low}–{high} people"),
        None => String::new(),
    };
    let ask = match subject {
        ResearchSubject::Rooms => format!(
            "Research live-music venues in {city}{genre_clause}{capacity_clause}. \
             A room that has stopped programming is still useful — mark its Status \
             \"Closed\" rather than dropping it, so it is not researched twice."
        ),
        ResearchSubject::BookingContact { venue } => format!(
            "{venue} in {city} is the right room and we have no way to reach \
             whoever books it. Find the booking contact — the address that \
             books the room, not the general inbox — and the venue row for it."
        ),
    };
    format!(
        "{ask}\n\nReturn one row per venue with exactly this header:\n{header}\n\n\
         Rules: Source_URL is required on every row — a room with no link is a \
         rumour, and the intake will drop it. Booking_Contact only when the \
         address is specifically for booking; use Email for a general address. \
         Public_Financial_Info records what the venue publishes — write \"No \
         public booking deal terms found in reviewed public source\" when \
         nothing is published. Research_Date is today. Drop the result in the \
         venues sheet in Drive and the intake reads it back.",
        header = columns::ALL.join(", ")
    )
}

/// The registry-verification brief — the delegation loop's other direction.
///
/// `research_brief` asks for rooms in a city; this asks the model to check
/// the rooms the registry already holds and to name the ones it misses. The
/// venue list is embedded in the prompt because the AI's job is to *verify*
/// — a prompt that asks for "venues in Poland" rediscovers from scratch and
/// never reconciles against what we already believe.
///
/// `venues` is `(display_name, city_name)` per row — the pair a person would
/// type into a search box, not the ids. An empty registry produces no brief:
/// there is nothing to verify, and discovery is `research_brief`'s job.
///
/// The rules the prompt states are the intake's contract: `Status` says
/// "Closed" or "Active" and nothing else, `Source_URL` carries the *evidence*
/// for the claim (the closure post, the listing page — not the venue's
/// homepage by default), and new rooms the registry does not hold are simply
/// appended rows.
#[must_use]
pub fn verification_brief(
    venues: &[(String, String)],
    acts: &[(String, Option<String>)],
    agents: &[(String, Option<String>, String)],
    genre: Option<&str>,
) -> Option<String> {
    if venues.is_empty() && acts.is_empty() && agents.is_empty() {
        return None;
    }
    let genre_clause = match genre {
        Some(genre) => format!(" that book {genre} acts"),
        None => String::new(),
    };
    let act_genre_clause = match genre {
        Some(genre) => format!(" in the {genre} scene"),
        None => String::new(),
    };
    let mut venue_list = String::new();
    for (name, city) in venues {
        venue_list.push_str(&format!("- {name} ({city})\n"));
    }
    let mut act_list = String::new();
    for (name, city) in acts {
        match city {
            Some(city) => act_list.push_str(&format!("- {name} ({city})\n")),
            None => act_list.push_str(&format!("- {name}\n")),
        }
    }
    let mut agent_list = String::new();
    for (name, agency, email) in agents {
        match agency {
            Some(agency) => {
                agent_list.push_str(&format!("- {name} ({agency}) <{email}>\n"));
            }
            None => agent_list.push_str(&format!("- {name} <{email}>\n")),
        }
    }
    let cities = venues
        .iter()
        .map(|(_, city)| city.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let venue_block = if venues.is_empty() {
        String::new()
    } else {
        format!(
            "## Sheet \"venues\" — {count} rooms across {cities} cities:\n\n\
             {venue_list}\n\
             Verify each room still operates: a live website or social profile, \
             a post or event listing within the last ~3 months, upcoming shows on \
             a listings page (goout.net, Resident Advisor, Bandsintown, Facebook \
             events). Status is exactly \"Active\" or \"Closed\"; a Closed row's \
             Source_URL carries the closure evidence (a closure post, a \
             \"permanently closed\" listing), an Active row's a current events \
             page or recent post. Then append active live-music venues{genre_clause} \
             missing from the list. Header:\n{venue_header}\n\n",
            count = venues.len(),
            cities = cities,
            venue_list = venue_list,
            genre_clause = genre_clause,
            venue_header = columns::ALL.join(", ")
        )
    };
    let act_block = if acts.is_empty() {
        String::new()
    } else {
        format!(
            "## Sheet \"bands\" — {count} acts:\n\n\
             {act_list}\n\
             Verify each act is still a working band: a release, show or post \
             within the last ~12 months on its own page or a listings site. Status \
             is exactly \"Active\" or \"Inactive\"; an Inactive row's Source_URL \
             carries the evidence (a farewell post, a disbanded listing). Inactive \
             retires an act we already track — never a reason to add a new row. \
             Then append active bands{act_genre_clause} the list misses. Header:\n\
             {act_header}\n\n",
            count = acts.len(),
            act_list = act_list,
            act_genre_clause = act_genre_clause,
            act_header = crate::peer_act_seed::columns::ALL.join(", ")
        )
    };
    let agent_block = if agents.is_empty() {
        String::new()
    } else {
        format!(
            "## Sheet \"agents\" — {count} booking agents:\n\n\
             {agent_list}\n\
             Verify each agent still represents artists: a live agency site, a \
             roster page that still names them, recent show announcements they \
             booked. Status is exactly \"Active\" or \"Inactive\"; an Inactive \
             row's Notes names the evidence (a dead roster link, a farewell \
             announcement, a moved-to note). Inactive retires the agent — it is \
             never a reason to add a row. Then append working booking \
             agents{act_genre_clause} the list misses — a new agent needs a real \
             Email to enter the registry at all. Every row's Contact_Type is \
             exactly \"booking_agent\" — anything else and the intake cannot \
             file it to the agent list. Header:\n{agent_header}\n\n",
            count = agents.len(),
            agent_list = agent_list,
            act_genre_clause = act_genre_clause,
            agent_header = AGENT_SHEET_HEADER.join(", ")
        )
    };
    Some(format!(
        "This is the standing registry — venues, bands and booking agents. \
         Verify every listed entry and write the results back into the matching \
         Google Sheet in Drive; the intake reads each sheet back on its own \
         header.\n\n\
         {venue_block}{act_block}{agent_block}\
         Rules for every sheet: Research_Date is today. A row with no checkable \
         link is a rumour and the intake drops it. Never mark an entry dead on \
         absence of evidence alone — a quiet page is not a closure. Keep every \
         existing row; do not duplicate an entry that is already listed."
    ))
}

/// The agents sheet rides the generic contacts intake — its header is that
/// reader's own vocabulary: `Contact_Type` marks the row a booking agent
/// (the extractor's `role`/`type` contains-match finds it), `Status` is the
/// verification verdict, `Organization` is the agency, and `Notes` carries
/// the evidence link.
const AGENT_SHEET_HEADER: &[&str] = &[
    "Email",
    "Name",
    "Organization",
    "Contact_Type",
    "Status",
    "City",
    "Notes",
    "Research_Date",
];

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
        assert_eq!(parsed.room.status.as_deref(), Some("closed"));
    }

    /// The reopen path: "active" is a claim worth writing — it outranks a
    /// stale `closed` on `observed_at` — while a word outside the vocabulary
    /// is no claim at all.
    #[test]
    fn status_parses_the_two_word_vocabulary_only() {
        for (written, expected) in [
            ("Active", Some("active")),
            ("closed", Some("closed")),
            ("reopened", None),
            ("seasonal", None),
        ] {
            let mut row = progresja();
            row.insert("Status", written);
            let parsed = parse_seed_row(&row).expect("a status word never refuses");
            assert_eq!(parsed.room.status.as_deref(), expected, "Status={written}");
        }
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

    /// 4G.6's done-when: a refusal produces a prompt, and the sheet that comes
    /// back parses without editing. The brief's header is the parser's column
    /// list rendered, so the proof is mechanical — take the header line out of
    /// the prompt verbatim and drive a row built on those names through the
    /// parser.
    #[test]
    fn a_briefs_header_parses_without_editing() {
        let brief = research_brief(
            &ResearchSubject::Rooms,
            "wroclaw",
            Some("metal"),
            Some((150, 600)),
        );
        assert!(brief.contains("wroclaw"));
        assert!(brief.contains("metal"));
        assert!(brief.contains("150–600"));

        // The header line as the operator's tool would echo it back.
        let header_line = brief
            .lines()
            .find(|line| line.contains("Source_URL"))
            .expect("the brief carries the column header");
        let names: Vec<&str> = header_line.split(',').map(str::trim).collect();
        assert_eq!(names, columns::ALL, "the brief drifted from the parser");

        let mut row = SeedRow::new();
        for name in &names {
            row.insert(*name, "x");
        }
        // The three columns the intake refuses without get real values; the
        // rest prove only that the names resolve.
        row.insert(columns::NAME, "Progresja");
        row.insert(columns::CITY, "Warsaw");
        row.insert(columns::SOURCE_URL, "https://example.com/p");
        row.insert(columns::STATUS, "Active");
        row.insert(columns::CAPACITY, "600");
        let parsed = parse_seed_row(&row).expect("the brief's own sheet parses");
        assert_eq!(parsed.room.name, "Progresja");
    }

    /// The verification brief embeds the registry rather than asking for
    /// cities from scratch — verify-then-discover, on the same sheet
    /// contracts the intake parses: rooms, bands and agents each get their
    /// own sheet section.
    #[test]
    fn a_verification_brief_embeds_the_registry() {
        let venues = vec![
            ("Łykend".to_owned(), "Wrocław".to_owned()),
            ("Stodoła".to_owned(), "Warsaw".to_owned()),
            ("Klub B".to_owned(), "Wrocław".to_owned()),
        ];
        let acts = vec![
            ("Hortus".to_owned(), Some("Świdnica".to_owned())),
            ("Alkaloid".to_owned(), None),
        ];
        let agents = vec![(
            "Anna Nowak".to_owned(),
            Some("Mystic".to_owned()),
            "anna@mystic.example.com".to_owned(),
        )];
        let brief = verification_brief(&venues, &acts, &agents, Some("metal"))
            .expect("a held registry yields a brief");
        for (name, city) in &venues {
            assert!(brief.contains(name), "the brief names {name}");
            assert!(brief.contains(city), "the brief names {city}");
        }
        assert!(brief.contains("3 rooms across 2 cities"));
        assert!(brief.contains("\"Closed\"") && brief.contains("\"Active\""));
        assert!(brief.contains("metal"));
        for (name, _) in &acts {
            assert!(brief.contains(name), "the brief names {name}");
        }
        assert!(brief.contains("2 acts"));
        assert!(brief.contains("\"Inactive\""));
        assert!(brief.contains("Anna Nowak (Mystic) <anna@mystic.example.com>"));
        // Same header-parity proof as the research brief — one per sheet.
        let venue_header = brief
            .lines()
            .find(|line| line.contains("Audience_Genre"))
            .expect("the brief carries the venue header");
        let names: Vec<&str> = venue_header.split(',').map(str::trim).collect();
        assert_eq!(names, columns::ALL, "the venue header drifted");
        let band_header = brief
            .lines()
            .find(|line| line.contains("Outreach_Readiness"))
            .expect("the brief carries the band header");
        let names: Vec<&str> = band_header.split(',').map(str::trim).collect();
        assert_eq!(
            names,
            crate::peer_act_seed::columns::ALL,
            "the band header drifted"
        );
        assert!(brief.contains("Contact_Type"));
        // An empty registry is nothing to verify.
        assert_eq!(verification_brief(&[], &[], &[], None), None);
        // A registry holding only bands still briefs.
        assert!(
            verification_brief(&[], &acts, &[], None)
                .expect("bands alone yield a brief")
                .contains("2 acts")
        );
    }

    /// The other refusal shape: the room is known, the route is not. The
    /// brief names the venue rather than asking for rooms in general.
    #[test]
    fn a_contact_refusal_asks_about_the_room_not_the_city() {
        let brief = research_brief(
            &ResearchSubject::BookingContact {
                venue: "Klub X".to_owned(),
            },
            "wroclaw",
            None,
            None,
        );
        assert!(brief.contains("Klub X"));
        assert!(brief.contains("booking contact"));
        assert!(!brief.contains("venues in wroclaw"));
        // No genre was stated and none is invented.
        assert!(brief.contains("band's genre") || !brief.contains("genre"));
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

    /// A grid with no header row at all is not a venue sheet — and neither
    /// is one whose header is a contact list's. `Source_URL` is the column
    /// no contact export carries, which is what makes it the tell.
    #[test]
    fn a_sheet_without_the_seed_header_is_not_a_seed_sheet() {
        assert_eq!(extract_seed_sheet(&[]), None);
        assert_eq!(extract_seed_sheet(&[vec![]]), None);

        // A contact list: email/phone headers, no Source_URL.
        let contacts = vec![
            vec![
                "Name".to_owned(),
                "Email".to_owned(),
                "Phone".to_owned(),
                "City".to_owned(),
            ],
            vec![
                "Somebody".to_owned(),
                "a@b.c".to_owned(),
                "+48".to_owned(),
                "Warsaw".to_owned(),
            ],
        ];
        assert_eq!(extract_seed_sheet(&contacts), None);
    }

    /// The same sheet after a round trip through somebody's spreadsheet:
    /// lowercased headers, a space where the brief wrote an underscore.
    ///
    /// Read strictly, this file is not a venue sheet, falls through to the
    /// contact reader, and every room in it is staged as a press contact —
    /// a misfile an operator then undoes by hand, one row at a time.
    #[test]
    fn a_sheet_whose_headers_were_retyped_is_still_a_venue_sheet() {
        let header = vec![
            "name".to_owned(),
            "city".to_owned(),
            "Source URL".to_owned(),
            "CAPACITY".to_owned(),
        ];
        assert!(is_seed_sheet(&header));

        let grid = vec![
            header,
            vec![
                "Progresja".to_owned(),
                "Warsaw".to_owned(),
                "https://goout.net/en/progresja/vzxueb/".to_owned(),
                "600".to_owned(),
            ],
        ];
        let report = extract_seed_sheet(&grid).expect("a retyped header still reads as a sheet");
        assert_eq!(report.venues.len(), 1);
        assert_eq!(report.venues[0].room.name, "Progresja");
        assert_eq!(report.venues[0].room.capacity, Some(600));
        assert!(report.refusals.is_empty());
    }

    /// A column nobody here knows is dropped rather than carried: the parser
    /// reads by name, and an unknown name has no reader.
    #[test]
    fn an_unknown_column_does_not_become_a_field() {
        assert_eq!(canonical_column("Name"), Some("Name"));
        assert_eq!(canonical_column("  source-url "), Some("Source_URL"));
        assert_eq!(canonical_column("Phone"), None);
        assert_eq!(canonical_column(""), None);
    }

    /// The tell the worker relies on: a venue sheet that also carries an
    /// Email column is still a venue sheet. If the venue check ever ran
    /// second, this grid would stage rooms as contacts.
    #[test]
    fn a_venue_sheet_with_an_email_column_is_still_a_venue_sheet() {
        let mut header: Vec<String> = columns::ALL.iter().map(|s| (*s).to_owned()).collect();
        header.push("Phone".to_owned());
        assert!(is_seed_sheet(&header));

        let mut row: Vec<String> = vec![String::new(); header.len()];
        for (i, cell) in header.iter().enumerate() {
            let value = match cell.as_str() {
                "Name" => "Progresja",
                "City" => "Warsaw",
                "Source_URL" => "https://goout.net/en/progresja/vzxueb/",
                "Email" => "klub@progresja.com",
                _ => "",
            };
            row[i] = value.to_owned();
        }
        let grid = vec![header, row];
        let report = extract_seed_sheet(&grid).expect("a venue sheet with Email parses as venues");
        assert_eq!(report.venues.len(), 1);
        assert_eq!(report.venues[0].room.name, "Progresja");
        assert!(report.refusals.is_empty());
    }

    /// The pinned band header also carries `Name`, `City` and `Source_URL`
    /// — the venue pin alone would claim it and mint bands as rooms. The
    /// band-identity columns are the tell the venue check defers to.
    #[test]
    fn a_band_sheet_is_not_a_venue_sheet() {
        let band_header: Vec<String> = crate::peer_act_seed::columns::ALL
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(!is_seed_sheet(&band_header));
        assert!(crate::peer_act_seed::is_seed_sheet(&band_header));

        // The same guard holds under the deep-scan spelling.
        let deep_scan: Vec<String> = [
            "Country",
            "Band",
            "City / Region",
            "Genre",
            "Activity evidence",
            "Activity source",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert!(!is_seed_sheet(&deep_scan));
    }

    /// A full seed grid: good rows become venues, bad rows keep the sheet's
    /// own row numbers so the operator can find them, and a blank tail row
    /// is neither.
    #[test]
    fn a_seed_grid_reports_venues_and_numbered_refusals() {
        let header: Vec<String> = columns::ALL.iter().map(|s| (*s).to_owned()).collect();
        let cell = |name: &str, value: &str, row: &mut Vec<String>| {
            let i = header.iter().position(|h| h == name).expect("column");
            row[i] = value.to_owned();
        };
        let mut good = vec![String::new(); header.len()];
        cell("Name", "Stodoła", &mut good);
        cell("City", "Warsaw", &mut good);
        cell(
            "Source_URL",
            "https://goout.net/en/stodola/vzgueb/",
            &mut good,
        );

        let mut no_source = vec![String::new(); header.len()];
        cell("Name", "Rumour Club", &mut no_source);
        cell("City", "Warsaw", &mut no_source);

        let blank = vec![String::new(); header.len()];

        let mut no_name = vec![String::new(); header.len()];
        cell("City", "Warsaw", &mut no_name);
        cell("Source_URL", "https://example.com/x", &mut no_name);

        let grid = vec![header, good, no_source, blank, no_name];
        let report = extract_seed_sheet(&grid).expect("the seed header parses");
        assert_eq!(report.venues.len(), 1);
        assert_eq!(report.venues[0].room.name, "Stodoła");
        // Sheet rows: header is 1, good is 2, no_source is 3, blank is
        // skipped entirely, no_name is 5.
        assert_eq!(
            report.refusals,
            vec![
                (3, SeedRefusal::MissingSource),
                (5, SeedRefusal::MissingName),
            ]
        );
    }
}
