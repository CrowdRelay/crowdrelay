//! Community Intelligence worker — leadership-aware scheduled sweep.
//!
//! Respects the leadership lease (only runs when the worker holds
//! leadership, same as all other background loops). Per-source scheduling,
//! not "every 6h for all": each source has its own `next_due_at` tracked
//! in memory, with jitter and exponential backoff on errors.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crowdrelay_domain::community_intelligence::{validate_entity, validate_observation};
use crowdrelay_infra::community_intelligence::PostgresCommunityIntelligenceRepository;

use super::adapter::{AdapterError, ParsedEntity, ParsedObservation, SourceAdapter};

/// A minimal place row for the community intelligence worker.
/// Unlike the full `PlaceRow` (which joins rules and outreach), this only
/// needs the fields required to fetch and persist an observation.
#[derive(Debug, sqlx::FromRow)]
struct CommunityPlaceRow {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    platform: String,
    name: String,
    url: String,
}

impl From<&CommunityPlaceRow> for super::adapter::AdapterPlace {
    fn from(row: &CommunityPlaceRow) -> Self {
        super::adapter::AdapterPlace {
            id: row.id,
            workspace_id: row.workspace_id,
            platform: row.platform.clone(),
            name: row.name.clone(),
            url: row.url.clone(),
        }
    }
}

/// In-memory source health state (Sprint A — not persisted to DB).
#[derive(Clone, Debug)]
struct SourceHealth {
    last_success: Option<Instant>,
    last_error: Option<String>,
    consecutive_failures: u32,
    next_due_at: Instant,
}

impl SourceHealth {
    /// Seeds a source's schedule from how long ago it last observed anything.
    ///
    /// This used to be `now + interval` unconditionally, which put the first
    /// sweep of every source a full interval after process start. Reddit is
    /// observed every 12 hours, so a worker restarted more often than that —
    /// which is to say on any day with a deploy — never reached its first
    /// sweep at all, and `community_observations` stayed empty while the
    /// worker reported healthy.
    ///
    /// The clock belongs to the data. `elapsed` is the age of the newest
    /// observation for this source, or `None` if it has never produced one, in
    /// which case it is due now.
    fn new(interval: Duration, elapsed: Option<Duration>) -> Self {
        let next_due_at = match elapsed {
            // Never observed: go now, but stagger slightly so a worker with
            // several adapters does not open every source at once.
            None => Instant::now() + Duration::from_secs(10),
            Some(elapsed) if elapsed >= interval => Instant::now() + Duration::from_secs(10),
            Some(elapsed) => Instant::now() + jitter_interval(interval - elapsed),
        };
        Self {
            last_success: None,
            last_error: None,
            consecutive_failures: 0,
            next_due_at,
        }
    }

    fn record_success(&mut self, interval: Duration) {
        self.last_success = Some(Instant::now());
        self.last_error = None;
        self.consecutive_failures = 0;
        self.next_due_at = Instant::now() + jitter_interval(interval);
    }

    fn record_failure(&mut self, error: String, backoff_max: Duration) {
        self.last_error = Some(error);
        self.consecutive_failures += 1;
        // Exponential backoff: 30s, 60s, 120s, 240s, ... capped at backoff_max.
        let backoff = Duration::from_secs(30)
            .saturating_mul(2u32.saturating_pow(self.consecutive_failures.min(10)))
            .min(backoff_max);
        self.next_due_at = Instant::now() + backoff;
    }
}

