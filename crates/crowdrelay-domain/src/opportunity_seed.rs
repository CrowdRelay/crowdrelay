//! Reading the scout workbooks' `OPPORTUNITIES` tabs into
//! `team_opportunities`.
//!
//! Four workbooks keep an opportunities tab, in four grammars:
//!
//! | Source | Dialect | Discriminating headers |
//! |---|---|---|
//! | `VIRYA_MASTER.xlsx` | master CRM | `Opportunity_ID` + `Entity_ID` + `Opportunity_Name` |
//! | `SCOUT AUTO.xlsx` | n8n review queue | `Opportunity_ID` + `Dedupe_Key` + `Title` |
//! | `database_festivals.xlsx` (SCOUT_PL) | Polish scout snapshot | `Organizer` + `Application_Deadline` + `Current_Status` |
//! | `SCOUT.xlsx` + `MARCIN_FEED` | human scout file | `Name` + `Dedupe Key` + `Submission Method`/`Why Fit` |
//!
//! The venue reader's pin (`Name` + `City` + `Source_URL`) would claim the
//! SCOUT_PL grammar and mint festivals as rooms — the caller dispatches this
//! reader ahead of it, the same reason the agent and beacon readers run
//! before the registry-dump guard.
//!
//! # Cross-source identity
//!
//! `team_opportunities` dedupes on `(workspace_id, source, external_key)`.
//! Rows for the same opportunity arrive from several files, so `source` is
//! one constant (`scout_sheet`) and `external_key` is the sheet's own
//! destination, not its per-sheet bookkeeping:
//!
//!   1. `url:` + normalized destination URL — the language every grammar
//!      shares, and the link the FLOW_MAP dedupe rule puts first
//!      (`URL + tytuł + organizator`);
//!   2. else `name:` + normalized title + `|` + normalized city — a URL-less
//!      row still converges when two files describe it the same way.
//!
//! Sheet-local keys (`Dedupe_Key`, `Opportunity_ID`, `Source_ID`) travel in
//! `metadata` — they reconcile the row back to its sheet but are never the
//! conflict key, because each sheet mints them in its own namespace and the
//! same opportunity would land twice.
//!
//! # What stays with the operator
//!
//! `status`, `eligible` and `metadata` are written only on insert — the
//! sheet asserts an opportunity exists; what CrowdRelay did with it since
//! belongs to the loop. A stale snapshot re-run refreshes contact details
//! and deadlines, never reopens a dismissed row.

use time::Date;

use crate::drive_contacts::ExtractedContact;

/// Which grammar claimed the sheet. Kept on the report for metadata — the
/// dialect itself is evidence of which system asserted the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpportunityDialect {
    /// `VIRYA_MASTER` — the canonical CRM's opportunity register.
    Master,
    /// `SCOUT AUTO` — the n8n automation's review queue.
    ScoutAuto,
    /// `database_festivals.xlsx` — the SCOUT_PL run snapshot.
    ScoutPl,
    /// `SCOUT.xlsx OPPORTUNITIES` and SCOUT AUTO's `MARCIN_FEED` — same
    /// columns, same dedupe vocabulary.
    ScoutEn,
}

impl OpportunityDialect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::ScoutAuto => "scout_auto",
            Self::ScoutPl => "scout_pl",
            Self::ScoutEn => "scout_en",
        }
    }
}

/// One parsed `OPPORTUNITIES` row, normalised for `team_opportunities`.
#[derive(Clone, Debug, PartialEq)]
pub struct SeededOpportunity {
    /// The conflict key — `url:` + normalized destination, or
    /// `name:` + normalized title + city when the row carries no link.
    pub external_key: String,
    /// A `team_opportunities` CHECK value — `map_kind` has already folded
    /// the sheet's vocabulary onto it.
    pub kind: &'static str,
    /// What the sheet's Type/Category/Opportunity_Type cell actually said.
    pub raw_kind: String,
    pub title: String,
    pub organization: String,
    pub contact_email: Option<String>,
    pub destination_url: Option<String>,
    /// Two-letter ISO, or absent — the CHECK accepts `^[A-Z]{2}$` only, so
    /// a country the map does not know stays unclaimed, not guessed.
    pub country_code: Option<String>,
    /// Free text at this stage — `cities` resolution is the repository's.
    pub city: Option<String>,
    pub fit_basis_points: i32,
    pub confidence_basis_points: i32,
    pub strategic_value_basis_points: i32,
    pub deadline: Option<Date>,
    pub event_starts_on: Option<Date>,
    /// Verified one-way road distance supplied by the operator/scout. Absent
    /// stays absent: the intake never substitutes straight-line distance or a
    /// band average for a road-cost fact.
    pub distance_km: Option<i32>,
    /// Stated nights away when known. Otherwise tour economics applies its own
    /// overnight-threshold policy once distance is known.
    pub nights_away: Option<i16>,
    /// `false` when the sheet itself reports the route resolved or closed.
    pub eligible: bool,
    /// Only when the sheet carries an explicit verification verdict.
    pub verified_destination: bool,
    /// Every sheet column no field claimed, folded into `metadata.intake`.
    pub metadata: serde_json::Value,
}

/// Why a row did not seed. Each names the problem so the sheet can be
/// fixed — a refusal is a sheet edit away from importing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpportunityRefusal {
    /// No title — nothing to display, nothing to dedupe on.
    MissingTitle,
    /// A `Type`/`Category`/`Opportunity_Type` the CHECK vocabulary does not
    /// cover and the map does not name. Carries the raw value so a new kind
    /// reads as a vocabulary decision, not a parse failure.
    UnmappedKind(String),
}

