//! Reading a researched band sheet into the shared peer-act registry.
//!
//! `place_peer_acts` names a band that is not a tenant — the acts a bill
//! resolves to, the comparable acts a gig proposal cites. Until now the only
//! way in was being billed on a tenant's event or carrying a MusicBrainz id,
//! which is a slow door: a hand-built or researched sheet of local bands is
//! the cheap end to an empty graph, and this module is the door it comes
//! through — the same role `venue_seed` plays for rooms.
//!
//! # Why this is not the contact intake
//!
//! The same argument the venue reader makes: `drive_contacts` keys on an
//! email column, and a band sheet mostly carries none — a name and a social
//! page is still a band. A band sheet that *does* carry an Email column must
//! not stage the band as a press contact for it, so this reader runs before
//! `extract_contacts`, after `venue_seed::extract_seed_sheet` — venue sheets
//! are pinned by `Source_URL` and never carry a band sheet's identity
//! columns, so ordering is the disambiguation, not header guessing.
//!
//! # What is refused
//!
//! Screening on write, as `venue_seed` does it. A row with no name cannot
//! become an act. A row with no link at all — no social page, no website, no
//! source — is a bare name, and a bare name is a rumour: refused, and a
//! dead-band claim is held to the same bar because "inactive" with no
//! evidence is gossip, not a finding.
//!
//! A row marked inactive parses as a `PeerLiveness::Inactive` claim rather
//! than a refusal: the importer writes it as a `status` fact on an act the
//! registry already holds — which is how a verification pass retires a band
//! that has since died — while the same claim on a name nobody holds is
//! skipped, because the registry must not accumulate acts that entered the
//! world already dead.
//!
//! City is deliberately *not* required. A band whose home town is unstated
//! still feeds the peer graph by genre — the home-city claim is a fact that
//! can arrive later, never a gate the row has to pass.
//!
//! # Header contract
//!
//! The pinned header is English (`Name, Country, City, Genre, Email,
//! Social, Website, Links, Source_URL, Activity, Research_Date, Status,
//! Confidence, Contact_Type, Contact_Source, Outreach_Readiness, Notes`),
//! and the parser also reads the spellings the real sheets are written in —
//! the Świdnica cooperation sheet's `nazwa, social` and the deep-scan
//! workbook's `Band, City / Region, Activity evidence, Activity source,
//! Public contact email, Outreach readiness, Confidence`. Matching splits
//! on non-alphanumeric characters — "City / Region", "E-mail" and
//! `city_region` all fold to `city_region` — so punctuation style in a
//! retyped header never matters.

/// The sheet's column names, in header order.
pub mod columns {
    pub const NAME: &str = "Name";
    /// The band's home country as the sheet spells it — "Germany",
    /// "Czechia". Resolved to a code by the importer, which uses it to
    /// constrain the city match: a "Neustadt" in a Czechia row must not
    /// resolve to the German one.
    pub const COUNTRY: &str = "Country";
    /// Home town or home region as stated — the importer attempts a
    /// catalogue resolve on the text and keeps the raw claim as a fact
    /// either way, because a region name like "Saarland" is geography even
    /// when it is not a catalogue city.
    pub const CITY: &str = "City";
    pub const GENRE: &str = "Genre";
    pub const EMAIL: &str = "Email";
    /// The band's own public page — a Facebook, YouTube, Instagram or
    /// Bandcamp profile URL. Doubles as the row's source when no
    /// `Source_URL` is given: the page is where the claim can be checked.
    pub const SOCIAL: &str = "Social";
    pub const WEBSITE: &str = "Website";
    /// A general-purpose link column for sheets that carry several.
    pub const LINKS: &str = "Links";
    /// Where the row's claims can be checked.
    pub const SOURCE_URL: &str = "Source_URL";
    /// The researcher's activity finding, in their words — "2026 activity
    /// verified", "Active — Metal Underground snapshot". A value that names
    /// a dead band (the [`INACTIVE_STATUS`] spellings, contained anywhere in
    /// the prose) marks the row [`PeerLiveness::Inactive`]; any other claim
    /// is stored as the row's `activity` fact so the "still a working band"
    /// decision stays auditable rather than implied.
    pub const ACTIVITY: &str = "Activity";
    pub const RESEARCH_DATE: &str = "Research_Date";
    /// The researcher's liveness verdict: `Active`, `Inactive`, or an
    /// inactive spelling — see [`PeerLiveness`].
    pub const STATUS: &str = "Status";
    /// The researcher's confidence in the row — their judgment, so it lands
    /// in the private half, never the shared registry.
    pub const CONFIDENCE: &str = "Confidence";
    /// How the contact was characterised — "Booking worldwide", "Band /
    /// general". The lead's metadata: private.
    pub const CONTACT_TYPE: &str = "Contact_Type";
    /// Where the researcher found the contact — the lead's trail: private.
    pub const CONTACT_SOURCE: &str = "Contact_Source";
    /// The researcher's readiness verdict — "Email found", "Contact
    /// page/source needed". Private: it is their pipeline state.
    pub const OUTREACH_READINESS: &str = "Outreach_Readiness";
    pub const NOTES: &str = "Notes";

