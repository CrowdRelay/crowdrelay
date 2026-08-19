//! In-memory tenant product/branding profile.
//!
//! CrowdRelay data isolation already comes from the configured workspace ID.
//! This layer deliberately adds no request-time database lookup: one profile is
//! validated once at process startup and then reused by public meta and product
//! boundary checks. The current Virya workspace remains the zero-config default.

use crowdrelay_domain::WorkspaceSlug;
use serde::Serialize;
use std::{env, error::Error, fmt};

const DISPLAY_NAME_KEY: &str = "CROWDRELAY_TENANT_DISPLAY_NAME";
const COLOR_BACKGROUND_KEY: &str = "CROWDRELAY_TENANT_COLOR_BACKGROUND";
const COLOR_SURFACE_KEY: &str = "CROWDRELAY_TENANT_COLOR_SURFACE";
const COLOR_SURFACE_ALT_KEY: &str = "CROWDRELAY_TENANT_COLOR_SURFACE_ALT";
const COLOR_LINE_KEY: &str = "CROWDRELAY_TENANT_COLOR_LINE";
const COLOR_MUTED_KEY: &str = "CROWDRELAY_TENANT_COLOR_MUTED";
const COLOR_ACCENT_KEY: &str = "CROWDRELAY_TENANT_COLOR_ACCENT";
const COLOR_ACCENT_SOFT_KEY: &str = "CROWDRELAY_TENANT_COLOR_ACCENT_SOFT";
const COLOR_TEXT_KEY: &str = "CROWDRELAY_TENANT_COLOR_TEXT";
const COLOR_DANGER_KEY: &str = "CROWDRELAY_TENANT_COLOR_DANGER";
const COLOR_SUCCESS_KEY: &str = "CROWDRELAY_TENANT_COLOR_SUCCESS";
const COUNTRY_KEY: &str = "CROWDRELAY_DEFAULT_COUNTRY_CODE";
const REGION_KEY: &str = "CROWDRELAY_TENANT_REGION";
const LOCALE_KEY: &str = "CROWDRELAY_TENANT_LOCALE";
const TIMEZONE_KEY: &str = "CROWDRELAY_TENANT_TIMEZONE";
const CURRENCY_KEY: &str = "CROWDRELAY_TENANT_CURRENCY";
const DATE_FORMAT_KEY: &str = "CROWDRELAY_TENANT_DATE_FORMAT";
const NUMBER_FORMAT_KEY: &str = "CROWDRELAY_TENANT_NUMBER_FORMAT";
const DATA_REGION_KEY: &str = "CROWDRELAY_TENANT_DATA_REGION";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantPalette {
    pub background: String,
    pub surface: String,
    pub surface_alt: String,
    pub line: String,
    pub muted: String,
    pub accent: String,
    pub accent_soft: String,
    pub text: String,
    pub danger: String,
    pub success: String,
}

