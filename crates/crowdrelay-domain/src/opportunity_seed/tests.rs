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
    let report = extract_opportunity_sheet(&grid(EN_HEADER, &[&blank_title, &weird_kind])).unwrap();
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
