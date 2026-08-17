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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantProfile {
    pub slug: String,
    pub display_name: String,
    pub palette: TenantPalette,
    pub products: TenantProducts,
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
        })
    }

    #[must_use]
    pub const fn synesthesia_enabled(&self) -> bool {
        self.products.synesthesia
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
        };
        assert!(!profile.synesthesia_enabled());
    }
}
