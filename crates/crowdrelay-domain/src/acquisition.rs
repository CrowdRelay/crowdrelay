//! Domain models used by the smart-link and fan-acquisition slices.
//!
//! Defines the core value objects and validation rules for smart-link
//! resolution, click analytics, fan signup with consent, and city demand
//! signals.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    CampaignId, CityId, CitySlug, CountryCode, DestinationUrl, FanId, FanSessionToken,
    NormalizedEmail, ReferralCode, SmartLinkId, SmartLinkSlug, VisitorId, WorkspaceId,
};

/// A resolved smart-link ready for redirect, loaded from the in-memory cache.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSmartLink {
    id: SmartLinkId,
    workspace_id: WorkspaceId,
    campaign_id: Option<CampaignId>,
    slug: SmartLinkSlug,
    destination_url: DestinationUrl,
    version: u64,
}

impl ResolvedSmartLink {
    /// Creates a resolved smart-link, rejecting a zero version.
    pub fn new(
        id: SmartLinkId,
        workspace_id: WorkspaceId,
        campaign_id: Option<CampaignId>,
        slug: SmartLinkSlug,
        destination_url: DestinationUrl,
        version: u64,
    ) -> Result<Self, ResolvedSmartLinkError> {
        if version == 0 {
            return Err(ResolvedSmartLinkError::InvalidVersion);
        }
        Ok(Self {
            id,
            workspace_id,
            campaign_id,
            slug,
            destination_url,
            version,
        })
    }

    /// Returns the smart-link identifier.
    #[must_use]
    pub const fn id(&self) -> SmartLinkId {
        self.id
    }

    /// Returns the workspace that owns this smart-link.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Returns the optional campaign associated with this smart-link.
    #[must_use]
    pub const fn campaign_id(&self) -> Option<CampaignId> {
        self.campaign_id
    }

    /// Returns the slug used to resolve this smart-link.
    #[must_use]
    pub fn slug(&self) -> &SmartLinkSlug {
        &self.slug
    }

    /// Returns the redirect destination URL.
    #[must_use]
    pub fn destination_url(&self) -> &DestinationUrl {
        &self.destination_url
    }

    /// Returns the monotonic version used for cache invalidation.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

/// Error returned when constructing a [`ResolvedSmartLink`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ResolvedSmartLinkError {
    /// The version was zero.
    #[error("smart-link version must be positive")]
    InvalidVersion,
}

/// A non-critical click event queued for batch persistence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClickEvent {
    workspace_id: WorkspaceId,
    smart_link_id: SmartLinkId,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    referrer_host: Option<String>,
    occurred_at: OffsetDateTime,
}

impl ClickEvent {
    /// Derives a click event from a resolved smart-link, normalizing the
    /// referrer host to lowercase and rejecting hosts exceeding 253 bytes.
    pub fn from_link(
        link: &ResolvedSmartLink,
        visitor_id: Option<VisitorId>,
        referrer_host: Option<String>,
        occurred_at: OffsetDateTime,
    ) -> Result<Self, ClickEventError> {
        let referrer_host = normalize_referrer_host(referrer_host)?;
        Ok(Self {
            workspace_id: link.workspace_id(),
            smart_link_id: link.id(),
            campaign_id: link.campaign_id(),
            visitor_id,
            referrer_host,
            occurred_at,
        })
    }

    /// Returns the workspace that owns the clicked smart-link.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Returns the smart-link identifier.
    #[must_use]
    pub const fn smart_link_id(&self) -> SmartLinkId {
        self.smart_link_id
    }

    /// Returns the optional campaign associated with the click.
    #[must_use]
    pub const fn campaign_id(&self) -> Option<CampaignId> {
        self.campaign_id
    }

    /// Returns the optional visitor identifier.
    #[must_use]
    pub const fn visitor_id(&self) -> Option<VisitorId> {
        self.visitor_id
    }

