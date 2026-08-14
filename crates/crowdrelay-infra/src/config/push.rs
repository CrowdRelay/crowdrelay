use std::collections::HashMap;

use super::{ConfigError, parse_bool};

pub(super) const PUSH_DELIVERY_ENABLED_KEY: &str = "CROWDRELAY_PUSH_DELIVERY_ENABLED";
pub(super) const WEB_PUSH_VAPID_PUBLIC_KEY: &str = "CROWDRELAY_WEB_PUSH_VAPID_PUBLIC_KEY";
pub(super) const FCM_PROJECT_ID_KEY: &str = "CROWDRELAY_FCM_PROJECT_ID";

/// Public/runtime push controls shared by API and worker. Provider secrets are worker-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPublicConfig {
    /// Process-level gate; the persisted `push_delivery_enabled` flag must also be true.
    pub runtime_enabled: bool,
    /// URL-safe unpadded P-256 public key exposed to browser PushManager clients.
    pub web_push_vapid_public_key: Option<String>,
    /// Firebase project identifier; non-secret and used only to expose Android availability.
    pub fcm_project_id: Option<String>,
}

impl PushPublicConfig {
    pub(super) fn parse(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let runtime_enabled = parse_bool(
            values.get(PUSH_DELIVERY_ENABLED_KEY),
            PUSH_DELIVERY_ENABLED_KEY,
            false,
        )?;
        let web_push_vapid_public_key = values
            .get(WEB_PUSH_VAPID_PUBLIC_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(value) = web_push_vapid_public_key.as_deref()
            && (!(40..=160).contains(&value.len())
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        {
            return Err(ConfigError::InvalidSecret {
                name: WEB_PUSH_VAPID_PUBLIC_KEY,
            });
        }
        let fcm_project_id = values
            .get(FCM_PROJECT_ID_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(value) = fcm_project_id.as_deref()
            && (value.len() > 120
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.')))
        {
            return Err(ConfigError::InvalidSecret {
                name: FCM_PROJECT_ID_KEY,
            });
        }
        Ok(Self {
            runtime_enabled,
            web_push_vapid_public_key,
            fcm_project_id,
        })
    }
}