/// Adds ±10% jitter to an interval to avoid hammering simultaneously.
fn jitter_interval(interval: Duration) -> Duration {
    let jitter_range = interval / 10;
    let jitter_nanos = jitter_range.as_nanos() as u64;
    if jitter_nanos == 0 {
        return interval;
    }
    // Use getrandom for a single u64 — consistent with the rest of the worker.
    let mut buf = [0u8; 8];
    if getrandom::fill(&mut buf).is_err() {
        return interval;
    }
    // `random` walks the full ±jitter window, so the offset is its distance
    // from the midpoint. Halving it here (`random / 2`) collapsed the window
    // to -10%..+5% and put the largest *positive* jitter at the midpoint
    // instead of the top, which is the opposite of spreading the load.
    let random = u64::from_le_bytes(buf) % (2 * jitter_nanos + 1);
    let offset = random.abs_diff(jitter_nanos);
    if random >= jitter_nanos {
        interval + Duration::from_nanos(offset)
    } else {
        interval.saturating_sub(Duration::from_nanos(offset))
    }
}

/// The community intelligence worker. Runs as a background loop alongside
/// the other worker loops. Leadership is at the process level — only one
/// worker process runs at a time, so all loops run when the process is active.
pub struct CommunityIntelligenceWorker {
    adapters: Vec<Arc<dyn SourceAdapter>>,
    repo: Arc<PostgresCommunityIntelligenceRepository>,
    pool: sqlx::PgPool,
}

impl CommunityIntelligenceWorker {
    pub fn new(
        adapters: Vec<Arc<dyn SourceAdapter>>,
        repo: Arc<PostgresCommunityIntelligenceRepository>,
        pool: sqlx::PgPool,
    ) -> Self {
        Self {
            adapters,
            repo,
            pool,
        }
    }

    /// Runs the worker loop. Returns when the shutdown signal is received.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        info!("community intelligence worker starting");

