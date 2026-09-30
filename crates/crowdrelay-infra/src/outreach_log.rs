//! Writing an outreach-log sheet into `outreach_interactions`.
//!
//! The band's workbooks are a *send log*: each row is a letter that went
//! out (or, under `DRAFT`, one that never did — those mint nothing). A row
//! becomes an outbound interaction under `{book}:{outreach_id}` and, when
//! the sheet records an answer, an inbound reply under
//! `{book}:{outreach_id}:reply` — the exact convention the one-off import
//! wrote, so a re-scan refreshes history instead of duplicating it.
//!
//! # What the write does and does not do
//!
//! - **Matching is by contact email only.** A row whose `Recipient`/
//!   `Kontakt` cell names no `outreach_targets` row writes nothing and is
//!   counted `unmatched` — the log is a history, not a registry lane, and
//!   minting a target needs a kind the sheet does not reliably carry. The
//!   row imports on the next scan once the contact exists.
//! - **Replies move the target's reply state.** `last_reply_at` advances
//!   monotonically; `last_reply_disposition` follows the newest reply's
//!   mapped disposition; a `do_not_contact` verdict also raises the
//!   target's `do_not_contact` flag — the sheet's "wypisz" is the same
//!   wall an operator-recorded unsubscribe is.
//! - **Ambiguous verdicts mint triage rows.** A reply whose sheet verdict
//!   settles nothing terminal lands in `reply_classifications` as
//!   `needs_human`/`imported_verdict`, so the console's triage queue sees
//!   it — that surface was empty for the imported cohort precisely because
//!   nothing ever minted its rows.
//!
//! Each row is its own transaction: one bad cell must not take the
//! sheet's other sends with it.

use crowdrelay_domain::outreach_log::{OutreachBook, OutreachLogEntry, OutreachLogReport};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// What one sheet's import did — counted so the cycle report can say
/// "refreshed" apart from "new" and "no target" apart from "refused".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutreachLogImportSummary {
    /// Outbound interactions written (`{book}:{id}` keys that were new).
    pub sends_recorded: u64,
    /// Inbound reply interactions written (`{book}:{id}:reply`).
    pub replies_recorded: u64,
    /// Rows already present under their source key — a re-scan, not a
    /// second copy.
    pub already_present: u64,
    /// Rows the sheet records as drafts or otherwise never-sent.
    pub drafts_skipped: u64,
    /// Rows whose contact cell matched no `outreach_targets` row — the
    /// write is impossible until the registry knows the counterparty.
    pub unmatched: u64,
    /// Replies whose verdict settled nothing, queued for a human.
    pub triage_minted: u64,
    /// Rows whose own write failed — isolated per row, logged, counted.
    pub failed: u64,
}

/// Postgres-backed outreach-log importer.
#[derive(Clone)]
pub struct PostgresOutreachLogRepository {
    pool: PgPool,
}

