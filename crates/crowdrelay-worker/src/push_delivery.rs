//! Durable fan push transport worker.
//!
//! CrowdRelay owns eligibility, claims and terminal delivery truth. Provider
//! acceptance is not called delivered; a device/service-worker acknowledgement
//! after display is required for the terminal `delivered` state.

mod crypto;
mod providers;
mod repository;

use std::{env, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::WorkspaceId;
use getrandom::fill as fill_random;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    task::JoinSet,
    time::{MissedTickBehavior, interval},
};

use self::{
    providers::{ProviderConfig, ProviderOutcome, PushPayload, PushProviders},
    repository::{ClaimedDelivery, ProviderTerminal, PushDeliveryRepository},
};

const PUSH_BATCH_SIZE: i64 = 8;
const PUSH_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct PushDeliveryWorker {
    repository: PushDeliveryRepository,
    providers: Arc<PushProviders>,
    /// Last observed state of the persisted `push_delivery_enabled` flag, so a
    /// change is reported once rather than on every five-second poll. `None`
    /// until the first read, which makes the first observation always log.
    flag_state: Option<bool>,
}

impl PushDeliveryWorker {
    pub fn from_env(
        database: PgPool,
        workspace_id: WorkspaceId,
        operation_timeout: Duration,
    ) -> Result<Self> {
        let providers = PushProviders::new(ProviderConfig::from_env()?, operation_timeout)
            .context("invalid fan push provider configuration")?;
        let workspace_slug =
            env::var("CROWDRELAY_WORKSPACE_SLUG").unwrap_or_else(|_| "virya".to_owned());
        let quiet_timezone = match env::var("CROWDRELAY_TENANT_TIMEZONE") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
            _ if workspace_slug == "virya" => "Europe/Warsaw".to_owned(),
            _ => {
                anyhow::bail!("CROWDRELAY_TENANT_TIMEZONE is required for non-Virya push delivery")
            }
        };
        if !crowdrelay_infra::regional::is_known_iana_timezone(&quiet_timezone) {
            anyhow::bail!("CROWDRELAY_TENANT_TIMEZONE is not a known IANA timezone");
        }
        Ok(Self {
            repository: PushDeliveryRepository::new(
                database,
                workspace_id.into_uuid(),
                operation_timeout,
                quiet_timezone,
            ),
            providers: Arc::new(providers),
            flag_state: None,
        })
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(PUSH_POLL_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticks.tick() => {
                    if let Err(error) = self.run_once().await {
                        tracing::warn!(error = %error, "fan push delivery cycle failed");
                    }
                }
            }
        }
    }

    async fn run_once(&mut self) -> Result<()> {
        self.repository.maintain().await?;
        let enabled = self.repository.feature_enabled().await?;
        // The process gate decides whether this worker exists at all; the
        // persisted flag decides whether it delivers. Returning silently on the
        // second one meant a workspace with push switched off queued deliveries
        // forever and never said so -- the rows pile up in `fan_push_deliveries`
        // and the only symptom is that fans hear nothing. Report the transition
        // once, with the backlog it is holding, rather than every poll.
        match flag_transition(self.flag_state, enabled) {
            FlagTransition::Unchanged => {}
            FlagTransition::TurnedOn => {
                tracing::info!(
                    "fan push delivery is enabled; draining any backlog queued while it was off"
                );
            }
            FlagTransition::TurnedOff => {
                let waiting = self.repository.pending_delivery_count().await.unwrap_or(-1);
                tracing::warn!(
                    waiting,
                    "fan push delivery is OFF for this workspace: the `push_delivery_enabled` \
                     feature flag is false, so queued pushes will not be sent. Enable it in \
                     ecosystem_feature_flags to deliver them."
                );
            }
        }
        self.flag_state = Some(enabled);
        if !enabled {
            return Ok(());
        }
        let deliveries = self.repository.claim_due(PUSH_BATCH_SIZE).await?;
        if deliveries.is_empty() {
            return Ok(());
        }
        tracing::debug!(count = deliveries.len(), "claimed fan push deliveries");
        // Deliveries are independent: each claim runs its own provider round
        // trip and persistence so one slow or failing delivery cannot stall
        // the batch. The claimed batch size is the concurrency cap.
        let mut tasks = JoinSet::new();
        for delivery in deliveries {
            let repository = self.repository.clone();
            let providers = Arc::clone(&self.providers);
            tasks.spawn(async move { deliver_one(&repository, &providers, delivery).await });
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "fan push delivery failed; siblings continue");
                }
                Err(join_error) => {
                    tracing::error!(
                        cancelled = join_error.is_cancelled(),
                        panic = join_error.is_panic(),
                        "fan push delivery task stopped unexpectedly"
                    );
                }
            }
        }
        Ok(())
    }
}