    /// The header row, in the order the contract pins it.
    pub const ALL: &[&str] = &[
        NAME,
        COUNTRY,
        CITY,
        GENRE,
        EMAIL,
        SOCIAL,
        WEBSITE,
        LINKS,
        SOURCE_URL,
        ACTIVITY,
        RESEARCH_DATE,
        STATUS,
        CONFIDENCE,
        CONTACT_TYPE,
        CONTACT_SOURCE,
        OUTREACH_READINESS,
        NOTES,
    ];
}

/// Whether the sheet claims the act is still a working band.
///
/// A dead-band claim is never how an act enters the registry — the importer
/// writes it only onto an act already known, where it retires the band from
/// proposals. An `Active` claim is the reopen: the newest status fact wins
/// on the read side, so a re-verified band lifts its own stale inactive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerLiveness {
    Active,
    Inactive,
}

/// The act, as everyone sees it. Nothing here is specific to one tenant.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeededPeerAct {
    pub name: String,
    /// The band's stated home country, as written — resolved to a code by
    /// the importer, which then uses it to constrain the city match.
    /// `None` means nobody said.
    pub country: Option<String>,
    /// The band's stated home town — a raw sheet value, resolved against
    /// the city catalogue by the importer. `None` means nobody said, which
    /// is not the same as a wrong guess.
    pub city: Option<String>,
    /// Free-form tags split from the sheet's prose — a bias, never a
    /// filter, the same rule `venue_seed::genre_tags` states.
    pub genre_tags: Vec<String>,
    /// The band's own public page. Written as a global `link:social` fact
    /// and, when it names a platform the observer reads, what makes a
    /// confirmed peer watchable from the first sweep.
    pub social: Option<String>,
    pub website: Option<String>,
    /// Where the claim came from, and when. Falls back to the band's own
    /// page when the sheet names no separate source — the row must carry
    /// *something* checkable or it does not parse.
    pub source_url: Option<String>,
    /// The researcher's activity finding in their words — "2026 activity
    /// verified". Written as a global `activity` fact so the working-band
    /// claim carries its evidence rather than being implied by the row's
    /// presence.
    pub activity: Option<String>,
    /// The row's liveness verdict — `Some` only when the sheet actually
    /// claimed one (`Status` filled, or an `Activity` finding), so a bare
    /// name-and-social row invents no claim. `None` writes no status fact.
    pub status: Option<PeerLiveness>,
    pub researched_on: Option<String>,
}

