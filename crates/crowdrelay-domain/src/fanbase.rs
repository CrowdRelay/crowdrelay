//! Fanbase policy: which acquisition origins exist, what they can yield, and
//! how a candidate becomes a member.
//!
//! A fanbase is an addressable audience block. Its origin (the provider that
//! fills it) is swappable data — Meta lead ads today, a partner export or a
//! Reddit community tomorrow — so the policy here speaks about CAPABILITIES,
//! never about concrete platforms:
//!
//! - **PII-capable origins** can produce direct contact data (an email) and
//!   therefore fans. Origins without PII feed the Audience Graph instead; the
//!   adapter refuses to invent identities.
//! - **Everything lands pending.** Even a platform-verified lead confirms its
//!   address through our own double-opt-in before it is a fan we can write to.
//! - **Opt-outs are terminal at admission time.** A suppressed address in an
//!   imported batch is counted and skipped, never resurrected.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    HttpJsonPull,
    CsvInline,
    ManualImport,
    MetaLeadAds,
    BandsintownFollowers,
    GoogleCustomerMatch,
    RedditCommunity,
}

impl SourceKind {
    pub const ALL: [SourceKind; 7] = [
        SourceKind::HttpJsonPull,
        SourceKind::CsvInline,
        SourceKind::ManualImport,
        SourceKind::MetaLeadAds,
        SourceKind::BandsintownFollowers,
        SourceKind::GoogleCustomerMatch,
        SourceKind::RedditCommunity,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HttpJsonPull => "http_json_pull",
            Self::CsvInline => "csv_inline",
            Self::ManualImport => "manual_import",
            Self::MetaLeadAds => "meta_lead_ads",
            Self::BandsintownFollowers => "bandsintown_followers",
            Self::GoogleCustomerMatch => "google_customer_match",
            Self::RedditCommunity => "reddit_community",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// Can this origin yield a direct contact channel (an email)? Community
    /// and follower platforms expose only audience signals, never addresses;
    /// those candidates belong to the Audience Graph, not to the fans table.
    pub const fn pii_capable(self) -> bool {
        !matches!(self, Self::BandsintownFollowers | Self::RedditCommunity)
    }
}

/// External platforms a fanbase connection can link to. The DB CHECK
/// constraint is the authority; this enum mirrors it for type safety.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Meta,
    Tiktok,
    GoogleAds,
    Reddit,
    Bandsintown,
    Spotify,
    Youtube,
    X,
}

impl Platform {
    pub const ALL: [Platform; 8] = [
        Platform::Meta,
        Platform::Tiktok,
        Platform::GoogleAds,
        Platform::Reddit,
        Platform::Bandsintown,
        Platform::Spotify,
        Platform::Youtube,
        Platform::X,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Meta => "meta",
            Self::Tiktok => "tiktok",
            Self::GoogleAds => "google_ads",
            Self::Reddit => "reddit",
            Self::Bandsintown => "bandsintown",
            Self::Spotify => "spotify",
            Self::Youtube => "youtube",
            Self::X => "x",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|platform| platform.as_str() == value)
    }

    /// Human-readable label for UI display.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Meta => "Meta",
            Self::Tiktok => "TikTok",
            Self::GoogleAds => "Google Ads",
            Self::Reddit => "Reddit",
            Self::Bandsintown => "Bandsintown",
            Self::Spotify => "Spotify",
            Self::Youtube => "YouTube",
            Self::X => "X",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connected,
    Expired,
    Disconnected,
}

impl ConnectionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Expired => "expired",
            Self::Disconnected => "disconnected",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "connected" => Some(Self::Connected),
            "expired" => Some(Self::Expired),
            "disconnected" => Some(Self::Disconnected),
            _ => None,
        }
    }
}

/// What admission does with one candidate given the known state of that
/// address in the workspace. Counters mirror the pilot import so reporting
/// stays uniform across acquisition surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionAction {
    /// No known fan: create `pending` behind the confirmation email.
    CreatePending,
    /// Known pending whose confirmation window lapsed: resend once.
    ResendPending,
    /// Already a confirmed fan: count and skip, never downgrade.
    AlreadyActive,
    /// Opted out at some point: counted, never contacted again.
    SkipSuppressed,
}

#[must_use]
pub fn admission_for(existing_status: Option<&str>) -> AdmissionAction {
    match existing_status {
        None => AdmissionAction::CreatePending,
        Some("pending") => AdmissionAction::ResendPending,
        Some("active") => AdmissionAction::AlreadyActive,
        Some("unsubscribed") | Some("suppressed") => AdmissionAction::SkipSuppressed,
        Some(_) => AdmissionAction::SkipSuppressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_round_trip_is_total() {
        for kind in SourceKind::ALL {
            assert_eq!(SourceKind::from_storage(kind.as_str()), Some(kind));
        }
        assert_eq!(SourceKind::from_storage("webring"), None);
    }

    #[test]
    fn community_platforms_are_not_pii_capable() {
        assert!(SourceKind::CsvInline.pii_capable());
        assert!(SourceKind::MetaLeadAds.pii_capable());
        assert!(SourceKind::HttpJsonPull.pii_capable());
        // Follower/community counts are signals for the graph, not addresses.
        assert!(!SourceKind::BandsintownFollowers.pii_capable());
        assert!(!SourceKind::RedditCommunity.pii_capable());
    }

    #[test]
    fn platform_storage_round_trip_is_total() {
        for platform in Platform::ALL {
            assert_eq!(Platform::from_storage(platform.as_str()), Some(platform));
        }
        assert_eq!(Platform::from_storage("myspace"), None);
    }

    #[test]
    fn connection_status_round_trip_is_total() {
        for status in [
            ConnectionStatus::Connected,
            ConnectionStatus::Expired,
            ConnectionStatus::Disconnected,
        ] {
            assert_eq!(
                ConnectionStatus::from_storage(status.as_str()),
                Some(status)
            );
        }
        assert_eq!(ConnectionStatus::from_storage("pending"), None);
    }

    #[test]
    fn admission_respects_known_status() {
        assert_eq!(admission_for(None), AdmissionAction::CreatePending);
        assert_eq!(
            admission_for(Some("pending")),
            AdmissionAction::ResendPending
        );
        assert_eq!(
            admission_for(Some("active")),
            AdmissionAction::AlreadyActive
        );
        for opted_out in ["unsubscribed", "suppressed"] {
            assert_eq!(
                admission_for(Some(opted_out)),
                AdmissionAction::SkipSuppressed
            );
        }
    }
}
