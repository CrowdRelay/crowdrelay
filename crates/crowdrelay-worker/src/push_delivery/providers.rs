use std::{env, fs::File, io::Read, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use super::{crypto, repository::ClaimedDelivery};

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const MAX_SERVICE_ACCOUNT_BYTES: u64 = 128 * 1024;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub fcm_project_id: Option<String>,
    pub fcm_service_account_file: Option<String>,
    pub vapid_private_key: Option<String>,
    pub vapid_public_key: Option<String>,
    pub vapid_subject: Option<String>,
}

impl ProviderConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            fcm_project_id: optional_env("CROWDRELAY_FCM_PROJECT_ID"),
            fcm_service_account_file: optional_env("CROWDRELAY_FCM_SERVICE_ACCOUNT_FILE"),
            vapid_private_key: optional_env("CROWDRELAY_WEB_PUSH_VAPID_PRIVATE_KEY"),
            vapid_public_key: optional_env("CROWDRELAY_WEB_PUSH_VAPID_PUBLIC_KEY"),
            vapid_subject: optional_env("CROWDRELAY_WEB_PUSH_SUBJECT"),
        };
        if config.fcm_project_id.is_some() != config.fcm_service_account_file.is_some() {
            bail!(
                "FCM push requires both CROWDRELAY_FCM_PROJECT_ID and CROWDRELAY_FCM_SERVICE_ACCOUNT_FILE"
            );
        }
        let web_values = [
            config.vapid_private_key.is_some(),
            config.vapid_public_key.is_some(),
            config.vapid_subject.is_some(),
        ];
        if web_values.iter().any(|value| *value) && !web_values.iter().all(|value| *value) {
            bail!("Web Push requires private key, public key and subject together");
        }
        if let Some(subject) = config.vapid_subject.as_deref() {
            ensure!(
                subject.starts_with("mailto:") || subject.starts_with("https://"),
                "CROWDRELAY_WEB_PUSH_SUBJECT must be mailto: or https://"
            );
            ensure!(subject.len() <= 240, "Web Push subject is too long");
        }
        Ok(config)
    }
}

#[derive(Debug, Clone)]
pub struct PushPayload<'a> {
    pub delivery_id: Uuid,
    pub ack_token: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub target_path: &'a str,
    pub collapse_key: Option<&'a str>,
}

impl<'a> PushPayload<'a> {
    pub fn from_delivery(delivery: &'a ClaimedDelivery, ack_token: &'a str) -> Self {
        Self {
            delivery_id: delivery.id,
            ack_token,
            title: &delivery.title,
            body: &delivery.body,
            target_path: &delivery.target_path,
            collapse_key: delivery.collapse_key.as_deref(),
        }
    }
}

#[derive(Debug)]
pub enum ProviderOutcome {
    Accepted {
        reference: Option<String>,
    },
    Retry {
        code: &'static str,
    },
    Failed {
        code: &'static str,
        invalidate_endpoint: bool,
    },
    Ambiguous {
        code: &'static str,
    },
}

#[derive(Debug, Deserialize)]
struct ServiceAccount {
    project_id: String,
    client_email: String,
    private_key: String,
    token_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct FcmSuccess {
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct ServiceAccountClaims<'a> {
    iss: &'a str,
    scope: &'static str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: i64,
}

#[derive(Debug)]
enum TokenError {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

pub struct PushProviders {
    client: Client,
    fcm: Option<FcmProvider>,
    web_push: Option<WebPushProvider>,
}

struct FcmProvider {
    project_id: String,
    service_account: ServiceAccount,
    /// Shared token cache so concurrent sends can take `&self`. The async
    /// mutex is held ACROSS the refresh, making the first expired-token sender
    /// single-flight: the rest of the batch awaits it and then reuses the
    /// fresh token instead of each minting its own JWT and OAuth round trip.
    cached_token: tokio::sync::Mutex<Option<CachedToken>>,
}

struct WebPushProvider {
    private_key: String,
    public_key: String,
    subject: String,
}

impl PushProviders {
    pub fn new(config: ProviderConfig, operation_timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
            .timeout(operation_timeout)
            .user_agent("CrowdRelay/1.0 fan-push")
            .build()
            .context("build push provider HTTP client")?;

        let fcm = match (config.fcm_project_id, config.fcm_service_account_file) {
            (Some(project_id), Some(path)) => {
                let service_account = load_service_account(&path)?;
                ensure!(
                    service_account.project_id == project_id,
                    "FCM project id does not match service-account project_id"
                );
                Some(FcmProvider {
                    project_id,
                    service_account,
                    cached_token: tokio::sync::Mutex::new(None),
                })
            }
            (None, None) => None,
            _ => bail!("incomplete FCM provider configuration"),
        };
        let web_push = match (
            config.vapid_private_key,
            config.vapid_public_key,
            config.vapid_subject,
        ) {
            (Some(private_key), Some(public_key), Some(subject)) => Some(WebPushProvider {
                private_key,
                public_key,
                subject,
            }),
            (None, None, None) => None,
            _ => bail!("incomplete Web Push provider configuration"),
        };
        ensure!(
            fcm.is_some() || web_push.is_some(),
            "push runtime enabled but no provider is configured"
        );
        Ok(Self {
            client,
            fcm,
            web_push,
        })
    }

