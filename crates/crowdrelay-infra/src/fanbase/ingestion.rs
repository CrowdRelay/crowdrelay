//! The fanbase ingestion batch.
//!
//! Split out of `fanbase.rs` when the sequential per-entry loop became eight
//! set-based phases: the phases carry the counting rules as prose, and the
//! parent module is already at the size ratchet's ceiling with the fanbase
//! CRUD and the platform connections it also owns.
//!
//! A child module, so it keeps access to the parent's private helpers
//! (`assert_fanbase`, `unexpected`) without widening them.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use super::{FanbaseEntry, FanbaseError, IngestionCounts, PostgresFanbaseRepository};
use crowdrelay_domain::fanbase::{AdmissionAction, admission_for};

impl PostgresFanbaseRepository {
    /// Runs one ingestion batch. Per candidate the admission policy decides:
    /// create pending (+ canonical confirmation email), resend within cooldown,
    /// count as already-active, or skip an opt-out. Membership is attributed
    /// per external id either way, and the ledger row closes atomically.
    ///
    /// # Shape
    ///
    /// Eight set-based phases, not a loop. This used to issue up to eight
    /// round trips *per entry* inside one transaction — 500 entries is the
    /// API's cap, so roughly four thousand round trips while holding row
    /// locks the whole time. The work is now a fixed number of statements
    /// whatever the batch size, and the `FOR UPDATE` sweep takes its locks in
    /// address order, so two concurrent batches over overlapping addresses
    /// can no longer deadlock by locking the same rows in caller order.
    ///
    /// The counting rules are unchanged and are the reason this is written
    /// out rather than expressed as one clever statement:
    ///
    /// * the counters partition the batch — they sum to `received`;
    /// * an address repeated inside one batch is admitted once and sends one
    ///   confirmation; later occurrences land in the cooldown the first one
    ///   opened, exactly as the sequential version did when it re-read the
    ///   row it had just written;
    /// * membership is keyed by external id, so every entry with an address
    ///   is attributed even when several share a fan.
    pub async fn ingest_candidates(
        &self,
        workspace_id: Uuid,
        fanbase_id: Uuid,
        entries: &[FanbaseEntry],
        access_token_ttl_days: i64,
        resend_cooldown_seconds: i64,
    ) -> Result<IngestionCounts, FanbaseError> {
        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        Self::assert_fanbase(&mut tx, workspace_id, fanbase_id).await?;

        let run_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO fanbase_ingestions (workspace_id, fanbase_id, status)
            VALUES ($1, $2, 'running')
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(fanbase_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(id)) => id,
            Ok(None) => return Err(FanbaseError::NotFound),
            Err(e) => return Err(FanbaseError::Database(e)),
        };

