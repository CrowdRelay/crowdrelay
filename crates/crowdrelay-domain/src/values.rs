//! Validated acquisition value objects.
//!
//! This module provides parse-time validated wrappers for slugs, URLs, email
//! addresses, referral codes, and country codes. Each type normalizes its
//! input and rejects malformed values at the boundary, so downstream code can
//! rely on structural invariants without re-validating.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use url::{Host, Url};

/// Maximum byte length accepted for slug value objects.
pub const MAX_SLUG_LENGTH: usize = 128;

/// Maximum byte length accepted for [`DestinationUrl`] values.
pub const MAX_DESTINATION_URL_LENGTH: usize = 2_048;

/// Maximum byte length accepted for [`NormalizedEmail`] values.
pub const MAX_EMAIL_LENGTH: usize = 254;

/// Generates a validated slug value object with `parse`, `as_str`, and
/// `into_inner` methods backed by [`validate_slug`].
macro_rules! validated_slug {
    ($name:ident, $allow_uppercase:expr) => {
        #[doc = concat!("Validated slug identifying a ", stringify!($name), " resource.")]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Parses and validates a slug, returning an error on malformed input.
            pub fn parse(value: impl AsRef<str>) -> Result<Self, SlugError> {
                validate_slug(value.as_ref(), $allow_uppercase).map(Self)
            }

            /// Returns the validated slug as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the value object and returns the underlying string.
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = SlugError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = SlugError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

validated_slug!(WorkspaceSlug, false);
validated_slug!(SmartLinkSlug, true);
validated_slug!(CitySlug, false);
validated_slug!(EventSlug, false);

/// Error returned when a slug fails validation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SlugError {
    /// The slug was empty.
    #[error("slug must not be empty")]
    Empty,
    /// The slug exceeded [`MAX_SLUG_LENGTH`] bytes.
    #[error("slug must contain at most {max} ASCII characters")]
    TooLong { max: usize },
    /// The slug did not start with an ASCII letter or digit.
    #[error("slug must start with an ASCII letter or digit")]
    InvalidStart,
    /// The slug contained a character other than ASCII alphanumeric, `_`, or `-`.
    #[error("slug contains an invalid character at byte {index}")]
    InvalidCharacter { index: usize },
}

fn validate_slug(value: &str, allow_uppercase: bool) -> Result<String, SlugError> {
    if value.is_empty() {
        return Err(SlugError::Empty);
    }
    if value.len() > MAX_SLUG_LENGTH {
        return Err(SlugError::TooLong {
            max: MAX_SLUG_LENGTH,
        });
    }

    let is_alphanumeric = |byte: u8| {
        byte.is_ascii_digit()
            || byte.is_ascii_lowercase()
            || (allow_uppercase && byte.is_ascii_uppercase())
    };
    let bytes = value.as_bytes();
    let Some(first) = bytes.first().copied() else {
        return Err(SlugError::Empty);
    };
    if !is_alphanumeric(first) {
        return Err(SlugError::InvalidStart);
    }
    if let Some((index, _)) = bytes
        .iter()
        .enumerate()
        .find(|(_, byte)| !is_alphanumeric(**byte) && !matches!(**byte, b'_' | b'-'))
    {
        return Err(SlugError::InvalidCharacter { index });
    }

    Ok(value.to_owned())
}

/// A safe HTTP(S) redirect target.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DestinationUrl(String);

impl DestinationUrl {
    /// Parses and validates an HTTP or HTTPS URL, rejecting empty values,
    /// non-HTTP schemes, embedded credentials, and URLs exceeding
    /// [`MAX_DESTINATION_URL_LENGTH`] bytes.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, DestinationUrlError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(DestinationUrlError::Empty);
        }
        if value.len() > MAX_DESTINATION_URL_LENGTH {
            return Err(DestinationUrlError::TooLong {
                max: MAX_DESTINATION_URL_LENGTH,
            });
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(DestinationUrlError::InvalidSyntax);
        }

        let parsed = Url::parse(value).map_err(|_| DestinationUrlError::InvalidSyntax)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
            return Err(DestinationUrlError::UnsupportedScheme);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(DestinationUrlError::CredentialsForbidden);
        }

        Ok(Self(parsed.into()))
    }

    /// Returns the validated URL as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for DestinationUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DestinationUrl")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for DestinationUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for DestinationUrl {
    type Err = DestinationUrlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for DestinationUrl {
    type Error = DestinationUrlError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for DestinationUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DestinationUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// Error returned when a destination URL fails validation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DestinationUrlError {
    /// The URL was empty.
    #[error("destination URL must not be empty")]
    Empty,
    /// The URL exceeded [`MAX_DESTINATION_URL_LENGTH`] bytes.
    #[error("destination URL must contain at most {max} bytes")]
    TooLong { max: usize },
    /// The URL could not be parsed.
    #[error("destination URL is malformed")]
    InvalidSyntax,
    /// The URL used a scheme other than `http` or `https`, or had no host.
    #[error("destination URL must use HTTP or HTTPS and include a host")]
    UnsupportedScheme,
    /// The URL contained embedded username or password credentials.
    #[error("destination URL must not contain credentials")]
    CredentialsForbidden,
}

/// Canonical email stored by CrowdRelay.
///
/// `Debug` is deliberately redacted. Callers must opt into accessing the
/// normalized value through `as_str`.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NormalizedEmail(String);