    /// Returns the normalized referrer host, if present.
    #[must_use]
    pub fn referrer_host(&self) -> Option<&str> {
        self.referrer_host.as_deref()
    }

    /// Returns the timestamp at which the click occurred.
    #[must_use]
    pub const fn occurred_at(&self) -> OffsetDateTime {
        self.occurred_at
    }
}

fn normalize_referrer_host(value: Option<String>) -> Result<Option<String>, ClickEventError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 253 {
        return Err(ClickEventError::ReferrerHostTooLong);
    }
    if !value.is_ascii()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
    {
        return Err(ClickEventError::InvalidReferrerHost);
    }
    Ok(Some(value.to_ascii_lowercase()))
}

/// Error returned when a click event fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ClickEventError {
    /// The referrer host exceeded 253 bytes.
    #[error("referrer host must contain at most 253 bytes")]
    ReferrerHostTooLong,
    /// The referrer host contained invalid characters.
    #[error("referrer host is invalid")]
    InvalidReferrerHost,
}

/// Marketing consent record captured at signup, with policy version and source.
///
/// `Debug` is deliberately redacted to avoid leaking consent metadata in logs.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct MarketingConsent {
    granted: bool,
    policy_version: String,
    source: String,
}

impl fmt::Debug for MarketingConsent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MarketingConsent")
            .field("granted", &self.granted)
            .field("policy_version", &"[REDACTED]")
            .field("source", &"[REDACTED]")
            .finish()
    }
}

impl MarketingConsent {
    /// Creates a consent record, trimming and validating the policy version
    /// and source string.
    pub fn new(
        granted: bool,
        policy_version: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Self, MarketingConsentError> {
        let consent = Self {
            granted,
            policy_version: policy_version.into().trim().to_owned(),
            source: source.into().trim().to_owned(),
        };
        consent.validate()?;
        Ok(consent)
    }

    /// Validates the policy version and source against length and character constraints.
    pub fn validate(&self) -> Result<(), MarketingConsentError> {
        if self.policy_version.trim().is_empty()
            || self.policy_version.trim() != self.policy_version
            || self.policy_version.len() > 128
            || self.policy_version.chars().any(char::is_control)
        {
            return Err(MarketingConsentError::InvalidPolicyVersion);
        }
        if self.source.trim().is_empty()
            || self.source.trim() != self.source
            || self.source.len() > 128
            || self.source.chars().any(char::is_control)
        {
            return Err(MarketingConsentError::InvalidSource);
        }
        Ok(())
    }

    /// Returns whether the fan granted marketing consent.
    #[must_use]
    pub const fn granted(&self) -> bool {
        self.granted
    }

    /// Returns the privacy policy version accepted by the fan.
    #[must_use]
    pub fn policy_version(&self) -> &str {
        &self.policy_version
    }

    /// Returns the source channel where consent was captured.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Error returned when consent metadata fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MarketingConsentError {
    /// The policy version was empty, too long, or contained control characters.
    #[error("consent policy version must contain 1 to 128 bytes")]
    InvalidPolicyVersion,
    /// The source was empty, too long, or contained control characters.
    #[error("consent source must contain 1 to 128 bytes")]
    InvalidSource,
}

/// Input for constructing a [`FanSignup`].
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct FanSignupInput {
    pub workspace_id: WorkspaceId,
    pub email: NormalizedEmail,
    pub display_name: Option<String>,
    pub city_slug: CitySlug,
    pub locale: Option<String>,
    pub campaign_id: Option<CampaignId>,
    pub visitor_id: Option<VisitorId>,
    pub claimed_referral_code: Option<ReferralCode>,
    pub consent: MarketingConsent,
}

/// A validated fan signup with consent, city interest, and optional referral.
///
/// `Debug` is deliberately redacted to prevent leaking personal data.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct FanSignup {
    workspace_id: WorkspaceId,
    email: NormalizedEmail,
    display_name: Option<String>,
    city_slug: CitySlug,
    locale: Option<String>,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    claimed_referral_code: Option<ReferralCode>,
    consent: MarketingConsent,
}