impl OpportunityRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingTitle => "a row with no title cannot become an opportunity".to_owned(),
            Self::UnmappedKind(kind) => {
                format!(
                    "opportunity kind '{kind}' is not one the registry knows — map it or rename it in the sheet"
                )
            }
        }
    }
}

/// What one grid yielded: the rows that parsed, the rows refused, and the
/// contact-shaped cells worth staging into `drive_contacts` — a scout row's
/// `Public Contact`/`Contact_Email` is a lead the contact path should
/// dedupe against every other file, not a detail lost inside the row.
#[derive(Clone, Debug, Default)]
pub struct OpportunitySeedReport {
    pub dialect: Option<OpportunityDialect>,
    pub rows: Vec<SeededOpportunity>,
    /// (1-based sheet row, refusal) — the number the spreadsheet shows.
    pub refusals: Vec<(usize, OpportunityRefusal)>,
    pub contacts: Vec<ExtractedContact>,
}

// ---------------------------------------------------------------------------
// Header recognition
// ---------------------------------------------------------------------------

/// Normalise a header cell the same way the sibling readers do: case,
/// spaces, slashes and hyphens all fold to one `snake_case` token, so
/// `Country / City` and `country_city` and `Country-City` name one column.
fn normalise(cell: &str) -> String {
    cell.trim()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn dialect_for(header: &[String]) -> Option<OpportunityDialect> {
    let has = |needle: &str| header.iter().any(|cell| normalise(cell) == needle);
    if has("opportunity_id") && has("entity_id") && has("opportunity_name") {
        Some(OpportunityDialect::Master)
    } else if has("opportunity_id") && has("dedupe_key") && has("title") {
        Some(OpportunityDialect::ScoutAuto)
    } else if has("organizer") && has("application_deadline") && has("current_status") {
        Some(OpportunityDialect::ScoutPl)
    } else if has("name") && has("dedupe_key") && (has("submission_method") || has("why_fit")) {
        Some(OpportunityDialect::ScoutEn)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Cell parsing helpers
// ---------------------------------------------------------------------------

fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("n/a") || value == "-" {
        return None;
    }
    Some(value.to_owned())
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

fn email_of(value: &str) -> Option<String> {
    let value = value.trim();
    looks_emailish(value).then(|| value.to_owned())
}

fn url_of(value: &str) -> Option<String> {
    let value = value.trim();
    (value.starts_with("http://") || value.starts_with("https://")).then(|| value.to_owned())
}

/// The first `YYYY-MM-DD` inside a cell. Scout date cells range from a bare
/// ISO date to `2026-09-25 to 2026-09-27; 2027 date TBA` — the earliest date
/// is the conservative read (an event that *starts* then, a deadline the
/// sheet led with). A cell with no ISO date claims nothing.
fn first_iso_date(value: &str) -> Option<Date> {
    let bytes = value.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    for (start, byte) in bytes.iter().enumerate().take(bytes.len() - 9) {
        if !byte.is_ascii_digit() {
            continue;
        }
        // `get` returns None on a non-char boundary — multibyte cells skip
        // cleanly rather than panic.
        let Some(window) = value.get(start..start + 10) else {
            continue;
        };
        let looks_iso = window.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
        if looks_iso
            && let Ok(date) = Date::parse(
                window,
                &time::macros::format_description!("[year]-[month]-[day]"),
            )
        {
            return Some(date);
        }
    }
    None
}

/// 0–100 sheet score → basis points, `Option` because a blank cell asserts
/// nothing. Non-numeric claims nothing either — a `n/a` is not a zero.
fn score_basis_points(value: &str) -> Option<i32> {
    let value: f64 = value.trim().parse().ok()?;
    Some((value * 100.0).round().clamp(0.0, 10000.0) as i32)
}

fn bounded_i32(value: &str, max: i32) -> Option<i32> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed: i32 = value.parse().ok()?;
    (0..=max).contains(&parsed).then_some(parsed)
}

fn bounded_i16(value: &str, max: i16) -> Option<i16> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed: i16 = value.parse().ok()?;
    (0..=max).contains(&parsed).then_some(parsed)
}

/// The band's A–D priority column → `strategic_value_basis_points`, on the
/// same tiers `import_opportunities` uses so one column means one thing
/// whichever route the row arrived by.
fn letter_priority(value: &str) -> Option<i32> {
    match value.trim().to_ascii_uppercase().as_str() {
        "A" => Some(9_000),
        "B" => Some(6_500),
        "C" => Some(4_000),
        "D" => Some(1_500),
        _ => None,
    }
}

/// The master CRM's HIGH/MEDIUM/LOW vocabulary.
fn word_priority(value: &str) -> Option<i32> {
    match value.trim().to_ascii_uppercase().as_str() {
        "HIGH" => Some(7_500),
        "MEDIUM" => Some(5_000),
        "LOW" => Some(2_500),
        _ => None,
    }
}

/// Sheet statuses that mean "this route is resolved or spoken for" — a
/// closed edition, a rejection, or an application already sent. They mark
/// the row ineligible on insert; `new` status is unchanged because the loop
/// still has to see the finding.
const INELIGIBLE_STATUS_WORDS: &[&str] = &[
    "closed",
    "zamknięte",
    "zamkniete",
    "rejected",
    "odrzucone",
    "submitted",
    "zgłoszone",
    "zgloszone",
    // The human SCOUT sheet uses this after a manual form/email was already
    // submitted. Importing it as eligible would let the live-opportunity
    // loop propose the same application again before the reply arrives.
    "czeka na odpowiedź",
    "czeka na odpowiedz",
    // A blocked route is deliberately not actionable even when every other
    // field still looks like a strong opportunity.
    "blocked",
    "done",
    "nieaktualne",
    "merged",
    "archived",
    "expired",
];

fn eligible_for(status: &str) -> bool {
    let status = status.trim().to_lowercase();
    !INELIGIBLE_STATUS_WORDS
        .iter()
        .any(|word| status.contains(word))
}

/// Country names the sheets actually write → ISO-3166 alpha-2. A name the
/// map does not know returns `None`: `country_code` takes `^[A-Z]{2}$` and
/// a wrong two letters is worse than none.
fn country_code_for(value: &str) -> Option<String> {
    let raw = value.trim();
    if raw.len() == 2 && raw.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(raw.to_ascii_uppercase());
    }
    let code = match raw.to_lowercase().as_str() {
        "poland" | "polska" => "PL",
        "germany" | "niemcy" | "deutschland" => "DE",
        "czechia" | "czech republic" | "czechy" => "CZ",
        "slovakia" | "słowacja" | "slowacja" => "SK",
        "united kingdom" | "uk" | "great britain" | "wielka brytania" | "england" => "GB",
        "austria" => "AT",
        "netherlands" | "holandia" => "NL",
        "france" | "francja" => "FR",
        "spain" | "hiszpania" => "ES",
        "italy" | "włochy" | "wlochy" => "IT",
        "sweden" | "szwecja" => "SE",
        "norway" | "norwegia" => "NO",
        "denmark" | "dania" => "DK",
        "finland" | "finlandia" => "FI",
        "belgium" | "belgia" => "BE",
        "switzerland" | "szwajcaria" => "CH",
        "ireland" | "irlandia" => "IE",
        "estonia" => "EE",
        "latvia" | "łotwa" | "lotwa" => "LV",
        "lithuania" | "litwa" => "LT",
        "hungary" | "węgry" | "wegry" => "HU",
        "croatia" | "chorwacja" => "HR",
        "ukraine" | "ukraina" => "UA",
        "romania" | "rumunia" => "RO",
        "bulgaria" | "bułgaria" => "BG",
        "greece" | "grecja" => "GR",
        "turkey" | "turcja" => "TR",
        "united states" | "usa" | "us" => "US",
        "canada" | "kanada" => "CA",
        "portugal" | "portugalia" => "PT",
        "japan" | "japonia" => "JP",
        "australia" => "AU",
        _ => return None,
    };
    Some(code.to_owned())
}