/// One tenant's knowledge of the act. Never shared, never global — the same
/// split `SeededRoomView` makes for rooms.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeededPeerActView {
    /// A contact address the sheet carried. Contact is never global: the
    /// address is the contributing tenant's lead, not the platform's.
    pub email: Option<String>,
    /// How the researcher characterised the contact ("Booking worldwide").
    pub contact_type: Option<String>,
    /// Where they found it — the lead's trail.
    pub contact_source: Option<String>,
    /// Their readiness verdict ("Email found", "Contact page/source
    /// needed") — the importing tenant's pipeline state.
    pub outreach_readiness: Option<String>,
    /// Their confidence in the row ("High", "Medium", "Low").
    pub confidence: Option<String>,
    pub notes: Option<String>,
}

/// A parsed row: the shared half and the private half, already separated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededPeer {
    pub act: SeededPeerAct,
    pub view: SeededPeerActView,
}

/// Why a row was not usable. Each names the column so a sheet can be fixed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerSeedRefusal {
    MissingName,
    /// A name with no checkable link — no social page, no website, no
    /// source — is a rumour, the same rule venue rows are held to. A dead-
    /// band claim is held to it too: "inactive" with no evidence is gossip,
    /// not a finding.
    MissingLink,
}

impl PeerSeedRefusal {
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::MissingName => "a row with no band name cannot become a peer act",
            Self::MissingLink => {
                "a band with no social page, website or source link cannot be checked"
            }
        }
    }
}

/// The spellings of "not a working band anymore" — English and Polish.
/// Status matches exactly; the Activity column is prose and matches on
/// containment ("Inactive — no shows since 2022" still drops the row).
const INACTIVE_STATUS: &[&str] = &[
    "inactive",
    "dead",
    "hiatus",
    "split",
    "disbanded",
    "frozen",
    "rozwiązany",
    "rozwiazany",
    "nieaktywny",
    "zawieszony",
];

/// The spellings of "still a working band" a Status column may carry —
/// English and Polish. Status matches exactly; the Activity column's prose
/// is itself the active claim, no word list needed.
const ACTIVE_STATUS: &[&str] = &["active", "aktywny", "reunited"];

fn clean(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") {
        return None;
    }
    Some(value.to_owned())
}

/// One field set from a sheet row, keyed by the sheet's own column names.
pub type SeedRow<'a> = std::collections::BTreeMap<&'a str, &'a str>;

