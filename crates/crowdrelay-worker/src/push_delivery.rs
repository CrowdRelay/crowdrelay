//! Durable fan push transport worker.
//!
//! CrowdRelay owns eligibility, claims and terminal delivery truth. Provider
//! acceptance is not called delivered; a device/service-worker acknowledgement
//! after display is required for the terminal `delivered` state.

mod crypto;
mod providers;
mod repository;

use std::{env, time::Duration};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::WorkspaceId;
use getrandom::fill as fill_random;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

use self::{
    providers::{ProviderConfig, ProviderOutcome, PushPayload, PushProviders},
    repository::{ProviderTerminal, PushDeliveryRepository},
};

const PUSH_BATCH_SIZE: i64 = 8;
const PUSH_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct PushDeliveryWorker {
    repository: PushDeliveryRepository,
    providers: PushProviders,
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
        if quiet_timezone.len() > 64
            || !quiet_timezone.contains('/')
            || !quiet_timezone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
        {
            anyhow::bail!("CROWDRELAY_TENANT_TIMEZONE is not an IANA-style timezone");
        }
        Ok(Self {
            repository: PushDeliveryRepository::new(
                database,
                workspace_id.into_uuid(),
                operation_timeout,
                quiet_timezone,
            ),
            providers,
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
        if !self.repository.feature_enabled().await? {
            return Ok(());
        }
        let deliveries = self.repository.claim_due(PUSH_BATCH_SIZE).await?;
        if deliveries.is_empty() {
            return Ok(());
        }
        tracing::debug!(count = deliveries.len(), "claimed fan push deliveries");
        for delivery in deliveries {
            let ack_token = new_ack_token()?;
            if !self
                .repository
                .start_provider(&delivery, &ack_token)
                .await?
            {
                tracing::warn!(delivery_id = %delivery.id, "push claim changed before provider start");
                continue;
            }
            let payload = PushPayload::from_delivery(&delivery, &ack_token);
            let outcome = self.providers.send(&delivery, &payload).await;
            match outcome {
                ProviderOutcome::Accepted { reference } => {
                    self.repository
                        .provider_accepted(delivery.id, reference.as_deref())
                        .await?;
                    tracing::debug!(
                        delivery_id = %delivery.id,
                        transport = delivery.transport,
                        "push provider accepted delivery; awaiting device acknowledgement"
                    );
                }
                ProviderOutcome::Retry { code } => {
                    self.repository.retry_later(delivery.id, code).await?;
                    tracing::warn!(delivery_id = %delivery.id, code, "push delivery scheduled for safe retry");
                }
                ProviderOutcome::Failed {
                    code,
                    invalidate_endpoint,
                } => {
                    self.repository
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
                    self.repository
                        .terminal(delivery.id, ProviderTerminal::Ambiguous, code, false)
                        .await?;
                    tracing::error!(
                        delivery_id = %delivery.id,
                        code,
                        "push provider outcome ambiguous; automatic resend suppressed"
                    );
                }
            }
        }
        Ok(())
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
}