impl PostgresOutreachLogRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports every entry of one sheet. `source_label` is the
    /// `{file}#{sheet}` provenance string the metadata carries.
    ///
    /// # Errors
    /// Only pool-level failures propagate; a row's own error is isolated,
    /// logged and counted in the summary so the rest of the sheet still
    /// lands.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        source_label: &str,
        report: &OutreachLogReport,
    ) -> Result<OutreachLogImportSummary, sqlx::Error> {
        let mut summary = OutreachLogImportSummary::default();
        for entry in &report.entries {
            if !entry.was_sent() && !entry.was_replied() {
                summary.drafts_skipped += 1;
                continue;
            }
            match self
                .import_entry(workspace_id, source_label, report.book, entry)
                .await
            {
                Ok(outcome) => {
                    summary.sends_recorded += outcome.sends_recorded;
                    summary.replies_recorded += outcome.replies_recorded;
                    summary.already_present += outcome.already_present;
                    summary.triage_minted += outcome.triage_minted;
                    summary.unmatched += outcome.unmatched;
                }
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        %error,
                        source = %source_label,
                        outreach_id = %entry.outreach_id,
                        "outreach log row failed to write"
                    );
                }
            }
        }
        Ok(summary)
    }

    async fn import_entry(
        &self,
        workspace_id: Uuid,
        source_label: &str,
        book: OutreachBook,
        entry: &OutreachLogEntry,
    ) -> Result<EntryOutcome, sqlx::Error> {
        let mut outcome = EntryOutcome::default();
        // No route, no row: a name in the contact column is not an
        // address, and an address no target carries cannot hold an
        // interaction under the FK.
        let Some(email) = entry.contact.as_deref() else {
            outcome.unmatched = 1;
            return Ok(outcome);
        };
        // Two targets can share an inbox (a station's press line and its
        // playlist desk). The one with the freshest history takes the row
        // — arbitrary ties break on `id` so re-imports land the same row.
        let target: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT id, target_kind FROM outreach_targets \
             WHERE workspace_id = $1 AND lower(contact_email) = lower($2) \
             ORDER BY last_reply_at DESC NULLS LAST, \
                      last_outreach_at DESC NULLS LAST, id \
             LIMIT 1",
        )
        .bind(workspace_id)
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        let Some((target_id, target_kind)) = target else {
            outcome.unmatched = 1;
            return Ok(outcome);
        };

        let mut tx = self.pool.begin().await?;

        if entry.was_sent() {
            let Some(occurred_at) = entry.send_occurred_at() else {
                // A send the sheet cannot date cannot be ordered against
                // history — refuse the row rather than stamp it "now".
                tx.rollback().await?;
                return Err(sqlx::Error::Protocol(
                    "sent log row carries no honest timestamp".to_owned(),
                ));
            };
            // `FOLLOW_UP` is the only purpose the one-off import read as
            // a follow-up phase; every other send is the first touch.
            let phase = if entry
                .purpose
                .as_deref()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("follow_up"))
            {
                "followup"
            } else {
                "initial"
            };
            let metadata = outbound_metadata(source_label, book, entry);
            // `xmax = 0` tells a true insert from a conflict no-op.
            let inserted: bool = sqlx::query_scalar(
                r#"
                INSERT INTO outreach_interactions (
                    workspace_id, target_id, direction, phase,
                    disposition, source_key, occurred_at, metadata
                ) VALUES ($1,$2,'outbound',$3,'none',$4,$5,$6)
                ON CONFLICT (workspace_id, target_id, source_key)
                    DO NOTHING
                RETURNING (xmax = 0)
                "#,
            )
            .bind(workspace_id)
            .bind(target_id)
            .bind(phase)
            .bind(format!(
                "{}:{}",
                book.source_key_prefix(),
                entry.outreach_id
            ))
            .bind(occurred_at)
            .bind(&metadata)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
            if inserted {
                outcome.sends_recorded = 1;
                sqlx::query(
                    r#"
                    UPDATE outreach_targets SET
                        last_outreach_at = GREATEST(
                            COALESCE(last_outreach_at, '-infinity'::timestamptz), $3),
                        version = version + 1
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(workspace_id)
                .bind(target_id)
                .bind(occurred_at)
                .execute(&mut *tx)
                .await?;
            } else {
                outcome.already_present += 1;
            }
        }

        if entry.was_replied() {
            let Some(occurred_at) = entry.reply_occurred_at() else {
                tx.rollback().await?;
                return Err(sqlx::Error::Protocol(
                    "replied log row carries no honest timestamp".to_owned(),
                ));
            };
            let disposition = entry.reply_disposition();
            let metadata = reply_metadata(book, entry);
            let inserted: bool = sqlx::query_scalar(
                r#"
                INSERT INTO outreach_interactions (
                    workspace_id, target_id, direction, phase,
                    disposition, source_key, occurred_at, metadata
                ) VALUES ($1,$2,'inbound','reply',$3,$4,$5,$6)
                ON CONFLICT (workspace_id, target_id, source_key)
                    DO NOTHING
                RETURNING (xmax = 0)
                "#,
            )
            .bind(workspace_id)
            .bind(target_id)
            .bind(disposition.as_str())
            .bind(format!(
                "{}:{}:reply",
                book.source_key_prefix(),
                entry.outreach_id
            ))
            .bind(occurred_at)
            .bind(&metadata)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
            if inserted {
                outcome.replies_recorded = 1;
                // The reply clock only ever moves forward; the disposition
                // a target remembers is the NEWEST reply's, and the SET
                // list reads the pre-update row, so both expressions judge
                // against the same clock value.
                sqlx::query(
                    r#"
                    UPDATE outreach_targets SET
                        last_reply_disposition = CASE
                            WHEN last_reply_at IS NULL OR $3 >= last_reply_at
                            THEN $4 ELSE last_reply_disposition END,
                        last_reply_at = GREATEST(
                            COALESCE(last_reply_at, '-infinity'::timestamptz), $3),
                        do_not_contact = do_not_contact OR ($4 = 'do_not_contact'),
                        version = version + 1
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(workspace_id)
                .bind(target_id)
                .bind(occurred_at)
                .bind(disposition.as_str())
                .execute(&mut *tx)
                .await?;

                // A verdict the map could not settle goes to a human —
                // the same needs_human queue the text classifier feeds,
                // with `imported_verdict` as the honest reason: no
                // classifier ran, the sheet's own word is what arrived.
                // `reply_text` carries the verdict itself — it is the only
                // text the sheet recorded.
                // Agent/label targets sit outside the triage CHECK's kind
                // vocabulary — their replies still record, they just do
                // not mint a queue row the CHECK would refuse.
                const TRIAGE_KINDS: &[&str] = &[
                    "playlist",
                    "radio",
                    "press",
                    "creator",
                    "support_slot",
                    "endorsement",
                    "media_patronage",
                    "organiser",
                ];
                if entry.needs_review() && TRIAGE_KINDS.contains(&target_kind.as_str()) {
                    let verdict_text = entry
                        .verdict()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("(no verdict recorded)");
                    let reply_text = format!(
                        "[imported sheet verdict] {}",
                        verdict_text.chars().take(3900).collect::<String>()
                    );
                    // Re-scans reach here only for rows the interaction
                    // insert did not dedupe (a send+reply pair the sheet
                    // gained), so the triage row still needs its own
                    // conflict guard: the unique key on
                    // (workspace_id, target_id, classified_at) can already
                    // hold the backfill-minted row for the same reply, and
                    // an unguarded insert would roll back the whole row.
                    let minted = sqlx::query(
                        r#"
                        INSERT INTO reply_classifications (
                            workspace_id, target_id, target_kind, reply_text,
                            previous_disposition, classification_result,
                            classified_disposition, human_review_reason,
                            confidence_basis_points, matched_rules, classified_at
                        ) VALUES ($1,$2,$3,$4,NULL,'needs_human',NULL,'imported_verdict',
                                  0,'[]'::jsonb,$5)
                        ON CONFLICT (workspace_id, target_id, classified_at) DO NOTHING
                        "#,
                    )
                    .bind(workspace_id)
                    .bind(target_id)
                    .bind(&target_kind)
                    .bind(reply_text)
                    .bind(occurred_at)
                    .execute(&mut *tx)
                    .await?;
                    if minted.rows_affected() > 0 {
                        outcome.triage_minted = 1;
                    }
                }
            } else {
                outcome.already_present += 1;
            }
        }

        tx.commit().await?;
        Ok(outcome)
    }
}