impl Default for TenantPalette {
    fn default() -> Self {
        Self {
            background: "#080808".to_owned(),
            surface: "#11110f".to_owned(),
            surface_alt: "#171713".to_owned(),
            line: "#292925".to_owned(),
            muted: "#8f8f87".to_owned(),
            accent: "#f3c51a".to_owned(),
            accent_soft: "#2a2308".to_owned(),
            text: "#f5f5ef".to_owned(),
            danger: "#ff655d".to_owned(),
            success: "#70db91".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantProducts {
    pub crowdrelay: bool,
    pub signal: bool,
    pub synesthesia: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RegionalSource {
    TenantProfile,
    PlatformDefault,
    Unclassified,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantRegionalProfile {
    pub country_code: String,
    pub region: String,
    pub locale: String,
    pub timezone: String,
    pub currency: String,
    pub date_format: String,
    pub number_format: String,
    pub data_region: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantRegionalProvenance {
    pub country_code: RegionalSource,
    pub region: RegionalSource,
    pub locale: RegionalSource,
    pub timezone: RegionalSource,
    pub currency: RegionalSource,
    pub date_format: RegionalSource,
    pub number_format: RegionalSource,
    pub data_region: RegionalSource,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantProfile {
    pub slug: String,
    pub display_name: String,
    pub palette: TenantPalette,
    pub products: TenantProducts,
    pub regional: TenantRegionalProfile,
    pub regional_provenance: TenantRegionalProvenance,
}

#[derive(Debug)]
pub struct TenantConfigError(&'static str);

impl fmt::Display for TenantConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}
impl Error for TenantConfigError {}

impl TenantProfile {
    pub fn from_process_env(workspace: &WorkspaceSlug) -> Result<Self, TenantConfigError> {
        let is_virya = workspace.as_str() == "virya";
        let display_name = env::var(DISPLAY_NAME_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                if is_virya {
                    "Virya".to_owned()
                } else {
                    workspace.as_str().to_owned()
                }
            });
        if display_name.chars().count() > 80 {
            return Err(TenantConfigError(
                "tenant display name must contain at most 80 characters",
            ));
        }
        let defaults = TenantPalette::default();
        let palette = TenantPalette {
            background: color(COLOR_BACKGROUND_KEY, defaults.background)?,
            surface: color(COLOR_SURFACE_KEY, defaults.surface)?,
            surface_alt: color(COLOR_SURFACE_ALT_KEY, defaults.surface_alt)?,
            line: color(COLOR_LINE_KEY, defaults.line)?,
            muted: color(COLOR_MUTED_KEY, defaults.muted)?,
            accent: color(COLOR_ACCENT_KEY, defaults.accent)?,
            accent_soft: color(COLOR_ACCENT_SOFT_KEY, defaults.accent_soft)?,
            text: color(COLOR_TEXT_KEY, defaults.text)?,
            danger: color(COLOR_DANGER_KEY, defaults.danger)?,
            success: color(COLOR_SUCCESS_KEY, defaults.success)?,
        };
        let (country_code, country_source) = regional(COUNTRY_KEY, is_virya, "PL")?;
        let (region, region_source) = regional(REGION_KEY, is_virya, "eu")?;
        let (locale, locale_source) = regional(LOCALE_KEY, is_virya, "pl-PL")?;
        let (timezone, timezone_source) = regional(TIMEZONE_KEY, is_virya, "Europe/Warsaw")?;
        let (currency, currency_source) = regional(CURRENCY_KEY, is_virya, "PLN")?;
        let (date_format, date_source) = regional(DATE_FORMAT_KEY, is_virya, "dmy")?;
        let (number_format, number_source) =
            regional(NUMBER_FORMAT_KEY, is_virya, "comma_decimal")?;
        let explicit_data_region = env::var(DATA_REGION_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let (data_region, data_region_source) = match explicit_data_region {
            Some(value) => (Some(value), RegionalSource::TenantProfile),
            None if is_virya => (None, RegionalSource::Unclassified),
            None => {
                return Err(TenantConfigError(
                    "non-Virya tenant requires explicit CROWDRELAY_TENANT_DATA_REGION",
                ));
            }
        };
        validate_country(&country_code)?;
        validate_region(&region)?;
        validate_locale(&locale)?;
        validate_timezone(&timezone)?;
        validate_currency(&currency)?;
        validate_date_format(&date_format)?;
        validate_number_format(&number_format)?;
        if let Some(value) = data_region.as_deref() {
            validate_region(value)?;
        }

        Ok(Self {
            slug: workspace.as_str().to_owned(),
            display_name,
            palette,
            products: TenantProducts {
                crowdrelay: true,
                signal: true,
                // Synesthesia is a Virya product, never a tenant entitlement.
                synesthesia: is_virya,
            },
            regional: TenantRegionalProfile {
                country_code,
                region,
                locale,
                timezone,
                currency,
                date_format,
                number_format,
                data_region,
            },
            regional_provenance: TenantRegionalProvenance {
                country_code: country_source,
                region: region_source,
                locale: locale_source,
                timezone: timezone_source,
                currency: currency_source,
                date_format: date_source,
                number_format: number_source,
                data_region: data_region_source,
            },
        })
    }

    #[must_use]
    pub const fn synesthesia_enabled(&self) -> bool {
        self.products.synesthesia
    }
}

pub async fn public_config(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CACHE_CONTROL,
            "public, max-age=300, s-maxage=300",
        )],
        axum::Json(state.tenant.clone()),
    )
}

fn regional(
    key: &'static str,
    is_virya: bool,
    virya_default: &'static str,
) -> Result<(String, RegionalSource), TenantConfigError> {
    if let Some(value) = env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok((value, RegionalSource::TenantProfile));
    }
    if is_virya {
        return Ok((virya_default.to_owned(), RegionalSource::PlatformDefault));
    }
    Err(TenantConfigError(
        "non-Virya tenant regional profile is incomplete",
    ))
}

fn validate_country(value: &str) -> Result<(), TenantConfigError> {
    if value.len() == 2 && value.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant country must be two uppercase letters",
        ))
    }
}
fn validate_currency(value: &str) -> Result<(), TenantConfigError> {
    if value.len() == 3 && value.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant currency must be three uppercase letters",
        ))
    }
}
fn validate_region(value: &str) -> Result<(), TenantConfigError> {
    if matches!(value, "eu" | "us") {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant region/data region must be eu or us",
        ))
    }
}
fn validate_locale(value: &str) -> Result<(), TenantConfigError> {
    if (4..=35).contains(&value.len())
        && value.contains('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant locale must be an explicit BCP-47 style tag",
        ))
    }
}
fn validate_timezone(value: &str) -> Result<(), TenantConfigError> {
    if (3..=64).contains(&value.len())
        && value.contains('/')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
    {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant timezone must be an explicit IANA-style zone",
        ))
    }
}
fn validate_date_format(value: &str) -> Result<(), TenantConfigError> {
    if matches!(value, "dmy" | "mdy" | "ymd") {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant date format must be dmy, mdy or ymd",
        ))
    }
}
fn validate_number_format(value: &str) -> Result<(), TenantConfigError> {
    if matches!(value, "comma_decimal" | "dot_decimal") {
        Ok(())
    } else {
        Err(TenantConfigError(
            "tenant number format must be comma_decimal or dot_decimal",
        ))
    }
}

