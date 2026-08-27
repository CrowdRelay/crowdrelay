use std::collections::HashMap;

use super::{ConfigError, parse_bool};

pub(super) const META_CAPI_ENABLED_KEY: &str = "CROWDRELAY_META_CAPI_ENABLED";
pub(super) const META_PIXEL_ID_KEY: &str = "CROWDRELAY_META_PIXEL_ID";
pub(super) const META_CAPI_ACCESS_TOKEN_KEY: &str = "CROWDRELAY_META_CAPI_ACCESS_TOKEN";
pub(super) const META_CAPI_API_VERSION_KEY: &str = "CROWDRELAY_META_CAPI_API_VERSION";
pub(super) const META_CAPI_TEST_EVENT_CODE_KEY: &str = "CROWDRELAY_META_CAPI_TEST_EVENT_CODE";
pub(super) const META_CAPI_VERIFY_TOKEN_KEY: &str = "CROWDRELAY_META_CAPI_VERIFY_TOKEN";

pub(super) const GOOGLE_ADS_ENABLED_KEY: &str = "CROWDRELAY_GOOGLE_ADS_ENABLED";
pub(super) const GOOGLE_ADS_CUSTOMER_ID_KEY: &str = "CROWDRELAY_GOOGLE_ADS_CUSTOMER_ID";
pub(super) const GOOGLE_ADS_DEVELOPER_TOKEN_KEY: &str = "CROWDRELAY_GOOGLE_ADS_DEVELOPER_TOKEN";
pub(super) const GOOGLE_ADS_REFRESH_TOKEN_KEY: &str = "CROWDRELAY_GOOGLE_ADS_REFRESH_TOKEN";
pub(super) const GOOGLE_ADS_CLIENT_ID_KEY: &str = "CROWDRELAY_GOOGLE_ADS_CLIENT_ID";
pub(super) const GOOGLE_ADS_CLIENT_SECRET_KEY: &str = "CROWDRELAY_GOOGLE_ADS_CLIENT_SECRET";
pub(super) const GOOGLE_ADS_CONVERSION_ACTION_ID_KEY: &str =
    "CROWDRELAY_GOOGLE_ADS_CONVERSION_ACTION_ID";

pub(super) const BANDSINTOWN_CONVERSION_ENABLED_KEY: &str =
    "CROWDRELAY_BANDSINTOWN_CONVERSION_ENABLED";
pub(super) const BANDSINTOWN_API_TOKEN_KEY: &str = "CROWDRELAY_BANDSINTOWN_API_TOKEN";

const DEFAULT_META_CAPI_API_VERSION: &str = "v21.0";

/// Meta Conversions API configuration. The access token is a long-lived
/// system-user token; it is never exposed to the browser. The Pixel ID is
/// public and also embedded in the browser Pixel snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaCapiConfig {
    /// Process-level gate; when false the worker stays dark.
    pub enabled: bool,
    /// Meta Pixel / Dataset ID (numeric string).
    pub pixel_id: String,
    /// Long-lived system-user access token for the Graph API.
    pub access_token: String,
    /// Graph API version, e.g. "v21.0".
    pub api_version: String,
    /// Optional test event code for Events Manager validation.
    pub test_event_code: Option<String>,
    /// Optional verify token for Lead Ads webhook subscription verification.
    pub verify_token: Option<String>,
}

impl MetaCapiConfig {
    pub(super) fn parse(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let enabled = parse_bool(
            values.get(META_CAPI_ENABLED_KEY),
            META_CAPI_ENABLED_KEY,
            false,
        )?;
        let pixel_id = optional_trimmed(values.get(META_PIXEL_ID_KEY));
        let access_token = optional_trimmed(values.get(META_CAPI_ACCESS_TOKEN_KEY));
        let api_version = values
            .get(META_CAPI_API_VERSION_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| DEFAULT_META_CAPI_API_VERSION.to_owned());
        if !api_version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            || !api_version.starts_with('v')
        {
            return Err(ConfigError::InvalidSecret {
                name: META_CAPI_API_VERSION_KEY,
            });
        }
        let test_event_code = optional_trimmed(values.get(META_CAPI_TEST_EVENT_CODE_KEY));
        let verify_token = optional_trimmed(values.get(META_CAPI_VERIFY_TOKEN_KEY));

        if enabled {
            let pixel_id = pixel_id.ok_or(ConfigError::Missing {
                name: META_PIXEL_ID_KEY,
            })?;
            if !pixel_id.bytes().all(|byte| byte.is_ascii_digit())
                || !(1..=32).contains(&pixel_id.len())
            {
                return Err(ConfigError::InvalidSecret {
                    name: META_PIXEL_ID_KEY,
                });
            }
            let access_token = access_token.ok_or(ConfigError::Missing {
                name: META_CAPI_ACCESS_TOKEN_KEY,
            })?;
            if !(32..=512).contains(&access_token.len())
                || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ConfigError::InvalidSecret {
                    name: META_CAPI_ACCESS_TOKEN_KEY,
                });
            }
            Ok(Self {
                enabled,
                pixel_id,
                access_token,
                api_version,
                test_event_code,
                verify_token,
            })
        } else {
            Ok(Self {
                enabled: false,
                pixel_id: String::new(),
                access_token: String::new(),
                api_version,
                test_event_code,
                verify_token,
            })
        }
    }
}

impl Default for MetaCapiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pixel_id: String::new(),
            access_token: String::new(),
            api_version: DEFAULT_META_CAPI_API_VERSION.to_owned(),
            test_event_code: None,
            verify_token: None,
        }
    }
}