#[derive(Default)]
struct EntryOutcome {
    sends_recorded: u64,
    replies_recorded: u64,
    already_present: u64,
    unmatched: u64,
    triage_minted: u64,
}

/// The send-side metadata the one-off import wrote, kept verbatim:
/// master's full set plus `source: "{file}#{sheet}"` on both books (the
/// promo rows carried it and master rows are better with it than without).
fn outbound_metadata(source_label: &str, book: OutreachBook, entry: &OutreachLogEntry) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("source".to_owned(), json!(source_label));
    map.insert("outreach_id".to_owned(), json!(entry.outreach_id));
    let map = &mut map;
    put(map, "entity_ref", &entry.entity_ref);

    if matches!(book, OutreachBook::Promo | OutreachBook::Scout) {
        put(map, "name", &entry.name);
    }
    if book == OutreachBook::Promo {
        // The one-off import keyed promo's lead under `lead_id`.
        if let Some(lead) = &entry.entity_ref {
            map.insert("lead_id".to_owned(), json!(lead));
        }
        put(map, "segment", &entry.segment);
    }
    put(map, "channel", &entry.channel);
    put(map, "purpose", &entry.purpose);
    put(map, "subject", &entry.subject);
    put(map, "status", &entry.status);
    put(map, "result", &entry.result);
    put(map, "followup_due", &entry.followup_due_raw);
    put(map, "next_step", &entry.next_step);
    put(map, "source_system", &entry.source_system);
    put(map, "source_id", &entry.source_id);
    put(map, "send_guard_key", &entry.send_guard_key);
    put(map, "gmail_thread_id", &entry.gmail_thread_id);
    put(map, "gmail_message_id", &entry.gmail_message_id);
    Value::Object(map.clone())
}

/// The reply-side metadata — `result`/`reply_type` on master rows,
/// `response_type` (plus `result` when `Wynik` carried a value) on promo,
/// matching the keys the stored rows already hold.
fn reply_metadata(book: OutreachBook, entry: &OutreachLogEntry) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("outreach_id".to_owned(), json!(entry.outreach_id));
    let map = &mut map;
    put(map, "result", &entry.result);
    match book {
        OutreachBook::Master => put(map, "reply_type", &entry.reply_type),
        OutreachBook::Promo => put(map, "response_type", &entry.response_type),
        // SCOUT has no separate reply-type column; its Result / Status cell
        // is already preserved above as `result`.
        OutreachBook::Scout => {}
    }
    Value::Object(map.clone())
}

fn put(map: &mut serde_json::Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), json!(value));
    }
}