/// The `team_opportunities` kind vocabulary onto which the sheets' own
/// kind words fold. Order matters — `CONTEST / FESTIVAL` is a band contest
/// at a festival, so the more precise `review_contest` wins the overlap.
#[must_use]
pub fn opportunity_kind_for(raw: &str) -> Option<&'static str> {
    let text = raw.trim().to_lowercase();
    let kind = if text.contains("showcase") {
        "showcase"
    } else if text.contains("contest")
        || text.contains("konkurs")
        || text.contains("competition")
        || text.contains("przegląd")
        || text.contains("przeglad")
    {
        "review_contest"
    } else if text.contains("support") {
        "support_slot"
    } else if text.contains("fund") || text.contains("grant") {
        "funding"
    } else if text.contains("booking")
        || text.contains("venue")
        || text.contains("club")
        || text.contains("koncert")
        || text.contains("gig")
    {
        "booking"
    } else if text.contains("interview") || text.contains("wywiad") {
        "interview"
    } else if text.contains("festiv") {
        "festival"
    } else if text.contains("press")
        || text.contains("media")
        || text.contains("radio")
        || text.contains("podcast")
        || text.contains("zine")
        || text.contains("recenzja")
        || text.contains("review")
    {
        "press"
    } else if text.contains("sync") {
        "sync"
    } else {
        return None;
    };
    Some(kind)
}

// ---------------------------------------------------------------------------
// Cross-source identity
// ---------------------------------------------------------------------------

/// ASCII-fold the handful of diacritics these sheets produce, so the same
/// title spelled `Żmigrock` in one file and `Zmigrock` in another lands on
/// one slug. This is a dedupe key, not display text.
fn ascii_fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ą' => 'a',
            'ć' => 'c',
            'ę' => 'e',
            'ł' => 'l',
            'ń' => 'n',
            'ó' => 'o',
            'ś' => 's',
            'ź' | 'ż' => 'z',
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            'ß' => 's',
            'é' | 'è' | 'ë' => 'e',
            'á' | 'à' | 'â' => 'a',
            'í' | 'ì' | 'î' => 'i',
            'ú' | 'ù' | 'û' => 'u',
            'č' => 'c',
            'ď' => 'd',
            'ě' => 'e',
            'ň' => 'n',
            'ř' => 'r',
            'š' => 's',
            'ť' => 't',
            'ů' => 'u',
            'ý' => 'y',
            'ž' => 'z',
            other => other,
        })
        .collect()
}

