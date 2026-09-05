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

/// External platforms a fanbase connection can link to.
///
/// This enum is the authority for the vocabulary, not a mirror of it:
/// `scripts/test_platform_vocabulary_v1.py` fails when the
/// `fanbase_connections_platform_check` constraint and `Platform::ALL`
/// disagree, so a migration cannot add or drop a value on its own. Adding a
/// variant here breaks every `match` below until it is handled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Meta,
    #[serde(rename = "tiktok")]
    Tiktok,
    GoogleAds,
    Reddit,
    Bandsintown,
    Spotify,
    Youtube,
    Facebook,
    Instagram,
    #[serde(rename = "soundcloud")]
    Soundcloud,
    Discord,
    Telegram,
    #[serde(rename = "lastfm")]
    LastFm,
    Deezer,
    Discogs,
    Bluesky,
    Bandcamp,
    X,
}

impl Platform {
    pub const ALL: [Platform; 18] = [
        Platform::Meta,
        Platform::Tiktok,
        Platform::GoogleAds,
        Platform::Reddit,
        Platform::Bandsintown,
        Platform::Spotify,
        Platform::Youtube,
        Platform::Facebook,
        Platform::Instagram,
        Platform::Soundcloud,
        Platform::Discord,
        Platform::Telegram,
        Platform::LastFm,
        Platform::Deezer,
        Platform::Discogs,
        Platform::Bluesky,
        Platform::Bandcamp,
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
            Self::Facebook => "facebook",
            Self::Instagram => "instagram",
            Self::Soundcloud => "soundcloud",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::LastFm => "lastfm",
            Self::Deezer => "deezer",
            Self::Discogs => "discogs",
            Self::Bluesky => "bluesky",
            Self::Bandcamp => "bandcamp",
            Self::X => "x",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|platform| platform.as_str() == value)
    }

    /// Whether the growth metric sync worker polls this platform for an
    /// audience level.
    ///
    /// Meta and Google Ads carry spend, not an audience the brain can grow.
    /// Bandsintown is excluded for a different reason: its tracker counts
    /// arrive through the event sync worker, which already holds the event
    /// context, so polling it here would write the same series twice.
    pub const fn polled_by_growth_metric_sync(self) -> bool {
        match self {
            Self::Meta | Self::GoogleAds | Self::Bandsintown => false,
            // Reddit is polled via the agents service /reddit/observe
            // endpoint, which uses the official OAuth API. The old proxy-based
            // scrape was abandoned (Reddit blocks datacenter IPs), but the
            // API path works from the production host. The subscriber count
            // is a community's size, recorded under the 'social' coverage
            // bucket — not this artist's audience, but a useful reach signal.
            Self::Reddit => true,
            Self::Tiktok
            | Self::Spotify
            | Self::Youtube
            | Self::Facebook
            | Self::Instagram
            | Self::Soundcloud
            | Self::Discord
            | Self::Telegram
            | Self::LastFm
            | Self::Deezer
            | Self::Discogs
            | Self::Bluesky
            | Self::Bandcamp
            | Self::X => true,
        }
    }

    /// Whether a connection to this platform can be synced with no
    /// process-level credential. Discord reads a free public API, Telegram
    /// carries its bot token on the connection row, Reddit goes through the
    /// proxy pool and Spotify mints a token from the public embed page.
    pub const fn syncs_without_process_credential(self) -> bool {
        matches!(
            self,
            Self::Discord
                | Self::Telegram
                | Self::Reddit
                | Self::Spotify
                | Self::Deezer
                | Self::Bluesky
                | Self::Bandcamp
        )
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
            Self::Facebook => "Facebook",
            Self::Instagram => "Instagram",
            Self::Soundcloud => "SoundCloud",
            Self::Discord => "Discord",
            Self::Telegram => "Telegram",
            Self::LastFm => "Last.fm",
            Self::Deezer => "Deezer",
            Self::Discogs => "Discogs",
            Self::Bluesky => "Bluesky",
            Self::Bandcamp => "Bandcamp",
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
    /// Provider proved the identity is wrong (e.g. 404). The sync worker
    /// skips invalid connections.
    Invalid,
    /// Creation-time probe could not establish identity (network error,
    /// rate limit, missing credential). A successful sync promotes it
    /// to `Connected`.
    Unverified,
}

impl ConnectionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Expired => "expired",
            Self::Disconnected => "disconnected",
            Self::Invalid => "invalid",
            Self::Unverified => "unverified",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "connected" => Some(Self::Connected),
            "expired" => Some(Self::Expired),
            "disconnected" => Some(Self::Disconnected),
            "invalid" => Some(Self::Invalid),
            "unverified" => Some(Self::Unverified),
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
    fn platform_all_covers_every_variant() {
        // Exhaustive on purpose: a new variant stops this compiling until it is
        // added to ALL, which is what `from_storage` and the migration contract
        // both read.
        for platform in Platform::ALL {
            match platform {
                Platform::Meta
                | Platform::Tiktok
                | Platform::GoogleAds
                | Platform::Reddit
                | Platform::Bandsintown
                | Platform::Spotify
                | Platform::Youtube
                | Platform::Facebook
                | Platform::Instagram
                | Platform::Soundcloud
                | Platform::Discord
                | Platform::Telegram
                | Platform::LastFm
                | Platform::Deezer
                | Platform::Discogs
                | Platform::Bluesky
                | Platform::Bandcamp
                | Platform::X => {}
            }
        }
        let mut keys: Vec<&str> = Platform::ALL.iter().map(|p| p.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), Platform::ALL.len(), "duplicate storage key");
    }

    #[test]
    fn platform_serde_agrees_with_storage_key() {
        // `rename_all = "snake_case"` renders CamelCase runs with an underscore
        // (`Tiktok` is fine, `TikTok` would be `tik_tok`), so a variant added
        // without a matching rename would serialize a value storage rejects.
        for platform in Platform::ALL {
            let encoded = serde_json::to_string(&platform).expect("serialize");
            assert_eq!(encoded, format!("\"{}\"", platform.as_str()));
            let decoded: Platform = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, platform);
        }
    }

    #[test]
    fn connection_status_round_trip_is_total() {
        for status in [
            ConnectionStatus::Connected,
            ConnectionStatus::Expired,
            ConnectionStatus::Disconnected,
            ConnectionStatus::Invalid,
            ConnectionStatus::Unverified,
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
