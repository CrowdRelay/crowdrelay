//! Gmail contacts sync: harvest correspondent addresses from the connected
//! mailbox's message headers and stage them into the same review queue the
//! Drive connector feeds — dedup by (workspace, normalized_email) means an
//! address already found in a spreadsheet converges to one row, `sources`
//! recording both sightings.
//!
//! Privacy boundary: `format=metadata` with an explicit metadataHeaders
//! allowlist — From/To/Cc/Subject/Date only. Message bodies are never
//! requested, never read, never stored.
//!
//! Incremental: the connection's `sync_cursor` holds Gmail's historyId.
//! First run is a bounded full sweep (newest messages first); later cycles
//! walk `history.list` for added messages only, and the cursor advances
//! only to the last history record actually consumed — a cycle that hits
//! the message cap leaves the rest for the next cycle instead of skipping
//! them forever.
//!
//! Known gap: Gmail keeps history records for ~30 days. A cursor that
//! falls behind that window 404s and falls back to a bounded full sweep
//! of the newest messages — mail between the lost window and the sweep's
//! reach is never scanned. A pageToken backfill is the v2 answer; for v1
//! the gap is a bounded, logged, one-time loss on long-disconnected
//! mailboxes, never a silent ongoing one.
//!
//! The cursor advances only after the cycle's upserts committed — a crash
//! re-reads the same history and dedup makes it a no-op.
//!
//! Nothing here classifies: every address lands `staged`, suggested_kind
//! NULL — the operator decides fan vs outreach per destination.

use std::collections::HashSet;
use std::time::Duration;

use crowdrelay_domain::drive_contacts::{ExtractedContact, extract_header_contacts};
use crowdrelay_infra::{
    gdrive::{GDriveError, PostgresGDriveRepository},
    sensitive_response::SensitiveResponseKey,
};
use sqlx::{PgPool, postgres::PgListener};
use thiserror::Error;
use tokio::{sync::watch, time::interval};
use uuid::Uuid;

use crate::google_oauth::{access_token_for_connection, resolve_google_access_token};

const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Bound on messages listed in one cycle — a first sweep covers recent
/// mail; the history cursor covers the tail thereafter.
const MAX_MESSAGES_PER_CYCLE: usize = 500;
/// Bound on history records read in one incremental cycle.
const MAX_HISTORY_RECORDS: usize = 2000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "CrowdRelay/1.0 (gmail contacts sync)";
/// Header allowlist — the only fields the API is asked for.
const METADATA_HEADERS: &[&str] = &["From", "To", "Cc", "Subject", "Date"];