async fn deliver_one(
    repository: &PushDeliveryRepository,
    providers: &PushProviders,
    delivery: ClaimedDelivery,
) -> Result<()> {
    let ack_token = new_ack_token()?;
    if !repository.start_provider(&delivery, &ack_token).await? {
        tracing::warn!(delivery_id = %delivery.id, "push claim changed before provider start");
        return Ok(());
    }
    let payload = PushPayload::from_delivery(&delivery, &ack_token);
    match providers.send(&delivery, &payload).await {
        ProviderOutcome::Accepted { reference } => {
            repository
                .provider_accepted(delivery.id, reference.as_deref())
                .await?;
            tracing::debug!(
                delivery_id = %delivery.id,
                transport = delivery.transport,
                "push provider accepted delivery; awaiting device acknowledgement"
            );
        }
        ProviderOutcome::Retry { code } => {
            repository.retry_later(delivery.id, code).await?;
            tracing::warn!(delivery_id = %delivery.id, code, "push delivery scheduled for safe retry");
        }
        ProviderOutcome::Failed {
            code,
            invalidate_endpoint,
        } => {
            repository
                .terminal(
                    delivery.id,
                    ProviderTerminal::Failed,
                    code,
                    invalidate_endpoint,
                )
                .await?;
            tracing::warn!(delivery_id = %delivery.id, code, "push delivery failed closed");
        }
        ProviderOutcome::Ambiguous { code } => {
            repository
                .terminal(delivery.id, ProviderTerminal::Ambiguous, code, false)
                .await?;
            tracing::error!(
                delivery_id = %delivery.id,
                code,
                "push provider outcome ambiguous; automatic resend suppressed"
            );
        }
    }
    Ok(())
}

/// What the persisted flag did since the last poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagTransition {
    Unchanged,
    TurnedOn,
    TurnedOff,
}

/// Decides whether this poll should say anything about the flag.
///
/// The worker polls every five seconds, so reporting the state every time would
/// bury the log; reporting only on a change would mean a worker that starts with
/// push already off never says so at all. `None` is "nothing observed yet",
/// which is why the first read always reports.
fn flag_transition(previous: Option<bool>, enabled: bool) -> FlagTransition {
    match (previous, enabled) {
        (Some(before), now) if before == now => FlagTransition::Unchanged,
        (_, true) => FlagTransition::TurnedOn,
        (_, false) => FlagTransition::TurnedOff,
    }
}

fn new_ack_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    fill_random(&mut bytes).context("fan push acknowledgement token RNG failed")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledgement_token_has_bounded_entropy_encoding() {
        let token = new_ack_token().ok();
        assert!(token.as_ref().is_some_and(|value| value.len() == 43));
        assert!(token.as_ref().is_some_and(|value| value.is_ascii()));
    }

    #[test]
    fn a_worker_that_starts_with_push_off_says_so() {
        // The failure this exists to prevent: push disabled, deliveries piling
        // up in `fan_push_deliveries`, and not one line in the log explaining
        // why fans hear nothing. A first observation is always worth reporting
        // even though nothing "changed".
        assert_eq!(flag_transition(None, false), FlagTransition::TurnedOff);
        assert_eq!(flag_transition(None, true), FlagTransition::TurnedOn);
    }

    #[test]
    fn a_steady_flag_is_reported_once_not_every_poll() {
        // Five-second polls: repeating the state would make the warning
        // worthless and hide everything else in the log.
        assert_eq!(
            flag_transition(Some(false), false),
            FlagTransition::Unchanged
        );
        assert_eq!(flag_transition(Some(true), true), FlagTransition::Unchanged);
    }

    #[test]
    fn flipping_the_flag_is_reported_in_both_directions() {
        // Enabling push must not need a worker restart, so the enable has to be
        // visible too -- it is the confirmation that a backlog is about to
        // drain.
        assert_eq!(flag_transition(Some(false), true), FlagTransition::TurnedOn);
        assert_eq!(
            flag_transition(Some(true), false),
            FlagTransition::TurnedOff
        );
    }
}
