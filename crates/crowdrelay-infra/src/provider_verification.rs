//! Creation-time provider identity verification.
//!
//! When a new fanbase connection is created, the API handler calls a
//! `ProviderVerifier` to probe the provider and confirm the external
//! identity exists. The probe result is returned to the operator as a
//! creation-time diagnostic — it is NOT persisted to the database and is
//! NOT a durable health state.
//!
//! Three outcomes:
//! - `Verified` — the provider confirmed the identity exists.
//! - `Invalid` — the provider proved the identity is wrong (e.g. 404).
//!   The connection is persisted with `status = 'invalid'` so the sync
//!   worker skips it.
//! - `Unavailable` — could not establish identity (network error, rate
//!   limit, missing credential). The connection is persisted with
//!   `status = 'connected'` — we don't know yet.
//!
//! All probes have a 5-second timeout. If the probe times out, the result
//! is `Unavailable`. The POST endpoint never hangs.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

/// Probe timeout for all provider verifiers.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

// Every `reqwest::Error` that reaches a `reason` string is stripped with
// `without_url()` first. YouTube carries its API key and the Graph API carries
// the page access token as query parameters, and `reason` is both logged and
// returned to the caller in the creation response — so an un-stripped decode
// error would publish the credential twice over.

/// Result of a creation-time provider probe.
///
/// `verified` describes the creation-time probe result, NOT durable
/// provider health. A connection verified at 11:00 can be broken at 12:00
/// when a token expires.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationResult {
    /// Provider confirmed the identity exists.
    Verified {
        #[serde(skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// Provider proved the identity is wrong (e.g. 404, empty items).
    /// The connection is persisted with `status = 'invalid'`, NOT
    /// `'connected'`.
    Invalid { reason: String },
    /// Could not establish identity (network error, rate limit, missing
    /// credential, timeout). The connection is persisted with
    /// `status = 'connected'` — we don't know yet.
    Unavailable { reason: String },
}

impl VerificationResult {
    /// Returns `true` if the probe confirmed the identity (Verified).
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    /// Returns `true` if the provider proved the identity is wrong (Invalid).
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid { .. })
    }
}

/// Verifies that an external account identity exists at the provider.
///
/// Implementations live in this module; the API handler calls the trait.
/// The handler never talks to YouTube/Facebook/Reddit directly — that
/// would turn `connections_simple.rs` into a mini integration layer.
#[async_trait]
pub trait ProviderVerifier: Send + Sync {
    /// Probes the provider to confirm the external identity exists.
    async fn verify(&self, account_id: &str) -> VerificationResult;
}

/// A no-op verifier that always returns `Unavailable`. Used when no
/// process-level credential is configured for a provider.
pub struct NoCredentialVerifier;

#[async_trait]
impl ProviderVerifier for NoCredentialVerifier {
    async fn verify(&self, _account_id: &str) -> VerificationResult {
        VerificationResult::Unavailable {
            reason: "credential not configured".to_owned(),
        }
    }
}

// --- YouTube ---

/// Verifies a YouTube channel ID via the Data API v3.
pub struct YoutubeVerifier {
    api_key: String,
    http_client: reqwest::Client,
}

impl YoutubeVerifier {
    #[must_use]
    pub fn new(api_key: String, _http_client: reqwest::Client) -> Self {
        Self {
            api_key,
            http_client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl ProviderVerifier for YoutubeVerifier {
    async fn verify(&self, channel_id: &str) -> VerificationResult {
        let url = format!(
            "https://www.googleapis.com/youtube/v3/channels?part=snippet&id={channel_id}&key={}",
            self.api_key
        );
        let response = match self.http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("network error: {}", e.without_url()),
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 404 || status.as_u16() == 400 {
            return VerificationResult::Invalid {
                reason: format!("YouTube API returned HTTP {status}"),
            };
        }
        if !status.is_success() {
            return VerificationResult::Unavailable {
                reason: format!("YouTube API returned HTTP {status}"),
            };
        }
        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("response parse error: {}", e.without_url()),
                };
            }
        };
        let items = body.get("items").and_then(|v| v.as_array());
        match items {
            Some(arr) if !arr.is_empty() => {
                let display_name = arr
                    .first()
                    .and_then(|item| item.get("snippet"))
                    .and_then(|s| s.get("title"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_owned());
                VerificationResult::Verified { display_name }
            }
            _ => VerificationResult::Invalid {
                reason: "YouTube API returned no items for this channel ID".to_owned(),
            },
        }
    }
}

// --- Facebook ---

/// Verifies a Facebook Page ID via the Graph API.
pub struct FacebookVerifier {
    access_token: String,
    http_client: reqwest::Client,
}

impl FacebookVerifier {
    #[must_use]
    pub fn new(access_token: String, _http_client: reqwest::Client) -> Self {
        Self {
            access_token,
            http_client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl ProviderVerifier for FacebookVerifier {
    async fn verify(&self, page_id: &str) -> VerificationResult {
        let url = format!(
            "https://graph.facebook.com/v21.0/{page_id}?fields=name&access_token={}",
            self.access_token
        );
        let response = match self.http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("network error: {}", e.without_url()),
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 404 {
            return VerificationResult::Invalid {
                reason: "Facebook Graph API returned 404 — page does not exist".to_owned(),
            };
        }
        if !status.is_success() {
            return VerificationResult::Unavailable {
                reason: format!("Facebook Graph API returned HTTP {status}"),
            };
        }
        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("response parse error: {}", e.without_url()),
                };
            }
        };
        // Facebook returns {"error": {...}} for non-existent pages on some
        // code paths, even with a 200.
        if let Some(error) = body.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            if code == 100 || code == 803 {
                return VerificationResult::Invalid {
                    reason: "Facebook Graph API error — page does not exist".to_owned(),
                };
            }
            return VerificationResult::Unavailable {
                reason: format!("Facebook Graph API error (code {code})"),
            };
        }
        let display_name = body
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_owned());
        VerificationResult::Verified { display_name }
    }
}

