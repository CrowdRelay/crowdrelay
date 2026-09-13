//! Bulk import of hand-curated live opportunities: festivals, competitions,
//! showcases.
//!
//! `viryaos_team_opportunities` is what the `booking_opportunity` and
//! `live_opportunity` autopilot contexts rank and act on, and in production it
//! held zero rows. The whole live arm of the growth loop had never had a single
//! unit to work with — the same shape as `communities_joined` sitting at 0 — while
//! the band's CRM held 797 festivals and competitions gathered by hand, with fit
//! scores, addresses and deadlines.
//!
//! Shows are the top of the fan funnel: a support slot puts the band in front of
//! somebody else's audience, which is the only channel here that reaches people
//! who have never heard of them.
//!
//! `ops/import/xlsx_to_opportunities_csv.py` does the reading and the vocabulary
//! mapping. This owns the writes, so imported rows land through the same table,
//! the same conflict key and the same status rules as anything the brain proposes.
//!
//! Re-running is safe. The conflict key is `(workspace_id, source, external_key)`
//! and an existing row keeps its `status`, `eligible` and `metadata` — so a second
//! import refreshes contact details and deadlines without reopening something an
//! operator dismissed, and without overwriting what the loop has since learned.

use std::path::Path;

use crowdrelay_domain::WorkspaceId;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Date, OffsetDateTime, Time, UtcOffset, format_description::well_known::Iso8601};

/// The source every row from this sheet carries. Half of the unique key, so it
/// is a constant rather than a CSV column: a row that claimed a different source
/// would dedupe against nothing.
const SOURCE: &str = "crm:zgloszenia";

/// One row of the converter's CSV.
#[derive(Debug, serde::Deserialize)]
struct OpportunityRow {
    external_key: String,
    opportunity_kind: String,
    title: String,
    organization: String,
    #[serde(default)]
    contact_email: String,
    #[serde(default)]
    destination_url: String,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    fit_basis_points: String,
    #[serde(default)]
    confidence_basis_points: String,
    #[serde(default)]
    deadline: String,
    #[serde(default)]
    eligible: String,
    #[serde(default)]
    typical_month: String,
    #[serde(default)]
    deadline_certainty: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    genres: String,
    #[serde(default)]
    next_step: String,
    #[serde(default)]
    why_fit: String,
    #[serde(default)]
    verification: String,
}

/// What an import run did, so the operator sees more than "done".
#[derive(Debug, Default)]
pub struct ImportSummary {
    pub read: usize,
    pub skipped: usize,
    pub written: usize,
    /// Rows carrying a real deadline, as opposed to a month estimate. Reported
    /// separately because it is the number that decides whether the brain's live
    /// calendar has anything to sort by.
    pub with_deadline: usize,
}

/// The vocabulary `viryaos_team_opportunities_opportunity_kind_check` accepts.
///
/// Checked here rather than left to the database so a bad row is reported with
/// its title and line instead of aborting the run with a constraint error.
const ACCEPTED_KINDS: [&str; 5] = [
    "festival",
    "showcase",
    "review_contest",
    "support_slot",
    "funding",
];

/// Basis points, or `None` when the cell was blank or unparseable.
fn basis_points(value: &str) -> Option<i32> {
    let parsed: i32 = value.trim().parse().ok()?;
    Some(parsed.clamp(0, 10_000))
}

/// Midnight UTC on an ISO date.
///
/// A festival application deadline is a day, not an instant. Anchoring it to
/// midnight is the conservative reading: the brain treats the deadline as passed
/// from the start of that day rather than believing it has until the evening.
fn deadline_at(value: &str) -> Option<OffsetDateTime> {
    let date = Date::parse(value.trim(), &Iso8601::DATE).ok()?;
    Some(OffsetDateTime::new_in_offset(
        date,
        Time::MIDNIGHT,
        UtcOffset::UTC,
    ))
}

fn usable(row: &OpportunityRow) -> Result<(), String> {
    if row.title.trim().is_empty() {
        return Err("title is empty".to_owned());
    }
    if row.external_key.trim().is_empty() {
        return Err("external_key is empty".to_owned());
    }
    if !ACCEPTED_KINDS.contains(&row.opportunity_kind.as_str()) {
        return Err(format!(
            "opportunity_kind {:?} is not accepted",
            row.opportunity_kind
        ));
    }
    // A row with no address and no submission form has no route. The press-pitch
    // lesson: an action with no destination is a succeeded dispatch that reached
    // nobody, and a row the brain cannot act on is worse than no row.
    if !row.contact_email.contains('@') && row.destination_url.trim().is_empty() {
        return Err("neither a contact address nor a submission URL".to_owned());
    }
    // The column's own CHECK enforces this pattern; failing here names the row.
    if !row.contact_email.trim().is_empty() && !row.contact_email.contains('@') {
        return Err(format!(
            "contact_email {:?} is not an address",
            row.contact_email
        ));
    }
    if basis_points(&row.fit_basis_points).is_none() {
        return Err("fit_basis_points is not a number".to_owned());
    }
    Ok(())
}