fn color(key: &'static str, default: String) -> Result<String, TenantConfigError> {
    let Ok(value) = env::var(key) else {
        return Ok(default);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(default);
    }
    if value.len() != 7
        || !value.starts_with('#')
        || !value.chars().skip(1).all(|c| c.is_ascii_hexdigit())
    {
        return Err(TenantConfigError("tenant colors must use #RRGGBB format"));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virya_defaults_preserve_current_palette_and_product_boundary() {
        let workspace = WorkspaceSlug::parse("virya").expect("valid workspace");
        let profile = TenantProfile {
            slug: workspace.as_str().to_owned(),
            display_name: "Virya".to_owned(),
            palette: TenantPalette::default(),
            products: TenantProducts {
                crowdrelay: true,
                signal: true,
                synesthesia: true,
            },
            regional: TenantRegionalProfile {
                country_code: "PL".to_owned(),
                region: "eu".to_owned(),
                locale: "pl-PL".to_owned(),
                timezone: "Europe/Warsaw".to_owned(),
                currency: "PLN".to_owned(),
                date_format: "dmy".to_owned(),
                number_format: "comma_decimal".to_owned(),
                data_region: None,
            },
            regional_provenance: TenantRegionalProvenance {
                country_code: RegionalSource::PlatformDefault,
                region: RegionalSource::PlatformDefault,
                locale: RegionalSource::PlatformDefault,
                timezone: RegionalSource::PlatformDefault,
                currency: RegionalSource::PlatformDefault,
                date_format: RegionalSource::PlatformDefault,
                number_format: RegionalSource::PlatformDefault,
                data_region: RegionalSource::Unclassified,
            },
        };
        assert_eq!(profile.palette.accent, "#f3c51a");
        assert_eq!(profile.palette.background, "#080808");
        assert!(profile.synesthesia_enabled());
    }

    #[test]
    fn non_virya_tenant_never_gets_synesthesia_product() {
        let workspace = WorkspaceSlug::parse("another-band").expect("valid workspace");
        let profile = TenantProfile {
            slug: workspace.as_str().to_owned(),
            display_name: "Another Band".to_owned(),
            palette: TenantPalette::default(),
            products: TenantProducts {
                crowdrelay: true,
                signal: true,
                synesthesia: false,
            },
            regional: TenantRegionalProfile {
                country_code: "PL".to_owned(),
                region: "eu".to_owned(),
                locale: "pl-PL".to_owned(),
                timezone: "Europe/Warsaw".to_owned(),
                currency: "PLN".to_owned(),
                date_format: "dmy".to_owned(),
                number_format: "comma_decimal".to_owned(),
                data_region: None,
            },
            regional_provenance: TenantRegionalProvenance {
                country_code: RegionalSource::PlatformDefault,
                region: RegionalSource::PlatformDefault,
                locale: RegionalSource::PlatformDefault,
                timezone: RegionalSource::PlatformDefault,
                currency: RegionalSource::PlatformDefault,
                date_format: RegionalSource::PlatformDefault,
                number_format: RegionalSource::PlatformDefault,
                data_region: RegionalSource::Unclassified,
            },
        };
        assert!(!profile.synesthesia_enabled());
    }
}