impl NormalizedEmail {
    /// Parses, trims, and lowercases an email address. Rejects empty values,
    /// non-ASCII input, IP-address domains, and addresses exceeding
    /// [`MAX_EMAIL_LENGTH`] bytes.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, NormalizedEmailError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(NormalizedEmailError::Empty);
        }
        if value.len() > MAX_EMAIL_LENGTH {
            return Err(NormalizedEmailError::TooLong {
                max: MAX_EMAIL_LENGTH,
            });
        }
        if !value.is_ascii() || value.chars().any(char::is_control) {
            return Err(NormalizedEmailError::Invalid);
        }

        let (local, domain) = value.split_once('@').ok_or(NormalizedEmailError::Invalid)?;
        if domain.contains('@')
            || local.is_empty()
            || local.len() > 64
            || local.starts_with('.')
            || local.ends_with('.')
            || local.contains("..")
            || !local.bytes().all(is_valid_email_local_byte)
        {
            return Err(NormalizedEmailError::Invalid);
        }

        let canonical_domain =
            match Host::parse(domain).map_err(|_| NormalizedEmailError::Invalid)? {
                Host::Domain(domain) => domain,
                Host::Ipv4(_) | Host::Ipv6(_) => return Err(NormalizedEmailError::Invalid),
            };

        Ok(Self(format!(
            "{}@{}",
            local.to_ascii_lowercase(),
            canonical_domain.to_ascii_lowercase()
        )))
    }

    /// Returns the canonicalized email as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

fn is_valid_email_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}

impl fmt::Debug for NormalizedEmail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("NormalizedEmail")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl FromStr for NormalizedEmail {
    type Err = NormalizedEmailError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for NormalizedEmail {
    type Error = NormalizedEmailError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for NormalizedEmail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// Error returned when an email address fails validation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NormalizedEmailError {
    /// The email was empty after trimming.
    #[error("email must not be empty")]
    Empty,
    /// The email exceeded [`MAX_EMAIL_LENGTH`] bytes.
    #[error("email must contain at most {max} bytes")]
    TooLong { max: usize },
    /// The email was structurally invalid.
    #[error("email is invalid")]
    Invalid,
}

/// A fan's unique referral code, validated at parse time.
///
/// `Debug` is deliberately redacted to prevent accidental logging.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ReferralCode(String);

impl ReferralCode {
    /// Parses a referral code, accepting 6–128 ASCII letters, digits, `_`, or `-`.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, ReferralCodeError> {
        let value = value.as_ref();
        if !(6..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ReferralCodeError);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the referral code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for ReferralCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ReferralCode")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl FromStr for ReferralCode {
    type Err = ReferralCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ReferralCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// Error returned when a referral code fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("referral code must contain 6 to 128 ASCII letters, digits, `_` or `-`")]
pub struct ReferralCodeError;

/// An ISO 3166-1 alpha-2 country code (two uppercase ASCII letters).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CountryCode(String);

impl CountryCode {
    /// Parses a country code, accepting exactly two uppercase ASCII letters.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CountryCodeError> {
        let value = value.as_ref();
        if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(CountryCodeError);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the country code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for CountryCode {
    type Err = CountryCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for CountryCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// Error returned when a country code fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("country code must contain exactly two uppercase ASCII letters")]
pub struct CountryCodeError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_validation_matches_database_constraints() {
        for value in ["a", "abc-123", "a_b", "Z9"] {
            assert!(SmartLinkSlug::parse(value).is_ok(), "{value}");
        }
        for value in ["", "-abc", "abc/def", "ąć", &"a".repeat(129)] {
            assert!(SmartLinkSlug::parse(value).is_err(), "{value}");
        }
        assert!(WorkspaceSlug::parse("uppercase").is_ok());
        assert!(WorkspaceSlug::parse("Uppercase").is_err());
        assert!(CitySlug::parse("wroclaw").is_ok());
        assert!(CitySlug::parse("Wroclaw").is_err());
    }

    #[test]
    fn destination_allows_only_credential_free_http_urls() {
        for value in [
            "https://virya.music/join?from=tiktok",
            "http://localhost:4321/path#cta",
        ] {
            assert!(DestinationUrl::parse(value).is_ok(), "{value}");
        }
        for value in [
            "javascript:alert(1)",
            "data:text/plain,test",
            "file:///etc/passwd",
            "https://user:secret@example.com/",
            "https://",
            " https://example.com",
        ] {
            assert!(DestinationUrl::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn email_normalization_is_deterministic_and_debug_is_redacted()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (" Fan.Name+Live@Example.COM ", "fan.name+live@example.com"),
            ("A@B.COM", "a@b.com"),
        ];
        for (input, expected) in cases {
            let email = NormalizedEmail::parse(input)?;
            assert_eq!(email.as_str(), expected);
            assert!(!format!("{email:?}").contains(expected));
        }
        Ok(())
    }

    #[test]
    fn email_rejects_malformed_inputs_without_echoing_them() {
        for input in [
            "",
            "missing-at.example.com",
            "@example.com",
            "a@@example.com",
            ".a@example.com",
            "a..b@example.com",
            "a@127.0.0.1",
            "ą@example.com",
        ] {
            let error = NormalizedEmail::parse(input).unwrap_err();
            if !input.is_empty() {
                assert!(!error.to_string().contains(input));
            }
        }
    }
}
