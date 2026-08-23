// Predicted show cost against settled show cost.
//
// Two writes in a fixed order, and the order is the contract. A prediction is
// frozen while the show is still ahead; a settlement is reported after it. A
// settlement without a prediction is refused rather than backfilled, because
// recomputing an estimate at settlement time scores today's model against
// itself and always passes.

/// Freezes what the model says a show will cost, with the rates it used.
///
/// The logistics are supplied rather than read from the opportunity row: a show
/// on the calendar may never have been an opportunity, and the operator knows
/// the distance and the deal either way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreezeShowCostPrediction {
    pub event_id: EventId,
    pub distance_km: Option<u32>,
    pub nights_away: Option<u8>,
    pub offered_fee_minor: i64,
    pub application_fee_minor: i64,
}

/// Reports what the show actually cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettleShowCost {
    pub event_id: EventId,
    pub settled: SettledShowCost,
    /// Who is accounting for it. A settlement is somebody's account of what
    /// happened, and an unattributed one cannot be questioned later.
    pub settled_by: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShowCostMutation {
    pub operation_id: uuid::Uuid,
    pub event_id: EventId,
    /// `measured` verdicts carry `calibrated` or `drifting`; a settlement that
    /// could not be scored carries `insufficient` and names why.
    pub accuracy: Option<String>,
    pub accuracy_reason: Option<String>,
    /// True when the write found the work already done. A prediction is frozen
    /// once and a settlement is reported once; retrying either is not an error.
    pub replayed: bool,
}

/// One show, predicted against settled, as an operator reads it.
#[derive(Clone, Debug, Serialize)]
pub struct ShowCostLedgerEntry {
    pub event_id: EventId,
    pub event_title: String,
    pub starts_at: OffsetDateTime,
    pub predicted_at: OffsetDateTime,
    pub offered_fee_minor: i64,
    /// Absent when the estimate was an honest refusal; `prediction_missing_input`
    /// names the input it needed.
    pub predicted_total_cost_minor: Option<i64>,
    pub predicted_net_margin_minor: Option<i64>,
    pub prediction_missing_input: Option<String>,
    pub settled_at: Option<OffsetDateTime>,
    pub settled_by: Option<String>,
    pub settled_total_cost_minor: Option<i64>,
    pub settled_net_margin_minor: Option<i64>,
    pub fee_received_minor: Option<i64>,
    /// `calibrated`, `drifting` or `insufficient`. Absent until a settlement
    /// arrives — an unsettled show has no verdict, not a neutral one.
    pub accuracy: Option<String>,
    pub accuracy_reason: Option<String>,
    pub total_variance_basis_points: Option<i32>,
    /// The line that moved the most money, and what an operator does about it.
    /// Present only on a drifting verdict.
    pub worst_line: Option<String>,
    pub worst_line_delta_minor: Option<i64>,
    pub worst_line_remedy: Option<&'static str>,
    /// What this show says the road actually costs, in minor units per 100 km
    /// of round trip. Evidence for an operator, never applied: a rate is the
    /// band's declaration about their own van, and one show is a data point.
    pub implied_transport_rate_minor_per_100km: Option<i64>,
}

#[async_trait]
pub trait AutopilotShowCostRepository: Send + Sync {
    /// Computes the estimate from the current tour policy and stores it with
    /// that policy, once. A second call returns the frozen row unchanged:
    /// re-freezing after the show would let the goalposts move.
    async fn freeze_show_cost_prediction(
        &self,
        workspace_id: WorkspaceId,
        command: FreezeShowCostPrediction,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ShowCostMutation, RepositoryError>;

    /// Records the settlement and derives the verdict in the same transaction.
    /// Returns `NotFound` when no prediction was frozen for the show.
    async fn settle_show_cost(
        &self,
        workspace_id: WorkspaceId,
        command: SettleShowCost,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ShowCostMutation, RepositoryError>;

    async fn load_show_cost_ledger(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowCostLedgerEntry>, RepositoryError>;
}
