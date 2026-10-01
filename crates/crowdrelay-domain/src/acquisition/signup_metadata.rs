use serde::{Deserialize, Serialize};

/// Initial signup preferences and consented ad matching context.
/// These values must never authorize changes to an existing fan.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignupMetadata {
    /// Opt-in and radius for the explicitly supplied city.
    pub nearby_gigs: Option<(bool, i32)>,
    /// Meta browser identifier.
    pub meta_fbp: Option<String>,
    /// Meta click identifier.
    pub meta_fbc: Option<String>,
    /// Google click identifier.
    pub google_gclid: Option<String>,
    /// Bandsintown referral identifier.
    pub bandsintown_ref: Option<String>,
    /// Campaign source.
    pub utm_source: Option<String>,
    /// Campaign medium.
    pub utm_medium: Option<String>,
    /// Campaign name.
    pub utm_campaign: Option<String>,
    /// Campaign content.
    pub utm_content: Option<String>,
    /// Campaign term.
    pub utm_term: Option<String>,
    /// First-party landing URL.
    pub event_source_url: Option<String>,
    /// Transport address; excluded from the retry fingerprint.
    pub client_ip_address: Option<String>,
    /// Transport user agent; excluded from the retry fingerprint.
    pub client_user_agent: Option<String>,
}