// --- Instagram ---

/// Verifies an Instagram Business account ID via the Graph API.
/// Uses the same Facebook Page access token — the IG Business account is
/// linked to the Facebook Page.
pub struct InstagramVerifier {
    access_token: String,
    http_client: reqwest::Client,
}

impl InstagramVerifier {
    #[must_use]
    pub fn new(access_token: String, _http_client: reqwest::Client) -> Self {
        Self {
            access_token,
            http_client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl ProviderVerifier for InstagramVerifier {
    async fn verify(&self, ig_user_id: &str) -> VerificationResult {
        let url = format!(
            "https://graph.facebook.com/v21.0/{ig_user_id}?fields=username&access_token={}",
            self.access_token
        );
        let response = match self.http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("network error: {}", e.without_url()),
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 404 {
            return VerificationResult::Invalid {
                reason: "Instagram Graph API returned 404 — account does not exist".to_owned(),
            };
        }
        if !status.is_success() {
            return VerificationResult::Unavailable {
                reason: format!("Instagram Graph API returned HTTP {status}"),
            };
        }
        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("response parse error: {}", e.without_url()),
                };
            }
        };
        if let Some(error) = body.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            if code == 100 || code == 803 {
                return VerificationResult::Invalid {
                    reason: "Instagram Graph API error — account does not exist".to_owned(),
                };
            }
            return VerificationResult::Unavailable {
                reason: format!("Instagram Graph API error (code {code})"),
            };
        }
        let display_name = body
            .get("username")
            .and_then(|n| n.as_str())
            .map(|s| s.to_owned());
        VerificationResult::Verified { display_name }
    }
}

// --- SoundCloud ---

/// Verifies a SoundCloud permalink by fetching the public artist page and
/// extracting the embedded user data. No API key needed.
pub struct SoundcloudVerifier {
    http_client: reqwest::Client,
}

impl SoundcloudVerifier {
    #[must_use]
    pub fn new(_http_client: reqwest::Client) -> Self {
        Self {
            http_client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl ProviderVerifier for SoundcloudVerifier {
    async fn verify(&self, permalink: &str) -> VerificationResult {
        let url = format!("https://soundcloud.com/{permalink}");
        let response = match self.http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("network error: {}", e.without_url()),
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 404 {
            return VerificationResult::Invalid {
                reason: "SoundCloud returned 404 — permalink does not exist".to_owned(),
            };
        }
        if !status.is_success() {
            return VerificationResult::Unavailable {
                reason: format!("SoundCloud returned HTTP {status}"),
            };
        }
        let html = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("response read error: {}", e.without_url()),
                };
            }
        };
        // Extract the username from the hydration data.
        let marker = "window.__sc_hydration = ";
        let Some(start) = html.find(marker) else {
            return VerificationResult::Unavailable {
                reason: "could not find hydration data in SoundCloud page".to_owned(),
            };
        };
        // Use `.get()` to avoid UTF-8 boundary panics — `marker` is ASCII
        // and `find` returns a byte offset, but clippy enforces safe slicing.
        let rest_bytes = html.as_bytes().get(start + marker.len()..).unwrap_or(&[]);
        let rest_str = std::str::from_utf8(rest_bytes).unwrap_or("");
        let Some(end) = rest_str.find(";</script>") else {
            return VerificationResult::Unavailable {
                reason: "could not parse hydration data in SoundCloud page".to_owned(),
            };
        };
        let json_str = rest_str.get(..end).unwrap_or("");
        let hydration: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("hydration JSON parse error: {e}"),
                };
            }
        };
        for entry in &hydration {
            if entry.get("hydratable").and_then(|v| v.as_str()) == Some("user")
                && let Some(data) = entry.get("data")
            {
                let display_name = data
                    .get("username")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_owned());
                return VerificationResult::Verified { display_name };
            }
        }
        VerificationResult::Unavailable {
            reason: "could not find user entry in SoundCloud hydration data".to_owned(),
        }
    }
}

// --- Reddit ---

/// Verifies a subreddit name by fetching `about.json`. No API key needed,
/// but may use a proxy if the direct connection is blocked.
pub struct RedditVerifier {
    http_client: reqwest::Client,
    proxy_url: Option<String>,
}