/// Lowercase, ASCII-fold, collapse every non-alphanumeric run to `-`.
fn slug(text: &str) -> String {
    ascii_fold(&text.to_lowercase())
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalise a destination URL into its dedupe shape: lowercase, scheme and
/// `www.` gone, query and fragment dropped (a tracking suffix is not a
/// different route), trailing slashes folded. `euroblast.net/en/contact`
/// is the same page however the sheet spelled it.
fn normalise_url(value: &str) -> Option<String> {
    let trimmed = value.trim().to_lowercase();
    let mut url = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?
        .to_owned();
    if let Some(rest) = url.strip_prefix("www.") {
        url = rest.to_owned();
    }
    if let Some(cut) = url.find(['?', '#']) {
        url.truncate(cut);
    }
    let url = url.trim_end_matches('/').to_owned();
    if url.is_empty() { None } else { Some(url) }
}

/// The conflict key. Destination URL first — it is the only identity every
/// grammar shares and the same opportunity listed in MASTER, SCOUT and the
/// GitHub snapshot carries the same application link. A URL-less row keys
/// on its title plus city so `KozyNostra` in two files still converges.
fn external_key(title: &str, url: Option<&str>, city: Option<&str>) -> String {
    if let Some(key) = url.and_then(normalise_url) {
        return format!("url:{key}");
    }
    let city = city.map_or_else(String::new, |c| format!("|{}", slug(c)));
    format!("name:{}{}", slug(title), city)
}

// ---------------------------------------------------------------------------
// Row reading
// ---------------------------------------------------------------------------

type Index = std::collections::BTreeMap<String, usize>;

fn header_index(header: &[String]) -> Index {
    let mut index = Index::new();
    for (i, cell) in header.iter().enumerate() {
        let key = normalise(cell);
        if !key.is_empty() {
            index.entry(key).or_insert(i);
        }
    }
    index
}

fn cell<'a>(index: &Index, row: &'a [String], name: &str) -> &'a str {
    index
        .get(name)
        .and_then(|i| row.get(*i))
        .map_or("", String::as_str)
        .trim()
}

/// `Title — subtitle` → `Title`: SCOUT grammars append the route after an
/// em-dash, so the lead segment is the closest thing to an organiser name
/// the row carries.
fn title_lead(title: &str) -> &str {
    title.split(['—', '–']).next().map_or(title, str::trim)
}

/// A row's own organisation claim, or the title's lead segment when the
/// grammar carries no organiser column — `organization` is NOT NULL, and
/// a name segment beats an ID string.
fn organization_of(explicit: &str, title: &str) -> String {
    clean(explicit).unwrap_or_else(|| {
        let lead = title_lead(title);
        if lead.is_empty() {
            title.to_owned()
        } else {
            lead.to_owned()
        }
    })
}

/// `Country / City` → (country, city): SCOUT_EN's compound column splits on
/// the last `/` — `Germany / Cologne` is a German city, `Polska` alone is a
/// country with no city named.
fn split_country_city(value: &str) -> (Option<String>, Option<String>) {
    let value = value.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Some((country, city)) = value.rsplit_once('/') {
        (clean(country), clean(city))
    } else {
        (Some(value.to_owned()), None)
    }
}

/// The historical Polish sheet used `Voivodeship` as a Polish-region column.
/// The GitHub festival registry kept that header for compatibility but Europe
/// rows now carry `Country / Region` there. Prefer an explicit `Country`
/// column when the evolved registry supplies one; otherwise recognise a
/// country prefix before falling back to Poland for an old plain voivodeship.
fn scout_registry_country(explicit: &str, region: &str) -> String {
    if let Some(country) = clean(explicit) {
        return country;
    }
    let region = region.trim();
    if region.is_empty() {
        return String::new();
    }
    if let Some((country, _)) = region.split_once('/') {
        let country = country.trim();
        if country_code_for(country).is_some() {
            return country.to_owned();
        }
    }
    "PL".to_owned()
}

fn metadata_from(pairs: &[(&str, String)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        if !value.trim().is_empty() {
            map.insert((*key).to_owned(), serde_json::json!(value.trim()));
        }
    }
    serde_json::Value::Object(map)
}

/// `verification_status`/`Verification`/`Current_Status` → verified? Only a
/// definitive verdict asserts the destination was checked — `PARTIALLY_*`
/// and a blank claim nothing.
fn verified_for(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_uppercase().as_str(),
        "VERIFIED" | "CONFIRMED" | "CONFIRMED_CURRENT" | "CONFIRMED_HISTORICAL"
    ) || raw
        .trim()
        .to_ascii_uppercase()
        .starts_with("CONFIRMED_CURRENT")
        || raw
            .trim()
            .to_ascii_uppercase()
            .starts_with("CONFIRMED_HISTORICAL")
}

fn contact_for(
    email: Option<&str>,
    display: &str,
    organization: &str,
    city: Option<&str>,
) -> Option<ExtractedContact> {
    let email = email?;
    let mut extras = std::collections::BTreeMap::new();
    if let Some(org) = clean(organization) {
        extras.insert("organization".to_owned(), org);
    }
    Some(ExtractedContact {
        email: email.to_owned(),
        display_name: clean(display),
        organization: clean(organization),
        phone: None,
        suggested_kind: None,
        city: city.and_then(clean),
        staged_status: None,
        notes: None,
        extras,
    })
}

