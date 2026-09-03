//! Advertising the capabilities this worker executes in-process.
//!
//! The action dispatcher parks any action whose capability no live executor
//! advertises, and only external executors ever registered — n8n posts its
//! manifest to the registry and gets `content.artifact`, `team.email` and the
//! rest. The executors that live inside this process never did, because they
//! claim work by querying `viryaos_autopilot_actions` directly and nobody
//! noticed the dispatcher was a separate gate in front of that query.
//!
//! So `community.engage` actions were created, parked with
//! `last_error_kind = 'awaiting_executor'`, and re-parked every cycle, while
//! the community executor sat idle a few threads away logging that it was
//! running. Production carried two of them for hours and the log said, once
//! every five minutes, that no executor advertised the capability — which was
//! true, and was the whole problem.
//!
//! Registration is a heartbeat rather than a one-off announcement. A worker
//! that dies stops refreshing, the advertisement expires, and its actions park
//! again — which is the correct behaviour and the reason the registry has an
//! expiry at all. Announcing once at startup would leave a dead worker
//! advertising capabilities forever.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

/// The executor id this process registers under.
///
/// One id per process role, not per capability: the registry's unit is an
/// executor, and splitting one process into several imaginary ones would let
/// half of it look alive after the other half stopped.
const EXECUTOR_ID: &str = "crowdrelay-worker-inprocess";

/// How long an advertisement survives without a refresh.
///
/// Long enough to ride out a slow cycle or a restart, short enough that a
/// worker that stops for good takes its capabilities down with it while an
/// operator is still looking at the incident.
const ADVERTISEMENT_TTL: Duration = Duration::from_secs(300);

/// How often the advertisement is refreshed. A third of the TTL, so two
/// missed beats are survivable.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(100);

/// Publishes the in-process executor's capabilities and keeps them fresh.
pub struct ExecutorRegistrar {
    pool: PgPool,
    workspace_id: WorkspaceId,
    capabilities: Vec<&'static str>,
}

impl ExecutorRegistrar {
    /// Builds a registrar for the capabilities this process actually executes.
    ///
    /// `capabilities` must list only what is really running. Advertising a
    /// capability whose executor was not started is worse than not advertising
    /// it: the action unparks, gets claimed by nothing, and burns attempts
    /// instead of waiting visibly.
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        capabilities: Vec<&'static str>,
    ) -> Option<Self> {
        if capabilities.is_empty() {
            return None;
        }
        Some(Self {
            pool,
            workspace_id,
            capabilities,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(HEARTBEAT_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Advertise before the first tick so the executor is claimable within
        // a second of startup rather than after one heartbeat.
        self.advertise_once().await;
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        // Withdraw on a clean shutdown so actions park
                        // immediately instead of waiting out the TTL against
                        // a process that is already gone.
                        self.withdraw().await;
                        return;
                    }
                }
                _ = ticker.tick() => self.advertise_once().await,
            }
        }
    }

    async fn advertise_once(&self) {
        if let Err(error) = self.advertise().await {
            // A failed heartbeat is not fatal: the advertisement has a TTL
            // and the next beat is 100 seconds away. Saying so once per
            // failure is what makes a persistent one visible.
            tracing::warn!(error = %error, "failed to refresh in-process executor capabilities");
        }
    }

    async fn advertise(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let ttl_seconds = i64::try_from(ADVERTISEMENT_TTL.as_secs()).unwrap_or(300);
        sqlx::query(
            r#"
            INSERT INTO viryaos_executor_instances
                (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at, metadata)
            VALUES ($1, $2, $3, $4, now(), now() + ($5 || ' seconds')::interval,
                    jsonb_build_object('kind', 'in_process'))
            ON CONFLICT (workspace_id, executor_id) DO UPDATE SET
                version = EXCLUDED.version,
                manifest_sha = EXCLUDED.manifest_sha,
                observed_at = now(),
                expires_at = EXCLUDED.expires_at
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(EXECUTOR_ID)
        .bind(env!("CARGO_PKG_VERSION"))
        // The registry wants a manifest hash to tell builds apart. This
        // process has no manifest to hash, so it reports the capability set
        // it is advertising — which is the thing a reader actually wants to
        // know changed.
        .bind(capability_fingerprint(&self.capabilities))
        .bind(ttl_seconds.to_string())
        .execute(&mut *tx)
        .await?;

        for capability in &self.capabilities {
            sqlx::query(
                r#"
                INSERT INTO viryaos_executor_capabilities
                    (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
                VALUES ($1, $2, $3, $4, now(), now() + ($5 || ' seconds')::interval)
                ON CONFLICT (workspace_id, executor_id, capability) DO UPDATE SET
                    capability_version = EXCLUDED.capability_version,
                    observed_at = now(),
                    expires_at = EXCLUDED.expires_at
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(EXECUTOR_ID)
            .bind(capability)
            .bind(env!("CARGO_PKG_VERSION"))
            .bind(ttl_seconds.to_string())
            .execute(&mut *tx)
            .await?;
        }

        // A capability this build no longer executes must stop being
        // advertised, or a removed executor keeps unparking work nothing will
        // claim. Expiring rather than deleting keeps the history readable.
        sqlx::query(
            r#"
            UPDATE viryaos_executor_capabilities
            SET expires_at = now()
            WHERE workspace_id = $1
              AND executor_id = $2
              AND capability <> ALL($3)
              AND expires_at > now()
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(EXECUTOR_ID)
        .bind(&self.capabilities)
        .execute(&mut *tx)
        .await?;

        tx.commit().await
    }

    async fn withdraw(&self) {
        let result = sqlx::query(
            r#"
            UPDATE viryaos_executor_instances
            SET expires_at = now()
            WHERE workspace_id = $1 AND executor_id = $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(EXECUTOR_ID)
        .execute(&self.pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(error = %error, "failed to withdraw in-process executor capabilities");
        }
    }
}

/// A stable short identifier for a capability set, used where the registry
/// asks for a manifest hash.
fn capability_fingerprint(capabilities: &[&'static str]) -> String {
    let mut sorted: Vec<&str> = capabilities.to_vec();
    sorted.sort_unstable();
    sorted.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_registrar_with_nothing_to_advertise_is_not_built() {
        // Advertising an empty capability set would register a live executor
        // that executes nothing, which reads as healthy and is not.
        let pool = PgPool::connect_lazy("postgres://invalid/invalid").expect("lazy pool");
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
        assert!(ExecutorRegistrar::new(pool.clone(), workspace, Vec::new()).is_none());
        assert!(
            ExecutorRegistrar::new(pool, workspace, vec!["community.engage"]).is_some(),
            "a real capability must produce a registrar"
        );
    }

    #[test]
    fn the_fingerprint_does_not_depend_on_the_order_capabilities_were_listed() {
        assert_eq!(
            capability_fingerprint(&["community.engage", "agent.content"]),
            capability_fingerprint(&["agent.content", "community.engage"]),
        );
    }

    #[test]
    fn the_heartbeat_refreshes_well_inside_the_advertisement_lifetime() {
        // Two missed beats must not expire the advertisement, or a slow cycle
        // parks every action it was in the middle of.
        assert!(HEARTBEAT_INTERVAL * 2 < ADVERTISEMENT_TTL);
    }
}
