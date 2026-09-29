// The per-video scorecard view types, kept in their own chunk so the
// parent stays under the modularity contract's cap.

/// One video's scorecard — the "every new video +1000 CrowdRelay-driven views
/// in 14 days" plan read against one `video` content source.
///
/// Attribution is `None` while the Analytics split was never recorded: zero
/// would claim nobody came, `None` says we cannot tell. The pace label and the
/// missing-reasons list come from `crowdrelay_domain::video_scorecard`, the
/// rows come from the infra read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoScorecardView {
    /// The content source the card is about.
    pub source_id: uuid::Uuid,
    /// `youtube:{video_id}` — the natural key the watcher writes.
    pub source_key: String,
    /// The bare provider id — what a `watch?v=` link or the Analytics
    /// `video==` filter names.
    pub video_id: String,
    pub title: String,
    /// The watch link the source carries.
    pub url: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub published_at: OffsetDateTime,
    /// Whole days since publication.
    pub age_days: i64,
    /// The goal this card scores against, and the window it runs in.
    pub view_target: u64,
    pub window_days: i64,
    /// The attributed-view count the age already expects.
    pub expected_by_now: u64,
    /// Views Analytics attributes to surfaces CrowdRelay touched. `None` means
    /// no `traffic:*` series exists for the source — never a zero stand-in.
    pub attributed_views: Option<u64>,
    /// The public view counter's latest reading.
    pub total_views: Option<u64>,
    /// Views Analytics classes as advertising — the part of `total_views` a
    /// paid campaign bought, when the split was recorded.
    pub ads_views: Option<u64>,
    /// Newest `traffic:*` reading — how fresh the attribution is.
    #[serde(with = "time::serde::rfc3339::option")]
    pub analytics_through: Option<OffsetDateTime>,
    pub pace: crowdrelay_domain::video_scorecard::Pace,
    /// Clicks on this video's tracked links, per lane plus the total. A link
    /// chained onto another of the video's links counts on the first hop only.
    pub tracked_clicks: VideoClickLedger,
    /// What each lane sent, is still holding, or already spent — see the
    /// fields for the per-lane vocabulary.
    pub sends: VideoSendsLedger,
    /// The ordered list of what is missing, most structural first.
    pub missing: Vec<crowdrelay_domain::video_scorecard::MissingReason>,
    /// The account's Reddit standing the way the executor reads it — a halted
    /// account is why `RedditHalted` shows in `missing`.
    pub reddit: VideoRedditStanding,
}

/// The video's tracked-link clicks split by the lane that carried the link.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoClickLedger {
    pub community: u64,
    pub telegram: u64,
    pub discord: u64,
    pub social: u64,
    pub total: u64,
}

/// Post counts one lane holds for the video — the ledger's own statuses
/// grouped into what went out, what is still in flight, and what died.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoPostCounts {
    pub posted: u64,
    /// `pending`, `posting`, `rate_limited`, `awaiting_manual_post`.
    pub waiting: u64,
    pub failed: u64,
    /// The waiting subset parked for an operator to post by hand.
    pub awaiting_manual_post: u64,
}

/// The release's fan-email campaigns and their deliveries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoEmailLedger {
    /// `crowdrelay-release-{plan}-*` campaigns, by their own status.
    pub campaigns_scheduled: u64,
    pub campaigns_completed: u64,
    pub campaigns_cancelled: u64,
    /// Deliveries across those campaigns: `delivered`, `failed`, `claimed`.
    pub delivered: u64,
    pub failed: u64,
    pub claimed: u64,
}

/// Push deliveries the release's campaigns produced.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoPushLedger {
    pub delivered: u64,
    /// Everything still in the pipe: queued, claimed, retry_wait, provider_*.
    pub in_flight: u64,
    pub failed: u64,
}

/// The release's press wave: seeded opportunities against what was pitched
/// and what still waits on the send lane.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoPressLedger {
    pub seeded: u64,
    /// Opportunities whose target already has an outbound touch.
    pub pitched: u64,
    /// Seeded and never pitched — the queue draining at the lane's day cap.
    pub remaining: u64,
    /// Inbound answers with no later outbound touch on the same target.
    pub replies_unanswered: u64,
}

/// The manual curator-DM lane: admitted handle candidates, and how many carry
/// a recorded send. Workspace-wide — candidates are per-band, not per-video.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoCuratorLedger {
    pub unsent: u64,
    pub sent: u64,
}

/// The Reddit standing the scorecard cites, read the way the community
/// executor reads it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoRedditStanding {
    pub open: bool,
    /// The account's post-per-day budget while open.
    pub daily_cap: u32,
    /// Why posting is halted, worded for a person.
    pub hold_reason: Option<String>,
    /// When the halt clears on its own — the removal that keeps it closed
    /// aging out of the standing window.
    #[serde(with = "time::serde::rfc3339::option")]
    pub halted_until: Option<OffsetDateTime>,
}

/// What each lane did for the video, grouped so a card can render
/// "sent / waiting / clicks" without knowing the ledger vocabularies.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoSendsLedger {
    pub community: VideoPostCounts,
    pub telegram: VideoPostCounts,
    pub discord: VideoPostCounts,
    pub social: VideoPostCounts,
    /// `None` only when no release plan exists — with no plan there are no
    /// campaigns to count, and an empty ledger would read as "zero sent".
    pub fan_email: Option<VideoEmailLedger>,
    pub push: Option<VideoPushLedger>,
    pub press: Option<VideoPressLedger>,
    pub curator_queue: VideoCuratorLedger,
    /// YouTube reply drafts the operator approved that never posted.
    pub youtube_replies_approved_waiting: u64,
}
