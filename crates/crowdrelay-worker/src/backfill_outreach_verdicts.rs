//! One-time repair of imported outreach replies.
//!
//! The `master:`/`promo:` sheet import wrote every answer it did not
//! recognise as `received` — including fourteen `POSITIVE`s and three
//! `NEGATIVE`s — and never minted a `reply_classifications` row for any of
//! them, because that table is populated by the reply-text ingress path the
//! import bypassed. Two consequences this job repairs:
//!
//! 1. Terminal sheet verdicts (`POSITIVE`, `Odrzucone`, `LISTING_ACCEPTED`,
//!    …) are mapped onto the interaction's disposition by the domain's
//!    [`reply_verdict_map`], so the positive/declined signal is real data
//!    instead of buried metadata.
//! 2. Verdicts that record an answer without recording what it means
//!    (`GMAIL_REPLY`, `Decision required`, `NEGOTIATING`, unknown codes)
//!    mint a `needs_human` triage row with reason `imported_verdict`, so
//!    the reply board shows them instead of reporting "nothing needs you".
//!
//! Re-runnable: disposition updates only touch rows still `received`, and
//! triage rows carry `classified_at = occurred_at` under the
//! `(workspace_id, target_id, classified_at)` unique constraint, so a
//! second run inserts nothing.

use crowdrelay_domain::{
    WorkspaceId,
    reply_triage::HumanReviewReason,
    reply_verdict_map::{ImportedVerdict, map_sheet_verdict},
};
use sqlx::PgPool;

/// What one backfill pass did.
#[derive(Debug, Default)]
pub struct BackfillSummary {
    /// Inbound reply rows examined.
    pub scanned: usize,
    /// `received` → `positive`.
    pub remapped_positive: usize,
    /// `received` → `declined`.
    pub remapped_declined: usize,
    /// `received` → `do_not_contact`.
    pub remapped_do_not_contact: usize,
    /// `needs_human` triage rows minted (skipped by the unique constraint
    /// on re-runs do not count).
    pub triage_minted: usize,
    /// Rows with no verdict in metadata at all.
    pub no_verdict: usize,
}

/// The kinds `reply_classifications.target_kind` accepts — agent/label
/// replies cannot mint queue rows the CHECK would refuse.
const TRIAGE_KINDS: &[&str] = &[
    "playlist",
    "radio",
    "press",
    "creator",
    "support_slot",
    "endorsement",
    "media_patronage",
];

/// One imported reply row, as stored.
struct ImportedReplyRow {
    id: i64,
    target_id: uuid::Uuid,
    occurred_at: time::OffsetDateTime,
    target_kind: String,
    disposition: String,
    /// `response_type` (promo sheet) or `result` (master sheet), raw.
    verdict: Option<String>,
}

/// Reads every inbound reply and applies the verdict map.
///
/// # Errors
/// Propagates database failures; per-row verdict problems are counted, not
/// fatal.
pub async fn backfill_outreach_verdicts(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    dry_run: bool,
) -> anyhow::Result<BackfillSummary> {
    let rows = sqlx::query_as::<_, ImportedReplyRow>(
        r#"
        SELECT i.id, i.target_id, i.occurred_at, t.target_kind, i.disposition,
               COALESCE(
                   NULLIF(i.metadata->>'response_type', ''),
                   NULLIF(i.metadata->>'result', '')
               ) AS verdict
        FROM outreach_interactions i
        JOIN outreach_targets t
          ON t.workspace_id = i.workspace_id AND t.id = i.target_id
        WHERE i.workspace_id = $1 AND i.direction = 'inbound'
        ORDER BY i.occurred_at
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?;

    let mut summary = BackfillSummary::default();
    for row in &rows {
        summary.scanned += 1;
        let verdict = row.verdict.as_deref().unwrap_or("");
        if verdict.is_empty() {
            summary.no_verdict += 1;
        }
        // Terminal rows are already correct — nothing to repair.
        if row.disposition != "received" {
            continue;
        }
        match map_sheet_verdict(verdict) {
            ImportedVerdict::Terminal(disposition) => {
                let label = disposition.as_str();
                if dry_run {
                    tracing::info!(id = row.id, %verdict, %label, "would remap");
                } else {
                    remap_disposition(pool, workspace_id, row.id, label).await?;
                }
                match label {
                    "positive" => summary.remapped_positive += 1,
                    "declined" => summary.remapped_declined += 1,
                    "do_not_contact" => summary.remapped_do_not_contact += 1,
                    _ => {}
                }
            }
            ImportedVerdict::NeedsHuman => {
                // The triage CHECK does not name agent/label kinds — a
                // reply on one still remaps its interaction, it just
                // cannot mint a queue row the constraint would refuse.
                if !TRIAGE_KINDS.contains(&row.target_kind.as_str()) {
                    continue;
                }
                if dry_run {
                    tracing::info!(id = row.id, %verdict, "would mint triage row");
                    summary.triage_minted += 1;
                } else if mint_triage_row(pool, workspace_id, row).await? {
                    summary.triage_minted += 1;
                }
            }
        }
    }
    Ok(summary)
}

/// Writes the mapped disposition and stamps the correction into metadata —
/// the source verdict stays, and `verdict_backfilled_from` records what the
/// row claimed before the repair.
async fn remap_disposition(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    interaction_id: i64,
    disposition: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE outreach_interactions
        SET disposition = $3,
            metadata = metadata || jsonb_build_object(
                'verdict_backfilled_from', 'received',
                'verdict_backfilled_at', now()::text
            )
        WHERE workspace_id = $1 AND id = $2 AND disposition = 'received'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(interaction_id)
    .bind(disposition)
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts one `needs_human` triage row for an imported reply the verdict
/// map cannot settle. `classified_at` mirrors the reply's own timestamp —
/// the constraint `(workspace_id, target_id, classified_at)` then makes a
/// re-run a no-op. `reply_text` carries the sheet's verdict code because
/// that is the only reply text the import recorded.
///
/// Returns whether a row was inserted (false when the constraint dedupes).
async fn mint_triage_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    row: &ImportedReplyRow,
) -> anyhow::Result<bool> {
    let reply_text = match row.verdict.as_deref() {
        Some(verdict) if !verdict.is_empty() => {
            format!("[imported sheet verdict] {verdict}")
        }
        _ => "[imported reply — no verdict recorded]".to_owned(),
    };
    let inserted = sqlx::query(
        r#"
        INSERT INTO reply_classifications
            (workspace_id, target_id, target_kind, reply_text,
             previous_disposition, classification_result, human_review_reason,
             confidence_basis_points, matched_rules, classified_at)
        VALUES ($1, $2, $3, $4, $5, 'needs_human', $6, 10000, $7, $8)
        ON CONFLICT (workspace_id, target_id, classified_at) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(row.target_id)
    .bind(&row.target_kind)
    .bind(reply_text)
    .bind(&row.disposition)
    .bind(HumanReviewReason::ImportedVerdict.as_str())
    .bind(serde_json::json!(["import:sheet_verdict"]))
    .bind(row.occurred_at)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ImportedReplyRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            target_id: row.try_get("target_id")?,
            occurred_at: row.try_get("occurred_at")?,
            target_kind: row.try_get("target_kind")?,
            disposition: row.try_get("disposition")?,
            verdict: row.try_get("verdict")?,
        })
    }
}