/// Parses one researched row into its shared and private halves.
///
/// # Errors
///
/// Refuses a row missing a name or carrying no checkable link. An inactive
/// verdict is not an error — it parses as `status: Some(Inactive)` and the
/// importer decides what it means for an act already held.
pub fn parse_seed_row(row: &SeedRow<'_>) -> Result<SeededPeer, PeerSeedRefusal> {
    // Keys arrive as the sheet spelled them — canonicalize once here so a
    // caller that skipped the header pass still parses. `extract_seed_sheet`
    // already hands canonical names; `canonical_column` maps those to
    // themselves, so the double pass is identity, not drift. First spelling
    // of a duplicated canonical column wins, matching the header pass.
    let mut canonical: SeedRow<'_> = SeedRow::new();
    for (key, cell) in row {
        if let Some(name) = canonical_column(key) {
            canonical.entry(name).or_insert(cell);
        }
    }
    let get = |key: &str| clean(canonical.get(key).copied());

    let name = get(columns::NAME).ok_or(PeerSeedRefusal::MissingName)?;

    // Two cells can carry the "still a working band" verdict, with
    // different grammars: a Status column is a one-word verdict and matches
    // exactly, while an Activity column is prose ("2026 activity verified",
    // "Active — Metal Underground snapshot") where the dead spelling sits
    // inside a sentence — so activity matches on containment, status on
    // equality. A "hiatus ended" status surviving is worth more than a dead
    // band slipping through a substring hole.
    //
    // The verdict stays a claim on the row rather than a refusal: the
    // importer retires a known act on `Inactive` and refuses to mint a dead
    // one, while `Active` is the claim that re-opens a stale inactive.
    // `None` means the sheet made no claim — a name-and-social row is a
    // band, not a liveness verdict.
    let status = get(columns::STATUS);
    let activity = get(columns::ACTIVITY);
    let liveness = if status
        .as_deref()
        .is_some_and(|s| INACTIVE_STATUS.contains(&s.to_lowercase().as_str()))
        || activity
            .as_deref()
            .is_some_and(|a| INACTIVE_STATUS.iter().any(|d| a.to_lowercase().contains(d)))
    {
        Some(PeerLiveness::Inactive)
    } else if status
        .as_deref()
        .is_some_and(|s| ACTIVE_STATUS.contains(&s.to_lowercase().as_str()))
        || activity.is_some()
    {
        // An explicit "active" verdict, or any activity finding that isn't
        // a dead spelling — "2026 activity verified" is a working-band
        // claim. An unrecognised Status word ("unclear") claims nothing.
        Some(PeerLiveness::Active)
    } else {
        None
    };

    // One social cell may hold several pasted links, and a Links column is
    // the same thing by another name — keep every link the row carries.
    let mut links: Vec<String> = Vec::new();
    for key in [columns::SOCIAL, columns::LINKS] {
        if let Some(raw) = canonical.get(key) {
            links.extend(
                raw.split([',', ';'])
                    .filter_map(|part| clean(Some(part)))
                    .filter(|part| part.contains('.')),
            );
        }
    }
    links.dedup();
    let social = links.first().cloned();

    let website = get(columns::WEBSITE);
    let source_url = get(columns::SOURCE_URL);
    // The row needs one place a claim can be checked. An email alone does
    // not make the band checkable — a name plus an inbox is a contact lead,
    // and the contact intake owns that shape.
    if social.is_none() && website.is_none() && source_url.is_none() {
        return Err(PeerSeedRefusal::MissingLink);
    }

    Ok(SeededPeer {
        act: SeededPeerAct {
            name,
            country: get(columns::COUNTRY),
            city: get(columns::CITY),
            genre_tags: genre_tags(canonical.get(columns::GENRE).copied()),
            social,
            website,
            source_url,
            activity,
            status: liveness,
            researched_on: get(columns::RESEARCH_DATE),
        },
        view: SeededPeerActView {
            email: get(columns::EMAIL),
            contact_type: get(columns::CONTACT_TYPE),
            contact_source: get(columns::CONTACT_SOURCE),
            outreach_readiness: get(columns::OUTREACH_READINESS),
            confidence: get(columns::CONFIDENCE),
            notes: get(columns::NOTES),
        },
    })
}

/// Splits a sheet's genre prose into tags — the same separator rule
/// `venue_seed::genre_tags` applies, kept as a copy rather than a cross-
/// module call because the two sheets' columns are different contracts.
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

/// Whether a header row is the band sheet this module parses.
///
/// A band sheet is a `Name`-ish column plus at least one identity column a
/// contact list never carries: `Social`, `Links` or `Genre`. Venue sheets
/// never reach this check — the caller tries `venue_seed` first — so the
/// rule does not have to defend against them, only against ordinary contact
/// lists, which carry `Email` but none of the identity columns.
#[must_use]
pub fn is_seed_sheet(header: &[String]) -> bool {
    let has = |names: &[&str]| {
        header
            .iter()
            .any(|cell| canonical_column(cell).is_some_and(|c| names.contains(&c)))
    };
    has(&[columns::NAME]) && has(&[columns::SOCIAL, columns::LINKS, columns::GENRE])
}