#[derive(Debug, Error)]
pub enum GmailContactsSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct GmailContactsSyncWorker {
    repo: PostgresGDriveRepository,
    http_client: reqwest::Client,
    workspace_id: Uuid,
    response_encryption_key: SensitiveResponseKey,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct GmailProfile {
    #[serde(rename = "emailAddress")]
    email_address: String,
    #[serde(rename = "historyId")]
    history_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct MessageList {
    messages: Option<Vec<MessageRef>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct HistoryList {
    history: Option<Vec<HistoryRecord>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct HistoryRecord {
    // Each record carries its own historyId — the cursor may only advance
    // to the last record whose messages were actually consumed.
    id: Option<String>,
    #[serde(rename = "messagesAdded")]
    messages_added: Option<Vec<HistoryMessageAdded>>,
}

#[derive(Debug, serde::Deserialize)]
struct HistoryMessageAdded {
    message: MessageRef,
}

#[derive(Debug, serde::Deserialize)]
struct GmailMessage {
    id: String,
    payload: Option<MessagePayload>,
}

#[derive(Debug, serde::Deserialize)]
struct MessagePayload {
    headers: Option<Vec<MessageHeader>>,
}

#[derive(Debug, serde::Deserialize)]
struct MessageHeader {
    name: String,
    value: String,
}

impl GmailContactsSyncWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: Uuid,
        response_encryption_key: SensitiveResponseKey,
    ) -> Result<Self, GmailContactsSyncError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(GmailContactsSyncError::ClientBuild)?;
        Ok(Self {
            repo: PostgresGDriveRepository::new(pool),
            http_client,
            workspace_id,
            response_encryption_key,
            google_client_id: std::env::var("CROWDRELAY_GOOGLE_ADS_CLIENT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            google_client_secret: std::env::var("CROWDRELAY_GOOGLE_ADS_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        })
    }

    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), GmailContactsSyncError> {
        tracing::info!("gmail contacts sync worker started");

        let mut listener = PgListener::connect_with(self.repo.pool())
            .await
            .map_err(GmailContactsSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(GmailContactsSyncError::Database)?;
        listener
            .listen("gdrive_contacts")
            .await
            .map_err(GmailContactsSyncError::Database)?;

        self.sync_cycle().await;

        let mut tick = interval(SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("gmail contacts sync worker shutting down");
                        return Ok(());
                    }
                }
                _ = listener.recv() => {
                    self.sync_cycle().await;
                }
                _ = tick.tick() => {
                    self.sync_cycle().await;
                }
            }
        }
    }

    async fn sync_cycle(&self) {
        let connections = match self.repo.due_connections(self.workspace_id, "gmail").await {
            Ok(c) => c,
            Err(error) => {
                tracing::warn!(%error, "gmail contacts: connection list failed");
                return;
            }
        };
        for (connection_id, account_ref) in connections {
            if let Err(error) = self.sync_connection(connection_id, &account_ref).await {
                tracing::warn!(%error, connection_id = %connection_id, "gmail contacts sync failed");
                let message = error.to_string();
                // Expired means "needs an operator reconnect", not "Google
                // had a bad minute": only auth-level failures qualify. A
                // transient 5xx/429 from the token endpoint stays a sync
                // error and retries next cycle.
                let expired = message.contains("invalid_grant")
                    || message.contains("refresh failed status=400")
                    || message.contains("refresh failed status=401")
                    || message.contains("missing encrypted_refresh_token");
                let mark = if expired {
                    self.repo
                        .mark_expired(self.workspace_id, connection_id, &message)
                        .await
                } else {
                    self.repo
                        .mark_sync_error(self.workspace_id, connection_id, &message)
                        .await
                };
                if let Err(e) = mark {
                    tracing::warn!(%e, "gmail contacts: sync status write failed");
                }
            }
        }
    }

    async fn access_token(&self, connection_id: Uuid, account_ref: &str) -> Result<String, String> {
        resolve_google_access_token(
            &self.repo,
            &self.http_client,
            &self.response_encryption_key,
            self.workspace_id,
            connection_id,
            account_ref,
            "gmail",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        connection_id: Uuid,
        url: &str,
        params: &[(&str, String)],
    ) -> Result<T, String> {
        let token = access_token_for_connection(
            &self.repo,
            &self.http_client,
            &self.response_encryption_key,
            self.workspace_id,
            connection_id,
            "gmail",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await?;
        let response = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .query(params)
            .send()
            .await
            .map_err(|e| format!("gmail request failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "gmail request failed status={}",
                response.status().as_u16()
            ));
        }
        response
            .json::<T>()
            .await
            .map_err(|e| format!("gmail response parse failed: {e}"))
    }

    /// Returns the HTTP status of a failed GET so the history fallback can
    /// tell "cursor too old" (404) from a real failure.
    async fn get_or_status(
        &self,
        connection_id: Uuid,
        url: &str,
        params: &[(&str, String)],
    ) -> Result<(u16, Option<serde_json::Value>), String> {
        let token = access_token_for_connection(
            &self.repo,
            &self.http_client,
            &self.response_encryption_key,
            self.workspace_id,
            connection_id,
            "gmail",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await?;
        let response = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .query(params)
            .send()
            .await
            .map_err(|e| format!("gmail request failed: {e}"))?;
        let status = response.status().as_u16();
        let body = response.json::<serde_json::Value>().await.ok();
        Ok((status, body))
    }

    async fn sync_connection(&self, connection_id: Uuid, account_ref: &str) -> Result<(), String> {
        let _ = self.access_token(connection_id, account_ref).await?;
        let profile: GmailProfile = self
            .get(
                connection_id,
                "https://gmail.googleapis.com/gmail/v1/users/me/profile",
                &[],
            )
            .await?;
        let self_email = profile.email_address;
        let new_cursor = profile.history_id;
        let old_cursor = self
            .repo
            .sync_cursor(self.workspace_id, connection_id)
            .await
            .map_err(|e| e.to_string())?;

        let mut ids: Vec<String> = Vec::new();
        let mut full_sweep = old_cursor.is_none();
        // The cursor may only advance to history we actually consumed —
        // jumping to the profile's historyId after truncating the list
        // would skip messages permanently.
        let mut cursor_to_set = new_cursor.clone();
        if let Some(cursor) = old_cursor.as_deref() {
            match self.history_ids(connection_id, cursor).await {
                Ok((history, last_consumed)) => {
                    ids.extend(history);
                    if let Some(consumed_id) = last_consumed {
                        cursor_to_set = consumed_id;
                    }
                }
                Err(HistoryError::StaleCursor) => full_sweep = true,
                Err(HistoryError::Other(e)) => return Err(e),
            }
        }
        if full_sweep {
            ids.extend(self.list_message_ids(connection_id).await?);
        }
        ids.truncate(MAX_MESSAGES_PER_CYCLE);

        let mut scanned = 0usize;
        let mut failed = 0usize;
        let mut upserted = 0u64;
        for id in &ids {
            match self.scan_message(connection_id, id, &self_email).await {
                Ok(n) => {
                    scanned += 1;
                    upserted += n;
                }
                Err(error) => {
                    failed += 1;
                    tracing::warn!(%error, message_id = %id, "gmail message scan failed");
                }
            }
        }

        // The cursor covers everything listed this cycle (history ids are
        // monotonic); advancing it is what makes the next cycle incremental.
        self.repo
            .set_sync_cursor(self.workspace_id, connection_id, &cursor_to_set)
            .await
            .map_err(|e| e.to_string())?;
        self.repo
            .mark_sync_ok(self.workspace_id, connection_id)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            full_sweep,
            messages = scanned,
            failed,
            contacts_upserted = upserted,
            "gmail contacts sync cycle complete"
        );
        Ok(())
    }

    /// Message ids added since `cursor`, paged and bounded. A 404 means the
    /// historyId predates Gmail's retention of history records — the caller
    /// falls back to a bounded full sweep.
    ///
    /// Returns `(message ids, last consumed history id)`. Collection stops
    /// once `MAX_MESSAGES_PER_CYCLE` ids are gathered and the second tuple
    /// item is the id of the last history record actually consumed — the
    /// caller stores it as the new cursor so records past the cap stay in
    /// the next cycle's window instead of being skipped forever.
    async fn history_ids(
        &self,
        connection_id: Uuid,
        cursor: &str,
    ) -> Result<(Vec<String>, Option<String>), HistoryError> {
        let mut ids = Vec::new();
        let mut last_consumed: Option<String> = None;
        let mut capped = false;
        let mut page_token: Option<String> = None;
        'pages: loop {
            let mut params: Vec<(&str, String)> = vec![
                ("startHistoryId", cursor.to_string()),
                ("historyTypes", "messageAdded".to_string()),
                ("maxResults", "500".to_string()),
            ];
            if let Some(token) = &page_token {
                params.push(("pageToken", token.clone()));
            }
            let (status, body) = self
                .get_or_status(
                    connection_id,
                    "https://gmail.googleapis.com/gmail/v1/users/me/history",
                    &params,
                )
                .await
                .map_err(HistoryError::Other)?;
            if status == 404 || status == 400 {
                return Err(HistoryError::StaleCursor);
            }
            if status != 200 {
                return Err(HistoryError::Other(format!(
                    "history.list failed status={status}"
                )));
            }
            let page: HistoryList = serde_json::from_value(body.unwrap_or_default())
                .map_err(|e| HistoryError::Other(format!("history.list parse failed: {e}")))?;
            for record in page.history.unwrap_or_default() {
                if ids.len() >= MAX_MESSAGES_PER_CYCLE {
                    capped = true;
                    break 'pages;
                }
                for added in record.messages_added.unwrap_or_default() {
                    if ids.len() >= MAX_MESSAGES_PER_CYCLE {
                        capped = true;
                        break;
                    }
                    ids.push(added.message.id);
                }
                if let Some(id) = record.id {
                    last_consumed = Some(id);
                }
                if capped {
                    break 'pages;
                }
            }
            match page.next_page_token {
                Some(token) if ids.len() < MAX_HISTORY_RECORDS => page_token = Some(token),
                _ => break,
            }
        }
        // One message can appear in several history records.
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
        // A cap mid-stream means unconsumed history still sits past
        // `last_consumed` — the cursor must stop there, not at the profile's
        // newest historyId. An uncapped read consumed everything, so None
        // lets the caller advance to the profile cursor.
        Ok((ids, capped.then_some(last_consumed).flatten()))
    }

    /// Bounded first sweep: newest messages first, spam and trash excluded.
    async fn list_message_ids(&self, connection_id: Uuid) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut params: Vec<(&str, String)> = vec![
                ("q", "-in:spam -in:trash".to_string()),
                ("maxResults", "100".to_string()),
            ];
            if let Some(token) = &page_token {
                params.push(("pageToken", token.clone()));
            }
            let page: MessageList = self
                .get(
                    connection_id,
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages",
                    &params,
                )
                .await?;
            ids.extend(page.messages.unwrap_or_default().into_iter().map(|m| m.id));
            match page.next_page_token {
                Some(token) if ids.len() < MAX_MESSAGES_PER_CYCLE => page_token = Some(token),
                _ => break,
            }
        }
        Ok(ids)
    }

    /// One message → header contacts → staging upsert. Provenance is the
    /// message id plus "From: … · Subject: …" — enough for the reviewer to
    /// place the contact without the body ever being read.
    async fn scan_message(
        &self,
        connection_id: Uuid,
        message_id: &str,
        self_email: &str,
    ) -> Result<u64, String> {
        let mut params: Vec<(&str, String)> = vec![("format", "metadata".to_string())];
        for header in METADATA_HEADERS {
            params.push(("metadataHeaders", (*header).to_string()));
        }
        let message: GmailMessage = self
            .get(
                connection_id,
                &format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{message_id}"),
                &params,
            )
            .await?;
        let headers = message.payload.and_then(|p| p.headers).unwrap_or_default();
        let values_of = |name: &str| -> Vec<String> {
            headers
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case(name))
                .map(|h| h.value.clone())
                .collect()
        };
        let address_headers: Vec<String> = ["From", "To", "Cc"]
            .iter()
            .flat_map(|name| values_of(name))
            .collect();
        let contacts = extract_header_contacts(&address_headers, self_email);
        if contacts.is_empty() {
            return Ok(0);
        }
        let from = values_of("From").into_iter().next().unwrap_or_default();
        let subject = values_of("Subject").into_iter().next().unwrap_or_default();
        let provenance = format!("{} — {}", from.trim(), subject.trim())
            .trim_matches(|c| c == '—' || c == ' ')
            .chars()
            .take(500)
            .collect::<String>();
        // source_file_name's CHECK rejects empty — a draft or malformed
        // header pair still needs a name for the review row.
        let provenance = if provenance.is_empty() {
            format!("gmail message {message_id}")
        } else {
            provenance
        };
        let extracted: Vec<ExtractedContact> = contacts
            .into_iter()
            .map(|c| ExtractedContact {
                email: c.email,
                display_name: c.display_name,
                organization: None,
                phone: None,
                suggested_kind: None,
                notes: None,
            })
            .collect();
        let summary = self
            .repo
            .upsert_contacts_for_source(
                self.workspace_id,
                "gmail",
                &message.id,
                &provenance,
                &extracted,
                false,
            )
            .await
            .map_err(|e: GDriveError| e.to_string())?;
        Ok(summary.upserted)
    }
}

enum HistoryError {
    StaleCursor,
    Other(String),
}
