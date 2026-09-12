//! Bulk import of hand-curated outreach contacts.
//!
//! The band's CRM holds thousands of radio shows, stations, press outlets,
//! blogs and curators gathered by hand over years. CrowdRelay held 22 press
//! targets, nine of them with an address, so a press pitch had almost nobody
//! to reach and the growth loop's whole third-party side ran on a rounding
//! error of the real list.
//!
//! `ops/import/xlsx_to_outreach_csv.py` does the reading and the category
//! mapping and writes a CSV. This owns the writes, so imported rows land
//! through the same table, the same conflict key and the same status rules as
//! anything an agent proposes. Nothing here bypasses screening: rows arrive as
//! `proposed`, exactly as an agent proposal does, and the existing promotion
//! path decides what the growth loop may act on.
//!
//! Re-running is safe. The conflict key is `(workspace_id, display_name,
//! target_kind)`, a discarded target stays discarded, and an existing row keeps
//! its status — so a second import updates contact details without quietly
//! re-admitting something an operator threw out.

use std::path::Path;

use crowdrelay_domain::WorkspaceId;
use serde_json::json;
use sqlx::PgPool;

/// One row of the converter's CSV.
#[derive(Debug, serde::Deserialize)]
struct ContactRow {
    display_name: String,
    contact_email: String,
    target_kind: String,
    #[serde(default)]
    evidence_url: String,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    fit_score: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    source: String,
}

/// What an import run did, so the operator sees more than "done".
#[derive(Debug, Default)]
pub struct ImportSummary {
    pub read: usize,
    pub skipped: usize,
    pub written: usize,
}

/// The vocabulary `agent_outreach_targets_target_kind_check` accepts.
///
/// Checked here rather than left to the database so a bad row is reported with
/// its name and line instead of aborting the run with a constraint error.
const ACCEPTED_KINDS: [&str; 7] = [
    "press",
    "radio",
    "playlist",
    "media_patronage",
    "endorsement",
    "creator",
    "community",
];

fn usable(row: &ContactRow) -> Result<(), String> {
    if row.display_name.trim().is_empty() {
        return Err("display_name is empty".to_owned());
    }
    if !row.contact_email.contains('@') {
        return Err(format!(
            "contact_email {:?} is not an address",
            row.contact_email
        ));
    }
    if !ACCEPTED_KINDS.contains(&row.target_kind.as_str()) {
        return Err(format!("target_kind {:?} is not accepted", row.target_kind));
    }
    Ok(())
}

/// Reads the CSV and upserts every usable row.
///
/// # Errors
/// Returns the underlying failure when the file cannot be read or a write
/// fails. A row that fails validation is counted and reported, not fatal — one
/// malformed line in eleven hundred should not cost the whole import.
pub async fn import_outreach(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    path: &Path,
) -> anyhow::Result<ImportSummary> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut summary = ImportSummary::default();

    for (line, record) in reader.deserialize::<ContactRow>().enumerate() {
        let row: ContactRow = match record {
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
            tracing::warn!(line = line + 2, name = %row.display_name, %reason, "skipping row");
            continue;
        }

        // Everything the sheet knew that the schema has no column for. The
        // submission route in `notes` is the most useful of them: it is how a
        // pitch actually reaches this outlet, and a model drafting one should
        // see it.
        let mut evidence = Vec::new();
        if !row.evidence_url.trim().is_empty() {
            for url in row.evidence_url.split_whitespace() {
                evidence.push(json!({ "url": url, "source": row.source }));
            }
        }
        let why_fit = {
            let mut parts = Vec::new();
            if !row.notes.trim().is_empty() {
                parts.push(row.notes.clone());
            }
            if !row.country_code.trim().is_empty() {
                parts.push(format!("country={}", row.country_code));
            }
            if !row.fit_score.trim().is_empty() {
                parts.push(format!("crm_fit={}", row.fit_score));
            }
            parts.join(" · ")
        };
        let domain = row
            .contact_email
            .rsplit_once('@')
            .map(|(_, domain)| domain.to_owned());

        sqlx::query(
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email,
                 contact_domain, why_fit, evidence, status)
            VALUES ($1,$2,$3,$4,$5,$6,$7,'proposed')
            ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
                contact_email = COALESCE(EXCLUDED.contact_email, agent_outreach_targets.contact_email),
                contact_domain = COALESCE(EXCLUDED.contact_domain, agent_outreach_targets.contact_domain),
                why_fit = COALESCE(NULLIF(EXCLUDED.why_fit, ''), agent_outreach_targets.why_fit),
                evidence = CASE
                    WHEN jsonb_array_length(EXCLUDED.evidence) > 0 THEN EXCLUDED.evidence
                    ELSE agent_outreach_targets.evidence
                END
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&row.target_kind)
        .bind(row.display_name.trim())
        .bind(row.contact_email.trim())
        .bind(domain)
        .bind(&why_fit)
        .bind(serde_json::Value::Array(evidence))
        .execute(pool)
        .await?;
        summary.written += 1;
    }
    Ok(summary)
}