    pub async fn send(
        &self,
        delivery: &ClaimedDelivery,
        payload: &PushPayload<'_>,
    ) -> ProviderOutcome {
        match delivery.transport.as_str() {
            "android_fcm" => self.send_fcm(delivery, payload).await,
            "web_push" => self.send_web_push(delivery, payload).await,
            _ => ProviderOutcome::Failed {
                code: "unsupported_push_transport",
                invalidate_endpoint: true,
            },
        }
    }

    async fn send_fcm(
        &self,
        delivery: &ClaimedDelivery,
        payload: &PushPayload<'_>,
    ) -> ProviderOutcome {
        let Some(provider) = self.fcm.as_ref() else {
            return ProviderOutcome::Failed {
                code: "fcm_not_configured",
                invalidate_endpoint: false,
            };
        };
        let access_token = match provider.access_token(&self.client).await {
            Ok(value) => value,
            Err(TokenError::Retry(error)) => {
                tracing::warn!(%error, "FCM OAuth token refresh transiently failed");
                return ProviderOutcome::Retry {
                    code: "fcm_oauth_retry",
                };
            }
            Err(TokenError::Fatal(error)) => {
                tracing::error!(%error, "FCM OAuth token refresh failed closed");
                return ProviderOutcome::Failed {
                    code: "fcm_oauth_failed",
                    invalidate_endpoint: false,
                };
            }
        };
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            provider.project_id
        );
        let collapse_key = payload.collapse_key.unwrap_or(&provider.project_id);
        let message = serde_json::json!({
            "message": {
                "token": delivery.endpoint_address,
                "data": {
                    "delivery_id": payload.delivery_id.to_string(),
                    "ack_token": payload.ack_token,
                    "title": payload.title,
                    "body": payload.body,
                    "target_path": payload.target_path,
                    "collapse_key": collapse_key,
                },
                "android": {
                    "priority": "high",
                    "collapse_key": collapse_key,
                    "ttl": "900s"
                }
            }
        });
        let result = self
            .client
            .post(url)
            .bearer_auth(access_token)
            .json(&message)
            .send()
            .await;
        classify_fcm_response(result).await
    }