        // Per-source health state, keyed by adapter id, seeded from when each
        // source last observed anything rather than from process start.
        let last_seen: HashMap<String, Duration> =
            match self.repo.seconds_since_last_observation().await {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|(source, seconds)| {
                        (seconds >= 0.0).then(|| (source, Duration::from_secs_f64(seconds)))
                    })
                    .collect(),
                Err(error) => {
                    // Treat every source as never-observed: due soon rather than
                    // silently postponed by a full interval.
                    warn!(%error, "could not read last observation times; sources will sweep now");
                    HashMap::new()
                }
            };
        let mut health: HashMap<String, SourceHealth> = HashMap::new();
        for adapter in &self.adapters {
            let adapter_id = adapter.id();
            health.insert(
                adapter_id.to_owned(),
                SourceHealth::new(
                    adapter.recommended_interval(),
                    last_seen.get(adapter_id).copied(),
                ),
            );
        }

        let mut tick_interval = tokio::time::interval(Duration::from_secs(30));
        // Skip, not Delay: after a stall the missed ticks are worthless — the
        // sweep only asks "is any source due?", and answering it five times in
        // a row bursts scrapes at the very moment the process is recovering.
        tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    self.run_sweep(&mut health).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("community intelligence worker shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Runs one sweep pass: for each due source, find matching places,
    /// fetch, validate, and persist.
    async fn run_sweep(&self, health: &mut HashMap<String, SourceHealth>) {
        let now = Instant::now();

        for adapter in &self.adapters {
            let adapter_id = adapter.id();
            let source_health = match health.get_mut(adapter_id) {
                Some(h) => h,
                None => continue,
            };

            if now < source_health.next_due_at {
                continue;
            }

            if let Err(e) = self.run_one_source(adapter.as_ref(), source_health).await {
                warn!(adapter = adapter_id, error = %e, "source sweep failed");
            }
        }
    }

    /// Runs one source adapter against all matching places.
    async fn run_one_source(
        &self,
        adapter: &dyn SourceAdapter,
        source_health: &mut SourceHealth,
    ) -> Result<(), String> {
        let adapter_id = adapter.id();
        let backoff_max = adapter.rate_limit_policy().backoff_max;

        // Find matching discovery_places rows by platform field.
        let places = find_places_for_adapter(&self.pool, adapter_id)
            .await
            .map_err(|e| {
                let msg = format!("failed to query places: {e}");
                source_health.record_failure(msg.clone(), backoff_max);
                msg
            })?;

        if places.is_empty() {
            // A source that claims no places is not healthy, it is unused —
            // and silence here is what hid a real gap for months. Production
            // had 28 active Reddit places and only a `brutalland` adapter, so
            // every sweep matched nothing, recorded a success, and wrote
            // nothing. The dashboards stayed green over an empty
            // `community_observations` table.
            warn!(
                adapter = adapter_id,
                "no active discovery_places match this adapter's platform; \
                 the source will observe nothing until a place is added"
            );
            source_health.record_success(adapter.recommended_interval());
            return Ok(());
        }

        let mut success_count = 0u32;
        let mut fail_count = 0u32;

        for place in &places {
            let adapter_place = super::adapter::AdapterPlace::from(place);
            match adapter.fetch(&adapter_place).await {
                Ok(parsed) => {
                    if let Err(e) = self.persist_observation(place, &parsed).await {
                        error!(
                            adapter = adapter_id,
                            place_id = %place.id,
                            error = %e,
                            "failed to persist observation"
                        );
                        fail_count += 1;
                    } else {
                        success_count += 1;
                    }
                }
                Err(AdapterError::StructureChanged) => {
                    error!(
                        adapter = adapter_id,
                        place_id = %place.id,
                        "page structure changed — markers not found, failing closed"
                    );
                    fail_count += 1;
                }
                Err(e) => {
                    warn!(
                        adapter = adapter_id,
                        place_id = %place.id,
                        error = %e,
                        "fetch failed"
                    );
                    fail_count += 1;
                }
            }
        }

        if fail_count > 0 && success_count == 0 {
            source_health.record_failure(
                format!("{fail_count} places failed, 0 succeeded"),
                backoff_max,
            );
        } else {
            source_health.record_success(adapter.recommended_interval());
            info!(
                adapter = adapter_id,
                success = success_count,
                failed = fail_count,
                "source sweep complete"
            );
        }

        Ok(())
    }

    /// Validates and persists a parsed observation.
    async fn persist_observation(
        &self,
        place: &CommunityPlaceRow,
        parsed: &ParsedObservation,
    ) -> Result<(), String> {
        // Build the domain observation from the parsed data.
        let observation = crowdrelay_domain::community_intelligence::CommunityObservation {
            source: parsed.source.clone(),
            source_url: parsed.source_url.clone(),
            collector_version: parsed.collector_version.clone(),
            raw_activity_metrics: parsed.raw_activity_metrics.clone(),
            observation_quality: parsed.observation_quality,
        };

        // Validate the observation via domain policy.
        validate_observation(&observation).map_err(|e| e.to_string())?;

        // Convert parsed entities to domain entities and validate each.
        let entities: Vec<crowdrelay_domain::community_intelligence::CommunityEntity> = parsed
            .entities
            .iter()
            .map(|p| {
                let cloned: ParsedEntity = ParsedEntity {
                    entity_type: p.entity_type,
                    entity_ref: p.entity_ref.clone(),
                    strength: p.strength,
                };
                cloned.into()
            })
            .collect();

        for entity in &entities {
            validate_entity(entity).map_err(|e| e.to_string())?;
        }

        // Persist via the repository.
        self.repo
            .insert_observation(place.workspace_id, place.id, &observation, &entities)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }
}

