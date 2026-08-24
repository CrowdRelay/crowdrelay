// Operator-declared targets, and where they stand.
//
// Nothing about progress is stored. State is derived on read from the series
// the objective names, the same way growth-metric trends are: a stored "on
// track" goes stale silently and a derived one cannot.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclareGrowthObjective {
    pub platform: MetricPlatform,
    pub metric_key: String,
    pub scope: ObjectiveScope,
    pub direction: MetricDirection,
    pub target_value: i64,
    pub deadline: OffsetDateTime,
    /// Who promised it. An objective nobody owns is a wish.
    pub declared_by: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GrowthObjectiveMutation {
    pub operation_id: uuid::Uuid,
    pub objective_id: uuid::Uuid,
    /// The series value frozen as the baseline. `None` when the series had
    /// nothing to say, which the objective then reports as unmeasurable rather
    /// than treating as zero.
    pub baseline_value: Option<i64>,
    pub replayed: bool,
}

/// One objective, with the state its own series implies.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthObjectiveView {
    pub objective_id: uuid::Uuid,
    pub platform: String,
    pub metric_key: String,
    pub scope_kind: String,
    pub scope_id: Option<uuid::Uuid>,
    pub baseline_value: i64,
    pub target_value: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub declared_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub deadline: OffsetDateTime,
    pub declared_by: String,
    /// The latest value of the series, and when it describes. Absent when the
    /// series has nothing to say.
    pub observed_value: Option<i64>,
    /// `met`, `on_track`, `behind`, `missed` or `unmeasurable` — derived here
    /// and never stored.
    pub state: ObjectiveState,
}

#[async_trait]
pub trait AutopilotObjectiveRepository: Send + Sync {
    /// Declares a target and freezes the series' current value as its baseline.
    ///
    /// One live objective per series per scope; re-declaring returns the
    /// existing one rather than opening a second target somebody could pick
    /// between.
    async fn declare_growth_objective(
        &self,
        workspace_id: WorkspaceId,
        command: DeclareGrowthObjective,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthObjectiveMutation, RepositoryError>;

    /// Retires a target without deleting it. A target that was declared and
    /// then removed is exactly what a later review needs to see.
    async fn retire_growth_objective(
        &self,
        workspace_id: WorkspaceId,
        objective_id: uuid::Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthObjectiveMutation, RepositoryError>;

    async fn load_growth_objectives(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthObjectiveView>, RepositoryError>;
}