    async fn send_web_push(
        &self,
        delivery: &ClaimedDelivery,
        payload: &PushPayload<'_>,
    ) -> ProviderOutcome {
        let Some(provider) = self.web_push.as_ref() else {
            return ProviderOutcome::Failed {
                code: "web_push_not_configured",
                invalidate_endpoint: false,
            };
        };
        let endpoint = match Url::parse(&delivery.endpoint_address) {
            Ok(value) if valid_push_origin(&value) => value,
            _ => {
                return ProviderOutcome::Failed {
                    code: "web_push_endpoint_invalid",
                    invalidate_endpoint: true,
                };
            }
        };
        let Some(p256dh) = delivery.p256dh.as_deref() else {
            return ProviderOutcome::Failed {
                code: "web_push_p256dh_missing",
                invalidate_endpoint: true,
            };
        };
        let Some(auth) = delivery.auth_secret.as_deref() else {
            return ProviderOutcome::Failed {
                code: "web_push_auth_missing",
                invalidate_endpoint: true,
            };
        };
        let audience = endpoint.origin().ascii_serialization();
        let serialized = match serde_json::to_vec(&serde_json::json!({
            "delivery_id": payload.delivery_id,
            "ack_token": payload.ack_token,
            "title": payload.title,
            "body": payload.body,
            "target_path": payload.target_path,
            "collapse_key": payload.collapse_key,
        })) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(%error, "could not serialize Web Push payload");
                return ProviderOutcome::Failed {
                    code: "web_push_payload_invalid",
                    invalidate_endpoint: false,
                };
            }
        };
        let envelope = match crypto::web_push_envelope(
            &serialized,
            p256dh,
            auth,
            &provider.private_key,
            &provider.public_key,
            &audience,
            &provider.subject,
        ) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, endpoint_id = %delivery.endpoint_id, "could not encrypt Web Push delivery");
                return ProviderOutcome::Failed {
                    code: "web_push_crypto_invalid",
                    invalidate_endpoint: true,
                };
            }
        };
        let result = self
            .client
            .post(endpoint)
            .header("Authorization", envelope.authorization)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "900")
            .body(envelope.body)
            .send()
            .await;
        classify_web_push_response(result).await
    }
}

impl FcmProvider {
    async fn access_token(&self, client: &Client) -> Result<String, TokenError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut cached = self.cached_token.lock().await;
        if let Some(token) = cached.as_ref()
            && token.expires_at > now.saturating_add(90)
        {
            return Ok(token.value.clone());
        }
        let token_uri = self
            .service_account
            .token_uri
            .as_deref()
            .unwrap_or(GOOGLE_TOKEN_URI);
        if token_uri != GOOGLE_TOKEN_URI {
            return Err(TokenError::Fatal(anyhow!("unexpected FCM OAuth token URI")));
        }
        let claims = ServiceAccountClaims {
            iss: &self.service_account.client_email,
            scope: FCM_SCOPE,
            aud: token_uri,
            iat: now,
            exp: now.saturating_add(3600),
        };
        let assertion = crypto::rsa_jwt(&self.service_account.private_key, &claims)
            .map_err(TokenError::Fatal)?;
        let response = client
            .post(token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() {
                    TokenError::Retry(error.into())
                } else {
                    TokenError::Fatal(error.into())
                }
            })?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status().is_server_error()
        {
            return Err(TokenError::Retry(anyhow!(
                "FCM OAuth returned {}",
                response.status()
            )));
        }
        if !response.status().is_success() {
            return Err(TokenError::Fatal(anyhow!(
                "FCM OAuth returned {}",
                response.status()
            )));
        }
        let body = read_limited_provider_response(response)
            .await
            .map_err(TokenError::Fatal)?;
        let token: OAuthTokenResponse =
            serde_json::from_slice(&body).map_err(|error| TokenError::Fatal(error.into()))?;
        if token.access_token.trim().is_empty() || !(60..=7200).contains(&token.expires_in) {
            return Err(TokenError::Fatal(anyhow!(
                "FCM OAuth returned invalid token metadata"
            )));
        }
        cached.replace(CachedToken {
            value: token.access_token.clone(),
            expires_at: now.saturating_add(token.expires_in),
        });
        Ok(token.access_token)
    }
}

async fn read_limited_provider_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        bail!("push provider response exceeds size limit");
    }

    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(4 * 1024)
        .min(MAX_PROVIDER_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read push provider response")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            bail!("push provider response exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn classify_fcm_response(
    result: Result<reqwest::Response, reqwest::Error>,
) -> ProviderOutcome {
    let response = match result {
        Ok(value) => value,
        Err(error) if error.is_connect() => {
            return ProviderOutcome::Retry {
                code: "fcm_connect_failed",
            };
        }
        Err(error) => {
            tracing::warn!(%error, "FCM request outcome is ambiguous");
            return ProviderOutcome::Ambiguous {
                code: "fcm_transport_ambiguous",
            };
        }
    };
    let status = response.status();
    if status.is_success() {
        let reference = read_limited_provider_response(response)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<FcmSuccess>(&body).ok())
            .and_then(|value| value.name)
            .filter(|value| value.len() <= 240);
        return ProviderOutcome::Accepted { reference };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ProviderOutcome::Retry {
            code: "fcm_rate_limited",
        };
    }
    let body = read_limited_provider_response(response)
        .await
        .ok()
        .and_then(|body| String::from_utf8(body).ok())
        .unwrap_or_default();
    let invalid_endpoint = status == StatusCode::NOT_FOUND
        || body.contains("UNREGISTERED")
        || body.contains("registration-token-not-registered");
    if invalid_endpoint {
        return ProviderOutcome::Failed {
            code: "fcm_endpoint_invalid",
            invalidate_endpoint: true,
        };
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ProviderOutcome::Failed {
            code: "fcm_provider_unauthorized",
            invalidate_endpoint: false,
        };
    }
    if status.is_server_error() {
        return ProviderOutcome::Ambiguous {
            code: "fcm_server_ambiguous",
        };
    }
    ProviderOutcome::Failed {
        code: "fcm_rejected",
        invalidate_endpoint: false,
    }
}

