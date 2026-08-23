// Ingress and read-model port for external growth metrics. Kept separate from
// the evaluator port so operator and provider concerns cannot leak into
// decision code, and separate from the other state ports because a metric
// observation is evidence about the outside world rather than a change to
// first-party business state.

/// Optional first-party subject a series annotates. Deliberately a loose
/// reference: external evidence must never block deletion of the business row
/// it describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrowthMetricSubject {
    Event(EventId),
    City(CityId),
    ReleasePlan(ReleasePlanId),
    ContentSource(ContentSourceId),
    Beacon(BeaconId),
}

impl GrowthMetricSubject {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Event(_) => "event",
            Self::City(_) => "city",
            Self::ReleasePlan(_) => "release_plan",
            Self::ContentSource(_) => "content_source",
            Self::Beacon(_) => "beacon",
        }
    }

    #[must_use]
    pub fn uuid(self) -> uuid::Uuid {
        match self {
            Self::Event(id) => id.into_uuid(),
            Self::City(id) => id.into_uuid(),
            Self::ReleasePlan(id) => id.into_uuid(),
            Self::ContentSource(id) => id.into_uuid(),
            Self::Beacon(id) => id.into_uuid(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpsertGrowthMetricSeries {
    pub platform: MetricPlatform,
    pub metric_key: String,
    pub subject: Option<GrowthMetricSubject>,
    pub display_name: String,
    pub direction: MetricDirection,
    pub value_tier: MetricValueTier,
    pub expected_interval_hours: u32,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct GrowthMetricSeriesMutation {
    pub operation_id: uuid::Uuid,
    pub series_id: GrowthMetricSeriesId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct RecordGrowthMetricPoint {
    pub series_id: GrowthMetricSeriesId,
    /// The provider's own observation time, not ingestion time. Re-delivering
    /// the same observation is a no-op and out-of-order delivery still lands in
    /// the right place on the timeline.
    pub captured_at: OffsetDateTime,
    pub value: i64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GrowthMetricPointMutation {
    pub operation_id: uuid::Uuid,
    pub series_id: GrowthMetricSeriesId,
    pub replayed: bool,
    /// False when an observation already existed at that `captured_at`. The
    /// stored value is left untouched: a provider correcting history is a
    /// different operation from reporting it, and silently overwriting would
    /// make an already-derived trend irreproducible.
    pub accepted: bool,
}

/// One series as an operator sees it. Absent windows are reported as `null`
/// rather than `0`, because "we have no observation that old" and "the number
/// did not move" are different facts and only one of them is actionable.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthMetricTrendView {
    pub series_id: GrowthMetricSeriesId,
    pub platform: MetricPlatform,
    pub metric_key: String,
    pub display_name: String,
    pub subject_kind: Option<String>,
    pub subject_id: Option<uuid::Uuid>,
    pub direction: MetricDirection,
    pub value_tier: MetricValueTier,
    pub expected_interval_hours: u32,
    pub latest_value: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub latest_at: OffsetDateTime,
    pub delta_24h: Option<i64>,
    pub delta_7d: Option<i64>,
    pub delta_28d: Option<i64>,
    pub velocity_milli_per_day: Option<i64>,
    pub baseline_milli_per_day: Option<i64>,
    /// Recent velocity against the series' own baseline. `10_000` is exactly on
    /// baseline. Absent when the baseline is too flat to divide by.
    pub velocity_ratio_basis_points: Option<u32>,
    pub points_in_window: u32,
    pub age_seconds: i64,
    pub stale: bool,
}

#[async_trait]
pub trait AutopilotGrowthMetricRepository: Send + Sync {
    /// Declares or updates the identity of one tracked number. Matching is by
    /// `(platform, metric_key, subject)`, so re-declaring a series is an update
    /// and never a second timeline for the same thing.
    async fn upsert_growth_metric_series(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertGrowthMetricSeries,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthMetricSeriesMutation, RepositoryError>;

    async fn record_growth_metric_point(
        &self,
        workspace_id: WorkspaceId,
        command: RecordGrowthMetricPoint,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthMetricPointMutation, RepositoryError>;

    async fn load_growth_metric_trends(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricTrendView>, RepositoryError>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct FirstPartyGrowthMetricReport {
    pub series_tracked: u32,
    pub series_retired: u32,
    pub points_recorded: u32,
}

/// Kept apart from the operator ingress port so the background worker depends
/// on exactly one method and can never reach the operator write path.
#[async_trait]
pub trait AutopilotFirstPartyGrowthMetrics: Send + Sync {
    /// Records the first-party numbers CrowdRelay already owns as ordinary
    /// metric observations, so tickets, fans and merch sit on the same timeline
    /// as anything an external adapter reports.
    ///
    /// This exists because the strongest metrics in the system need no provider
    /// at all: routing them through the ingest endpoint would mean an adapter
    /// reading our own database back to us, with a delivery failure mode we do
    /// not have to accept.
    async fn materialize_first_party_growth_metrics(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<FirstPartyGrowthMetricReport, RepositoryError>;
}