fn seed_row(
    dialect: OpportunityDialect,
    index: &Index,
    row: &[String],
) -> Result<(SeededOpportunity, Option<ExtractedContact>), OpportunityRefusal> {
    macro_rules! c {
        ($name:literal) => {
            cell(index, row, $name)
        };
    }

    // Per-dialect field selection. Each grammar names the same concepts
    // differently; the arms below pick the cells, the shared tail validates.
    let (
        title,
        organization,
        raw_kind,
        country_cell,
        city,
        deadline_cell,
        event_cell,
        url_cell,
        email_cell,
        fit,
        strategic,
        distance_km,
        nights_away,
        eligible,
        verified,
        metadata,
    ) = match dialect {
        OpportunityDialect::Master => {
            let title = c!("opportunity_name").to_owned();
            let priority_bps = word_priority(c!("priority"));
            let metadata = metadata_from(&[
                ("opportunity_id", c!("opportunity_id").to_owned()),
                ("entity_id", c!("entity_id").to_owned()),
                ("organizer_entity_id", c!("organizer_entity_id").to_owned()),
                ("contact_id", c!("contact_id").to_owned()),
                ("sheet_status", c!("status").to_owned()),
                ("verification_status", c!("verification_status").to_owned()),
                ("eligibility", c!("eligibility").to_owned()),
                ("requirements", c!("requirements").to_owned()),
                ("application_fee", c!("application_fee").to_owned()),
                ("prize_benefit", c!("prize_benefit").to_owned()),
                ("financial_terms", c!("financial_terms").to_owned()),
                (
                    "expected_response_date",
                    c!("expected_response_date").to_owned(),
                ),
                ("last_verified", c!("last_verified").to_owned()),
            ]);
            (
                title.clone(),
                c!("organizer_entity_id").to_owned(),
                c!("opportunity_type").to_owned(),
                c!("country").to_owned(),
                clean(c!("city")),
                c!("deadline").to_owned(),
                c!("event_date").to_owned(),
                c!("application_url").to_owned(),
                String::new(),
                priority_bps.unwrap_or(0),
                priority_bps.unwrap_or(0),
                None,
                None,
                eligible_for(c!("status")),
                verified_for(c!("verification_status")),
                metadata,
            )
        }
        OpportunityDialect::ScoutAuto => {
            let title = c!("title").to_owned();
            let metadata = metadata_from(&[
                ("opportunity_id", c!("opportunity_id").to_owned()),
                ("source_type", c!("source_type").to_owned()),
                ("source_name", c!("source_name").to_owned()),
                ("dedupe_key", c!("dedupe_key").to_owned()),
                ("sheet_status", c!("status").to_owned()),
                ("submission_method", c!("submission_method").to_owned()),
                ("free_to_apply", c!("free_to_apply").to_owned()),
                ("why_fit", c!("why_fit").to_owned()),
                ("red_flags", c!("red_flags").to_owned()),
                ("next_step", c!("next_step").to_owned()),
                ("owner", c!("owner").to_owned()),
                ("discord_status", c!("discord_status").to_owned()),
            ]);
            (
                title.clone(),
                c!("organization").to_owned(),
                c!("category").to_owned(),
                c!("country").to_owned(),
                clean(c!("city")),
                c!("deadline").to_owned(),
                c!("event_date").to_owned(),
                c!("url").to_owned(),
                c!("contact_email").to_owned(),
                score_basis_points(c!("relevance_score")).unwrap_or(0),
                letter_priority(c!("priority")).unwrap_or(0),
                bounded_i32(c!("distance_km"), 20_000),
                bounded_i16(c!("nights_away"), 30),
                eligible_for(c!("status")),
                false,
                metadata,
            )
        }
        OpportunityDialect::ScoutPl => {
            let title = c!("name").to_owned();
            let verification = if c!("verification").is_empty() {
                c!("verification_status")
            } else {
                c!("verification")
            };
            let email = if !c!("contact_email").is_empty() {
                c!("contact_email")
            } else if !c!("public_contact").is_empty() {
                c!("public_contact")
            } else {
                c!("email")
            };
            let metadata = metadata_from(&[
                ("country", c!("country").to_owned()),
                ("voivodeship", c!("voivodeship").to_owned()),
                ("sheet_status", c!("current_status").to_owned()),
                ("next_cycle", c!("next_cycle").to_owned()),
                ("eligibility", c!("eligibility").to_owned()),
                ("economics", c!("economics").to_owned()),
                ("virya_history", c!("virya_history").to_owned()),
                ("next_action", c!("next_action").to_owned()),
                ("submission_method", c!("submission_method").to_owned()),
                ("free_to_apply", c!("free_to_apply").to_owned()),
                ("routing_source", c!("routing_source").to_owned()),
                ("why_fit", c!("why_fit").to_owned()),
                ("red_flags", c!("red_flags").to_owned()),
                ("dedupe_key", c!("dedupe_key").to_owned()),
                ("source_checked", c!("source_checked").to_owned()),
                ("verification", verification.to_owned()),
                ("evidence", c!("evidence").to_owned()),
            ]);
            let country = scout_registry_country(c!("country"), c!("voivodeship"));
            let verified = if verification.is_empty() {
                // Backward compatibility for the original Polish registry,
                // whose status cell sometimes carried the verification verdict.
                verified_for(c!("current_status"))
            } else {
                verified_for(verification)
            };
            (
                title.clone(),
                c!("organizer").to_owned(),
                c!("type").to_owned(),
                country,
                clean(c!("city")),
                c!("application_deadline").to_owned(),
                c!("event_date").to_owned(),
                c!("source_url").to_owned(),
                email.to_owned(),
                score_basis_points(c!("relevance_score")).unwrap_or(0),
                letter_priority(c!("priority")).unwrap_or(0),
                bounded_i32(c!("distance_km"), 20_000),
                bounded_i16(c!("nights_away"), 30),
                eligible_for(c!("current_status")),
                verified,
                metadata,
            )
        }
        OpportunityDialect::ScoutEn => {
            let title = c!("name").to_owned();
            let (country, city) = split_country_city(c!("country_city"));
            let metadata = metadata_from(&[
                ("dedupe_key", c!("dedupe_key").to_owned()),
                ("sheet_status", c!("status").to_owned()),
                ("submission_method", c!("submission_method").to_owned()),
                ("free_to_apply", c!("free_to_apply").to_owned()),
                ("why_fit", c!("why_fit").to_owned()),
                ("red_flags", c!("red_flags").to_owned()),
                ("next_step", c!("next_step").to_owned()),
                ("raw_snippet", {
                    let snippet = c!("raw_snippet");
                    snippet.chars().take(500).collect::<String>()
                }),
                ("source_checked", c!("source_checked").to_owned()),
                ("outreach_date", c!("outreach_date").to_owned()),
                ("notes", c!("notes").to_owned()),
            ]);
            (
                title.clone(),
                String::new(),
                c!("type").to_owned(),
                country.unwrap_or_default(),
                city,
                c!("deadline").to_owned(),
                c!("event_date").to_owned(),
                c!("url").to_owned(),
                c!("public_contact").to_owned(),
                score_basis_points(c!("relevance_score")).unwrap_or(0),
                letter_priority(c!("priority")).unwrap_or(0),
                bounded_i32(c!("distance_km"), 20_000),
                bounded_i16(c!("nights_away"), 30),
                eligible_for(c!("status")),
                false,
                metadata,
            )
        }
    };

    if title.trim().is_empty() {
        return Err(OpportunityRefusal::MissingTitle);
    }
    let kind = opportunity_kind_for(&raw_kind)
        .ok_or_else(|| OpportunityRefusal::UnmappedKind(raw_kind.trim().to_owned()))?;
    let destination_url = url_of(&url_cell);
    let contact_email = email_of(&email_cell);
    let country_code = country_code_for(&country_cell);
    let opportunity = SeededOpportunity {
        external_key: external_key(&title, destination_url.as_deref(), city.as_deref()),
        kind,
        raw_kind: raw_kind.trim().to_owned(),
        title: title.trim().to_owned(),
        organization: organization_of(&organization, &title),
        contact_email: contact_email.clone(),
        destination_url,
        country_code,
        city,
        fit_basis_points: fit,
        // A definitive verification verdict in an operator-owned registry is
        // strong evidence. Rows without that explicit verdict remain cautious
        // at the historical 2500 floor; presence in a sheet alone proves
        // neither freshness nor routability.
        confidence_basis_points: if verified { 8_500 } else { 2_500 },
        strategic_value_basis_points: strategic,
        deadline: first_iso_date(&deadline_cell),
        event_starts_on: first_iso_date(&event_cell),
        distance_km,
        nights_away,
        eligible,
        verified_destination: verified,
        metadata,
    };
    let contact = contact_for(
        contact_email.as_deref(),
        title_lead(&title),
        &opportunity.organization,
        opportunity.city.as_deref(),
    );
    Ok((opportunity, contact))
}

