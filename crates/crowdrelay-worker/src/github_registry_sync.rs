//! GitHub registry sync: the shared registry workbook lives in a repo the
//! operator edits directly — `database.xlsx` with a tab per registry — and
//! this worker polls the repo's contents API for tabular files, then feeds
//! every sheet through the same intake the Drive scan uses.
//!
//! Where the Drive connector is connection-scoped OAuth, this source is
//! deployment-configured: `CROWDRELAY_GITHUB_REGISTRY_REPO` names
//! `owner/repo`, `…_PATH` a directory inside it (default: repo root),
//! `…_REF` the branch (default `main`), and `…_TOKEN` an optional PAT —
//! required only while the repo is private. With no repo configured the
//! worker is never built, like the other key-gated sweeps.
//!
//! Change detection rides the contents API's blob `sha` — a recommitted
//! file with the same bytes costs one listing, not a download. Files state
//! keys carry the `gh:` prefix so a Drive file and a repo path can never
//! collide in `drive_files`. A re-listed file marks its vanished rows
//! disappeared exactly like a Drive file does.

use std::time::Duration;

use crowdrelay_domain::drive_contacts::parse_delimited;
use crowdrelay_infra::gdrive::{GDriveError, PostgresGDriveRepository};
use sqlx::PgPool;
use thiserror::Error;
use time::{OffsetDateTime, PrimitiveDateTime, Time};
use time_tz::{Offset, OffsetResult, PrimitiveDateTimeExt, TimeZone, timezones};
use tokio::{sync::watch, time::sleep};
use uuid::Uuid;

use crate::sheet_intake::{SHEET_INTAKE_REVISION, harvest_grids, parse_xlsx_sheets};

/// The workbook is refreshed on the operator's schedule — a research pass
/// lands each morning — so one sync at 12:00 Europe/Warsaw (safely after
/// the 08:00–09:00 update window) is the whole cadence. Boot still runs an
/// immediate catch-up so a worker down at noon does not miss the day, and
/// a `github_registry` NOTIFY forces an early sync — no endpoint emits one
/// today; it is a manual (`psql NOTIFY`) wake channel only.
const SYNC_ZONE: &str = "Europe/Warsaw";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "CrowdRelay/1.0 (github registry sync)";
/// A registry workbook is megabytes, never gigabytes — the cap keeps a
/// stray artifact commit from wedging a cycle.
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// drive_files.file_id is capped at 200 — the `gh:` key truncates to a
/// prefix plus a hash of the full path, so two long paths that share a
/// prefix can never collide on one row's skip marker.
const FILE_ID_CAP: usize = 200;

#[derive(Debug, Error)]
pub enum GithubRegistrySyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct GithubRegistrySyncWorker {
    repo: PostgresGDriveRepository,
    http_client: reqwest::Client,
    workspace_id: Uuid,
    gh_repo: String,
    gh_path: String,
    gh_ref: String,
    token: Option<String>,
}

/// One entry from `GET /repos/{repo}/contents/{dir}` — the fields the scan
/// needs: name for logs, path for the raw fetch, sha for change detection,
/// size for the cap, type for the file filter.
#[derive(Debug, serde::Deserialize)]
struct ContentEntry {
    name: String,
    path: String,
    sha: String,
    #[serde(default)]
    size: u64,
    #[serde(rename = "type")]
    entry_type: String,
}

#[derive(Debug, Default)]
struct CycleCounts {
    files_considered: usize,
    files_scanned: usize,
    files_skipped_unchanged: usize,
    files_failed: usize,
    contacts_upserted: u64,
    agents_resolved: u64,
    rows_without_email: usize,
    marked_disappeared: u64,
    venues_imported: u64,
    venue_refusals: usize,
    venues_unknown_city: u64,
    venues_failed: u64,
    peer_acts_imported: u64,
    peer_act_refusals: usize,
    peer_acts_unresolved_city: u64,
    peer_acts_deactivated: u64,
    peer_acts_skipped_inactive: u64,
    peer_acts_failed: u64,
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
    /// Outreach-log rows: sends and replies written, rows already known,
    /// drafts skipped, contacts no target carries, and writes that failed.
    outreach_sends_recorded: u64,
    outreach_replies_recorded: u64,
    outreach_unmatched: u64,
    outreach_failed: u64,
    /// Scout `OPPORTUNITIES` sheets claimed and their `team_opportunities`
    /// writes — new, refreshed, refused by the parser, failed to write.
    opportunity_sheets: usize,
    opportunities_seeded: u64,
    opportunities_refreshed: u64,
    opportunity_refusals: usize,
    opportunities_failed: u64,
    /// Sheets carrying the human scout's `META_TARGETS` tab; their rows
    /// land in the beacon counters above.
    scout_meta_sheets: usize,
}