async fn classify_web_push_response(
    result: Result<reqwest::Response, reqwest::Error>,
) -> ProviderOutcome {
    let response = match result {
        Ok(value) => value,
        Err(error) if error.is_connect() => {
            return ProviderOutcome::Retry {
                code: "web_push_connect_failed",
            };
        }
        Err(error) => {
            tracing::warn!(%error, "Web Push request outcome is ambiguous");
            return ProviderOutcome::Ambiguous {
                code: "web_push_transport_ambiguous",
            };
        }
    };
    let status = response.status();
    if status.is_success() {
        let reference = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .filter(|value| value.len() <= 240);
        return ProviderOutcome::Accepted { reference };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ProviderOutcome::Retry {
            code: "web_push_rate_limited",
        };
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return ProviderOutcome::Failed {
            code: "web_push_endpoint_invalid",
            invalidate_endpoint: true,
        };
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ProviderOutcome::Failed {
            code: "web_push_provider_unauthorized",
            invalidate_endpoint: false,
        };
    }
    if status.is_server_error() {
        return ProviderOutcome::Ambiguous {
            code: "web_push_server_ambiguous",
        };
    }
    ProviderOutcome::Failed {
        code: "web_push_rejected",
        invalidate_endpoint: false,
    }
}

fn valid_push_origin(endpoint: &Url) -> bool {
    if endpoint.scheme() != "https"
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.port().is_some()
    {
        return false;
    }
    let Some(host) = endpoint.host_str().map(|value| value.to_ascii_lowercase()) else {
        return false;
    };
    host == "fcm.googleapis.com"
        || host.ends_with(".fcm.googleapis.com")
        || host == "updates.push.services.mozilla.com"
        || host.ends_with(".push.services.mozilla.com")
        || host == "web.push.apple.com"
        || host.ends_with(".push.apple.com")
        || host.ends_with(".notify.windows.com")
}

fn load_service_account(path: &str) -> Result<ServiceAccount> {
    let file = File::open(path).with_context(|| format!("open FCM service account at {path}"))?;
    let mut document = Vec::new();
    file.take(MAX_SERVICE_ACCOUNT_BYTES.saturating_add(1))
        .read_to_end(&mut document)
        .with_context(|| format!("read FCM service account at {path}"))?;
    ensure!(
        u64::try_from(document.len()).unwrap_or(u64::MAX) <= MAX_SERVICE_ACCOUNT_BYTES,
        "FCM service-account file exceeds size limit"
    );
    let account: ServiceAccount =
        serde_json::from_slice(&document).context("FCM service-account JSON is invalid")?;
    ensure!(
        !account.project_id.trim().is_empty() && account.project_id.len() <= 120,
        "FCM service-account project_id is invalid"
    );
    ensure!(
        account.client_email.ends_with(".gserviceaccount.com") && account.client_email.len() <= 240,
        "FCM service-account client_email is invalid"
    );
    ensure!(
        account.private_key.contains("BEGIN PRIVATE KEY"),
        "FCM service-account private_key is not unencrypted PKCS#8 PEM"
    );
    Ok(account)
}

fn optional_env(name: &'static str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_provider_origins_are_allowlisted() {
        let mozilla = Url::parse("https://updates.push.services.mozilla.com/wpush/v2/a").ok();
        let local = Url::parse("https://127.0.0.1/push").ok();
        assert!(mozilla.as_ref().is_some_and(valid_push_origin));
        assert!(!local.as_ref().is_some_and(valid_push_origin));
    }
}