/// Everything the sheet knew that the table has no column for.
///
/// The month estimate is the important one. 429 of the sheet's rows date their
/// deadline by the month the event happens in, at low certainty — useful to an
/// operator deciding when to look, and a lie if written into `deadline`, which
/// the brain sorts its live calendar by and cannot read as an estimate once it is
/// a timestamp. It travels here instead, labelled.
fn metadata(row: &OpportunityRow) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("import_source".into(), json!(SOURCE));
    for (key, value) in [
        ("typical_deadline_month", &row.typical_month),
        ("deadline_certainty", &row.deadline_certainty),
        ("crm_priority", &row.priority),
        ("genres", &row.genres),
        ("next_step", &row.next_step),
        ("why_fit", &row.why_fit),
        ("verification", &row.verification),
        ("city", &row.city),
    ] {
        if !value.trim().is_empty() {
            map.insert(key.to_owned(), json!(value.trim()));
        }
    }
    Value::Object(map)
}

/// Reads the CSV and upserts every usable row.
///
/// # Errors
/// Returns the underlying failure when the file cannot be read or a write fails.
/// A row that fails validation is counted and reported, not fatal — one malformed
/// line in eight hundred should not cost the whole import.
pub async fn import_opportunities(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    path: &Path,
) -> anyhow::Result<ImportSummary> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut summary = ImportSummary::default();

    for (line, record) in reader.deserialize::<OpportunityRow>().enumerate() {
        let row: OpportunityRow = match record {
            Ok(row) => row,
            Err(error) => {
                summary.skipped += 1;
                tracing::warn!(line = line + 2, error = %error, "unreadable row");
                continue;
            }
        };
        summary.read += 1;
        if let Err(reason) = usable(&row) {
            summary.skipped += 1;
            tracing::warn!(line = line + 2, title = %row.title, %reason, "skipping row");
            continue;
        }

        let deadline = deadline_at(&row.deadline);
        if deadline.is_some() {
            summary.with_deadline += 1;
        }
        let email = Some(row.contact_email.trim())
            .filter(|value| value.contains('@'))
            .map(str::to_owned);
        let url = Some(row.destination_url.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let country = Some(row.country_code.trim())
            .filter(|value| value.len() == 2)
            .map(str::to_ascii_uppercase);
        // Anything but an explicit `false` is eligible. The sheet marks closed
        // editions and nothing else, so a blank means "not known to be closed".
        let eligible = !row.eligible.trim().eq_ignore_ascii_case("false");

        sqlx::query(
            r#"
            INSERT INTO viryaos_team_opportunities
                (workspace_id, opportunity_kind, source, external_key, title,
                 organization, destination_url, contact_email, country_code,
                 fit_basis_points, confidence_basis_points, deadline, eligible,
                 metadata, status)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'new')
            ON CONFLICT (workspace_id, source, external_key) DO UPDATE SET
                -- Contact details and the deadline are what a re-import is for:
                -- the sheet is the band's live record and CrowdRelay's copy goes
                -- stale. COALESCE so a blanked cell does not erase what we have.
                contact_email = COALESCE(EXCLUDED.contact_email, viryaos_team_opportunities.contact_email),
                destination_url = COALESCE(EXCLUDED.destination_url, viryaos_team_opportunities.destination_url),
                country_code = COALESCE(EXCLUDED.country_code, viryaos_team_opportunities.country_code),
                deadline = COALESCE(EXCLUDED.deadline, viryaos_team_opportunities.deadline),
                fit_basis_points = EXCLUDED.fit_basis_points,
                confidence_basis_points = EXCLUDED.confidence_basis_points,
                title = EXCLUDED.title,
                -- `status`, `eligible` and `metadata` are deliberately absent.
                -- Status is the loop's own record of what it did — a re-import
                -- must not walk a `submitted` row back to `new` and cause a
                -- second application to the same festival. Eligibility and
                -- metadata may have been corrected by an operator or enriched by
                -- the loop since, and the sheet does not know that.
                updated_at = now(),
                version = viryaos_team_opportunities.version + 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&row.opportunity_kind)
        .bind(SOURCE)
        .bind(row.external_key.trim())
        .bind(row.title.trim())
        .bind(row.organization.trim())
        .bind(url)
        .bind(email)
        .bind(country)
        .bind(basis_points(&row.fit_basis_points).unwrap_or(0))
        .bind(basis_points(&row.confidence_basis_points).unwrap_or(2_500))
        .bind(deadline)
        .bind(eligible)
        .bind(metadata(&row))
        .execute(pool)
        .await?;
        summary.written += 1;
    }
    Ok(summary)
}