/// The column this header cell names, if it names one.
///
/// Canonical names first, then the spellings the operator's sheets actually
/// use — `nazwa`, `miasto`, `gatunek`, `social`, `linki`, `źródło`,
/// `notatki`, and the deep-scan workbook's `Band`, `City / Region`,
/// `Activity evidence`, `Activity source`, `Public contact email` — so the
/// real seeds parse without a translation pass.
#[must_use]
pub fn canonical_column(cell: &str) -> Option<&'static str> {
    // Tokens split on any non-letter/digit — "City / Region", "E-mail" and
    // "source_url" all fold to the same joined form, so punctuation style
    // in a retyped header never matters.
    let normalise = |value: &str| {
        value
            .trim()
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_")
    };
    let cell = normalise(cell);
    if let Some(name) = columns::ALL
        .iter()
        .copied()
        .find(|name| normalise(name) == cell)
    {
        return Some(name);
    }
    match cell.as_str() {
        "nazwa" | "zespol" | "zespoł" | "band" | "zespół" | "artist" => Some(columns::NAME),
        "kraj" | "country_code" => Some(columns::COUNTRY),
        "miasto" | "miejscowosc" | "miejscowość" | "city_region" | "region" | "city_country" => {
            Some(columns::CITY)
        }
        "gatunek" | "gatunki" | "styl" | "style" => Some(columns::GENRE),
        "mail" | "e_mail" | "public_contact_email" | "contact_email" => Some(columns::EMAIL),
        "fb" | "facebook" | "instagram" | "ig" => Some(columns::SOCIAL),
        "linki" | "link" | "url" | "urls" => Some(columns::LINKS),
        "strona" | "strona_www" | "www" | "homepage" => Some(columns::WEBSITE),
        "zrodlo" | "źródło" | "source" | "activity_source" | "evidence_source" => {
            Some(columns::SOURCE_URL)
        }
        "activity_evidence" | "aktywnosc" | "aktywność" | "activity_2026" => {
            Some(columns::ACTIVITY)
        }
        "data" | "checked" | "research_date" => Some(columns::RESEARCH_DATE),
        "notatki" | "uwagi" | "comment" => Some(columns::NOTES),
        _ => None,
    }
}

/// What one grid yielded: the acts that parsed and the rows that did not.
///
/// Refusals keep their 1-based sheet row number — the number a spreadsheet
/// shows its operator — so a refusal reads "row 17 has no checkable link",
/// not "record 15 failed".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PeerActSeedReport {
    pub acts: Vec<SeededPeer>,
    /// (1-based sheet row number, refusal) so the operator can fix the sheet.
    pub refusals: Vec<(usize, PeerSeedRefusal)>,
}