/// Reads one grid as a scout `OPPORTUNITIES` tab, or declines it.
///
/// `None` means no dialect's discriminating headers are present — another
/// reader may still claim the grid. `Some` means a dialect matched and
/// every non-empty row either parsed or carries a refusal naming the
/// problem. Pure: no IO, no clock — dates stay sheet-relative.
#[must_use]
pub fn extract_opportunity_sheet(grid: &[Vec<String>]) -> Option<OpportunitySeedReport> {
    let (header, rows) = grid.split_first()?;
    let dialect = dialect_for(header)?;
    let index = header_index(header);
    let mut report = OpportunitySeedReport {
        dialect: Some(dialect),
        ..OpportunitySeedReport::default()
    };
    for (offset, row) in rows.iter().enumerate() {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let sheet_row = offset + 2;
        match seed_row(dialect, &index, row) {
            Ok((opportunity, contact)) => {
                if let Some(contact) = contact {
                    report.contacts.push(contact);
                }
                report.rows.push(opportunity);
            }
            Err(refusal) => report.refusals.push((sheet_row, refusal)),
        }
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(header: &[&str], rows: &[&[&str]]) -> Vec<Vec<String>> {
        let mut grid = vec![header.iter().map(|s| s.to_string()).collect::<Vec<_>>()];
        for row in rows {
            grid.push(row.iter().map(|s| s.to_string()).collect());
        }
        grid
    }

    const MASTER_HEADER: &[&str] = &[
        "Opportunity_ID",
        "Entity_ID",
        "Opportunity_Type",
        "Opportunity_Name",
        "Related_Event_ID",
        "Organizer_Entity_ID",
        "Venue_Entity_ID",
        "Contact_ID",
        "Country",
        "City",
        "Application_Open",
        "Deadline",
        "Event_Date",
        "Expected_Response_Date",
        "Application_URL",
        "Eligibility",
        "Requirements",
        "Application_Fee",
        "Prize_Benefit",
        "Financial_Terms",
        "Status",
        "Priority",
        "Verification_Status",
        "Last_Verified",
    ];

    const AUTO_HEADER: &[&str] = &[
        "Opportunity_ID",
        "Found_At",
        "Source_Type",
        "Source_Name",
        "Title",
        "Organization",
        "Category",
        "Country",
        "City",
        "Deadline",
        "Event_Date",
        "URL",
        "Contact_Email",
        "Submission_Method",
        "Free_To_Apply",
        "Relevance_Score",
        "Priority",
        "Status",
        "Why_Fit",
        "Red_Flags",
        "Next_Step",
        "Owner",
        "Discord_Status",
        "Dedupe_Key",
        "Raw_Snippet",
    ];

    const PL_HEADER: &[&str] = &[
        "Name",
        "Organizer",
        "Type",
        "Voivodeship",
        "City",
        "Current_Status",
        "Event_Date",
        "Application_Deadline",
        "Next_Cycle",
        "Eligibility",
        "Economics",
        "VIRYA_History",
        "Next_Action",
        "Source_URL",
        "Evidence",
    ];

    const REGISTRY_HEADER: &[&str] = &[
        "Name",
        "Organizer",
        "Type",
        "Country",
        "Voivodeship",
        "City",
        "Current_Status",
        "Event_Date",
        "Application_Deadline",
        "Next_Cycle",
        "Eligibility",
        "Economics",
        "VIRYA_History",
        "Next_Action",
        "Source_URL",
        "Evidence",
        "Contact_Email",
        "Submission_Method",
        "Relevance_Score",
        "Priority",
        "Verification",
        "Dedupe_Key",
        "Source_Checked",
        "Why_Fit",
        "Red_Flags",
        "Distance_Km",
        "Nights_Away",
        "Routing_Source",
    ];

    const EN_HEADER: &[&str] = &[
        "Name",
        "Type",
        "Country / City",
        "Deadline",
        "Event Date",
        "URL",
        "Public Contact",
        "Submission Method",
        "Free to Apply",
        "Relevance Score",
        "Priority",
        "Status",
        "Why Fit",
        "Red Flags",
        "Next Step",
        "Dedupe Key",
        "Raw Snippet",
        "Source Checked",
        "Outreach Date",
        "Notes",
    ];

    fn master_row() -> Vec<&'static str> {
        vec![
            "OPP-000001",
            "ENT-000437",
            "FESTIVAL",
            "Euroblast Festival — Band Application",
            "",
            "",
            "",
            "CON-000389",
            "Germany",
            "Cologne",
            "",
            "",
            "2026-09-25",
            "",
            "https://www.euroblast.net/en/contact/",
            "",
            "Official email application",
            "",
            "",
            "",
            "RESEARCHING",
            "HIGH",
            "PARTIALLY_VERIFIED",
            "2026-08-29",
        ]
    }

    #[test]
    fn claims_each_dialect_and_rejects_lookalikes() {
        assert!(extract_opportunity_sheet(&grid(MASTER_HEADER, &[])).is_some());
        assert!(extract_opportunity_sheet(&grid(AUTO_HEADER, &[])).is_some());
        assert!(extract_opportunity_sheet(&grid(PL_HEADER, &[])).is_some());
        assert!(extract_opportunity_sheet(&grid(EN_HEADER, &[])).is_some());
        // A venue seed sheet and a contact list claim nothing here.
        let venue = grid(&["Name", "City", "Source_URL", "Address"], &[]);
        let contacts = grid(&["Name", "Email"], &[&["Somebody", "a@b.c"]]);
        assert!(extract_opportunity_sheet(&venue).is_none());
        assert!(extract_opportunity_sheet(&contacts).is_none());
        // The master's OPPORTUNITIES must not claim as the n8n queue: it has
        // Opportunity_ID but no Dedupe_Key/Title pair.
        let master = extract_opportunity_sheet(&grid(MASTER_HEADER, &[])).unwrap();
        assert_eq!(master.dialect, Some(OpportunityDialect::Master));
    }

    #[test]
    fn master_row_parses_with_url_identity() {
        let report = extract_opportunity_sheet(&grid(MASTER_HEADER, &[&master_row()])).unwrap();
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.kind, "festival");
        assert_eq!(row.title, "Euroblast Festival — Band Application");
        assert_eq!(row.external_key, "url:euroblast.net/en/contact");
        assert_eq!(row.country_code.as_deref(), Some("DE"));
        assert_eq!(row.city.as_deref(), Some("Cologne"));
        assert_eq!(row.fit_basis_points, 7_500);
        assert_eq!(
            row.event_starts_on,
            Some(Date::from_calendar_date(2026, time::Month::September, 25).unwrap())
        );
        assert!(!row.verified_destination);
        assert!(row.eligible);
    }

    #[test]
    fn scout_en_row_parses_and_converges_with_master() {
        let mut row = vec![""; EN_HEADER.len()];
        row[0] = "Euroblast Festival — Band Application";
        row[1] = "Festival / band application";
        row[2] = "Germany / Cologne";
        row[4] = "2026-09-25 to 2026-09-27; 2027 date TBA";
        row[5] = "https://www.euroblast.net/en/contact/";
        row[6] = "application@euroblast.net";
        row[9] = "95.0";
        row[10] = "A";
        row[11] = "Monitor";
        row[15] = "euroblast|band-application|virya";
        let report = extract_opportunity_sheet(&grid(EN_HEADER, &[&row])).unwrap();
        let opp = &report.rows[0];
        // Same destination URL → same key as the master row above.
        assert_eq!(opp.external_key, "url:euroblast.net/en/contact");
        assert_eq!(opp.kind, "festival");
        assert_eq!(opp.country_code.as_deref(), Some("DE"));
        assert_eq!(opp.city.as_deref(), Some("Cologne"));
        assert_eq!(opp.fit_basis_points, 9_500);
        assert_eq!(opp.strategic_value_basis_points, 9_000);
        assert_eq!(
            opp.contact_email.as_deref(),
            Some("application@euroblast.net")
        );
        assert_eq!(
            opp.event_starts_on,
            Some(Date::from_calendar_date(2026, time::Month::September, 25).unwrap())
        );
        // The Public Contact cell also stages as a drive contact.
        assert_eq!(report.contacts.len(), 1);
        assert_eq!(report.contacts[0].email, "application@euroblast.net");
    }

    #[test]
    fn github_registry_europe_row_keeps_country_score_route_and_verification() {
        let mut row = vec![""; REGISTRY_HEADER.len()];
        row[0] = "NEXUS 2027 — Titanium Stage / Band Application";
        row[1] = "OFFLINE Entertainment UG / NEXUS";
        row[2] = "Festival / rock-metal stage application";
        row[3] = "Germany";
        row[4] = "Germany / Saxony";
        row[5] = "Leipzig";
        row[6] = "Ręczne zgłoszenie";
        row[7] = "2027-09-17 to 2027-09-19";
        row[14] = "https://www.nerd-rock-festival.com/kopie-von-stage-act-anmeldung";
        row[16] = "hype@offline-entertainment.de";
        row[17] = "Official stage-act application";
        row[18] = "88";
        row[19] = "B";
        row[20] = "CONFIRMED_CURRENT — official application route checked";
        row[21] = "nexus-leipzig|2027|virya";
        row[22] = "Official NEXUS pages; checked 2026-10-01";
        row[23] = "Close routing and dedicated rock/metal stage";
        row[25] = "356";
        row[26] = "1";
        row[27] = "Verified road-route source";

        let report = extract_opportunity_sheet(&grid(REGISTRY_HEADER, &[&row])).unwrap();
        let opp = &report.rows[0];
        assert_eq!(report.dialect, Some(OpportunityDialect::ScoutPl));
        assert_eq!(opp.country_code.as_deref(), Some("DE"));
        assert_eq!(opp.city.as_deref(), Some("Leipzig"));
        assert_eq!(opp.fit_basis_points, 8_800);
        assert_eq!(opp.strategic_value_basis_points, 6_500);
        assert_eq!(opp.confidence_basis_points, 8_500);
        assert_eq!(opp.distance_km, Some(356));
        assert_eq!(opp.nights_away, Some(1));
        assert_eq!(opp.metadata["routing_source"], "Verified road-route source");
        assert!(opp.verified_destination);
        assert!(opp.eligible);
        assert_eq!(
            opp.contact_email.as_deref(),
            Some("hype@offline-entertainment.de")
        );
        assert_eq!(report.contacts.len(), 1);
        assert_eq!(opp.metadata["dedupe_key"], "nexus-leipzig|2027|virya");
    }

    #[test]
    fn github_registry_legacy_country_region_does_not_turn_germany_into_poland() {
        let mut row = vec![""; PL_HEADER.len()];
        row[0] = "Old Europe Registry Row";
        row[1] = "Organizer";
        row[2] = "Festival";
        row[3] = "Germany / Saxony";
        row[4] = "Leipzig";
        row[5] = "Monitor";
        row[13] = "https://example.test/application";
        let report = extract_opportunity_sheet(&grid(PL_HEADER, &[&row])).unwrap();
        assert_eq!(report.rows[0].country_code.as_deref(), Some("DE"));
    }

    #[test]
    fn scout_pl_closed_row_is_ineligible_and_verified() {
        let mut row = vec![""; PL_HEADER.len()];
        row[0] = "KozyNostra Rock Fest — Przegląd Zespołów";
        row[1] = "Dom Kultury w Kozach";
        row[2] = "CONTEST / FESTIVAL";
        row[3] = "śląskie";
        row[4] = "Kozy";
        row[5] = "CONFIRMED_HISTORICAL — 2026 closed";
        row[6] = "2026-08-22";
        row[7] = "2026-06-30";
        row[13] = "https://kozynostra.pl/";
        let report = extract_opportunity_sheet(&grid(PL_HEADER, &[&row])).unwrap();
        let opp = &report.rows[0];
        assert_eq!(opp.kind, "review_contest");
        assert_eq!(opp.country_code.as_deref(), Some("PL"));
        assert_eq!(opp.external_key, "url:kozynostra.pl");
        assert!(!opp.eligible);
        assert!(opp.verified_destination);
        assert_eq!(
            opp.deadline,
            Some(Date::from_calendar_date(2026, time::Month::June, 30).unwrap())
        );
    }

    #[test]
    fn scout_auto_row_parses_dedupe_key_into_metadata() {
        let mut row = vec![""; AUTO_HEADER.len()];
        row[0] = "OPP-20260722-005";
        row[4] = "Będzie Głośno — zgłoszenie zespołu";
        row[5] = "Polskie Radio Czwórka";
        row[6] = "Radio / live / wywiad";
        row[7] = "Polska";
        row[8] = "Warszawa";
        row[11] = "https://www.polskieradio.pl/10/5359";
        row[15] = "88.0";
        row[16] = "A";
        row[17] = "Zamknięte";
        row[23] = "czworka-bedzie-glosno-2026";
        let report = extract_opportunity_sheet(&grid(AUTO_HEADER, &[&row])).unwrap();
        let opp = &report.rows[0];
        assert_eq!(opp.kind, "interview");
        assert_eq!(opp.country_code.as_deref(), Some("PL"));
        assert!(!opp.eligible);
        assert_eq!(opp.metadata["dedupe_key"], "czworka-bedzie-glosno-2026");
    }

    #[test]
    fn rows_without_title_or_known_kind_are_refused() {
        let mut blank_title = vec![""; EN_HEADER.len()];
        blank_title[1] = "Festival";
        let mut weird_kind = vec![""; EN_HEADER.len()];
        weird_kind[0] = "Something";
        weird_kind[1] = "Quantum gathering";
        let report =
            extract_opportunity_sheet(&grid(EN_HEADER, &[&blank_title, &weird_kind])).unwrap();
        assert!(report.rows.is_empty());
        assert_eq!(
            report.refusals,
            vec![
                (2, OpportunityRefusal::MissingTitle),
                (
                    3,
                    OpportunityRefusal::UnmappedKind("Quantum gathering".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn human_scout_waiting_and_blocked_statuses_are_ineligible() {
        for status in ["Czeka na odpowiedź", "Czeka na odpowiedz", "Blocked"] {
            assert!(
                !eligible_for(status),
                "{status} means the route is already submitted or blocked"
            );
        }

        // These remain actionable states: one needs preparation, the other
        // needs a human to use a form/platform rather than an automatic send.
        assert!(eligible_for("Do przygotowania"));
        assert!(eligible_for("Ręczne zgłoszenie"));
        assert!(eligible_for("Monitor"));
    }

    #[test]
    fn url_normalisation_folds_scheme_www_and_trailing_slash() {
        assert_eq!(
            normalise_url("https://www.Euroblast.net/en/contact/"),
            Some("euroblast.net/en/contact".to_owned())
        );
        assert_eq!(
            normalise_url("http://euroblast.net/en/contact/?utm=1#top"),
            Some("euroblast.net/en/contact".to_owned())
        );
        assert_eq!(normalise_url("not a url"), None);
    }
}