impl GithubRegistrySyncWorker {
    /// Builds the worker only when a repo is configured — an unset
    /// `CROWDRELAY_GITHUB_REGISTRY_REPO` means the deployment keeps its
    /// registry in Drive and this source stays dark.
    pub fn maybe_new(
        pool: PgPool,
        workspace_id: Uuid,
    ) -> Result<Option<Self>, GithubRegistrySyncError> {
        let gh_repo = std::env::var("CROWDRELAY_GITHUB_REGISTRY_REPO")
            .ok()
            .map(|v| v.trim().trim_matches('/').to_owned())
            .filter(|v| !v.is_empty());
        let Some(gh_repo) = gh_repo else {
            return Ok(None);
        };
        let gh_path = std::env::var("CROWDRELAY_GITHUB_REGISTRY_PATH")
            .ok()
            .map(|v| v.trim().trim_matches('/').to_owned())
            .unwrap_or_default();
        let gh_ref = std::env::var("CROWDRELAY_GITHUB_REGISTRY_REF")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "main".to_owned());
        let token = std::env::var("CROWDRELAY_GITHUB_REGISTRY_TOKEN")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(GithubRegistrySyncError::ClientBuild)?;
        Ok(Some(Self {
            repo: PostgresGDriveRepository::new(pool),
            http_client,
            workspace_id,
            gh_repo,
            gh_path,
            gh_ref,
            token,
        }))
    }

    /// Build-from-env and spawn as one step — keeps the wiring a single
    /// line in `main.rs`, which is at its size ratchet. Returns `Ok(false)`
    /// when no registry repo is configured and the source stays dark.
    pub fn spawn_if_configured(
        tasks: &mut tokio::task::JoinSet<&'static str>,
        pool: PgPool,
        workspace_id: Uuid,
        shutdown: watch::Receiver<bool>,
    ) -> Result<bool, GithubRegistrySyncError> {
        let Some(worker) = Self::maybe_new(pool, workspace_id)? else {
            tracing::info!(
                "github registry sync disabled; set CROWDRELAY_GITHUB_REGISTRY_REPO=owner/repo \
                 (plus CROWDRELAY_GITHUB_REGISTRY_TOKEN for a private repo) to mirror it"
            );
            return Ok(false);
        };
        tasks.spawn(async move {
            let _ = worker.run(shutdown).await;
            "github registry sync"
        });
        Ok(true)
    }

    /// Main loop: catch-up sweep at boot, then `github_registry` NOTIFY
    /// wakes or the daily 12:00 Warsaw tick.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), GithubRegistrySyncError> {
        tracing::info!(
            repo = %self.gh_repo,
            path = %self.gh_path,
            gh_ref = %self.gh_ref,
            "github registry sync worker started"
        );

        let mut listener = sqlx::postgres::PgListener::connect_with(self.repo.pool())
            .await
            .map_err(GithubRegistrySyncError::Database)?;
        listener
            .listen("github_registry")
            .await
            .map_err(GithubRegistrySyncError::Database)?;

        self.sync_cycle().await;

        loop {
            let wait = duration_until_next_sync(OffsetDateTime::now_utc());
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("github registry sync worker shutting down");
                        return Ok(());
                    }
                }
                notice = listener.recv() => {
                    // An Err arm is a lost listener reconnecting, not a
                    // wake — syncing on it would call the rate-limited
                    // contents API once per reconnect attempt.
                    if notice.is_ok() {
                        self.sync_cycle().await;
                    } else {
                        sleep(Duration::from_secs(5)).await;
                    }
                }
                _ = sleep(wait) => {
                    self.sync_cycle().await;
                }
            }
        }
    }

    async fn sync_cycle(&self) {
        let entries = match self.list_tabular_files().await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, repo = %self.gh_repo, "github registry: listing failed");
                return;
            }
        };

        let mut counts = CycleCounts::default();
        for entry in &entries {
            counts.files_considered += 1;
            match self.scan_file(entry).await {
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
                    counts.venues_failed += file_counts.venues_failed;
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
                    counts.outreach_sends_recorded += file_counts.outreach_sends_recorded;
                    counts.outreach_replies_recorded += file_counts.outreach_replies_recorded;
                    counts.outreach_unmatched += file_counts.outreach_unmatched;
                    counts.outreach_failed += file_counts.outreach_failed;
                    counts.opportunity_sheets += file_counts.opportunity_sheets;
                    counts.opportunities_seeded += file_counts.opportunities_seeded;
                    counts.opportunities_refreshed += file_counts.opportunities_refreshed;
                    counts.opportunity_refusals += file_counts.opportunity_refusals;
                    counts.opportunities_failed += file_counts.opportunities_failed;
                    counts.scout_meta_sheets += file_counts.scout_meta_sheets;
                }
                Err(error) => {
                    counts.files_failed += 1;
                    tracing::warn!(%error, file = %entry.path, "github registry: file scan failed");
                }
            }
        }
        tracing::info!(
            considered = counts.files_considered,
            scanned = counts.files_scanned,
            unchanged = counts.files_skipped_unchanged,
            failed = counts.files_failed,
            contacts = counts.contacts_upserted,
            agents_resolved = counts.agents_resolved,
            disappeared = counts.marked_disappeared,
            venues = counts.venues_imported,
            venue_refusals = counts.venue_refusals,
            venues_unknown_city = counts.venues_unknown_city,
            venues_failed = counts.venues_failed,
            peer_acts = counts.peer_acts_imported,
            peer_act_refusals = counts.peer_act_refusals,
            peer_acts_unresolved_city = counts.peer_acts_unresolved_city,
            peer_acts_deactivated = counts.peer_acts_deactivated,
            peer_acts_skipped_inactive = counts.peer_acts_skipped_inactive,
            peer_acts_failed = counts.peer_acts_failed,
            rows_without_email = counts.rows_without_email,
            registry_dumps = counts.registry_dump_sheets,
            agent_sheets = counts.agent_sheets,
            agents_imported = counts.agents_imported,
            agents_refreshed = counts.agents_refreshed,
            agents_failed = counts.agents_failed,
            beacons = counts.beacons_imported,
            beacons_refreshed = counts.beacons_refreshed,
            beacon_refusals = counts.beacon_refusals,
            beacons_unresolved_city = counts.beacons_unresolved_city,
            beacons_failed = counts.beacons_failed,
            outreach_sends = counts.outreach_sends_recorded,
            outreach_replies = counts.outreach_replies_recorded,
            outreach_unmatched = counts.outreach_unmatched,
            outreach_failed = counts.outreach_failed,
            opportunity_sheets = counts.opportunity_sheets,
            opportunities_seeded = counts.opportunities_seeded,
            opportunities_refreshed = counts.opportunities_refreshed,
            opportunity_refusals = counts.opportunity_refusals,
            opportunities_failed = counts.opportunities_failed,
            scout_meta_sheets = counts.scout_meta_sheets,
            "github registry sync cycle"
        );
    }

    /// Tabular files in the configured directory: `.xlsx`, `.csv`, `.tsv`
    /// within the size cap. The listing endpoint answers a JSON array for a
    /// directory and a single object for a file — a configured path that
    /// names the workbook directly still scans. Two deliberate limits,
    /// shared with the Drive transport: the directory listing is
    /// uncapped-paged server-side (entries past 1000 are never seen), and a
    /// file deleted from the repo leaves its staged rows untouched —
    /// disappearance is computed only for rows missing from a still-listed
    /// file, which is the workbook workflow this source mirrors.
    async fn list_tabular_files(&self) -> Result<Vec<ContentEntry>, String> {
        let url = contents_url(&self.gh_repo, &self.gh_path, &self.gh_ref)?;
        let mut request = self
            .http_client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("contents list failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "contents list status={} — repo configured as {}, token {}",
                response.status().as_u16(),
                self.gh_repo,
                if self.token.is_some() {
                    "set"
                } else {
                    "unset (public repo required)"
                }
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("contents body read failed: {e}"))?;
        // A directory lists an array; a file path answers one object.
        let entries: Vec<ContentEntry> = match serde_json::from_str::<Vec<ContentEntry>>(&body) {
            Ok(list) => list,
            Err(_) => serde_json::from_str::<ContentEntry>(&body)
                .map(|one| vec![one])
                .map_err(|e| format!("contents list parse failed: {e}"))?,
        };
        Ok(entries
            .into_iter()
            .filter(|e| e.entry_type == "file")
            .filter(|e| e.size <= MAX_FILE_BYTES)
            .filter(|e| {
                let lower = e.name.to_ascii_lowercase();
                lower.ends_with(".xlsx") || lower.ends_with(".csv") || lower.ends_with(".tsv")
            })
            .collect())
    }

    /// One file: skip when the blob sha already scanned, else download and
    /// run the shared intake.
    async fn scan_file(&self, entry: &ContentEntry) -> Result<CycleCounts, String> {
        let mut counts = CycleCounts::default();
        let file_id = state_file_id(&self.gh_repo, &entry.path);
        let state_marker = format!("{}#{SHEET_INTAKE_REVISION}", entry.sha);
        if self
            .repo
            .file_mtime(self.workspace_id, &file_id)
            .await
            .map_err(|e| e.to_string())?
            .as_deref()
            == Some(state_marker.as_str())
        {
            counts.files_skipped_unchanged = 1;
            return Ok(counts);
        }

        let bytes = self.download_file(&entry.path).await?;
        let lower = entry.name.to_ascii_lowercase();
        let single =
            |grid: Vec<Vec<String>>| vec![crate::sheet_intake::SheetGrid { name: None, grid }];
        let sheets = if lower.ends_with(".xlsx") {
            parse_xlsx_sheets(&bytes)?
        } else if lower.ends_with(".tsv") {
            parse_delimited(&bytes, b'\t').map(single)?
        } else {
            parse_delimited(&bytes, b',').map(single)?
        };

        let harvest = harvest_grids(
            self.repo.pool(),
            self.workspace_id,
            &entry.name,
            sheets,
            // The GitHub mirror is the operator's own repo — registry
            // claims apply the same as the Drive folder's.
            crate::sheet_intake::SheetTrust::RegistryTrusted,
        )
        .await?;
        counts.rows_without_email = harvest.rows_without_email;
        counts.venues_imported = harvest.venues_imported;
        counts.venue_refusals = harvest.venue_refusals;
        counts.venues_unknown_city = harvest.venues_unknown_city;
        counts.venues_failed = harvest.venues_failed;
        counts.peer_acts_imported = harvest.peer_acts_imported;
        counts.peer_act_refusals = harvest.peer_act_refusals;
        counts.peer_acts_unresolved_city = harvest.peer_acts_unresolved_city;
        counts.peer_acts_deactivated = harvest.peer_acts_deactivated;
        counts.peer_acts_skipped_inactive = harvest.peer_acts_skipped_inactive;
        counts.peer_acts_failed = harvest.peer_acts_failed;
        counts.registry_dump_sheets = harvest.registry_dump_sheets;
        counts.agent_sheets = harvest.agent_sheets;
        counts.agents_imported = harvest.agents_imported;
        counts.agents_refreshed = harvest.agents_refreshed;
        counts.agents_failed = harvest.agents_failed;
        counts.beacons_imported = harvest.beacons_imported;
        counts.beacons_refreshed = harvest.beacons_refreshed;
        counts.beacon_refusals = harvest.beacon_refusals;
        counts.beacons_unresolved_city = harvest.beacons_unresolved_city;
        counts.beacons_failed = harvest.beacons_failed;
        counts.outreach_sends_recorded = harvest.outreach_sends_recorded;
        counts.outreach_replies_recorded = harvest.outreach_replies_recorded;
        counts.outreach_unmatched = harvest.outreach_unmatched;
        counts.outreach_failed = harvest.outreach_failed;
        counts.opportunity_sheets = harvest.opportunity_sheets;
        counts.opportunities_seeded = harvest.opportunities_seeded;
        counts.opportunities_refreshed = harvest.opportunities_refreshed;
        counts.opportunity_refusals = harvest.opportunity_refusals;
        counts.opportunities_failed = harvest.opportunities_failed;
        counts.scout_meta_sheets = harvest.scout_meta_sheets;
        let contacts = harvest.contacts;
        let saw_email_column = harvest.saw_email_column;
        let rows_read = harvest.rows_read;
        // Same rule as the Drive transport: a row-level write failure must
        // not be sealed under the unchanged marker or a transient error is
        // never retried. Validation refusals do not block the marker.
        let row_failures = harvest.venues_failed
            + harvest.peer_acts_failed
            + harvest.agents_failed
            + harvest.beacons_failed
            + harvest.outreach_failed
            + harvest.opportunities_failed;

        // A re-listed file is the truth about its rows: contacts it no
        // longer carries mark disappeared, exactly like a Drive file.
        let summary = self
            .repo
            .upsert_contacts_for_source(
                self.workspace_id,
                "github",
                &file_id,
                &entry.name,
                &contacts,
                true,
                true,
            )
            .await
            .map_err(|e: GDriveError| e.to_string())?;
        if row_failures == 0 {
            self.repo
                .record_file_state(
                    self.workspace_id,
                    &file_id,
                    &entry.name,
                    "github.com/repository",
                    &state_marker,
                    !saw_email_column,
                    rows_read as i32,
                    contacts.len() as i32,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        counts.files_scanned = 1;
        counts.contacts_upserted = summary.upserted;
        counts.agents_resolved = summary.agents_resolved;
        counts.marked_disappeared = summary.marked_disappeared;
        Ok(counts)
    }

    /// The raw file body via the contents API — `Accept: …raw` answers the
    /// blob directly for public and private repos alike, so no separate
    /// raw host or signed URL handling is needed.
    async fn download_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let url = contents_url(&self.gh_repo, path, &self.gh_ref)?;
        let mut request = self
            .http_client
            .get(&url)
            .header("Accept", "application/vnd.github.raw")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "download failed status={} path={}",
                response.status().as_u16(),
                path
            ));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("download body read failed: {e}"))
    }
}