/// Google Ads Enhanced Conversions configuration. The developer token and
/// OAuth refresh token are never exposed to the browser. The conversion
/// action ID identifies the "Fan signup" conversion action in your Google
/// Ads account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleAdsConfig {
    pub enabled: bool,
    /// Google Ads customer ID (numeric, no dashes, e.g. "1234567890").
    pub customer_id: String,
    /// Google Ads API developer token.
    pub developer_token: String,
    /// OAuth2 refresh token for the Google Ads API.
    pub refresh_token: String,
    /// OAuth2 client ID.
    pub client_id: String,
    /// OAuth2 client secret.
    pub client_secret: String,
    /// Conversion action resource name (e.g. "customers/123/conversionActions/456").
    pub conversion_action_id: String,
}

impl GoogleAdsConfig {
    pub(super) fn parse(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let enabled = parse_bool(
            values.get(GOOGLE_ADS_ENABLED_KEY),
            GOOGLE_ADS_ENABLED_KEY,
            false,
        )?;
        let customer_id = optional_trimmed(values.get(GOOGLE_ADS_CUSTOMER_ID_KEY));
        let developer_token = optional_trimmed(values.get(GOOGLE_ADS_DEVELOPER_TOKEN_KEY));
        let refresh_token = optional_trimmed(values.get(GOOGLE_ADS_REFRESH_TOKEN_KEY));
        let client_id = optional_trimmed(values.get(GOOGLE_ADS_CLIENT_ID_KEY));
        let client_secret = optional_trimmed(values.get(GOOGLE_ADS_CLIENT_SECRET_KEY));
        let conversion_action_id =
            optional_trimmed(values.get(GOOGLE_ADS_CONVERSION_ACTION_ID_KEY));

        if enabled {
            let customer_id = customer_id.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_CUSTOMER_ID_KEY,
            })?;
            if !customer_id.bytes().all(|byte| byte.is_ascii_digit())
                || !(8..=16).contains(&customer_id.len())
            {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_CUSTOMER_ID_KEY,
                });
            }
            let developer_token = developer_token.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_DEVELOPER_TOKEN_KEY,
            })?;
            if !(16..=256).contains(&developer_token.len())
                || !developer_token.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_DEVELOPER_TOKEN_KEY,
                });
            }
            let refresh_token = refresh_token.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_REFRESH_TOKEN_KEY,
            })?;
            if !(16..=512).contains(&refresh_token.len())
                || !refresh_token.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_REFRESH_TOKEN_KEY,
                });
            }
            let client_id = client_id.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_CLIENT_ID_KEY,
            })?;
            if !(16..=256).contains(&client_id.len()) {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_CLIENT_ID_KEY,
                });
            }
            let client_secret = client_secret.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_CLIENT_SECRET_KEY,
            })?;
            if !(16..=256).contains(&client_secret.len()) {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_CLIENT_SECRET_KEY,
                });
            }
            let conversion_action_id = conversion_action_id.ok_or(ConfigError::Missing {
                name: GOOGLE_ADS_CONVERSION_ACTION_ID_KEY,
            })?;
            if !(8..=256).contains(&conversion_action_id.len())
                || !conversion_action_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
            {
                return Err(ConfigError::InvalidSecret {
                    name: GOOGLE_ADS_CONVERSION_ACTION_ID_KEY,
                });
            }
            Ok(Self {
                enabled,
                customer_id,
                developer_token,
                refresh_token,
                client_id,
                client_secret,
                conversion_action_id,
            })
        } else {
            Ok(Self {
                enabled: false,
                customer_id: String::new(),
                developer_token: String::new(),
                refresh_token: String::new(),
                client_id: String::new(),
                client_secret: String::new(),
                conversion_action_id: String::new(),
            })
        }
    }
}

/// Bandsintown conversion tracking. Bandsintown does not have a server-side
/// conversion API like Meta or Google, but it supports:
/// 1. UTM/tracking parameters on event links back to your site
/// 2. A server-to-server tracking pixel callback for Boost campaigns
/// 3. Conversion attribution via the bandsintown_ref parameter
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BandsintownConversionConfig {
    pub enabled: bool,
    /// Bandsintown API token for server-to-server conversion callbacks.
    pub api_token: String,
}

impl BandsintownConversionConfig {
    pub(super) fn parse(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let enabled = parse_bool(
            values.get(BANDSINTOWN_CONVERSION_ENABLED_KEY),
            BANDSINTOWN_CONVERSION_ENABLED_KEY,
            false,
        )?;
        let api_token = optional_trimmed(values.get(BANDSINTOWN_API_TOKEN_KEY));

        if enabled {
            let api_token = api_token.ok_or(ConfigError::Missing {
                name: BANDSINTOWN_API_TOKEN_KEY,
            })?;
            if !(16..=256).contains(&api_token.len())
                || !api_token.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ConfigError::InvalidSecret {
                    name: BANDSINTOWN_API_TOKEN_KEY,
                });
            }
            Ok(Self { enabled, api_token })
        } else {
            Ok(Self {
                enabled: false,
                api_token: String::new(),
            })
        }
    }
}

/// All ad conversion platform configs bundled together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdConversionConfig {
    pub meta: MetaCapiConfig,
    pub google: GoogleAdsConfig,
    pub bandsintown: BandsintownConversionConfig,
}

impl AdConversionConfig {
    pub(super) fn parse(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Ok(Self {
            meta: MetaCapiConfig::parse(values)?,
            google: GoogleAdsConfig::parse(values)?,
            bandsintown: BandsintownConversionConfig::parse(values)?,
        })
    }

    #[must_use]
    pub fn any_enabled(&self) -> bool {
        self.meta.enabled || self.google.enabled || self.bandsintown.enabled
    }
}

fn optional_trimmed(value: Option<&String>) -> Option<String> {
    value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
