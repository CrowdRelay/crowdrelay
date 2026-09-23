//! Google Drive contacts sync: scan the connected Drive for tabular files,
//! extract email-bearing rows, and stage them for operator review.
//!
//! Everything this worker writes lands in `drive_contacts` —
//! never in `fans` or `agent_outreach_targets`. Classification is the
//! operator's job: a fan goes through `fan_import` (pending + DOI), a
//! beacon through the outreach screening queue, and one address may be
//! both.
//!
//! Wake paths: `growth_metric_sync` NOTIFY (a gdrive connection appears),
//! `gdrive_contacts` NOTIFY (Scan now), and an hourly sweep. Per-file the
//! Drive `modifiedTime` is compared against the mtime recorded in staging —
//! an unchanged file costs one tiny query, not an export.
//!
//! Bounds: one workspace, ≤200 files/cycle, ≤5000 rows/file, bounded page
//! sizes, per-connection failures logged not propagated.

use std::time::Duration;

use crowdrelay_domain::drive_contacts::parse_delimited;
use crowdrelay_domain::scan_scope::ScanScope;
use crowdrelay_infra::{
    gdrive::{GDriveError, PostgresGDriveRepository},
    sensitive_response::SensitiveResponseKey,
};

use crate::google_oauth::{access_token_for_connection, resolve_google_access_token};
use crate::sheet_intake::SHEET_INTAKE_REVISION;
use sqlx::{PgPool, postgres::PgListener};
use thiserror::Error;
use tokio::{sync::watch, time::interval};
use uuid::Uuid;

/// Contacts change slowly — an hour is fresh and leans on nothing.
const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Bound on Drive files considered per cycle.
const MAX_FILES_PER_CYCLE: usize = 200;
/// A folder scope walks this many levels deep and this many folders wide —
/// past that the scope is truncated, never unbounded.
const MAX_FOLDER_DEPTH: usize = 8;
const MAX_FOLDER_IDS: usize = 64;
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// Drive page size.
const DRIVE_PAGE_SIZE: usize = 100;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "CrowdRelay/1.0 (gdrive contacts sync)";

/// MIME types this connector reads. Google Docs and prose formats are
/// deliberately absent — v1 is tabular only, and a doc is not a contact
/// list.
const TABULAR_MIMES: &[&str] = &[
    "application/vnd.google-apps.spreadsheet",
    "text/csv",
    "text/tab-separated-values",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
];

#[derive(Debug, Error)]
pub enum GDriveContactsSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct GDriveContactsSyncWorker {
    repo: PostgresGDriveRepository,
    http_client: reqwest::Client,
    workspace_id: Uuid,
    response_encryption_key: SensitiveResponseKey,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
}

/// One Drive file descriptor from files.list.
#[derive(Debug, serde::Deserialize)]
struct DriveFile {
    id: String,
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
    #[serde(rename = "modifiedTime")]
    modified_time: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct DriveFileList {
    // Google omits empty repeated fields — a Drive with no tabular files
    // answers with no `files` key at all, not an empty array.
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Default)]
struct CycleCounts {
    files_considered: usize,
    files_scanned: usize,
    files_skipped_unchanged: usize,
    files_failed: usize,
    contacts_upserted: u64,
    /// Booking agents whose `active` flag this file's verdict rows flipped.
    agents_resolved: u64,
    rows_without_email: usize,
    marked_disappeared: u64,
    /// A venue seed sheet is not a contact list: rows that imported into
    /// the venue registry, rows refused, and rooms whose city the
    /// catalogue does not know.
    venues_imported: u64,
    venue_refusals: usize,
    venues_unknown_city: u64,
    /// The same accounting for a band seed sheet: acts that imported into
    /// the peer registry, rows refused, acts whose city named no catalogue
    /// city, and acts whose own write failed — a bad row is counted, never
    /// allowed to abort the rest of the sheet.
    peer_acts_imported: u64,
    peer_act_refusals: usize,
    peer_acts_unresolved_city: u64,
    /// The verification sheet's findings: acts a dead-band claim retired,
    /// and dead-band claims naming acts the registry never held.
    peer_acts_deactivated: u64,
    peer_acts_skipped_inactive: u64,
    peer_acts_failed: u64,
    /// Sheets a registry readout claimed — context, not intake — and
    /// sheets that fed the booking-agent registry.
    registry_dump_sheets: usize,
    agent_sheets: usize,
    /// Agent rows the seed upsert filed into `booking_agents` itself —
    /// new, refreshed, and failed-to-write.
    agents_imported: u64,
    agents_refreshed: u64,
    agents_failed: u64,
    /// Beacon-registry rows: newly inserted, refreshed in place, refused
    /// by the parser, imported global because the city did not resolve,
    /// and failed-to-write.
    beacons_imported: u64,
    beacons_refreshed: u64,
    beacon_refusals: usize,
    beacons_unresolved_city: u64,
    beacons_failed: u64,
}