/// Finds discovery_places rows that match a given adapter by platform field.
async fn find_places_for_adapter(
    pool: &sqlx::PgPool,
    adapter_id: &str,
) -> Result<Vec<CommunityPlaceRow>, sqlx::Error> {
    sqlx::query_as::<_, CommunityPlaceRow>(
        r#"SELECT id, workspace_id, platform, name, url
           FROM discovery_places
           WHERE platform = $1 AND status = 'active'
           ORDER BY name"#,
    )
    .bind(adapter_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_inside_the_ten_percent_window() {
        let interval = Duration::from_secs(6 * 3600);
        let low = interval - interval / 10;
        let high = interval + interval / 10;
        // Sampled rather than exhaustive: the failure mode this guards against
        // (a collapsed or inverted window) shows up within a few hundred draws.
        for _ in 0..1_000 {
            let jittered = jitter_interval(interval);
            assert!(
                jittered >= low && jittered <= high,
                "jitter {jittered:?} outside {low:?}..{high:?}"
            );
        }
    }

    #[test]
    fn jitter_reaches_both_ends_of_the_window() {
        // The previous implementation halved the random draw, so the interval
        // never exceeded +5% and the maximum draw produced no jitter at all.
        // A thousand draws over a ±10% window must land in both outer thirds.
        let interval = Duration::from_secs(6 * 3600);
        let jitter = interval / 10;
        let low_third = interval - jitter / 3;
        let high_third = interval + jitter / 3;
        let mut saw_low = false;
        let mut saw_high = false;
        for _ in 0..1_000 {
            let jittered = jitter_interval(interval);
            saw_low |= jittered < low_third;
            saw_high |= jittered > high_third;
        }
        assert!(saw_low, "jitter never reached the bottom of the window");
        assert!(saw_high, "jitter never reached the top of the window");
    }

    #[test]
    fn jitter_of_a_tiny_interval_is_the_interval() {
        // interval / 10 rounds to zero nanoseconds; no jitter is possible.
        let tiny = Duration::from_nanos(5);
        assert_eq!(jitter_interval(tiny), tiny);
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    /// How far in the future a source is scheduled, in whole seconds.
    fn delay_secs(health: &SourceHealth) -> u64 {
        health
            .next_due_at
            .saturating_duration_since(Instant::now())
            .as_secs()
    }

    #[test]
    fn a_source_that_never_observed_goes_almost_immediately() {
        // The old code put this a full interval out. With Reddit at 12h and
        // deploys more frequent than that, the first sweep never arrived and
        // community_observations stayed empty while the worker looked healthy.
        let health = SourceHealth::new(Duration::from_secs(12 * 3600), None);
        assert!(
            delay_secs(&health) <= 30,
            "a source with no observations should sweep now, not in {}s",
            delay_secs(&health),
        );
    }

    #[test]
    fn an_overdue_source_goes_almost_immediately() {
        let health = SourceHealth::new(
            Duration::from_secs(6 * 3600),
            Some(Duration::from_secs(9 * 3600)),
        );
        assert!(
            delay_secs(&health) <= 30,
            "9h old against a 6h interval is overdue"
        );
    }

    #[test]
    fn a_freshly_observed_source_waits_out_the_remainder() {
        // Restarting must not re-scrape something observed a minute ago; that
        // is what makes a deploy loop hammer every source it owns.
        let interval = Duration::from_secs(12 * 3600);
        let health = SourceHealth::new(interval, Some(Duration::from_secs(600)));
        let delay = delay_secs(&health);
        assert!(
            delay > 3600,
            "observed 10 minutes ago against a 12h interval should wait hours, waited {delay}s",
        );
        // `jitter_interval` spreads ±10%, so the ceiling is the remaining
        // window plus that, not the bare interval — asserting the interval
        // itself made this fail whenever the jitter landed high.
        let ceiling = (interval.as_secs() as f64 * 1.1) as u64;
        assert!(
            delay <= ceiling,
            "the wait must stay within the interval plus jitter ({ceiling}s), got {delay}s",
        );
    }

    #[test]
    fn the_schedule_survives_a_restart() {
        // Two consecutive restarts with the same observation age must produce
        // the same window. If the clock came from process start instead, each
        // restart would push the next sweep another full interval away.
        let interval = Duration::from_secs(6 * 3600);
        let elapsed = Some(Duration::from_secs(3600));
        let first = delay_secs(&SourceHealth::new(interval, elapsed));
        let second = delay_secs(&SourceHealth::new(interval, elapsed));
        let drift = first.abs_diff(second);
        assert!(
            drift <= interval.as_secs() / 5,
            "restarts drifted by {drift}s; the schedule is following the process, not the data",
        );
    }
}
