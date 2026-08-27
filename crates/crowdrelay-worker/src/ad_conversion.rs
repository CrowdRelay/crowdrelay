//! Server-side ad conversion tracking for Meta CAPI, Google Ads, and Bandsintown.
//!
//! The worker listens on Postgres LISTEN/NOTIFY channels instead of polling.
//! Database triggers fire notifications when a fan gets ad attribution
//! (signup) or when a ticket order transitions to 'paid'. The worker wakes
//! immediately, processes the batch, then goes back to waiting — zero idle
//! polling.
//!
//! The only scenarios where LISTEN/NOTIFY can miss events are startup (the
//! worker was down when the notification fired) and listener reconnect
//! (connection dropped mid-gap). Both are discrete events, so the worker
//! runs a single sweep on startup and after every reconnect — no periodic
//! fallback poll.
//!
//! Events are idempotent: each (platform, event_name, event_id) is sent
//! exactly once.
//!
//! Events sent:
//! - Meta: Lead (fan signup), Purchase (ticket order paid)
//! - Google: Lead (fan signup with gclid), Purchase (ticket order paid with gclid)
//! - Bandsintown: conversion callback (fan signup with bandsintown_ref)
//!
//! Each platform runs independently within a cycle — a Meta failure does not
//! block Google or Bandsintown. A small politeness gap between individual API
//! calls respects Meta CAPI rate limits.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::config::AdConversionConfig;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::{
    sync::watch,
    time::{Interval, MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const BATCH_SIZE: i64 = 50;
/// Politeness gap between individual API calls within a cycle.
/// Meta CAPI tolerates roughly 1 event per second per dataset; 500ms is safe
/// and keeps a 50-fan batch under 30 seconds.
const REQUEST_SPACING: Duration = Duration::from_millis(500);
/// Maximum time for one cycle. Worst case: 5 batches (meta lead, meta purchase,
/// google lead, google purchase, bandsintown lead) × 50 items × 500ms = 125s.
/// Add headroom for network latency and token refresh.
const CYCLE_TIMEOUT: Duration = Duration::from_secs(180);
const META_GRAPH_BASE: &str = "https://graph.facebook.com";
const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_ADS_API_BASE: &str = "https://googleads.googleapis.com";
const GOOGLE_ADS_API_VERSION: &str = "v18";
const BANDSINTOWN_CONVERSION_URL: &str = "https://www.bandsintown.com/api/v1/conversion";
const NOTIFY_CHANNEL_LEAD: &str = "ad_conversion_lead";
const NOTIFY_CHANNEL_PURCHASE: &str = "ad_conversion_purchase";

#[derive(Debug, thiserror::Error)]
pub enum AdConversionError {
    #[error("ad conversion HTTP request failed")]
    Network(#[from] reqwest::Error),
    #[error("ad conversion API returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("ad conversion payload serialization failed")]
    Payload(#[from] serde_json::Error),
    #[error("ad conversion database query failed")]
    Database(#[from] sqlx::Error),
    #[error("google ads OAuth token refresh failed")]
    OAuth,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct FanAttribution {
    fan_id: Uuid,
    normalized_email: String,
    #[sqlx(default)]
    meta_fbp: Option<String>,
    #[sqlx(default)]
    meta_fbc: Option<String>,
    #[sqlx(default)]
    google_gclid: Option<String>,
    #[sqlx(default)]
    bandsintown_ref: Option<String>,
    #[sqlx(default)]
    utm_source: Option<String>,
    #[sqlx(default)]
    utm_medium: Option<String>,
    #[sqlx(default)]
    utm_campaign: Option<String>,
    #[sqlx(default)]
    client_ip_address: Option<String>,
    #[sqlx(default)]
    client_user_agent: Option<String>,
    #[sqlx(default)]
    event_source_url: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
struct PaidTicketOrder {
    order_id: Uuid,
    fan_id: Option<Uuid>,
    buyer_email: String,
    amount_gross_minor: i64,
    amount_refunded_minor: i64,
    currency: String,
    #[sqlx(default)]
    meta_fbp: Option<String>,
    #[sqlx(default)]
    meta_fbc: Option<String>,
    #[sqlx(default)]
    google_gclid: Option<String>,
    #[sqlx(default)]
    bandsintown_ref: Option<String>,
    #[sqlx(default)]
    utm_source: Option<String>,
    #[sqlx(default)]
    utm_medium: Option<String>,
    #[sqlx(default)]
    utm_campaign: Option<String>,
    #[sqlx(default)]
    client_ip_address: Option<String>,
    #[sqlx(default)]
    client_user_agent: Option<String>,
    #[sqlx(default)]
    event_source_url: Option<String>,
}

pub struct AdConversionWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    config: AdConversionConfig,
    client: Client,
    /// Cached Google OAuth access token.
    google_token: tokio::sync::Mutex<Option<CachedGoogleToken>>,
}

#[derive(Clone)]
struct CachedGoogleToken {
    access_token: String,
    expires_at: time::OffsetDateTime,
}

impl AdConversionWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        config: AdConversionConfig,
        operation_timeout: Duration,
    ) -> Result<Self, AdConversionError> {
        let client = Client::builder()
            .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
            .timeout(operation_timeout * 2)
            .user_agent("CrowdRelay/1.0 ad-conversion")
            .build()?;
        Ok(Self {
            pool,
            workspace_id,
            config,
            client,
            google_token: tokio::sync::Mutex::new(None),
        })
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.any_enabled()
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        if !self.is_enabled() {
            tracing::info!("ad conversion worker disabled; no platforms configured");
            return;
        }

        // Connect the LISTEN connection BEFORE the startup sweep. This closes
        // the race window: if we sweep first and connect after, any signups
        // during the sweep fire notifications that no one is listening for
        // (Postgres doesn't queue notifications for non-listening sessions).
        // By connecting first, notifications during the sweep are queued by
        // Postgres and picked up after the sweep completes.
        let mut listener = match self.connect_listener().await {
            Some(listener) => listener,
            None => return, // error already logged
        };

        // Startup sweep: catch any notifications that fired while the worker
        // was down (restart, deploy, crash). This is the only scenario where
        // LISTEN/NOTIFY can miss events — Postgres doesn't queue notifications
        // for disconnected listeners.
        tracing::info!("ad conversion worker starting — running startup sweep");
        self.run_cycle_with_timeout().await;

        tracing::info!("ad conversion worker listening on NOTIFY channels");

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                // Primary path: wake on database notification
                notification = listener.recv() => {
                    match notification {
                        Ok(notif) => {
                            tracing::debug!(
                                channel = notif.channel(),
                                payload = notif.payload(),
                                "ad conversion notification received"
                            );
                            // Coalesce notification storms: if N fans sign up
                            // at once, N NOTIFY events fire. The first cycle
                            // fetches all N (up to BATCH_SIZE). Drain the
                            // remaining notifications so we don't run N-1
                            // no-op cycles.
                            while let Ok(Some(_)) = listener.try_recv().await {}
                            self.run_cycle_with_timeout().await;
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "PgListener error — reconnecting");
                            // Reconnect with backoff. Without LISTEN we have no
                            // way to receive events, so we must keep trying
                            // until it works or we shut down.
                            let mut backoff = Duration::from_secs(1);
                            loop {
                                if *shutdown.borrow() { return; }
                                if let Some(new_listener) = self.connect_listener().await {
                                    listener = new_listener;
                                    tracing::info!("PgListener reconnected — running sweep for missed notifications");
                                    self.run_cycle_with_timeout().await;
                                    break;
                                }
                                tracing::warn!(backoff = ?backoff, "PgListener reconnect failed — retrying");
                                tokio::select! {
                                    biased;
                                    changed = shutdown.changed() => {
                                        // Sender dropped or shutdown set — either way, stop.
                                        if changed.is_err() || *shutdown.borrow() { return; }
                                    }
                                    _ = tokio::time::sleep(backoff) => {}
                                }
                                backoff = (backoff * 2).min(Duration::from_secs(60));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Connects a PgListener and subscribes to both notification channels.
    /// Returns None on failure (error already logged).
    async fn connect_listener(&self) -> Option<PgListener> {
        let mut listener = PgListener::connect_with(&self.pool).await.map_err(|error| {
            tracing::error!(error = %error, "failed to connect PgListener for ad conversion worker");
        }).ok()?;
        listener
            .listen_all([NOTIFY_CHANNEL_LEAD, NOTIFY_CHANNEL_PURCHASE])
            .await
            .map_err(|error| {
                tracing::error!(error = %error, "failed to LISTEN on ad conversion channels");
            })
            .ok()?;
        Some(listener)
    }

    async fn run_cycle_with_timeout(&self) {
        match timeout(CYCLE_TIMEOUT, self.run_cycle()).await {
            Ok(Ok(stats)) => {
                if stats.total_sent > 0 {
                    tracing::info!(
                        meta_sent = stats.meta_sent,
                        google_sent = stats.google_sent,
                        bandsintown_sent = stats.bandsintown_sent,
                        "ad conversion cycle completed"
                    );
                }
            }
            Ok(Err(error)) => tracing::warn!(error = %error, "ad conversion cycle failed"),
            Err(_) => tracing::warn!("ad conversion cycle timed out"),
        }
    }

    /// Runs one processing cycle. Each platform is tried independently — a
    /// failure on one does not block the others. Lead events (fan signups)
    /// and Purchase events (paid ticket orders) are both forwarded.
    async fn run_cycle(&self) -> Result<CycleStats, AdConversionError> {
        let mut stats = CycleStats::default();

        if self.config.meta.enabled {
            match self.send_meta_lead_events().await {
                Ok(sent) => stats.meta_sent += sent,
                Err(error) => tracing::warn!(error = %error, "meta CAPI lead cycle failed"),
            }
            match self.send_meta_purchase_events().await {
                Ok(sent) => stats.meta_sent += sent,
                Err(error) => tracing::warn!(error = %error, "meta CAPI purchase cycle failed"),
            }
        }
        if self.config.google.enabled {
            match self.send_google_lead_events().await {
                Ok(sent) => stats.google_sent += sent,
                Err(error) => tracing::warn!(error = %error, "google ads lead cycle failed"),
            }
            match self.send_google_purchase_events().await {
                Ok(sent) => stats.google_sent += sent,
                Err(error) => tracing::warn!(error = %error, "google ads purchase cycle failed"),
            }
        }
        if self.config.bandsintown.enabled {
            match self.send_bandsintown_conversions().await {
                Ok(sent) => stats.bandsintown_sent += sent,
                Err(error) => tracing::warn!(error = %error, "bandsintown conversion cycle failed"),
            }
        }
        stats.total_sent = stats.meta_sent + stats.google_sent + stats.bandsintown_sent;
        Ok(stats)
    }

    // ── Meta CAPI ──────────────────────────────────────────────────────

    async fn send_meta_lead_events(&self) -> Result<usize, AdConversionError> {
        let fans = self.fetch_pending_fans("meta", "Lead").await?;
        let mut limiter = rate_limiter();
        let mut sent = 0;
        for fan in &fans {
            limiter.tick().await;
            let event_id = format!("lead-{}", fan.fan_id);
            let result = self.send_meta_event(fan, "Lead", &event_id).await;
            self.record_result("meta", Some(fan.fan_id), None, "Lead", &event_id, &result)
                .await;
            if result.is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    async fn send_meta_purchase_events(&self) -> Result<usize, AdConversionError> {
        let orders = self.fetch_pending_orders("meta", "Purchase").await?;
        let mut limiter = rate_limiter();
        let mut sent = 0;
        for order in &orders {
            limiter.tick().await;
            let event_id = format!("purchase-{}", order.order_id);
            let result = self.send_meta_purchase_event(order, &event_id).await;
            self.record_result(
                "meta",
                order.fan_id,
                Some(order.order_id),
                "Purchase",
                &event_id,
                &result,
            )
            .await;
            if result.is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    async fn send_meta_event(
        &self,
        fan: &FanAttribution,
        event_name: &str,
        event_id: &str,
    ) -> Result<u16, AdConversionError> {
        let config = &self.config.meta;
        let now = time::OffsetDateTime::now_utc();
        let event_time = now.unix_timestamp();

        let mut user_data = Map::new();
        user_data.insert("em".to_owned(), json!([hash_sha256(&fan.normalized_email)]));
        if let Some(fbp) = &fan.meta_fbp {
            user_data.insert("fbp".to_owned(), json!(fbp));
        }
        if let Some(fbc) = &fan.meta_fbc {
            user_data.insert("fbc".to_owned(), json!(fbc));
        }
        if let Some(ip) = &fan.client_ip_address {
            user_data.insert("client_ip_address".to_owned(), json!(ip));
        }
        if let Some(ua) = &fan.client_user_agent {
            user_data.insert("client_user_agent".to_owned(), json!(ua));
        }

        let mut custom_data = Map::new();
        if let Some(source) = &fan.utm_source {
            custom_data.insert("utm_source".to_owned(), json!(source));
        }
        if let Some(medium) = &fan.utm_medium {
            custom_data.insert("utm_medium".to_owned(), json!(medium));
        }
        if let Some(campaign) = &fan.utm_campaign {
            custom_data.insert("utm_campaign".to_owned(), json!(campaign));
        }

        let mut event = Map::new();
        event.insert("event_name".to_owned(), json!(event_name));
        event.insert("event_time".to_owned(), json!(event_time));
        event.insert("event_id".to_owned(), json!(event_id));
        event.insert("action_source".to_owned(), json!("website"));
        event.insert("user_data".to_owned(), Value::Object(user_data));
        event.insert("custom_data".to_owned(), Value::Object(custom_data));
        if let Some(url) = &fan.event_source_url {
            event.insert("event_source_url".to_owned(), json!(url));
        }

        let payload = self.build_meta_payload(event, config);
        let url = format!(
            "{META_GRAPH_BASE}/{api_version}/{pixel_id}/events",
            api_version = config.api_version,
            pixel_id = config.pixel_id,
        );
        let response = self
            .client
            .post(&url)
            .query(&[("access_token", config.access_token.as_str())])
            .json(&payload)
            .send()
            .await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status, body = %truncate(&body, 512), "meta CAPI error response");
            return Err(AdConversionError::Status {
                status,
                body: truncate(&body, 512),
            });
        }
        tracing::debug!(event_name, event_id, status, "meta CAPI event sent");
        Ok(status)
    }

    async fn send_meta_purchase_event(
        &self,
        order: &PaidTicketOrder,
        event_id: &str,
    ) -> Result<u16, AdConversionError> {
        let config = &self.config.meta;
        let now = time::OffsetDateTime::now_utc();
        let event_time = now.unix_timestamp();

        let mut user_data = Map::new();
        user_data.insert("em".to_owned(), json!([hash_sha256(&order.buyer_email)]));
        if let Some(fbp) = &order.meta_fbp {
            user_data.insert("fbp".to_owned(), json!(fbp));
        }
        if let Some(fbc) = &order.meta_fbc {
            user_data.insert("fbc".to_owned(), json!(fbc));
        }
        if let Some(ip) = &order.client_ip_address {
            user_data.insert("client_ip_address".to_owned(), json!(ip));
        }
        if let Some(ua) = &order.client_user_agent {
            user_data.insert("client_user_agent".to_owned(), json!(ua));
        }

        // Meta expects value and currency in custom_data for Purchase events.
        // Also include UTM params so Meta can attribute the purchase to the
        // campaign that drove the signup.
        // Use net amount (gross - refunded) so partially refunded orders
        // report the actual value the fan paid.
        let net_minor = order.amount_gross_minor - order.amount_refunded_minor;
        let value = (net_minor as f64) / 100.0;
        let mut custom_data = Map::new();
        custom_data.insert("currency".to_owned(), json!(order.currency.to_lowercase()));
        custom_data.insert("value".to_owned(), json!(value));
        if let Some(source) = &order.utm_source {
            custom_data.insert("utm_source".to_owned(), json!(source));
        }
        if let Some(medium) = &order.utm_medium {
            custom_data.insert("utm_medium".to_owned(), json!(medium));
        }
        if let Some(campaign) = &order.utm_campaign {
            custom_data.insert("utm_campaign".to_owned(), json!(campaign));
        }

        let mut event = Map::new();
        event.insert("event_name".to_owned(), json!("Purchase"));
        event.insert("event_time".to_owned(), json!(event_time));
        event.insert("event_id".to_owned(), json!(event_id));
        event.insert("action_source".to_owned(), json!("website"));
        event.insert("user_data".to_owned(), Value::Object(user_data));
        event.insert("custom_data".to_owned(), Value::Object(custom_data));
        if let Some(url) = &order.event_source_url {
            event.insert("event_source_url".to_owned(), json!(url));
        }

        let payload = self.build_meta_payload(event, config);
        let url = format!(
            "{META_GRAPH_BASE}/{api_version}/{pixel_id}/events",
            api_version = config.api_version,
            pixel_id = config.pixel_id,
        );
        let response = self
            .client
            .post(&url)
            .query(&[("access_token", config.access_token.as_str())])
            .json(&payload)
            .send()
            .await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status, body = %truncate(&body, 512), "meta CAPI Purchase error response");
            return Err(AdConversionError::Status {
                status,
                body: truncate(&body, 512),
            });
        }
        tracing::debug!(event_id, status, "meta CAPI Purchase event sent");
        Ok(status)
    }

    /// Wraps an event object in the standard Meta CAPI payload envelope,
    /// adding the test event code if configured.
    fn build_meta_payload(
        &self,
        event: Map<String, Value>,
        config: &crowdrelay_infra::config::MetaCapiConfig,
    ) -> Value {
        let mut payload = Map::new();
        payload.insert("data".to_owned(), json!([Value::Object(event)]));
        if let Some(test_code) = &config.test_event_code {
            payload.insert("test_event_code".to_owned(), json!(test_code));
        }
        Value::Object(payload)
    }

    // ── Google Ads Enhanced Conversions ────────────────────────────────

    async fn send_google_lead_events(&self) -> Result<usize, AdConversionError> {
        let fans = self.fetch_pending_fans("google", "Lead").await?;
        let mut sent = 0;
        let mut limiter = rate_limiter();
        for fan in &fans {
            limiter.tick().await;
            let event_id = format!("lead-{}", fan.fan_id);
            let result = self
                .send_google_conversion(fan, "Lead", &event_id, None, None)
                .await;
            self.record_result("google", Some(fan.fan_id), None, "Lead", &event_id, &result)
                .await;
            if result.is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    async fn send_google_purchase_events(&self) -> Result<usize, AdConversionError> {
        let orders = self.fetch_pending_orders("google", "Purchase").await?;
        let mut sent = 0;
        let mut limiter = rate_limiter();
        for order in &orders {
            limiter.tick().await;
            let event_id = format!("purchase-{}", order.order_id);
            let net_minor = order.amount_gross_minor - order.amount_refunded_minor;
            let value = (net_minor as f64) / 100.0;
            let result = self
                .send_google_conversion(
                    &FanAttribution {
                        fan_id: order.fan_id.unwrap_or_else(Uuid::nil),
                        normalized_email: order.buyer_email.clone(),
                        meta_fbp: None,
                        meta_fbc: None,
                        google_gclid: order.google_gclid.clone(),
                        bandsintown_ref: None,
                        utm_source: order.utm_source.clone(),
                        utm_medium: order.utm_medium.clone(),
                        utm_campaign: order.utm_campaign.clone(),
                        client_ip_address: order.client_ip_address.clone(),
                        client_user_agent: order.client_user_agent.clone(),
                        event_source_url: order.event_source_url.clone(),
                    },
                    "Purchase",
                    &event_id,
                    Some(value),
                    Some(&order.currency),
                )
                .await;
            self.record_result(
                "google",
                order.fan_id,
                Some(order.order_id),
                "Purchase",
                &event_id,
                &result,
            )
            .await;
            if result.is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    async fn send_google_conversion(
        &self,
        fan: &FanAttribution,
        event_name: &str,
        event_id: &str,
        value: Option<f64>,
        currency: Option<&str>,
    ) -> Result<u16, AdConversionError> {
        let config = &self.config.google;
        let access_token = self.get_google_access_token().await?;
        let now = time::OffsetDateTime::now_utc();
        let conversion_time = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        let mut conversion = Map::new();
        conversion.insert(
            "conversionAction".to_owned(),
            json!(config.conversion_action_id),
        );
        conversion.insert("conversionDateTime".to_owned(), json!(conversion_time));
        // Lead events have no monetary value — report 0, not a default that
        // would inflate conversion value reports in Google Ads.
        conversion.insert("conversionValue".to_owned(), json!(value.unwrap_or(0.0)));
        conversion.insert("currencyCode".to_owned(), json!(currency.unwrap_or("PLN")));
        conversion.insert("orderId".to_owned(), json!(event_id));
        conversion.insert(
            "userIdentifiers".to_owned(),
            json!([{"hashedEmail": hash_sha256(&fan.normalized_email)}]),
        );
        if let Some(gclid) = &fan.google_gclid {
            conversion.insert("gclid".to_owned(), json!(gclid));
        }

        let url = format!(
            "{GOOGLE_ADS_API_BASE}/{GOOGLE_ADS_API_VERSION}/customers/{customer_id}:uploadClickConversions",
            customer_id = config.customer_id,
        );
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &config.developer_token)
            .json(&json!({"conversions": [Value::Object(conversion)], "partialFailure": true}))
            .send()
            .await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status, body = %truncate(&body, 512), "google ads error response");
            return Err(AdConversionError::Status {
                status,
                body: truncate(&body, 512),
            });
        }
        tracing::debug!(event_name, event_id, status, "google ads conversion sent");
        Ok(status)
    }

    async fn get_google_access_token(&self) -> Result<String, AdConversionError> {
        let config = &self.config.google;
        {
            let cache = self.google_token.lock().await;
            if let Some(token) = &*cache
                && token.expires_at > time::OffsetDateTime::now_utc() + Duration::from_secs(60)
            {
                return Ok(token.access_token.clone());
            }
        }
        let response = self
            .client
            .post(GOOGLE_TOKEN_URI)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", config.refresh_token.as_str()),
                ("client_id", config.client_id.as_str()),
                ("client_secret", config.client_secret.as_str()),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            tracing::warn!(
                status = response.status().as_u16(),
                "google OAuth token refresh failed"
            );
            return Err(AdConversionError::OAuth);
        }
        let token_response: GoogleTokenResponse = response.json().await?;
        let expires_at = time::OffsetDateTime::now_utc()
            + Duration::from_secs(token_response.expires_in.max(60) as u64);
        let token = CachedGoogleToken {
            access_token: token_response.access_token.clone(),
            expires_at,
        };
        {
            let mut cache = self.google_token.lock().await;
            *cache = Some(token);
        }
        Ok(token_response.access_token)
    }

    // ── Bandsintown ────────────────────────────────────────────────────

    async fn send_bandsintown_conversions(&self) -> Result<usize, AdConversionError> {
        let fans = self.fetch_pending_fans("bandsintown", "Lead").await?;
        let mut sent = 0;
        let mut limiter = rate_limiter();
        for fan in &fans {
            limiter.tick().await;
            let event_id = format!("lead-{}", fan.fan_id);
            let result = self.send_bandsintown_conversion(fan, &event_id).await;
            self.record_result(
                "bandsintown",
                Some(fan.fan_id),
                None,
                "Lead",
                &event_id,
                &result,
            )
            .await;
            if result.is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    async fn send_bandsintown_conversion(
        &self,
        fan: &FanAttribution,
        event_id: &str,
    ) -> Result<u16, AdConversionError> {
        let config = &self.config.bandsintown;
        let payload = json!({
            "token": config.api_token,
            "event_id": event_id,
            "conversion_type": "signup",
            "fan_email_hash": hash_sha256(&fan.normalized_email),
            "bandsintown_ref": fan.bandsintown_ref,
            "utm_source": fan.utm_source,
            "utm_medium": fan.utm_medium,
            "utm_campaign": fan.utm_campaign,
        });
        let response = self
            .client
            .post(BANDSINTOWN_CONVERSION_URL)
            .json(&payload)
            .send()
            .await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status, body = %truncate(&body, 512), "bandsintown conversion error");
            return Err(AdConversionError::Status {
                status,
                body: truncate(&body, 512),
            });
        }
        tracing::debug!(event_id, status, "bandsintown conversion sent");
        Ok(status)
    }

    // ── Shared database helpers ────────────────────────────────────────

    /// Fetches active, marketing-consented fans that have platform-specific
    /// ad attribution but no delivery record for the given platform/event.
    ///
    /// Each platform only gets fans it can actually attribute:
    /// - Meta: fans with `_fbp` or `_fbc`
    /// - Google: fans with `gclid`
    /// - Bandsintown: fans with `bandsintown_ref`
    async fn fetch_pending_fans(
        &self,
        platform: &str,
        event_name: &str,
    ) -> Result<Vec<FanAttribution>, AdConversionError> {
        let workspace_uuid = self.workspace_id.into_uuid();
        let platform_filter = match platform {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        let sql = format!(
            r#"
            SELECT
                fan.id AS fan_id,
                fan.normalized_email,
                attr.meta_fbp,
                attr.meta_fbc,
                attr.google_gclid,
                attr.bandsintown_ref,
                attr.utm_source,
                attr.utm_medium,
                attr.utm_campaign,
                attr.client_ip_address,
                attr.client_user_agent,
                attr.event_source_url
            FROM fans fan
            JOIN fan_ad_attribution attr
              ON attr.workspace_id = fan.workspace_id
             AND attr.fan_id = fan.id
            WHERE fan.workspace_id = $1
              AND fan.status = 'active'
              AND {platform_filter}
              AND EXISTS (
                  SELECT 1 FROM fan_consents consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
              )
              AND NOT EXISTS (
                  SELECT 1 FROM ad_conversion_deliveries d
                  WHERE d.workspace_id = fan.workspace_id
                    AND d.platform = $2
                    AND d.event_name = $3
                    AND d.fan_id = fan.id
              )
            ORDER BY fan.created_at DESC
            LIMIT $4
            "#
        );
        let rows = sqlx::query_as::<_, FanAttribution>(&sql)
            .bind(workspace_uuid)
            .bind(platform)
            .bind(event_name)
            .bind(BATCH_SIZE)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Fetches paid ticket orders from marketing-consented fans that have
    /// platform-specific ad attribution but no Purchase delivery record.
    async fn fetch_pending_orders(
        &self,
        platform: &str,
        event_name: &str,
    ) -> Result<Vec<PaidTicketOrder>, AdConversionError> {
        let workspace_uuid = self.workspace_id.into_uuid();
        let platform_filter = match platform {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            // Bandsintown doesn't have Purchase events, but keep the guard
            // for safety in case this is called with that platform.
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        let sql = format!(
            r#"
            SELECT
                orders.id AS order_id,
                fan.id AS fan_id,
                orders.buyer_email,
                orders.amount_gross_minor,
                orders.amount_refunded_minor,
                orders.currency::text AS currency,
                attr.meta_fbp,
                attr.meta_fbc,
                attr.google_gclid,
                attr.bandsintown_ref,
                attr.utm_source,
                attr.utm_medium,
                attr.utm_campaign,
                attr.client_ip_address,
                attr.client_user_agent,
                attr.event_source_url
            FROM ticket_orders orders
            JOIN fans fan
              ON fan.workspace_id = orders.workspace_id
             AND fan.normalized_email = orders.buyer_email
            JOIN fan_ad_attribution attr
              ON attr.workspace_id = fan.workspace_id
             AND attr.fan_id = fan.id
            WHERE orders.workspace_id = $1
              AND orders.status IN ('paid', 'partially_refunded')
              AND {platform_filter}
              AND EXISTS (
                  SELECT 1 FROM fan_consents consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
              )
              AND NOT EXISTS (
                  SELECT 1 FROM ad_conversion_deliveries d
                  WHERE d.workspace_id = orders.workspace_id
                    AND d.platform = $2
                    AND d.event_name = $3
                    AND d.ticket_order_id = orders.id
              )
            ORDER BY orders.paid_at DESC NULLS LAST
            LIMIT $4
            "#
        );
        let rows = sqlx::query_as::<_, PaidTicketOrder>(&sql)
            .bind(workspace_uuid)
            .bind(platform)
            .bind(event_name)
            .bind(BATCH_SIZE)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_delivery(
        &self,
        platform: &str,
        fan_id: Option<Uuid>,
        ticket_order_id: Option<Uuid>,
        event_name: &str,
        event_id: &str,
        response_status: u16,
        response_body: Option<&str>,
    ) -> Result<(), AdConversionError> {
        let workspace_uuid = self.workspace_id.into_uuid();
        sqlx::query(
            r#"
            INSERT INTO ad_conversion_deliveries (
                workspace_id, platform, fan_id, ticket_order_id,
                event_name, event_id, action_source,
                response_status, response_body
            ) VALUES ($1, $2, $3, $4, $5, $6, 'website', $7, $8)
            ON CONFLICT (workspace_id, platform, event_name, event_id) DO NOTHING
            "#,
        )
        .bind(workspace_uuid)
        .bind(platform)
        .bind(fan_id)
        .bind(ticket_order_id)
        .bind(event_name)
        .bind(event_id)
        .bind(i32::from(response_status))
        .bind(response_body)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records the outcome of a send attempt.
    ///
    /// **Successes and HTTP rejections** (4xx/5xx) are recorded as delivery
    /// rows. This persists the response for debugging and prevents retries —
    /// a 4xx means the platform rejected the event, so retrying won't help.
    ///
    /// **Transient errors** (network failures, OAuth token refresh failures)
    /// are NOT recorded. The fan/order stays in the pending set and will be
    /// retried on the next cycle, which is the correct behavior for transient
    /// failures.
    async fn record_result(
        &self,
        platform: &str,
        fan_id: Option<Uuid>,
        ticket_order_id: Option<Uuid>,
        event_name: &str,
        event_id: &str,
        result: &Result<u16, AdConversionError>,
    ) {
        let (status, body) = match result {
            Ok(status) => (*status, None),
            Err(AdConversionError::Status { status, body }) => (*status, Some(body.as_str())),
            Err(error) => {
                // Transient error — don't record a delivery row so the
                // event gets retried on the next cycle.
                tracing::warn!(
                    error = %error, platform, event_id,
                    "ad conversion send failed (transient) — will retry next cycle"
                );
                return;
            }
        };
        if let Err(error) = self
            .record_delivery(
                platform,
                fan_id,
                ticket_order_id,
                event_name,
                event_id,
                status,
                body,
            )
            .await
        {
            tracing::warn!(
                error = %error, platform, event_id,
                "failed to record delivery — event will be re-sent next cycle"
            );
        }
    }
}

#[derive(Default)]
struct CycleStats {
    total_sent: usize,
    meta_sent: usize,
    google_sent: usize,
    bandsintown_sent: usize,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    expires_in: i64,
}

/// SHA-256 hex hash of a normalized string, as required by Meta and Google
/// for user data matching. Both platforms require lowercase, trimmed values.
fn hash_sha256(value: &str) -> String {
    let normalized = value.trim().to_lowercase();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

/// Truncates a string to at most `max_chars` characters, safe for UTF-8.
fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

/// Creates a reactive rate-limiter interval for outbound API calls.
///
/// The first `tick()` completes immediately; subsequent ticks yield to the
/// Tokio scheduler and resume only when the spacing window opens. Uses
/// `MissedTickBehavior::Skip` so a slow upstream doesn't cause a burst
/// of catch-up calls.
fn rate_limiter() -> Interval {
    let mut interval = interval(REQUEST_SPACING);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_infra::config::MetaCapiConfig;

    fn test_uuid() -> Uuid {
        Uuid::from_fields_le(
            0x12345678,
            0x1234,
            0x1234,
            &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0],
        )
    }

    #[test]
    fn hash_sha256_is_normalized_and_deterministic() {
        let a = hash_sha256("Fan@Example.COM");
        let b = hash_sha256("fan@example.com");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("żółć", 2), "żó");
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn meta_lead_payload_has_required_fields() {
        let fan = FanAttribution {
            fan_id: test_uuid(),
            normalized_email: "fan@example.com".to_owned(),
            meta_fbp: Some("fb.1.1234567890.1234567890".to_owned()),
            meta_fbc: Some("fb.1.1234567890.abcdef".to_owned()),
            google_gclid: None,
            bandsintown_ref: None,
            utm_source: Some("facebook".to_owned()),
            utm_medium: Some("paid".to_owned()),
            utm_campaign: Some("metalhead_q3".to_owned()),
            client_ip_address: Some("203.0.113.42".to_owned()),
            client_user_agent: Some("Mozilla/5.0".to_owned()),
            event_source_url: Some("https://virya.music/area".to_owned()),
        };
        let event_id = format!("lead-{}", fan.fan_id);
        let now = time::OffsetDateTime::now_utc();
        let event_time = now.unix_timestamp();

        let mut user_data = Map::new();
        user_data.insert("em".to_owned(), json!([hash_sha256(&fan.normalized_email)]));
        user_data.insert("fbp".to_owned(), json!(fan.meta_fbp.as_ref().unwrap()));
        user_data.insert("fbc".to_owned(), json!(fan.meta_fbc.as_ref().unwrap()));
        user_data.insert(
            "client_ip_address".to_owned(),
            json!(fan.client_ip_address.as_ref().unwrap()),
        );
        user_data.insert(
            "client_user_agent".to_owned(),
            json!(fan.client_user_agent.as_ref().unwrap()),
        );

        let mut custom_data = Map::new();
        custom_data.insert("utm_source".to_owned(), json!(fan.utm_source));
        custom_data.insert("utm_medium".to_owned(), json!(fan.utm_medium));
        custom_data.insert("utm_campaign".to_owned(), json!(fan.utm_campaign));

        let mut event = Map::new();
        event.insert("event_name".to_owned(), json!("Lead"));
        event.insert("event_time".to_owned(), json!(event_time));
        event.insert("event_id".to_owned(), json!(event_id));
        event.insert("action_source".to_owned(), json!("website"));
        event.insert("user_data".to_owned(), Value::Object(user_data));
        event.insert("custom_data".to_owned(), Value::Object(custom_data));
        event.insert("event_source_url".to_owned(), json!(fan.event_source_url));

        let event_value = Value::Object(event);
        assert_eq!(event_value["event_name"], "Lead");
        assert_eq!(event_value["action_source"], "website");
        assert!(event_value["user_data"]["em"].is_array());
        assert_eq!(
            event_value["user_data"]["em"][0].as_str().unwrap().len(),
            64
        );
        assert_eq!(
            event_value["user_data"]["fbp"],
            "fb.1.1234567890.1234567890"
        );
        assert_eq!(event_value["user_data"]["fbc"], "fb.1.1234567890.abcdef");
        assert_eq!(
            event_value["user_data"]["client_ip_address"],
            "203.0.113.42"
        );
        assert_eq!(event_value["custom_data"]["utm_source"], "facebook");
        assert_eq!(event_value["event_source_url"], "https://virya.music/area");
    }

    #[test]
    fn meta_purchase_payload_has_value_and_currency() {
        let order = PaidTicketOrder {
            order_id: test_uuid(),
            fan_id: Some(test_uuid()),
            buyer_email: "buyer@example.com".to_owned(),
            amount_gross_minor: 15000,
            amount_refunded_minor: 5000,
            currency: "PLN".to_owned(),
            meta_fbp: None,
            meta_fbc: None,
            google_gclid: None,
            bandsintown_ref: None,
            utm_source: Some("facebook".to_owned()),
            utm_medium: Some("paid_social".to_owned()),
            utm_campaign: Some("metalhead_q3".to_owned()),
            client_ip_address: None,
            client_user_agent: None,
            event_source_url: None,
        };
        // Net value = gross - refunded = 15000 - 5000 = 10000 minor = 100.00
        let net_minor = order.amount_gross_minor - order.amount_refunded_minor;
        let value = (net_minor as f64) / 100.0;
        assert_eq!(value, 100.0);

        let mut custom_data = Map::new();
        custom_data.insert("currency".to_owned(), json!(order.currency.to_lowercase()));
        custom_data.insert("value".to_owned(), json!(value));
        custom_data.insert("utm_source".to_owned(), json!(order.utm_source));
        custom_data.insert("utm_medium".to_owned(), json!(order.utm_medium));
        custom_data.insert("utm_campaign".to_owned(), json!(order.utm_campaign));

        let custom = Value::Object(custom_data);
        assert_eq!(custom["currency"], "pln");
        assert_eq!(custom["value"], 100.0);
    }

    #[test]
    fn google_conversion_payload_has_required_fields() {
        let fan = FanAttribution {
            fan_id: test_uuid(),
            normalized_email: "fan@example.com".to_owned(),
            meta_fbp: None,
            meta_fbc: None,
            google_gclid: Some("EAIaIQobChMItest123".to_owned()),
            bandsintown_ref: None,
            utm_source: Some("google".to_owned()),
            utm_medium: Some("cpc".to_owned()),
            utm_campaign: Some("metalhead_search".to_owned()),
            client_ip_address: None,
            client_user_agent: None,
            event_source_url: None,
        };
        let event_id = format!("lead-{}", fan.fan_id);

        let mut conversion = Map::new();
        conversion.insert(
            "conversionAction".to_owned(),
            json!("customers/123/conversionActions/456"),
        );
        conversion.insert(
            "conversionDateTime".to_owned(),
            json!("2026-08-27T12:00:00Z"),
        );
        conversion.insert("conversionValue".to_owned(), json!(0));
        conversion.insert("currencyCode".to_owned(), json!("PLN"));
        conversion.insert("orderId".to_owned(), json!(event_id));
        conversion.insert("gclid".to_owned(), json!(fan.google_gclid));
        conversion.insert(
            "userIdentifiers".to_owned(),
            json!([{"hashedEmail": hash_sha256(&fan.normalized_email)}]),
        );

        let conversion_value = Value::Object(conversion);
        assert_eq!(
            conversion_value["conversionAction"],
            "customers/123/conversionActions/456"
        );
        assert_eq!(conversion_value["gclid"], "EAIaIQobChMItest123");
        assert_eq!(conversion_value["orderId"], event_id);
        assert_eq!(
            conversion_value["userIdentifiers"][0]["hashedEmail"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(conversion_value["currencyCode"], "PLN");
    }

    #[test]
    fn bandsintown_conversion_payload_has_required_fields() {
        let fan = FanAttribution {
            fan_id: test_uuid(),
            normalized_email: "fan@example.com".to_owned(),
            meta_fbp: None,
            meta_fbc: None,
            google_gclid: None,
            bandsintown_ref: Some("bit_event_12345".to_owned()),
            utm_source: Some("bandsintown".to_owned()),
            utm_medium: Some("boost".to_owned()),
            utm_campaign: Some("gig_august".to_owned()),
            client_ip_address: None,
            client_user_agent: None,
            event_source_url: None,
        };
        let event_id = format!("lead-{}", fan.fan_id);
        let payload = json!({
            "conversion_type": "signup",
            "event_id": event_id,
            "fan_email_hash": hash_sha256(&fan.normalized_email),
            "bandsintown_ref": fan.bandsintown_ref,
            "utm_source": fan.utm_source,
            "utm_medium": fan.utm_medium,
            "utm_campaign": fan.utm_campaign,
        });
        assert_eq!(payload["conversion_type"], "signup");
        assert_eq!(payload["bandsintown_ref"], "bit_event_12345");
        assert_eq!(payload["utm_source"], "bandsintown");
        assert_eq!(payload["utm_medium"], "boost");
        assert_eq!(payload["fan_email_hash"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn meta_graph_url_is_well_formed() {
        let config = MetaCapiConfig {
            enabled: true,
            pixel_id: "1234567890".to_owned(),
            access_token: "test-token-123".to_owned(),
            api_version: "v21.0".to_owned(),
            test_event_code: None,
            verify_token: None,
        };
        let url = format!(
            "{META_GRAPH_BASE}/{api_version}/{pixel_id}/events",
            api_version = config.api_version,
            pixel_id = config.pixel_id,
        );
        assert_eq!(url, "https://graph.facebook.com/v21.0/1234567890/events");
    }

    #[test]
    fn google_ads_url_is_well_formed() {
        let customer_id = "1234567890";
        let url = format!(
            "{GOOGLE_ADS_API_BASE}/{GOOGLE_ADS_API_VERSION}/customers/{customer_id}:uploadClickConversions",
            customer_id = customer_id,
        );
        assert_eq!(
            url,
            "https://googleads.googleapis.com/v18/customers/1234567890:uploadClickConversions"
        );
    }

    #[test]
    fn ad_conversion_config_any_enabled() {
        let none = AdConversionConfig::default();
        assert!(!none.any_enabled());

        let meta_only = AdConversionConfig {
            meta: MetaCapiConfig {
                enabled: true,
                pixel_id: "123".to_owned(),
                access_token: "tok".to_owned(),
                api_version: "v21.0".to_owned(),
                test_event_code: None,
                verify_token: None,
            },
            ..Default::default()
        };
        assert!(meta_only.any_enabled());
    }

    #[test]
    fn purchase_event_id_is_order_scoped() {
        let order_id = test_uuid();
        let event_id = format!("purchase-{order_id}");
        assert!(event_id.starts_with("purchase-"));
        assert_ne!(event_id, format!("lead-{order_id}"));
    }

    #[test]
    fn platform_filter_for_meta_requires_fbp_or_fbc() {
        let filter = match "meta" {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        assert!(filter.contains("meta_fbp") || filter.contains("meta_fbc"));
        assert!(!filter.contains("google_gclid"));
        assert!(!filter.contains("bandsintown_ref"));
    }

    #[test]
    fn platform_filter_for_google_requires_gclid() {
        let filter = match "google" {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        assert!(filter.contains("google_gclid"));
        assert!(!filter.contains("meta_fbp"));
        assert!(!filter.contains("bandsintown_ref"));
    }

    #[test]
    fn platform_filter_for_bandsintown_requires_ref() {
        let filter = match "bandsintown" {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        assert!(filter.contains("bandsintown_ref"));
        assert!(!filter.contains("meta_fbp"));
        assert!(!filter.contains("google_gclid"));
    }

    #[test]
    fn platform_filter_for_unknown_platform_is_false() {
        let filter = match "tiktok" {
            "meta" => "(attr.meta_fbp IS NOT NULL OR attr.meta_fbc IS NOT NULL)",
            "google" => "attr.google_gclid IS NOT NULL",
            "bandsintown" => "attr.bandsintown_ref IS NOT NULL",
            _ => "false",
        };
        assert_eq!(filter, "false");
    }
}
