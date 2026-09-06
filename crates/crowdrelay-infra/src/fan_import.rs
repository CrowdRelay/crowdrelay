//! Pilot mailing-list import persistence.
//!
//! Consent comes first: every imported address lands as `pending` and receives
//! the canonical double-opt-in confirmation through the workspace's own outbox.
//! Active fans are never touched, opt-outs are never resurrected, and the
//! whole batch commits atomically with one source-labelled audit row.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone)]
pub struct PostgresFanImportRepository {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub email: String,
    pub display_name: Option<String>,
    pub locale: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportCounts {
    pub imported_pending: u32,
    pub confirmation_resent: u32,
    pub already_active: u32,
    pub skipped_suppressed: u32,
    pub cooldown_skipped: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FanImportError {
    #[error("fan import database operation failed")]
    Database(#[from] sqlx::Error),
}

impl PostgresFanImportRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports one validated batch. Returns per-outcome counters for the API
    /// response; addresses never appear in any return value.
    ///
    /// # Shape
    ///
    /// Seven set-based phases, not a loop. This used to issue up to seven
    /// round trips *per entry* inside one transaction, holding a row lock on
    /// every address for the whole batch. The statement count is now fixed
    /// whatever the batch size, and the lock sweep takes its rows in address
    /// order so two concurrent imports over overlapping addresses cannot
    /// deadlock on caller ordering.
    ///
    /// The counting rules are unchanged: an address repeated inside one batch
    /// is admitted once and confirmed once, with the later occurrences landing
    /// in the cooldown the first one opened — which is what the sequential
    /// version produced when it re-read the row it had just written.
    pub async fn import_batch(
        &self,
        workspace_id: Uuid,
        source: &str,
        entries: &[ImportEntry],
        access_token_ttl_days: i64,
        resend_cooldown_seconds: i64,
    ) -> Result<ImportCounts, FanImportError> {
        let mut tx = self.pool.begin().await.map_err(FanImportError::Database)?;

        let mut counts = ImportCounts::default();
        let batch_request_id = format!("fan-import-{}", Uuid::now_v7().simple());

        // Distinct addresses in first-seen order; a repeat is every later
        // occurrence of one already claimed.
        let mut emails: Vec<&str> = Vec::new();
        let mut candidates: Vec<(&ImportEntry, bool)> = Vec::with_capacity(entries.len());
        for entry in entries {
            let repeat = emails.contains(&entry.email.as_str());
            if !repeat {
                emails.push(entry.email.as_str());
            }
            candidates.push((entry, repeat));
        }

        if candidates.is_empty() {
            Self::record_audit(&mut tx, workspace_id, source, &counts).await?;
            tx.commit().await.map_err(FanImportError::Database)?;
            return Ok(counts);
        }

        // ── 1. Current status of every address, locked in a stable order ──
        let owned_emails: Vec<String> = emails.iter().map(|value| (*value).to_owned()).collect();
        let existing: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT normalized_email, status FROM fans
            WHERE workspace_id = $1 AND normalized_email = ANY($2)
            ORDER BY normalized_email
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(&owned_emails)
        .fetch_all(&mut *tx)
        .await
        .map_err(FanImportError::Database)?;
        let status_of: HashMap<&str, &str> = existing
            .iter()
            .map(|(email, status)| (email.as_str(), status.as_str()))
            .collect();

        // An address whose row carries a status this code does not model is a
        // schema surprise, not an import outcome. Refused before anything is
        // written, exactly as the sequential version refused mid-loop.
        for (email, status) in &status_of {
            if !matches!(
                *status,
                "active" | "unsubscribed" | "suppressed" | "pending"
            ) {
                tracing::error!(status = %status, "unexpected fan status during import");
                let _ = email;
                return Err(FanImportError::Database(sqlx::Error::ColumnDecode {
                    index: "status".to_owned(),
                    source: "unexpected fan status during import".into(),
                }));
            }
        }

        // ── 2. Create every unknown address as pending, in one statement ──
        //
        // `ON CONFLICT DO NOTHING` where the sequential version had a bare
        // insert: a concurrent import that admitted the same address between
        // the lock sweep and this write used to abort the whole transaction.
        let mut new_emails: Vec<String> = Vec::new();
        let mut new_names: Vec<Option<String>> = Vec::new();
        let mut new_locales: Vec<Option<String>> = Vec::new();
        for (entry, repeat) in &candidates {
            if *repeat || status_of.contains_key(entry.email.as_str()) {
                continue;
            }
            new_emails.push(entry.email.clone());
            new_names.push(entry.display_name.clone());
            new_locales.push(entry.locale.clone());
        }
        if !new_emails.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO fans (workspace_id, normalized_email, display_name, locale, status)
                SELECT $1, candidate.email, candidate.display_name, candidate.locale, 'pending'
                FROM unnest($2::text[], $3::text[], $4::text[])
                    AS candidate(email, display_name, locale)
                ON CONFLICT (workspace_id, normalized_email) DO NOTHING
                "#,
            )
            .bind(workspace_id)
            .bind(&new_emails)
            .bind(&new_names)
            .bind(&new_locales)
            .execute(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
        }

        // ── 3. Count, and collect the addresses still in the running ──────
        let mut senders: Vec<&str> = Vec::new();
        for (entry, repeat) in &candidates {
            let email = entry.email.as_str();
            match status_of.get(email).copied() {
                Some("active") => counts.already_active += 1,
                Some("unsubscribed" | "suppressed") => counts.skipped_suppressed += 1,
                // Known pending, or freshly created by phase 2.
                _ => {
                    if !status_of.contains_key(email) && !*repeat {
                        counts.imported_pending += 1;
                    }
                    if *repeat {
                        // The first occurrence opened the cooldown.
                        counts.cooldown_skipped += 1;
                    } else {
                        senders.push(email);
                    }
                }
            }
        }

        if senders.is_empty() {
            Self::record_audit(&mut tx, workspace_id, source, &counts).await?;
            tx.commit().await.map_err(FanImportError::Database)?;
            return Ok(counts);
        }

        // ── 4. Resolve the remaining addresses to their fans ──────────────
        let sender_emails: Vec<String> = senders.iter().map(|value| (*value).to_owned()).collect();
        let resolved: Vec<(String, Uuid)> = sqlx::query_as(
            "SELECT normalized_email, id FROM fans \
             WHERE workspace_id = $1 AND normalized_email = ANY($2)",
        )
        .bind(workspace_id)
        .bind(&sender_emails)
        .fetch_all(&mut *tx)
        .await
        .map_err(FanImportError::Database)?;
        let fan_of: HashMap<&str, Uuid> = resolved
            .iter()
            .map(|(email, id)| (email.as_str(), *id))
            .collect();

        // ── 5. Which of them are inside their confirmation cooldown ───────
        //
        // Only an address that already existed can be: a row created by phase
        // 2 has no token, and `fan_action_tokens.fan_id` is a foreign key, so
        // there is nothing for it to match.
        let cooldown_ids: Vec<Uuid> = senders
            .iter()
            .filter(|email| status_of.contains_key(**email))
            .filter_map(|email| fan_of.get(*email).copied())
            .collect();
        let mut in_cooldown: HashSet<Uuid> = HashSet::new();
        if !cooldown_ids.is_empty() {
            let rows: Vec<(Uuid,)> = sqlx::query_as(
                r#"
                SELECT DISTINCT fan_id FROM fan_action_tokens
                WHERE workspace_id = $1 AND fan_id = ANY($2)
                  AND purpose = 'confirm'
                  AND consumed_at IS NULL AND expires_at > now()
                  AND created_at > now() - ($3::bigint * interval '1 second')
                "#,
            )
            .bind(workspace_id)
            .bind(&cooldown_ids)
            .bind(resend_cooldown_seconds)
            .fetch_all(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
            in_cooldown = rows.into_iter().map(|(id,)| id).collect();
        }

        let mut recipients: Vec<(Uuid, &ImportEntry)> = Vec::new();
        for (entry, repeat) in &candidates {
            if *repeat {
                continue;
            }
            let email = entry.email.as_str();
            if !senders.contains(&email) {
                continue;
            }
            let Some(fan_id) = fan_of.get(email).copied() else {
                continue;
            };
            if in_cooldown.contains(&fan_id) {
                counts.cooldown_skipped += 1;
                continue;
            }
            recipients.push((fan_id, entry));
        }

        if !recipients.is_empty() {
            let sending: Vec<Uuid> = recipients.iter().map(|(fan_id, _)| *fan_id).collect();

            // ── 6. Retire every superseded confirmation token ─────────────
            sqlx::query(
                r#"
                UPDATE fan_action_tokens
                SET consumed_at = COALESCE(consumed_at, now())
                WHERE workspace_id = $1 AND fan_id = ANY($2)
                  AND purpose = 'confirm' AND consumed_at IS NULL
                "#,
            )
            .bind(workspace_id)
            .bind(&sending)
            .execute(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;

            // ── 7. Mint one token per recipient ───────────────────────────
            //
            // `MATERIALIZED` is load-bearing: `gen_random_bytes` is volatile
            // and the CTE is read twice, so an inlined CTE would hash one
            // value and hand back a different one.
            let minted: Vec<(Uuid, String)> = sqlx::query_as(
                r#"
                WITH material AS MATERIALIZED (
                    SELECT recipient.fan_id,
                           encode(gen_random_bytes(32), 'hex') AS token
                    FROM unnest($2::uuid[]) AS recipient(fan_id)
                ), inserted AS (
                    INSERT INTO fan_action_tokens (
                        workspace_id, fan_id, purpose, token_hash, expires_at
                    )
                    SELECT $1, material.fan_id, 'confirm',
                        digest(material.token, 'sha256'),
                        now() + ($3::bigint * interval '1 day')
                    FROM material
                    RETURNING fan_id
                )
                SELECT material.fan_id, material.token
                FROM material
                WHERE EXISTS (SELECT 1 FROM inserted)
                "#,
            )
            .bind(workspace_id)
            .bind(&sending)
            .bind(access_token_ttl_days)
            .fetch_all(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
            let token_of: HashMap<Uuid, &str> = minted
                .iter()
                .map(|(fan_id, token)| (*fan_id, token.as_str()))
                .collect();

            let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(recipients.len());
            let mut request_ids: Vec<String> = Vec::with_capacity(recipients.len());
            for (position, (fan_id, entry)) in recipients.iter().enumerate() {
                payloads.push(serde_json::json!({
                    "workspace_id": workspace_id,
                    "fan_id": fan_id,
                    "email": entry.email,
                    "display_name": entry.display_name,
                    "locale": entry.locale,
                    "confirmation_token": token_of.get(fan_id).copied(),
                    "import_source": source.trim(),
                }));
                request_ids.push(format!("{batch_request_id}:{position}"));
                counts.confirmation_resent += 1;
            }
            sqlx::query(
                r#"
                INSERT INTO outbox_events (
                    workspace_id, event_type, event_version, payload, request_id
                )
                SELECT $1, 'fan.confirmation_requested', 1,
                       event.payload, event.request_id
                FROM unnest($2::jsonb[], $3::text[]) AS event(payload, request_id)
                "#,
            )
            .bind(workspace_id)
            .bind(&payloads)
            .bind(&request_ids)
            .execute(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
        }

        Self::record_audit(&mut tx, workspace_id, source, &counts).await?;
        tx.commit().await.map_err(FanImportError::Database)?;
        Ok(counts)
    }

    async fn record_audit(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: Uuid,
        source: &str,
        counts: &ImportCounts,
    ) -> Result<(), FanImportError> {
        sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id, metadata
            ) VALUES ($1, 'service', 'fans.imported', 'workspace', $2, $3)
            "#,
        )
        .bind(workspace_id)
        .bind(workspace_id.to_string())
        .bind(serde_json::json!({
            "source": source.trim(),
            "imported_pending": counts.imported_pending,
            "confirmation_resent": counts.confirmation_resent,
            "already_active": counts.already_active,
            "skipped_suppressed": counts.skipped_suppressed,
            "cooldown_skipped": counts.cooldown_skipped,
        }))
        .execute(&mut **tx)
        .await
        .map_err(FanImportError::Database)?;
        Ok(())
    }
}