impl RedditVerifier {
    #[must_use]
    pub fn new(_http_client: reqwest::Client, proxy_url: Option<String>) -> Self {
        Self {
            http_client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            proxy_url,
        }
    }

    fn build_proxied_client(&self, proxy_url: &str) -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .proxy(reqwest::Proxy::all(proxy_url)?)
            .build()
    }
}

#[async_trait]
impl ProviderVerifier for RedditVerifier {
    async fn verify(&self, subreddit: &str) -> VerificationResult {
        let url = format!("https://www.reddit.com/r/{subreddit}/about.json");

        // Try proxy first if configured, then direct.
        if let Some(ref proxy_url) = self.proxy_url
            && let Ok(client) = self.build_proxied_client(proxy_url)
            && let Ok(response) = client.get(&url).send().await
            && let Some(result) = classify_reddit_response(response, subreddit).await
        {
            return result;
        }

        // Direct connection (last resort — may be blocked from datacenter IPs).
        let response = match self.http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return VerificationResult::Unavailable {
                    reason: format!("network error: {}", e.without_url()),
                };
            }
        };
        classify_reddit_response(response, subreddit)
            .await
            .unwrap_or(VerificationResult::Unavailable {
                reason: "could not parse Reddit response".to_owned(),
            })
    }
}

/// Classifies a Reddit `about.json` response. Returns `None` to signal the
/// caller should try the next strategy (e.g. direct after proxy).
async fn classify_reddit_response(
    response: reqwest::Response,
    subreddit: &str,
) -> Option<VerificationResult> {
    let status = response.status();
    if status.as_u16() == 404 || status.as_u16() == 403 {
        // 403 can mean the subreddit is banned/private. We treat both as
        // Invalid for the purpose of connection creation — the identity
        // is not publicly accessible.
        return Some(VerificationResult::Invalid {
            reason: format!("Reddit returned HTTP {status} for r/{subreddit}"),
        });
    }
    if !status.is_success() {
        return Some(VerificationResult::Unavailable {
            reason: format!("Reddit returned HTTP {status}"),
        });
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let data = body.get("data")?;
    let display_name = data
        .get("display_name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_owned());
    // If subscribers field exists, the subreddit is real and accessible.
    if data.get("subscribers").is_some() {
        Some(VerificationResult::Verified { display_name })
    } else {
        Some(VerificationResult::Unavailable {
            reason: "Reddit response missing subscribers field".to_owned(),
        })
    }
}

/// A bundle of provider verifiers, one per platform. Built once at startup
/// and injected into `AppState`.
#[derive(Clone)]
pub struct ProviderVerifiers {
    pub youtube: Option<Arc<dyn ProviderVerifier>>,
    pub facebook: Option<Arc<dyn ProviderVerifier>>,
    pub instagram: Option<Arc<dyn ProviderVerifier>>,
    pub soundcloud: Arc<dyn ProviderVerifier>,
    pub reddit: Arc<dyn ProviderVerifier>,
}

use std::sync::Arc;

impl ProviderVerifiers {
    /// Builds the verifier bundle from process-level credentials.
    ///
    /// If a credential is not configured, the corresponding verifier is
    /// `None` (for credentialled platforms) or a `NoCredentialVerifier`
    /// wrapped in `Arc` (for platforms that need no credential but still
    /// want a consistent interface).
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        youtube_api_key: Option<String>,
        facebook_page_access_token: Option<String>,
        reddit_proxy_url: Option<String>,
        http_client: reqwest::Client,
    ) -> Self {
        let youtube = youtube_api_key.map(|key| {
            Arc::new(YoutubeVerifier::new(key, http_client.clone())) as Arc<dyn ProviderVerifier>
        });
        let facebook = facebook_page_access_token.clone().map(|token| {
            Arc::new(FacebookVerifier::new(token, http_client.clone())) as Arc<dyn ProviderVerifier>
        });
        let instagram = facebook_page_access_token.map(|token| {
            Arc::new(InstagramVerifier::new(token, http_client.clone()))
                as Arc<dyn ProviderVerifier>
        });
        let soundcloud =
            Arc::new(SoundcloudVerifier::new(http_client.clone())) as Arc<dyn ProviderVerifier>;
        let reddit = Arc::new(RedditVerifier::new(http_client, reddit_proxy_url))
            as Arc<dyn ProviderVerifier>;
        Self {
            youtube,
            facebook,
            instagram,
            soundcloud,
            reddit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_credential_verifier_returns_unavailable() {
        let verifier = NoCredentialVerifier;
        let result = verifier.verify("test").await;
        assert!(matches!(result, VerificationResult::Unavailable { .. }));
    }

    #[test]
    fn verification_result_is_verified() {
        let result = VerificationResult::Verified {
            display_name: Some("test".to_owned()),
        };
        assert!(result.is_verified());
        assert!(!result.is_invalid());
    }

    #[test]
    fn verification_result_is_invalid() {
        let result = VerificationResult::Invalid {
            reason: "not found".to_owned(),
        };
        assert!(!result.is_verified());
        assert!(result.is_invalid());
    }
}