impl FanSignup {
    /// Creates a fan signup from the given input, normalizing optional fields
    /// and validating consent, display name, and locale.
    pub fn new(input: FanSignupInput) -> Result<Self, FanSignupError> {
        let signup = Self {
            workspace_id: input.workspace_id,
            email: input.email,
            display_name: normalize_display_name(input.display_name)?,
            city_slug: input.city_slug,
            locale: normalize_locale(input.locale)?,
            campaign_id: input.campaign_id,
            visitor_id: input.visitor_id,
            claimed_referral_code: input.claimed_referral_code,
            consent: input.consent,
        };
        signup.validate()?;
        Ok(signup)
    }

    /// Validates display name, locale, and consent state.
    pub fn validate(&self) -> Result<(), FanSignupError> {
        validate_normalized_display_name(self.display_name.as_deref())?;
        validate_normalized_locale(self.locale.as_deref())?;
        self.consent.validate()?;
        if !self.consent.granted() {
            return Err(FanSignupError::MarketingConsentRequired);
        }
        Ok(())
    }

    /// Returns the workspace that owns this fan.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Returns the fan's normalized email.
    #[must_use]
    pub fn email(&self) -> &NormalizedEmail {
        &self.email
    }

    /// Returns the optional display name.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Returns the fan's city of interest.
    #[must_use]
    pub fn city_slug(&self) -> &CitySlug {
        &self.city_slug
    }

    /// Returns the optional locale tag.
    #[must_use]
    pub fn locale(&self) -> Option<&str> {
        self.locale.as_deref()
    }

    /// Returns the optional campaign that referred this signup.
    #[must_use]
    pub const fn campaign_id(&self) -> Option<CampaignId> {
        self.campaign_id
    }

    /// Returns the optional visitor identifier.
    #[must_use]
    pub const fn visitor_id(&self) -> Option<VisitorId> {
        self.visitor_id
    }

    /// Returns the optional referral code claimed by this fan.
    #[must_use]
    pub fn claimed_referral_code(&self) -> Option<&ReferralCode> {
        self.claimed_referral_code.as_ref()
    }

    /// Returns the marketing consent record.
    #[must_use]
    pub fn consent(&self) -> &MarketingConsent {
        &self.consent
    }
}

impl fmt::Debug for FanSignup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FanSignup")
            .field("workspace_id", &self.workspace_id)
            .field("email", &self.email)
            .field(
                "display_name",
                &self.display_name.as_ref().map(|_| "[REDACTED]"),
            )
            .field("city_slug", &self.city_slug)
            .field("locale", &self.locale)
            .field("campaign_id", &self.campaign_id)
            .field("visitor_id_present", &self.visitor_id.is_some())
            .field("claimed_referral_code", &self.claimed_referral_code)
            .field("marketing_consent_granted", &self.consent.granted())
            .finish()
    }
}

fn normalize_display_name(value: Option<String>) -> Result<Option<String>, FanSignupError> {
    let value = value.map(|value| value.trim().to_owned());
    validate_normalized_display_name(value.as_deref())?;
    Ok(value.filter(|value| !value.is_empty()))
}

fn validate_normalized_display_name(value: Option<&str>) -> Result<(), FanSignupError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.trim() != value
            || value.len() > 120
            || value.chars().any(char::is_control)
    }) {
        return Err(FanSignupError::InvalidDisplayName);
    }
    Ok(())
}

fn normalize_locale(value: Option<String>) -> Result<Option<String>, FanSignupError> {
    let value = value.map(|value| value.trim().to_owned());
    validate_normalized_locale(value.as_deref())?;
    Ok(value.filter(|value| !value.is_empty()))
}