/// One contents-API URL, built with `Url` so a repo, directory or file
/// name carrying `?`, `#` or a space percent-encodes instead of malforming
/// the request.
fn contents_url(repo: &str, path: &str, gh_ref: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse("https://api.github.com/repos")
        .map_err(|e| format!("contents url build failed: {e}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "contents url is not a base".to_string())?;
        for segment in repo.split('/') {
            segments.push(segment);
        }
        segments.push("contents");
        for segment in path.split('/') {
            if !segment.is_empty() {
                segments.push(segment);
            }
        }
    }
    url.query_pairs_mut().append_pair("ref", gh_ref);
    Ok(url.into())
}

/// The `drive_files` state key for one repo path. Keys longer than the
/// 200-char column keep their readable prefix plus an FNV-1a hash of the
/// full path — two files sharing the prefix then collide on neither the
/// skip marker nor the staged-row anchor.
fn state_file_id(repo: &str, path: &str) -> String {
    let full = format!("gh:{repo}/{path}");
    if full.chars().count() <= FILE_ID_CAP {
        return full;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in full.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let keep = FILE_ID_CAP - 18; // 1 '#' + 16 hex + 1 headroom
    let prefix: String = full.chars().take(keep).collect();
    format!("{prefix}#{hash:016x}")
}

/// How long until the next daily sync — the next 12:00 wall clock in
/// Europe/Warsaw, computed through the tz database so the CET/CEST fold is
/// never guessed at. A noon that does not exist in local time (a spring
/// forward can delete it) falls back to the next day's, and a fold that
/// deletes both falls back to 24h.
fn duration_until_next_sync(now: OffsetDateTime) -> Duration {
    let fallback = Duration::from_secs(24 * 60 * 60);
    let Some(tz) = timezones::get_by_name(SYNC_ZONE) else {
        return fallback;
    };
    let local_date = now.to_offset(tz.get_offset_utc(&now).to_utc()).date();
    let noon = match Time::from_hms(12, 0, 0) {
        Ok(t) => t,
        Err(_) => return fallback,
    };
    for day in [local_date, local_date.next_day().unwrap_or(local_date)] {
        let candidate = PrimitiveDateTime::new(day, noon).assume_timezone(tz);
        if let OffsetResult::Some(when) = candidate
            && when > now
        {
            return (when - now).try_into().unwrap_or(fallback);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_daily_tick_is_the_next_warsaw_noon() {
        // Summer (CEST = UTC+2): 12:00 local is 10:00 UTC.
        let wait = duration_until_next_sync(datetime!(2026-09-22 08:00 UTC));
        assert_eq!(wait, Duration::from_secs(2 * 60 * 60));
        // Past noon → tomorrow's noon.
        let wait = duration_until_next_sync(datetime!(2026-09-22 11:00 UTC));
        assert_eq!(wait, Duration::from_secs(23 * 60 * 60));
        // Winter (CET = UTC+1): 12:00 local is 11:00 UTC.
        let wait = duration_until_next_sync(datetime!(2026-01-15 09:00 UTC));
        assert_eq!(wait, Duration::from_secs(2 * 60 * 60));
    }
}