impl GDriveContactsSyncWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: Uuid,
        response_encryption_key: SensitiveResponseKey,
    ) -> Result<Self, GDriveContactsSyncError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(GDriveContactsSyncError::ClientBuild)?;
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

    /// Main loop: initial sweep, then NOTIFY wakes or the hourly interval.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), GDriveContactsSyncError> {
        tracing::info!("gdrive contacts sync worker started");

        let mut listener = PgListener::connect_with(self.repo.pool())
            .await
            .map_err(GDriveContactsSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(GDriveContactsSyncError::Database)?;
        listener
            .listen("gdrive_contacts")
            .await
            .map_err(GDriveContactsSyncError::Database)?;

        self.sync_cycle().await;

        let mut tick = interval(SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("gdrive contacts sync worker shutting down");
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

    /// One cycle: every connected gdrive connection scans its Drive.
    /// Per-connection failures are recorded on the connection row, not
    /// propagated — one bad grant must not starve another account.
    async fn sync_cycle(&self) {
        let connections = match self.repo.due_connections(self.workspace_id, "gdrive").await {
            Ok(c) => c,
            Err(error) => {
                tracing::warn!(%error, "gdrive contacts: connection list failed");
                return;
            }
        };
        for (connection_id, account_ref, scan_scope) in connections {
            if let Err(error) = self
                .sync_connection(connection_id, &account_ref, scan_scope.as_ref())
                .await
            {
                tracing::warn!(%error, connection_id = %connection_id, "gdrive contacts sync failed");
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
                    tracing::warn!(%e, "gdrive contacts: sync status write failed");
                }
            }
        }
    }

    async fn sync_connection(
        &self,
        connection_id: Uuid,
        account_ref: &str,
        scan_scope: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        // 1A.6: the tenant's chosen boundary. NULL is "never chose" — nothing
        // scans until they do, and the message is why, not an error shrug.
        let scope = match ScanScope::stored("gdrive", scan_scope) {
            Ok(Some(scope)) => scope,
            Ok(None) => {
                return Err(
                    "nothing scanned — choose a folder, a shared drive, or the whole account for this connection"
                        .to_string(),
                );
            }
            Err(_) => {
                return Err(
                    "the stored scan scope is not one this connection understands — set it again"
                        .to_string(),
                );
            }
        };
        let access_token = self.access_token(connection_id, account_ref).await?;
        let files = self.list_tabular_files(&access_token, &scope).await?;

        let mut counts = CycleCounts::default();
        for file in files.iter().take(MAX_FILES_PER_CYCLE) {
            counts.files_considered += 1;
            match self.scan_file(connection_id, file).await {
                Ok(file_counts) => {
                    counts.files_scanned += file_counts.files_scanned;
                    counts.files_skipped_unchanged += file_counts.files_skipped_unchanged;
                    counts.contacts_upserted += file_counts.contacts_upserted;
                    counts.agents_resolved += file_counts.agents_resolved;
                    counts.rows_without_email += file_counts.rows_without_email;
                    counts.marked_disappeared += file_counts.marked_disappeared;
                    counts.venues_imported += file_counts.venues_imported;
                    counts.venue_refusals += file_counts.venue_refusals;
                    counts.venues_unknown_city += file_counts.venues_unknown_city;
                    counts.peer_acts_imported += file_counts.peer_acts_imported;
                    counts.peer_act_refusals += file_counts.peer_act_refusals;
                    counts.peer_acts_unresolved_city += file_counts.peer_acts_unresolved_city;
                    counts.peer_acts_deactivated += file_counts.peer_acts_deactivated;
                    counts.peer_acts_skipped_inactive += file_counts.peer_acts_skipped_inactive;
                    counts.peer_acts_failed += file_counts.peer_acts_failed;
                    counts.registry_dump_sheets += file_counts.registry_dump_sheets;
                    counts.agent_sheets += file_counts.agent_sheets;
                    counts.agents_imported += file_counts.agents_imported;
                    counts.agents_refreshed += file_counts.agents_refreshed;
                    counts.agents_failed += file_counts.agents_failed;
                    counts.beacons_imported += file_counts.beacons_imported;
                    counts.beacons_refreshed += file_counts.beacons_refreshed;
                    counts.beacon_refusals += file_counts.beacon_refusals;
                    counts.beacons_unresolved_city += file_counts.beacons_unresolved_city;
                    counts.beacons_failed += file_counts.beacons_failed;
                }
                Err(error) => {
                    counts.files_failed += 1;
                    tracing::warn!(%error, file = %file.name, "gdrive file scan failed");
                }
            }
        }

        self.repo
            .mark_sync_ok(self.workspace_id, connection_id)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            files_considered = counts.files_considered,
            files_scanned = counts.files_scanned,
            files_skipped_unchanged = counts.files_skipped_unchanged,
            files_failed = counts.files_failed,
            contacts_upserted = counts.contacts_upserted,
            agents_resolved = counts.agents_resolved,
            rows_without_email = counts.rows_without_email,
            marked_disappeared = counts.marked_disappeared,
            venues_imported = counts.venues_imported,
            venue_refusals = counts.venue_refusals,
            venues_unknown_city = counts.venues_unknown_city,
            peer_acts_imported = counts.peer_acts_imported,
            peer_act_refusals = counts.peer_act_refusals,
            peer_acts_unresolved_city = counts.peer_acts_unresolved_city,
            peer_acts_deactivated = counts.peer_acts_deactivated,
            peer_acts_skipped_inactive = counts.peer_acts_skipped_inactive,
            peer_acts_failed = counts.peer_acts_failed,
            registry_dump_sheets = counts.registry_dump_sheets,
            agent_sheets = counts.agent_sheets,
            agents_imported = counts.agents_imported,
            agents_refreshed = counts.agents_refreshed,
            agents_failed = counts.agents_failed,
            beacons_imported = counts.beacons_imported,
            beacons_refreshed = counts.beacons_refreshed,
            beacon_refusals = counts.beacon_refusals,
            beacons_unresolved_city = counts.beacons_unresolved_city,
            beacons_failed = counts.beacons_failed,
            "gdrive contacts sync cycle complete"
        );
        Ok(())
    }

    /// A valid access token for the connection — shared Google helper
    /// (decrypt, refresh through Google's token endpoint, re-encrypt).
    async fn access_token(&self, connection_id: Uuid, account_ref: &str) -> Result<String, String> {
        resolve_google_access_token(
            &self.repo,
            &self.http_client,
            &self.response_encryption_key,
            self.workspace_id,
            connection_id,
            account_ref,
            "gdrive",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await
    }

    /// The folder ids the scope names plus every subfolder beneath them —
    /// Drive's `in parents` matches direct children only, so a scoped folder
    /// means walking the tree. Bounded like everything else: past the caps
    /// the tree is simply truncated, not unbounded.
    async fn expand_folder_tree(
        &self,
        access_token: &str,
        roots: &[String],
    ) -> Result<Vec<String>, String> {
        let mut all: Vec<String> = roots.to_vec();
        let mut frontier: Vec<String> = roots.to_vec();
        // Depth and breadth both capped — a pathological tree stops at a
        // finite read, not an infinite loop.
        for _ in 0..MAX_FOLDER_DEPTH {
            if frontier.is_empty() || all.len() >= MAX_FOLDER_IDS {
                break;
            }
            let parents = frontier
                .iter()
                .map(|id| format!("'{id}' in parents"))
                .collect::<Vec<_>>()
                .join(" or ");
            let mut children: Vec<String> = Vec::new();
            // A level can hold more subfolders than one page returns —
            // page until the level is exhausted or the id cap is.
            let mut page_token: Option<String> = None;
            loop {
                let mut params: Vec<(&str, String)> = vec![
                    (
                        "q",
                        format!("trashed = false and mimeType = '{FOLDER_MIME}' and ({parents})"),
                    ),
                    ("fields", "nextPageToken,files(id)".to_string()),
                    ("pageSize", "100".to_string()),
                    ("includeItemsFromAllDrives", "true".to_string()),
                    ("supportsAllDrives", "true".to_string()),
                    ("spaces", "drive".to_string()),
                ];
                if let Some(token) = &page_token {
                    params.push(("pageToken", token.clone()));
                }
                let response = self
                    .http_client
                    .get("https://www.googleapis.com/drive/v3/files")
                    .bearer_auth(access_token)
                    .query(&params)
                    .send()
                    .await
                    .map_err(|e| format!("folder list request failed: {e}"))?;
                if !response.status().is_success() {
                    return Err(format!(
                        "folder list failed status={}",
                        response.status().as_u16()
                    ));
                }
                let page: DriveFileList = response
                    .json()
                    .await
                    .map_err(|e| format!("folder list parse failed: {e}"))?;
                children.extend(
                    page.files
                        .into_iter()
                        .map(|f| f.id)
                        .filter(|id| !all.contains(id)),
                );
                match page.next_page_token {
                    Some(token) if all.len() + children.len() < MAX_FOLDER_IDS => {
                        page_token = Some(token)
                    }
                    _ => break,
                }
            }
            if children.is_empty() {
                break;
            }
            all.extend(children.iter().cloned());
            frontier = children;
        }
        all.truncate(MAX_FOLDER_IDS);
        Ok(all)
    }

    /// Tabular files inside the tenant's chosen scope, paginated and
    /// bounded. `WholeAccount` is the historical query; `Folders` expands
    /// subfolders before listing (Drive has no recursive `in parents`);
    /// `SharedDrive` pins the corpus to one drive; `Since` is a date floor.
    async fn list_tabular_files(
        &self,
        access_token: &str,
        scope: &ScanScope,
    ) -> Result<Vec<DriveFile>, String> {
        let tabular = TABULAR_MIMES
            .iter()
            .map(|m| format!("mimeType = '{m}'"))
            .collect::<Vec<_>>()
            .join(" or ");
        let mut query = format!("trashed = false and ({tabular})");
        // Folder scope names the folders the tenant picked; the tabular
        // files are whatever sits inside them, recursively.
        let folder_ids: Vec<String> = match scope {
            ScanScope::Folders { folder_ids } => {
                self.expand_folder_tree(access_token, folder_ids).await?
            }
            _ => Vec::new(),
        };
        if !folder_ids.is_empty() {
            let parents = folder_ids
                .iter()
                .map(|id| format!("'{id}' in parents"))
                .collect::<Vec<_>>()
                .join(" or ");
            query = format!("{query} and ({parents})");
        }
        if let ScanScope::Since { since } = scope {
            query = format!("{query} and modifiedTime >= '{}T00:00:00Z'", since);
        }
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut params: Vec<(&str, String)> = vec![
                ("q", query.clone()),
                (
                    "fields",
                    "nextPageToken,files(id,name,mimeType,modifiedTime)".to_string(),
                ),
                ("pageSize", DRIVE_PAGE_SIZE.to_string()),
                // Shared drives and files shared *to* this account are in
                // scope — the tenant's contact sheets may live on someone
                // else's Drive.
                ("includeItemsFromAllDrives", "true".to_string()),
                ("supportsAllDrives", "true".to_string()),
                ("spaces", "drive".to_string()),
            ];
            if let ScanScope::SharedDrive { drive_id } = scope {
                params.push(("corpora", "drive".to_string()));
                params.push(("driveId", drive_id.clone()));
            }
            if let Some(token) = &page_token {
                params.push(("pageToken", token.clone()));
            }
            let response = self
                .http_client
                .get("https://www.googleapis.com/drive/v3/files")
                .bearer_auth(access_token)
                .query(&params)
                .send()
                .await
                .map_err(|e| format!("files.list request failed: {e}"))?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                return Err(format!("files.list failed status={status}"));
            }
            let page: DriveFileList = response
                .json()
                .await
                .map_err(|e| format!("files.list parse failed: {e}"))?;
            files.extend(page.files);
            match page.next_page_token {
                Some(token) if files.len() < MAX_FILES_PER_CYCLE => {
                    page_token = Some(token);
                }
                _ => break,
            }
        }
        files.truncate(MAX_FILES_PER_CYCLE);
        Ok(files)
    }

    /// One file: skip when unchanged, else fetch as a grid, extract, upsert.
    async fn scan_file(
        &self,
        connection_id: Uuid,
        file: &DriveFile,
    ) -> Result<CycleCounts, String> {
        let mut counts = CycleCounts::default();
        let mtime = file.modified_time.clone().unwrap_or_default();

        // The skip check keys on "this parser already read this exact file
        // version" — the marker carries the scanner revision so a change in
        // what a scan *does* (a new seed reader, multi-sheet xlsx) re-reads
        // every file once instead of leaving a file the old parser
        // misfiled — say, the 716-band workbook the single-sheet reader
        // counted as a 17-row contact sheet — skipped-unchanged forever.
        let state_marker = format!("{mtime}#{SHEET_INTAKE_REVISION}");
        if !mtime.is_empty()
            && self
                .repo
                .file_mtime(self.workspace_id, &file.id)
                .await
                .map_err(|e| e.to_string())?
                .as_deref()
                == Some(state_marker.as_str())
        {
            counts.files_skipped_unchanged = 1;
            return Ok(counts);
        }

        let sheets = self.fetch_sheets(connection_id, file).await?;
        // Workbook reality: a .xlsx can be a dashboard over a data tab —
        // the deep-scan feed keeps its 716-band table on "Master" behind a
        // "Summary" cover sheet. Seed readers therefore inspect EVERY sheet;
        // only sheets none claims reach the contact reader, which judges
        // each sheet on its own header. The dispatch — agent sheet, registry
        // dump, venue, band, contacts — is shared with the GitHub registry
        // mirror so both transports route a sheet the same way.
        let harvest = crate::sheet_intake::harvest_grids(
            self.repo.pool(),
            self.workspace_id,
            &file.name,
            sheets,
        )
        .await?;
        let rows_read = harvest.rows_read;
        counts.rows_without_email += harvest.rows_without_email;
        counts.venues_imported += harvest.venues_imported;
        counts.venue_refusals += harvest.venue_refusals;
        counts.venues_unknown_city += harvest.venues_unknown_city;
        counts.peer_acts_imported += harvest.peer_acts_imported;
        counts.peer_act_refusals += harvest.peer_act_refusals;
        counts.peer_acts_unresolved_city += harvest.peer_acts_unresolved_city;
        counts.peer_acts_deactivated += harvest.peer_acts_deactivated;
        counts.peer_acts_skipped_inactive += harvest.peer_acts_skipped_inactive;
        counts.peer_acts_failed += harvest.peer_acts_failed;
        counts.registry_dump_sheets += harvest.registry_dump_sheets;
        counts.agent_sheets += harvest.agent_sheets;
        counts.agents_imported += harvest.agents_imported;
        counts.agents_refreshed += harvest.agents_refreshed;
        counts.agents_failed += harvest.agents_failed;
        counts.beacons_imported += harvest.beacons_imported;
        counts.beacons_refreshed += harvest.beacons_refreshed;
        counts.beacon_refusals += harvest.beacon_refusals;
        counts.beacons_unresolved_city += harvest.beacons_unresolved_city;
        counts.beacons_failed += harvest.beacons_failed;
        let contacts = harvest.contacts;

        if !harvest.saw_email_column {
            // Not a contact list — record the mtime so we do not re-export
            // it every hour, and never count it as a failure. This also
            // fires when every sheet was a seed sheet: if the file *used
            // to* yield contacts (edited into a seed sheet, or its email
            // column removed), an empty upsert marks those staged rows
            // disappeared — the operator decides what that means.
            self.repo
                .upsert_contacts_for_source(
                    self.workspace_id,
                    "gdrive",
                    &file.id,
                    &file.name,
                    &[],
                    true,
                )
                .await
                .map_err(|e: GDriveError| e.to_string())?;
            self.repo
                .record_file_state(
                    self.workspace_id,
                    &file.id,
                    &file.name,
                    &file.mime_type,
                    &state_marker,
                    true,
                    rows_read as i32,
                    0,
                )
                .await
                .map_err(|e| e.to_string())?;
            counts.files_scanned = 1;
            return Ok(counts);
        }
        let summary = self
            .repo
            .upsert_contacts_for_source(
                self.workspace_id,
                "gdrive",
                &file.id,
                &file.name,
                &contacts,
                true,
            )
            .await
            .map_err(|e: GDriveError| e.to_string())?;
        self.repo
            .record_file_state(
                self.workspace_id,
                &file.id,
                &file.name,
                &file.mime_type,
                &state_marker,
                false,
                rows_read as i32,
                contacts.len() as i32,
            )
            .await
            .map_err(|e| e.to_string())?;
        counts.files_scanned = 1;
        counts.contacts_upserted = summary.upserted;
        counts.agents_resolved = summary.agents_resolved;
        counts.marked_disappeared = summary.marked_disappeared;
        Ok(counts)
    }

    /// Turns one file into one grid per worksheet. Sheets export as CSV —
    /// which carries only the first tab, a known limit noted below — real
    /// workbooks download and yield every non-empty sheet so a data tab
    /// behind a cover/dashboard sheet is not invisible; csv/tsv read raw
    /// as a single grid.
    async fn fetch_sheets(
        &self,
        connection_id: Uuid,
        file: &DriveFile,
    ) -> Result<Vec<Vec<Vec<String>>>, String> {
        let single = |grid: Vec<Vec<String>>| vec![grid];
        match file.mime_type.as_str() {
            "application/vnd.google-apps.spreadsheet" => {
                // The CSV export answers the first worksheet only — a
                // multi-tab Google workbook would drop every tab but one.
                // The xlsx export carries all of them through the same
                // `parse_xlsx_sheets` a native .xlsx upload takes.
                let bytes = self
                    .download_bytes(
                        connection_id,
                        &format!(
                            "https://www.googleapis.com/drive/v3/files/{}/export?mimeType=application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                            file.id
                        ),
                    )
                    .await?;
                crate::sheet_intake::parse_xlsx_sheets(&bytes)
            }
            "text/csv" => {
                let text = self.download_media(connection_id, &file.id).await?;
                parse_delimited(text.as_bytes(), b',').map(single)
            }
            "text/tab-separated-values" => {
                let text = self.download_media(connection_id, &file.id).await?;
                parse_delimited(text.as_bytes(), b'\t').map(single)
            }
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
                let bytes = self.download_media_bytes(connection_id, &file.id).await?;
                crate::sheet_intake::parse_xlsx_sheets(&bytes)
            }
            other => Err(format!("unsupported mime {other}")),
        }
    }

    /// The token refresh inside `access_token` may run once per call site;
    /// fetching re-reads it each time so a rotated token is never stale.
    async fn download(&self, connection_id: Uuid, url: &str) -> Result<String, String> {
        let token = self.access_token_for_connection(connection_id).await?;
        let response = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "download failed status={}",
                response.status().as_u16()
            ));
        }
        response
            .text()
            .await
            .map_err(|e| format!("download body read failed: {e}"))
    }

    async fn download_media(&self, connection_id: Uuid, file_id: &str) -> Result<String, String> {
        self.download(
            connection_id,
            &format!("https://www.googleapis.com/drive/v3/files/{file_id}?alt=media"),
        )
        .await
    }

    /// `download` for binary responses — a native Sheets export is a zip,
    /// not text, so `.text()` would corrupt it.
    async fn download_bytes(&self, connection_id: Uuid, url: &str) -> Result<Vec<u8>, String> {
        let token = self.access_token_for_connection(connection_id).await?;
        let response = self
            .http_client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "download failed status={}",
                response.status().as_u16()
            ));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("download body read failed: {e}"))
    }

    async fn download_media_bytes(
        &self,
        connection_id: Uuid,
        file_id: &str,
    ) -> Result<Vec<u8>, String> {
        let token = self.access_token_for_connection(connection_id).await?;
        let response = self
            .http_client
            .get(format!(
                "https://www.googleapis.com/drive/v3/files/{file_id}?alt=media"
            ))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "download failed status={}",
                response.status().as_u16()
            ));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("download body read failed: {e}"))
    }

    /// Point-of-use token resolution for downloads — delegates to the same
    /// decrypt/refresh path keyed by the connection's account ref.
    async fn access_token_for_connection(&self, connection_id: Uuid) -> Result<String, String> {
        access_token_for_connection(
            &self.repo,
            &self.http_client,
            &self.response_encryption_key,
            self.workspace_id,
            connection_id,
            "gdrive",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await
    }
}