        let mut counts = IngestionCounts {
            received: u32::try_from(entries.len()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        let batch_request_id = format!(
            "fanbase-ingest-{}-{}",
            fanbase_id.simple(),
            Uuid::now_v7().simple()
        );

        // ── Local classification ────────────────────────────────────────
        //
        // An entry without a usable address is invalid and never reaches the
        // database. `first_seen` records which occurrence of an address owns
        // the admission decision; every later one is a repeat.
        let mut candidates: Vec<(&FanbaseEntry, &str, bool)> = Vec::new();
        let mut emails: Vec<&str> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for entry in entries {
            let Some(email) = entry
                .email
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                counts.invalid += 1;
                continue;
            };
            let repeat = !seen.insert(email);
            if !repeat {
                emails.push(email);
            }
            candidates.push((entry, email, repeat));
        }

        if candidates.is_empty() {
            Self::close_ingestion(&mut tx, workspace_id, run_id, &counts).await?;
            tx.commit().await.map_err(Self::unexpected)?;
            return Ok(counts);
        }

        // ── 1. Current status of every address, resolved through the spine ──
        //
        // `fan_identifiers` wins so a merged-away address answers the
        // surviving fan's id/status; the fans row is the fallback for
        // records the spine does not cover. All resolved fans are locked
        // in a stable order before anything is written.
        let owned_emails: Vec<String> = emails.iter().map(|value| (*value).to_owned()).collect();
        let existing: Vec<(String, Uuid, String)> = sqlx::query_as(
            r#"
            SELECT i.value, f.id, f.status FROM fan_identifiers i
            JOIN fans f ON f.workspace_id = i.workspace_id AND f.id = i.fan_id
            WHERE i.workspace_id = $1 AND i.kind = 'email' AND i.value = ANY($2)
            UNION ALL
            SELECT f.normalized_email, f.id, f.status FROM fans f
            WHERE f.workspace_id = $1 AND f.normalized_email = ANY($2)
              AND NOT EXISTS (
                  SELECT 1 FROM fan_identifiers i
                  WHERE i.workspace_id = $1 AND i.kind = 'email'
                    AND i.value = f.normalized_email)
            "#,
        )
        .bind(workspace_id)
        .bind(&owned_emails)
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::unexpected)?;
        let resolved_ids: Vec<Uuid> = existing.iter().map(|(_, id, _)| *id).collect();
        if !resolved_ids.is_empty() {
            sqlx::query(
                "SELECT id FROM fans WHERE workspace_id = $1 AND id = ANY($2) \
                 ORDER BY id FOR UPDATE",
            )
            .bind(workspace_id)
            .bind(&resolved_ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
        }
        let status_of: HashMap<&str, &str> = existing
            .iter()
            .map(|(email, _, status)| (email.as_str(), status.as_str()))
            .collect();
        let id_of: HashMap<&str, Uuid> = existing
            .iter()
            .map(|(email, id, _)| (email.as_str(), *id))
            .collect();

        let action_of: HashMap<&str, AdmissionAction> = emails
            .iter()
            .map(|email| (*email, admission_for(status_of.get(*email).copied())))
            .collect();

        // ── 2. Create every fresh pending in one statement ────────────────
        //
        // `ON CONFLICT DO NOTHING` where the sequential version had a bare
        // insert: a concurrent batch that admitted the same address between
        // the lock sweep above and this write used to abort the whole
        // transaction, losing 499 good entries to one race.
        let mut new_emails: Vec<&str> = Vec::new();
        let mut new_names: Vec<Option<String>> = Vec::new();
        let mut new_locales: Vec<Option<String>> = Vec::new();
        for (entry, email, repeat) in &candidates {
            if *repeat {
                continue;
            }
            if action_of.get(email) == Some(&AdmissionAction::CreatePending) {
                new_emails.push(email);
                new_names.push(entry.display_name.clone());
                new_locales.push(entry.locale.clone());
            }
        }
        if !new_emails.is_empty() {
            let owned: Vec<String> = new_emails.iter().map(|value| (*value).to_owned()).collect();
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
            .bind(&owned)
            .bind(&new_names)
            .bind(&new_locales)
            .execute(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
        }

        // ── 3. Resolve every address to its fan ───────────────────────────
        //
        // `id_of` already carries the spine resolution from phase 1 (merged
        // addresses point at the survivor). Only addresses it does not
        // cover — the rows phase 2 just inserted — need a fresh lookup.
        let mut fan_of: HashMap<&str, Uuid> = HashMap::new();
        let mut unresolved: Vec<String> = Vec::new();
        for email in &emails {
            match id_of.get(*email) {
                Some(id) => {
                    fan_of.insert(*email, *id);
                }
                None => unresolved.push((*email).to_owned()),
            }
        }
        if !unresolved.is_empty() {
            let resolved: Vec<(String, Uuid)> = sqlx::query_as(
                "SELECT normalized_email, id FROM fans \
                 WHERE workspace_id = $1 AND normalized_email = ANY($2)",
            )
            .bind(workspace_id)
            .bind(&unresolved)
            .fetch_all(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
            for (email, id) in resolved {
                if let Some(key) = emails
                    .iter()
                    .find(|candidate| **candidate == email.as_str())
                {
                    fan_of.insert(key, id);
                }
            }
        }

        // ── 4. Count, and attribute membership per external id ────────────
        let mut member_fans: Vec<Uuid> = Vec::new();
        let mut member_externals: Vec<String> = Vec::new();
        // Addresses whose confirmation is still in question, in first-seen
        // order so the emitted request ids are stable for a given batch.
        let mut senders: Vec<&str> = Vec::new();
        for (entry, email, repeat) in &candidates {
            let Some(fan_id) = fan_of.get(email).copied() else {
                // The sequential version logged this as a fan row vanishing
                // between admission and lookup; batched, it means the insert
                // above neither created nor found the row.
                tracing::warn!(email, "fan row vanished after admission");
                counts.invalid += 1;
                continue;
            };
            member_fans.push(fan_id);
            member_externals.push(entry.external_id.clone());

            let action = action_of
                .get(email)
                .copied()
                .unwrap_or(AdmissionAction::SkipSuppressed);
            match action {
                AdmissionAction::AlreadyActive => counts.already_active += 1,
                AdmissionAction::SkipSuppressed => counts.skipped_suppressed += 1,
                AdmissionAction::CreatePending if !repeat => {
                    counts.imported_pending += 1;
                    senders.push(email);
                }
                // A repeated address, or one whose window may have lapsed.
                // Repeats are always in the cooldown the first occurrence
                // opened; a first ResendPending has to ask the database.
                AdmissionAction::CreatePending | AdmissionAction::ResendPending => {
                    if *repeat {
                        counts.cooldown_skipped += 1;
                    } else {
                        senders.push(email);
                    }
                }
            }
        }

        if !member_fans.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO fanbase_members (
                    workspace_id, fanbase_id, fan_id, external_id
                )
                SELECT $1, $2, member.fan_id, member.external_id
                FROM unnest($3::uuid[], $4::text[]) AS member(fan_id, external_id)
                ON CONFLICT (fanbase_id, external_id) DO UPDATE SET
                    last_seen_at = now(), fan_id = EXCLUDED.fan_id
                "#,
            )
            .bind(workspace_id)
            .bind(fanbase_id)
            .bind(&member_fans)
            .bind(&member_externals)
            .execute(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
        }

        // ── 5. Which candidates are inside their confirmation cooldown ────
        //
        // Only an address that was already `pending` can be: a fresh admission
        // has no token yet, which is why the sequential version short-circuited
        // on `existing == Some("pending")` before querying at all.
        let cooldown_ids: Vec<Uuid> = senders
            .iter()
            .filter(|email| status_of.get(**email).copied() == Some("pending"))
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
            .map_err(Self::unexpected)?;
            in_cooldown = rows.into_iter().map(|(id,)| id).collect();
        }

        let mut recipients: Vec<(Uuid, &str)> = Vec::new();
        for email in &senders {
            let Some(fan_id) = fan_of.get(*email).copied() else {
                continue;
            };
            if in_cooldown.contains(&fan_id) {
                counts.cooldown_skipped += 1;
                continue;
            }
            counts.confirmation_resent += 1;
            recipients.push((fan_id, email));
        }
        let sending: Vec<Uuid> = recipients.iter().map(|(fan_id, _)| *fan_id).collect();

        if !recipients.is_empty() {
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
            .map_err(Self::unexpected)?;

            // ── 7. Mint one token per recipient ───────────────────────────
            //
            // `MATERIALIZED` is load-bearing: `gen_random_bytes` is volatile
            // and the CTE is read twice, so an inlined CTE would hash one
            // value and return a different one.
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
            .map_err(Self::unexpected)?;
            let token_of: HashMap<Uuid, &str> = minted
                .iter()
                .map(|(fan_id, token)| (*fan_id, token.as_str()))
                .collect();

            // ── 8. One confirmation event per recipient ───────────────────
            //
            // Each carries its own request id. It used to interpolate
            // `counts.received`, which is the batch size and never changes, so
            // every event in a batch shipped the same
            // `X-CrowdRelay-Request-Id`. Delivery is at-least-once and
            // consumers dedupe, so a batch of five hundred confirmations was
            // one email to a consumer that took the contract at its word.
            let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(recipients.len());
            let mut request_ids: Vec<String> = Vec::with_capacity(recipients.len());
            for (position, (fan_id, email)) in recipients.iter().enumerate() {
                let entry = candidates
                    .iter()
                    .find(|(_, candidate, _)| candidate == email)
                    .map(|(entry, _, _)| *entry);
                payloads.push(serde_json::json!({
                    "workspace_id": workspace_id,
                    "fan_id": fan_id,
                    "email": email,
                    "display_name": entry.and_then(|value| value.display_name.clone()),
                    "locale": entry.and_then(|value| value.locale.clone()),
                    "confirmation_token": token_of.get(fan_id).copied(),
                }));
                request_ids.push(format!("{batch_request_id}:{position}"));
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
            .map_err(Self::unexpected)?;
        }

        Self::close_ingestion(&mut tx, workspace_id, run_id, &counts).await?;
        tx.commit().await.map_err(Self::unexpected)?;
        Ok(counts)
    }

    /// Closes the ledger row for one batch.
    ///
    /// Scoped on `workspace_id` as well as the row id. The id alone is unique,
    /// so the extra predicate changes no result -- it keeps the statement
    /// inside the rule the workspace-scope ratchet enforces, which is that a
    /// query touching a tenant table names the tenant, so tenant isolation
    /// never rests on an id being unguessable.
    async fn close_ingestion(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: Uuid,
        run_id: Uuid,
        counts: &IngestionCounts,
    ) -> Result<(), FanbaseError> {
        sqlx::query(
            r#"
            UPDATE fanbase_ingestions
            SET status = 'completed',
                received = $2, imported_pending = $3,
                already_active = $4, skipped_suppressed = $5,
                cooldown_skipped = $6, invalid = $7,
                confirmation_resent = $8,
                finished_at = now()
            WHERE workspace_id = $9 AND id = $1
            "#,
        )
        .bind(run_id)
        .bind(i32::try_from(counts.received).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.imported_pending).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.already_active).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.skipped_suppressed).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.cooldown_skipped).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.invalid).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.confirmation_resent).unwrap_or(i32::MAX))
        .bind(workspace_id)
        .execute(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }
}