fn validate_normalized_locale(value: Option<&str>) -> Result<(), FanSignupError> {
    if value.is_some_and(|value| {
        value.is_empty() || value.trim() != value || value.len() > 35 || !is_language_tag(value)
    }) {
        return Err(FanSignupError::InvalidLocale);
    }
    Ok(())
}

fn is_language_tag(value: &str) -> bool {
    let mut subtags = value.split('-');
    let Some(primary) = subtags.next() else {
        return false;
    };
    let valid_primary = (2..=8).contains(&primary.len())
        && primary.bytes().all(|byte| byte.is_ascii_alphabetic())
        || matches!(primary, "i" | "I" | "x" | "X");

    valid_primary
        && subtags.all(|subtag| {
            (1..=8).contains(&subtag.len())
                && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// Error returned when a fan signup fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FanSignupError {
    /// The display name was empty, too long, or contained control characters.
    #[error("display name must contain at most 120 bytes and no control characters")]
    InvalidDisplayName,
    /// The locale was not a valid language tag.
    #[error("locale must be a valid short language tag")]
    InvalidLocale,
    /// Marketing consent was not granted.
    #[error("explicit marketing consent is required for fan signup")]
    MarketingConsentRequired,
    /// The consent metadata was invalid.
    #[error(transparent)]
    InvalidConsent(#[from] MarketingConsentError),
}

/// Lifecycle status of a fan within CrowdRelay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanStatus {
    /// Awaiting email confirmation.
    Pending,
    /// Email confirmed and marketing consent active.
    Active,
    /// Fan opted out of marketing communications.
    Unsubscribed,
    /// Administratively suppressed.
    Suppressed,
}

/// Result returned after a fan signup operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FanSignupResult {
    pub fan_id: FanId,
    pub status: FanStatus,
    pub referral_code: Option<ReferralCode>,
    pub fan_session_token: Option<FanSessionToken>,
    pub confirmation_required: bool,
    pub created: bool,
}

/// Anonymous city demand signal derived from fan signups.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CitySignal {
    city_id: CityId,
    slug: CitySlug,
    name: String,
    country_code: CountryCode,
    fan_count: u64,
}

impl CitySignal {
    /// Creates a city signal, trimming and validating the display name.
    pub fn new(
        city_id: CityId,
        slug: CitySlug,
        name: impl Into<String>,
        country_code: CountryCode,
        fan_count: u64,
    ) -> Result<Self, CitySignalError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() || name.len() > 160 || name.chars().any(char::is_control) {
            return Err(CitySignalError::InvalidName);
        }
        Ok(Self {
            city_id,
            slug,
            name,
            country_code,
            fan_count,
        })
    }

    /// Returns the city identifier.
    #[must_use]
    pub const fn city_id(&self) -> CityId {
        self.city_id
    }

    /// Returns the city slug.
    #[must_use]
    pub fn slug(&self) -> &CitySlug {
        &self.slug
    }

    /// Returns the human-readable city name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the ISO 3166-1 alpha-2 country code.
    #[must_use]
    pub fn country_code(&self) -> &CountryCode {
        &self.country_code
    }

    /// Returns the number of fans who selected this city.
    #[must_use]
    pub const fn fan_count(&self) -> u64 {
        self.fan_count
    }
}

