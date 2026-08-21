// Delivery-side observability for the Autopilot growth loop.
//
// CrowdRelay creates the consented growth campaigns, but the actual sends are
// performed by n8n workers that claim deliveries over the internal API. A
// queued campaign therefore tells an operator nothing about whether growth is
// running: only the delivery ledger does. This read model exposes that ledger
// so the Control Plane can distinguish "no growth scheduled" from "growth
// scheduled and nobody is draining it".

/// Campaign templates the growth executors and n8n delivery workers share.
///
/// `show.growth.free_fan_push.v1` is the first-party lever template; the two
/// provider templates are created by the Spotify/Bandsintown executors. They
/// are listed explicitly so unrelated campaigns never enter the growth view.
pub const GROWTH_TEMPLATE_KEYS: [&str; 3] = [
    "show.growth.free_fan_push.v1",
    "autopilot.spotify.follow.v1",
    "autopilot.bandsintown.follow.v1",
];

/// Delivery template used by the release -> playlist pitching loop.
pub const PLAYLIST_TEMPLATE_KEY: &str = "release.playlist.v1";

/// A scheduled campaign is considered stalled once it has been due for this
/// long with recipients snapshotted but no delivery attempt claimed. The n8n
/// workers poll every five minutes, so this is well beyond a normal cycle.
pub const GROWTH_STALL_AFTER_MINUTES: i64 = 30;

#[derive(Clone, Debug, Serialize)]
pub struct GrowthCampaignProgress {
    pub campaign_id: String,
    pub slug: String,
    pub name: String,
    pub template_key: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scheduled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    /// Recipients frozen into the consent-filtered snapshot.
    pub recipient_count: i64,
    pub delivered_count: i64,
    pub failed_count: i64,
    /// Claimed but not yet resolved. A persistently non-zero value means a
    /// worker took the delivery and never reported a result.
    pub claimed_count: i64,
    /// Snapshotted recipients with no delivery row at all.
    pub pending_count: i64,
    /// Due, has recipients, and nothing has been claimed yet.
    pub stalled: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GrowthDeliveryTotals {
    pub scheduled_campaigns: i64,
    pub completed_campaigns: i64,
    pub cancelled_campaigns: i64,
    pub delivered: i64,
    pub failed: i64,
    pub pending: i64,
    pub claimed: i64,
    pub stalled_campaigns: i64,
}

/// Outreach progress for the press/playlist pitching side of the same loop.
#[derive(Clone, Debug, Default, Serialize)]
pub struct GrowthOutreachSummary {
    pub active_opportunities: i64,
    pub playlist_opportunities: i64,
    /// Contacted targets that have not replied yet.
    pub awaiting_reply: i64,
    pub replies_14d: i64,
    /// Verified, consenting playlist targets the seeder is allowed to use.
    pub eligible_playlist_targets: i64,
    pub suppressed_targets: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotGrowthOverview {
    /// True when first-party campaign delivery is switched off; every campaign
    /// count below is then historical rather than live.
    pub campaigns_enabled: bool,
    pub totals: GrowthDeliveryTotals,
    pub outreach: GrowthOutreachSummary,
    pub campaigns: Vec<GrowthCampaignProgress>,
}