/// Reads one whole grid as a band sheet, or declines it.
///
/// `None` means the header is not the seed sheet's — the file is somebody
/// else's table and the caller should try its other readers. `Some` means
/// the header matched, and every non-empty row below it was either parsed
/// into an act or refused with a reason that names the column problem.
/// Pure: no IO, no clock — what the sheet said is the whole input.
#[must_use]
pub fn extract_seed_sheet(grid: &[Vec<String>]) -> Option<PeerActSeedReport> {
    let (header, rows) = grid.split_first()?;
    if !is_seed_sheet(header) {
        return None;
    }
    // Column name → cell index, read off the sheet's own header so a sheet
    // with extra or reordered columns still parses. The first occurrence of
    // a name wins — a sheet that repeats a header is ambiguous, not richer.
    let mut index: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        if let Some(name) = canonical_column(cell) {
            index.entry(name).or_insert(i);
        }
    }
    let mut report = PeerActSeedReport::default();
    for (offset, row) in rows.iter().enumerate() {
        // A row of only whitespace is the tail of an edited sheet, not a
        // band missing everything.
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let seed_row: SeedRow<'_> = index
            .iter()
            .filter_map(|(name, i)| row.get(*i).map(|cell| (*name, cell.as_str())))
            .collect();
        match parse_seed_row(&seed_row) {
            Ok(act) => report.acts.push(act),
            // offset counts from the first data row; +2 lands on the sheet's
            // own row number (header is row 1, first data row is row 2).
            Err(refusal) => report.refusals.push((offset + 2, refusal)),
        }
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row in the shape the real seed carries — the Świdnica cooperation
    /// sheet is a name and a social page and nothing else.
    fn swidnica_row() -> SeedRow<'static> {
        SeedRow::from([
            ("nazwa", "Johnny Trzy Palce"),
            ("social", "https://www.facebook.com/johnnytrzypalce"),
        ])
    }

    /// A fully-researched row in the pinned English header.
    fn researched_row() -> SeedRow<'static> {
        SeedRow::from([
            ("Name", "Hortus"),
            ("City", "Świdnica"),
            ("Genre", "hard rock / heavy metal"),
            ("Email", "hortus@example.com"),
            ("Social", "https://www.facebook.com/hortus"),
            ("Website", "https://hortus.example.com"),
            (
                "Source_URL",
                "https://swidnica24.pl/2022/02/swidnickie-zespoly-zagraja-rocka-w-klubie-bolko/",
            ),
            ("Research_Date", "2026-09-19"),
            ("Status", "Active"),
            ("Notes", "played Bolko twice"),
        ])
    }

    /// A row in the deep-scan workbook's Master-sheet spelling — the feed
    /// that motivated the multi-sheet reader.
    fn deep_scan_row() -> SeedRow<'static> {
        SeedRow::from([
            ("Country", "Germany"),
            ("Band", "Alkaloid"),
            ("City / Region", "Erlangen"),
            ("Genre", "Progressive/Technical Death Metal"),
            ("Activity evidence", "2026 activity verified"),
            (
                "Activity source",
                "https://www.metalunderground.com/bands/country/Germany/",
            ),
            ("Outreach readiness", "Contact page/source needed"),
            ("Confidence", "Medium"),
        ])
    }

    #[test]
    fn a_name_and_a_social_page_is_a_band() {
        let parsed = parse_seed_row(&swidnica_row()).expect("row parses");
        assert_eq!(parsed.act.name, "Johnny Trzy Palce");
        assert_eq!(
            parsed.act.social.as_deref(),
            Some("https://www.facebook.com/johnnytrzypalce")
        );
        // Nobody stated a city or a genre — NULL, never a guess.
        assert_eq!(parsed.act.city, None);
        assert!(parsed.act.genre_tags.is_empty());
    }

    #[test]
    fn the_operators_email_and_notes_do_not_reach_the_shared_act() {
        let parsed = parse_seed_row(&researched_row()).expect("row parses");
        assert_eq!(parsed.act.city.as_deref(), Some("Świdnica"));
        assert_eq!(parsed.act.genre_tags, vec!["hard rock", "heavy metal"]);
        assert_eq!(parsed.view.email.as_deref(), Some("hortus@example.com"));
        assert_eq!(parsed.view.notes.as_deref(), Some("played Bolko twice"));
    }

    #[test]
    fn the_deep_scan_spelling_parses_with_its_evidence() {
        let parsed = parse_seed_row(&deep_scan_row()).expect("row parses");
        assert_eq!(parsed.act.name, "Alkaloid");
        assert_eq!(parsed.act.country.as_deref(), Some("Germany"));
        assert_eq!(parsed.act.city.as_deref(), Some("Erlangen"));
        assert_eq!(
            parsed.act.genre_tags,
            vec!["progressive", "technical death metal"]
        );
        // The activity claim survives as auditable evidence…
        assert_eq!(
            parsed.act.activity.as_deref(),
            Some("2026 activity verified")
        );
        // …and its source becomes the row's checkable link, because the
        // feed carries no per-band page.
        assert_eq!(
            parsed.act.source_url.as_deref(),
            Some("https://www.metalunderground.com/bands/country/Germany/")
        );
        // The researcher's verdicts stay in the private half.
        assert_eq!(
            parsed.view.outreach_readiness.as_deref(),
            Some("Contact page/source needed")
        );
        assert_eq!(parsed.view.confidence.as_deref(), Some("Medium"));
    }

    #[test]
    fn a_bare_name_is_a_rumour_not_a_band() {
        let mut row = swidnica_row();
        row.remove("social");
        assert_eq!(parse_seed_row(&row), Err(PeerSeedRefusal::MissingLink));
        // An email alone does not make the row checkable either.
        row.insert("Email", "band@example.com");
        assert_eq!(parse_seed_row(&row), Err(PeerSeedRefusal::MissingLink));
    }

    #[test]
    fn a_dead_band_parses_as_an_inactive_claim() {
        for status in [
            "inactive",
            "hiatus",
            "disbanded",
            "rozwiązany",
            "Nieaktywny",
        ] {
            let mut row = swidnica_row();
            row.insert("Status", status);
            assert_eq!(
                parse_seed_row(&row)
                    .expect("a dead-band claim still parses")
                    .act
                    .status,
                Some(PeerLiveness::Inactive),
                "status {status:?}"
            );
        }
        let mut live = swidnica_row();
        live.insert("Status", "active");
        assert_eq!(
            parse_seed_row(&live).expect("row parses").act.status,
            Some(PeerLiveness::Active)
        );
        // A verdict nobody recognises claims nothing either way.
        let mut unclear = swidnica_row();
        unclear.insert("Status", "unclear");
        assert_eq!(
            parse_seed_row(&unclear).expect("row parses").act.status,
            None
        );
    }

    #[test]
    fn an_activity_finding_of_dead_is_an_inactive_claim() {
        let mut row = deep_scan_row();
        row.insert(
            "Activity evidence",
            "Inactive — no shows or releases since 2022",
        );
        assert_eq!(
            parse_seed_row(&row)
                .expect("a dead-band claim still parses")
                .act
                .status,
            Some(PeerLiveness::Inactive)
        );
    }

    #[test]
    fn a_links_column_and_comma_joined_socials_all_parse() {
        let mut row = SeedRow::new();
        row.insert("Name", "RaF");
        row.insert(
            "Linki",
            "https://facebook.com/raf, https://youtube.com/@raf",
        );
        let parsed = parse_seed_row(&row).expect("row parses");
        assert_eq!(
            parsed.act.social.as_deref(),
            Some("https://facebook.com/raf")
        );
    }

    #[test]
    fn the_polish_header_detects_the_sheet() {
        let grid = vec![
            vec!["lp".to_owned(), "nazwa".to_owned(), "social".to_owned()],
            vec![
                "1".to_owned(),
                "30zeta".to_owned(),
                "https://www.facebook.com/30zeta".to_owned(),
            ],
        ];
        let report = extract_seed_sheet(&grid).expect("the real sheet parses");
        assert_eq!(report.acts.len(), 1);
        assert!(report.refusals.is_empty());
        assert_eq!(report.acts[0].act.name, "30zeta");
    }

    #[test]
    fn the_deep_scan_header_detects_the_sheet() {
        let header: Vec<String> = [
            "Country",
            "Band",
            "City / Region",
            "Genre",
            "Activity evidence",
            "Activity source",
            "Public contact email",
            "Contact type",
            "Contact source",
            "Outreach readiness",
            "Confidence",
            "Notes",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert!(is_seed_sheet(&header));
        // The workbook's summary sheet does not match.
        assert!(!is_seed_sheet(&[
            "Metric".to_owned(),
            "Value".to_owned(),
            "Research signal".to_owned(),
            "Count".to_owned()
        ]));
    }

    #[test]
    fn a_contact_list_is_not_a_band_sheet() {
        let header = vec!["Name".to_owned(), "Email".to_owned(), "City".to_owned()];
        assert!(!is_seed_sheet(&header));
    }

    #[test]
    fn refusals_carry_the_sheet_row_number() {
        let grid = vec![
            vec!["nazwa".to_owned(), "social".to_owned(), "Status".to_owned()],
            vec![
                "Living".to_owned(),
                "https://fb.com/living".to_owned(),
                "active".to_owned(),
            ],
            vec![
                "Gone".to_owned(),
                "https://fb.com/gone".to_owned(),
                "disbanded".to_owned(),
            ],
            vec!["Ghost".to_owned(), String::new(), String::new()],
        ];
        let report = extract_seed_sheet(&grid).expect("sheet parses");
        assert_eq!(report.acts.len(), 2);
        assert_eq!(report.acts[1].act.status, Some(PeerLiveness::Inactive));
        assert_eq!(report.refusals, vec![(4, PeerSeedRefusal::MissingLink)]);
    }
}