/// Error returned when a city signal fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CitySignalError {
    /// The city name was empty, too long, or contained control characters.
    #[error("city name must contain 1 to 160 bytes and no control characters")]
    InvalidName,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> Result<ResolvedSmartLink, Box<dyn std::error::Error>> {
        Ok(ResolvedSmartLink::new(
            SmartLinkId::new(),
            WorkspaceId::new(),
            Some(CampaignId::new()),
            SmartLinkSlug::parse("tour-2026")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
        )?)
    }

    #[test]
    fn resolved_link_requires_positive_version() -> Result<(), Box<dyn std::error::Error>> {
        let link = link()?;
        assert_eq!(link.version(), 1);
        assert!(
            ResolvedSmartLink::new(
                link.id(),
                link.workspace_id(),
                link.campaign_id(),
                link.slug().clone(),
                link.destination_url().clone(),
                0,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn click_is_derived_from_resolved_link() -> Result<(), Box<dyn std::error::Error>> {
        let link = link()?;
        let visitor = VisitorId::new();
        let event = ClickEvent::from_link(
            &link,
            Some(visitor),
            Some(" Ref.Example.COM ".to_owned()),
            OffsetDateTime::UNIX_EPOCH,
        )?;

        assert_eq!(event.workspace_id(), link.workspace_id());
        assert_eq!(event.smart_link_id(), link.id());
        assert_eq!(event.visitor_id(), Some(visitor));
        assert_eq!(event.referrer_host(), Some("ref.example.com"));
        Ok(())
    }

    #[test]
    fn fan_signup_debug_never_contains_personal_data() -> Result<(), Box<dyn std::error::Error>> {
        let signup = FanSignup::new(FanSignupInput {
            workspace_id: WorkspaceId::new(),
            email: NormalizedEmail::parse("private@example.com")?,
            display_name: Some("Private Name".to_owned()),
            city_slug: CitySlug::parse("wroclaw")?,
            locale: Some("pl-PL".to_owned()),
            campaign_id: Some(CampaignId::new()),
            visitor_id: Some(VisitorId::new()),
            claimed_referral_code: Some(ReferralCode::parse("Ref_123")?),
            consent: MarketingConsent::new(true, "2026-07", "virya_join")?,
        })?;
        let debug = format!("{signup:?}");

        assert!(!debug.contains("private@example.com"));
        assert!(!debug.contains("Private Name"));
        assert!(!debug.contains("Ref_123"));
        Ok(())
    }

    #[test]
    fn fan_signup_normalizes_optional_text() -> Result<(), Box<dyn std::error::Error>> {
        let signup = FanSignup::new(FanSignupInput {
            workspace_id: WorkspaceId::new(),
            email: NormalizedEmail::parse("fan@example.com")?,
            display_name: Some("  Virya Fan  ".to_owned()),
            city_slug: CitySlug::parse("warszawa")?,
            locale: Some("  pl-PL ".to_owned()),
            campaign_id: None,
            visitor_id: None,
            claimed_referral_code: None,
            consent: MarketingConsent::new(true, "v1", "join")?,
        })?;

        assert_eq!(signup.display_name(), Some("Virya Fan"));
        assert_eq!(signup.locale(), Some("pl-PL"));
        Ok(())
    }

    #[test]
    fn fan_signup_requires_explicit_marketing_consent() -> Result<(), Box<dyn std::error::Error>> {
        let result = FanSignup::new(FanSignupInput {
            workspace_id: WorkspaceId::new(),
            email: NormalizedEmail::parse("fan@example.com")?,
            display_name: None,
            city_slug: CitySlug::parse("warszawa")?,
            locale: None,
            campaign_id: None,
            visitor_id: None,
            claimed_referral_code: None,
            consent: MarketingConsent::new(false, "v1", "join")?,
        });

        assert!(matches!(
            result,
            Err(FanSignupError::MarketingConsentRequired)
        ));
        Ok(())
    }

    #[test]
    fn locale_rejects_empty_or_malformed_subtags() {
        for locale in ["-", "pl-", "-PL", "pl--PL", "p", "polski_PL"] {
            assert!(!is_language_tag(locale), "{locale:?} must be rejected");
        }
        for locale in ["pl", "pl-PL", "zh-Hant-TW", "de-CH-1901", "x-private"] {
            assert!(is_language_tag(locale), "{locale:?} must be accepted");
        }
    }

    #[test]
    fn consent_metadata_rejects_control_characters() {
        assert!(MarketingConsent::new(true, "privacy\nv1", "join").is_err());
        assert!(MarketingConsent::new(true, "privacy-v1", "join\tform").is_err());
    }
}
