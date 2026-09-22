// The agent scorecard — one endpoint that shows whether the agent is
// running, what it did, and whether it worked.
//
// Not logs. Results. The operator opens this and sees:
// - Is the agent on?
// - What did it do this week?
// - Did any of it work?
// - What's the track record?
// - What were the last 10 things it actually completed?
//
// Every number is derived from the existing ledger tables — no new state,
// no new writes, no new migrations. This is a read model, not a pipeline.

/// The complete agent scorecard, in one response.
#[derive(Debug, Serialize)]
pub struct AgentScorecard {
    /// Is the agent enabled, and in what posture?
    pub status: AgentStatus,
    /// 7-day action summary: how many executed, succeeded, failed, parked.
    pub week: WeekSummary,
    /// Measured outcomes: did the agent's work actually improve anything?
    pub track_record: TrackRecord,
    /// Actions by context: which parts of the brain are producing work.
    pub by_context: Vec<ContextBreakdown>,
    /// The last 10 completed actions with their outcomes, newest first.
    /// This is not a log — it's results: what the agent did and whether it
    /// worked, in human-readable form.
    pub recent_results: Vec<RecentResult>,
    /// The brain's learning loop state: metacognition, evidence readiness,
    /// and measurement horizon breakdown. Lets the operator see whether
    /// the brain is learning from its actions, not just whether it acted.
    pub learning: LearningState,
}

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub agent_enabled: bool,
    pub dry_run: bool,
    pub posture: Option<String>,
    /// Capabilities with a live executor heartbeat right now.
    pub live_capabilities: Vec<String>,
    /// Capabilities the agent tried to use but no executor advertises.
    /// Empty means the execution plane is healthy.
    pub parked_capabilities: Vec<String>,
    /// When the agent last produced a decision. None if never.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_decision_at: Option<OffsetDateTime>,
    /// When the last action completed (succeeded or failed).
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_action_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct WeekSummary {
    /// Actions that reached a terminal state this week: `succeeded` or
    /// `failed`. Does NOT include `unknown` — see that field.
    pub executed: i64,
    /// Actions whose recorded state is `succeeded`.
    ///
    /// That is what the system currently believes, which is not always the
    /// same as what a provider confirmed: `actions_execution.rs` marks an
    /// externally executed action `succeeded` at dispatch, and the provider's
    /// confirmation arrives later as an execution report. A premature success
    /// is correctable — see `SuccessEvidence` — and counted here until it is
    /// corrected.
    pub succeeded: i64,
    pub failed: i64,
    pub parked: i64,
    pub awaiting_approval: i64,
    /// Actions this week whose outcome could not be established — the external
    /// side effect may or may not have happened.
    ///
    /// These are excluded from `executed`, and therefore from the rate below.
    /// A rate of 10000 next to a large `unknown` is not a week that went
    /// perfectly; it is a week where the ambiguous cases were not counted. They
    /// are shown rather than folded in either direction, because "we cannot
    /// tell" is neither a success nor a failure.
    pub unknown: i64,
    /// Share of `executed` actions recorded as `succeeded`, in basis points.
    /// `None` when nothing reached a terminal state.
    ///
    /// Read it as "of the actions that resolved, how many resolved well",
    /// not as "how often the agent works". The denominator omits `unknown`,
    /// and the numerator counts believed success, not provider-confirmed
    /// success.
    pub success_rate_basis_points: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TrackRecord {
    /// Actions whose outcome was measured as 'improved'.
    pub improved: i64,
    /// Actions whose outcome was measured as 'neutral'.
    pub neutral: i64,
    /// Actions whose outcome was measured as 'worsened'.
    pub worsened: i64,
    /// Actions that executed but have no measured outcome.
    pub unmeasured: i64,
    /// Executed actions whose measurement is scheduled and not yet due.
    ///
    /// Separated from `unmeasured` because they mean opposite things: one is
    /// work nobody will ever be able to judge, the other is a 7, 14 or 30 day
    /// horizon that has not elapsed. Collapsing them reported a healthy system
    /// as 0% covered and told the operator nobody could tell if the work was
    /// paying off, four days before the first result was due.
    pub awaiting_measurement: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_measurement_due_at: Option<OffsetDateTime>,
    /// Share of executed actions that have a measured outcome, in basis
    /// points. Low coverage means the agent is busy but nobody can tell
    /// if the work is paying off.
    pub measurement_coverage_basis_points: Option<u32>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ContextBreakdown {
    pub context: String,
    pub executed: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub parked: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RecentResult {
    pub context: String,
    pub action_kind: String,
    /// Human-readable subject: "event" or "outreach_target" etc.
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub status: String,
    /// The outcome assessment, if measured.
    pub outcome: Option<String>,
    /// The metric that was measured, if any.
    pub metric_key: Option<String>,
    /// The delta in basis points.
    pub delta_basis_points: Option<i32>,
    #[serde(with = "time::serde::rfc3339")]
    pub completed_at: OffsetDateTime,
    /// The executor that confirmed the action, if any.
    pub executor_id: Option<String>,
}

/// The brain's learning loop state. Surfaces the causal chain
/// (action → measurement → evidence → learning → changed decision)
/// so the operator can see whether the brain is getting smarter, not
/// just whether it is busy.
#[derive(Debug, Serialize)]
pub struct LearningState {
    /// The brain's self-assessment from the most recent cycle:
    /// improving, learning, stagnant, regressing, or initializing.
    pub metacognition: Option<String>,
    /// Days on which a belief actually moved, from
    /// `viryaos_brain_belief_revisions`.
    ///
    /// Previously read off the serialized metacognition snapshot, where it
    /// could only ever be 0 or 1: the loader builds a fresh monitor each cycle
    /// and gives it one observation. A system that had learned a thousand
    /// times and one that had never learned both rendered `1`. It now counts
    /// the durable ledger, so an empty ledger reads 0 — which is the honest
    /// number, and the one that makes a stalled learning loop visible.
    pub learning_cycles: i64,
    /// Belief revisions recorded in total.
    ///
    /// Same provenance and the same correction. A revision is the only
    /// durable evidence of improvement the brain writes down, so this counts
    /// those rather than an in-memory tally that did not survive the cycle.
    pub improving_cycles_total: i64,
    /// Evidence rows with at least one partial resolution (3d/7d
    /// checkpoint stamped). The brain can learn from partial evidence
    /// before all horizons finish.
    pub evidence_partial: i64,
    /// Evidence rows fully resolved (all measurements terminal).
    pub evidence_resolved: i64,
    /// Evidence rows with action_id, not yet started learning.
    pub evidence_pending: i64,
    /// When the last partial resolution was stamped.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_partial_resolution_at: Option<OffsetDateTime>,
    /// When the last full resolution was stamped.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_full_resolution_at: Option<OffsetDateTime>,
    /// Measurements pending by horizon, so the operator can see
    /// when the next learning signal will arrive.
    pub measurements_by_horizon: Vec<MeasurementHorizon>,
    /// Evidence rows carrying at least one key in `observed_metrics` — the
    /// secondary-metric write-back the metric posteriors learn from.
    pub evidence_with_metrics: i64,
    /// Distinct metric keys ever written back. The vocabulary the brain is
    /// learning, not just the volume.
    pub metric_keys_observed: i64,
    /// The learned revenue-per-fan exchange the economic value term prices
    /// through. `None` while the tenant's history is too thin for a rate —
    /// the brain itself refuses to answer in that state, so the surface
    /// shows `null`, not a guess.
    pub exchange: Option<ExchangeState>,
}

/// The tenant's realized revenue-per-fan rate, as the causal model's
/// `ValueExchange` currently believes it. Persisted on the `causal_model`
/// brain-state module; shown here so the operator can see what a revenue
/// prediction converts into — and whether it converts at all.
#[derive(Debug, Serialize)]
pub struct ExchangeState {
    /// Distinct day-buckets folded into the rate.
    pub days_observed: u32,
    /// Cumulative new fans across those days.
    pub new_fans: f64,
    /// Cumulative revenue in minor units across the same days.
    pub revenue_minor: f64,
    /// Minor units per new fan. `None` until the exchange's own floors
    /// (enough days, enough fans, nonzero revenue) are met — the same
    /// answer `ValueExchange::minor_per_fan` gives the portfolio.
    pub minor_per_fan: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct MeasurementHorizon {
    /// e.g. "agent_run_signal_installs_7d", "incremental_fan_growth_14d"
    pub kind: String,
    pub pending: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// When the next pending measurement becomes due, if any.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_due_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct StatusRow {
    agent_enabled: bool,
    dry_run: bool,
    posture: Option<String>,
    last_decision_at: Option<OffsetDateTime>,
    last_action_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct CapabilityRow {
    capability: String,
}

#[derive(Debug, FromRow)]
struct ParkedCapabilityRow {
    capability: String,
}

#[derive(Debug, FromRow)]
struct WeekRow {
    executed: i64,
    succeeded: i64,
    failed: i64,
    parked: i64,
    awaiting_approval: i64,
    unknown: i64,
}

#[derive(Debug, FromRow)]
struct TrackRecordRow {
    improved: i64,
    neutral: i64,
    worsened: i64,
    /// Measurement scheduled, horizon not elapsed. Not a coverage failure.
    awaiting_measurement: i64,
    /// No measurement exists and none is coming. The real gap.
    unmeasured: i64,
    next_measurement_due_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MetacognitionRow {
    metacognition: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MetacognitionPayload {
    /// The assessed state, which is the only field of this payload the brain
    /// itself acts on and the only one worth reading here.
    ///
    /// The snapshot also carries `learning_cycles` and
    /// `improving_cycles_total`. They are deliberately not deserialized: the
    /// loader builds a fresh monitor every cycle, so both are always 0 or 1,
    /// and the scorecard now counts `viryaos_brain_belief_revisions` instead.
    /// Leaving the fields here would invite someone to read them again.
    state: Option<String>,
}

/// What the durable belief ledger says about learning that actually happened.
#[derive(Debug, FromRow)]
struct LearningHistoryRow {
    /// Distinct days on which a belief moved. Days, not rows: several
    /// revisions in one cycle are one cycle that learned.
    learning_cycles: i64,
    /// Every recorded revision. Named `improving_cycles_total` on the wire for
    /// compatibility; a revision is the only durable evidence of improvement
    /// the brain writes down.
    belief_revisions: i64,
}

#[derive(Debug, FromRow)]
struct EvidenceRow {
    evidence_partial: i64,
    evidence_resolved: i64,
    evidence_pending: i64,
    last_partial_resolution_at: Option<OffsetDateTime>,
    last_full_resolution_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MeasurementHorizonRow {
    kind: String,
    pending: i64,
    succeeded: i64,
    failed: i64,
    next_due_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MetricCoverageRow {
    evidence_with_metrics: i64,
    metric_keys_observed: i64,
}

/// The exchange fields as `ValueExchange` serializes them on the
/// `causal_model` brain-state blob. Field names are the contract — the api
/// crate deliberately holds no brain dependency, so the struct is mirrored
/// rather than imported.
#[derive(Debug, serde::Deserialize)]
struct PersistedExchange {
    revenue_minor: f64,
    new_fans: f64,
    days_observed: u32,
}

// The two floors `ValueExchange::minor_per_fan` gates on, mirrored from
// crates/crowdrelay-brain/src/value_exchange.rs (MIN_EXCHANGE_DAYS,
// MIN_EXCHANGE_FANS). They are duplicated rather than imported because the
// api crate carries no brain dependency; if the floors move, this read must
// move with them.
const EXCHANGE_MIN_DAYS: u32 = 7;
const EXCHANGE_MIN_FANS: f64 = 20.0;

async fn load_agent_scorecard(
    state: &AppState,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<AgentScorecard, sqlx::Error> {
    let pool = &state.database;

    // 1. Status: agent enabled, posture, last activity.
    // The kill switch and dry-run flag live on the growth envelope, while the
    // posture label lives on the growth posture table. Both are keyed by
    // workspace_id and provisioned on workspace creation, so a CROSS JOIN of
    // two single-row tables is the correct join here.
    let status_row = sqlx::query_as::<_, StatusRow>(
        r#"
        SELECT
            envelope.agent_enabled,
            envelope.dry_run,
            posture.posture,
            (SELECT max(evaluated_at) FROM viryaos_autopilot_decisions
             WHERE workspace_id = $1) AS last_decision_at,
            (SELECT max(finished_at) FROM viryaos_autopilot_actions
             WHERE workspace_id = $1 AND finished_at IS NOT NULL) AS last_action_at
        FROM viryaos_growth_envelope AS envelope
        LEFT JOIN viryaos_growth_posture AS posture
          ON posture.workspace_id = envelope.workspace_id
        WHERE envelope.workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // Live capabilities: executor heartbeats not expired.
    let live_caps = sqlx::query_as::<_, CapabilityRow>(
        r#"
        SELECT DISTINCT capability
        FROM viryaos_executor_capabilities
        WHERE workspace_id = $1 AND expires_at > $2
        ORDER BY capability
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;

    // Parked capabilities: capabilities the agent tried to use but no
    // executor advertises. Derived from actions stuck in 'queued' for
    // over an hour — we infer the capability from the action's durable
    // `action_kind` column (the payload serializes its tag as `kind`, never
    // `action_kind`, so reading it from the payload always read NULL).
    let parked_caps = sqlx::query_as::<_, ParkedCapabilityRow>(
        r#"
        SELECT DISTINCT
            CASE
                WHEN action_kind = 'booking.target_discovery.request' THEN 'booking.discovery'
                WHEN action_kind LIKE 'booking.%' THEN 'booking.outreach'
                WHEN action_kind = 'outreach.discovery.request' THEN 'outreach.discovery'
                WHEN action_kind LIKE 'outreach.%' THEN 'outreach.send'
                WHEN action_kind = 'beacon.discovery.request' THEN 'beacon.discovery'
                WHEN action_kind = 'beacon.invite_batch.request' THEN 'beacon.invite_batch'
                WHEN action_kind LIKE 'beacon.%' THEN 'beacon.outreach'
                WHEN action_kind LIKE 'latarnik.%' THEN 'latarnik.invite'
                WHEN action_kind LIKE 'gig.%' THEN 'gig.outreach'
                WHEN action_kind = 'content.artifact.request' THEN 'content.artifact'
                WHEN action_kind LIKE 'show_growth.%' THEN 'show.growth'
                WHEN action_kind = 'show.task.escalate' THEN 'show.escalation'
                WHEN action_kind = 'opportunity.counterparty_report.issue' THEN 'show.escalation'
                WHEN action_kind = 'release.editorial_pitch.escalate' THEN 'show.escalation'
                WHEN action_kind = 'fan.lifecycle.message.request' THEN 'fan.lifecycle.message'
                WHEN action_kind = 'play.step.run' AND payload->>'step_kind' = 'release_curator_wave'
                    THEN 'play.step.third_party'
                WHEN action_kind = 'play.step.run' THEN 'play.step'
                WHEN action_kind = 'funding.package.prepare' THEN 'funding.package'
                WHEN action_kind LIKE 'funding.%' THEN 'funding.submit'
                WHEN action_kind = 'opportunity.live.apply' THEN 'opportunity.application'
                WHEN action_kind LIKE 'opportunity.terms.%' THEN 'opportunity.terms'
                WHEN action_kind = 'playlist.placement.verify' THEN 'playlist.verify'
                WHEN action_kind = 'promotion.budget_change.request' THEN 'promotion.budget'
                WHEN action_kind = 'representation.approach.request' THEN 'representation.approach'
                WHEN action_kind = 'booking_agent.approach.request' THEN 'booking_agent.approach'
                WHEN action_kind = 'team.assignment.email' THEN 'team.email'
                WHEN action_kind = 'community.engage.request' THEN 'community.engage'
                WHEN action_kind = 'agent.content.request' THEN 'agent.content'
                ELSE action_kind
            END AS capability
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND status = 'queued'
          AND available_at < $2 - INTERVAL '1 hour'
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;

    // 2. Week summary.
    let week_row = sqlx::query_as::<_, WeekRow>(
        r#"
        SELECT
            count(*) FILTER (
                WHERE status IN ('succeeded', 'failed')
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS executed,
            count(*) FILTER (
                WHERE status = 'succeeded'
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS succeeded,
            count(*) FILTER (
                WHERE status = 'failed'
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS failed,
            count(*) FILTER (
                WHERE status = 'queued'
                  AND available_at < $2 - INTERVAL '1 hour'
            )::bigint AS parked,
            count(*) FILTER (
                WHERE status = 'awaiting_approval'
            )::bigint AS awaiting_approval,
            -- `unknown` is not terminal and carries no `finished_at`, so it is
            -- windowed on `updated_at` — the moment the state was recorded.
            count(*) FILTER (
                WHERE status = 'unknown'
                  AND updated_at >= $2 - INTERVAL '7 days'
            )::bigint AS unknown
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(pool)
    .await?;

    let success_rate = if week_row.executed > 0 {
        Some(
            u32::try_from(
                u64::try_from(week_row.succeeded).unwrap_or(0)
                    * 10_000
                    / u64::try_from(week_row.executed).unwrap_or(1),
            )
            .unwrap_or(u32::MAX),
        )
    } else {
        None
    };

    // 3. Track record: all-time measured outcomes.
    let track_row = sqlx::query_as::<_, TrackRecordRow>(
        r#"
        WITH executed_actions AS (
            SELECT id
            FROM viryaos_autopilot_actions
            WHERE workspace_id = $1
              AND status IN ('succeeded', 'failed')
              AND finished_at IS NOT NULL
        )
        SELECT
            count(*) FILTER (WHERE outcome.effect_assessment = 'improved')::bigint AS improved,
            count(*) FILTER (WHERE outcome.effect_assessment = 'neutral')::bigint AS neutral,
            count(*) FILTER (WHERE outcome.effect_assessment = 'worsened')::bigint AS worsened,
            count(*) FILTER (
                WHERE outcome.action_id IS NULL AND pending.action_id IS NOT NULL
            )::bigint AS awaiting_measurement,
            count(*) FILTER (
                WHERE outcome.action_id IS NULL AND pending.action_id IS NULL
            )::bigint AS unmeasured,
            min(pending.due_at) FILTER (WHERE outcome.action_id IS NULL) AS next_measurement_due_at
        FROM executed_actions AS action
        LEFT JOIN LATERAL (
            SELECT effect_assessment, action_id
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = $1
              AND action_id = action.id
            LIMIT 1
        ) AS outcome ON true
        -- An action whose measurement is scheduled but not yet due has not
        -- failed to be measured; its horizon has not elapsed. Counting those
        -- as `unmeasured` made a healthy system report 0% coverage and warn
        -- that "nobody can tell if the work is paying off", when in fact every
        -- one of them had a 7, 14 or 30 day measurement waiting on the clock.
        LEFT JOIN LATERAL (
            SELECT action_id, min(due_at) AS due_at
            FROM viryaos_autopilot_measurements
            WHERE workspace_id = $1
              AND action_id = action.id
              AND status IN ('pending', 'processing')
            GROUP BY action_id
        ) AS pending ON true
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // Coverage is measured-over-measurable. An action whose horizon has not
    // elapsed is not yet answerable, so counting it as a miss would keep the
    // figure near zero for as long as the system keeps working — the newer the
    // action, the worse the score.
    let measured = track_row.improved + track_row.neutral + track_row.worsened;
    let total_executed = measured + track_row.unmeasured;
    let coverage = if total_executed > 0 {
        Some(
            u32::try_from(
                u64::try_from(measured).unwrap_or(0) * 10_000
                    / u64::try_from(total_executed).unwrap_or(1),
            )
            .unwrap_or(u32::MAX),
        )
    } else {
        None
    };

    // 4. By context.
    let contexts = sqlx::query_as::<_, ContextBreakdown>(
        r#"
        SELECT
            context,
            count(*) FILTER (
                WHERE status IN ('succeeded', 'failed')
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS executed,
            count(*) FILTER (
                WHERE status = 'succeeded'
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS succeeded,
            count(*) FILTER (
                WHERE status = 'failed'
                  AND finished_at >= $2 - INTERVAL '7 days'
            )::bigint AS failed,
            count(*) FILTER (
                WHERE status = 'queued'
                  AND available_at < $2 - INTERVAL '1 hour'
            )::bigint AS parked
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
        GROUP BY context
        ORDER BY executed DESC, context
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;

    // 5. Recent results: last 10 completed actions with outcomes.
    let recent = sqlx::query_as::<_, RecentResult>(
        r#"
        SELECT
            action.context,
            action.action_kind,
            action.subject_kind,
            action.subject_id,
            action.status,
            outcome.effect_assessment AS outcome,
            outcome.metric_key,
            outcome.delta_basis_points,
            action.finished_at AS completed_at,
            report.executor_id
        FROM viryaos_autopilot_actions AS action
        LEFT JOIN LATERAL (
            SELECT effect_assessment, metric_key, delta_basis_points
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = $1
              AND action_id = action.id
            LIMIT 1
        ) AS outcome ON true
        LEFT JOIN LATERAL (
            SELECT executor_id
            FROM viryaos_autopilot_execution_reports
            WHERE workspace_id = $1
              AND action_id = action.id
              AND status = 'succeeded'
            ORDER BY occurred_at DESC
            LIMIT 1
        ) AS report ON true
        WHERE action.workspace_id = $1
          AND action.status IN ('succeeded', 'failed')
          AND action.finished_at IS NOT NULL
        ORDER BY action.finished_at DESC
        LIMIT 10
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    // 6. Learning state: metacognition from the most recent decision that
    //    carries a snapshot, evidence readiness, and measurement horizon
    //    breakdown. This is the causal-chain visibility the operator needs
    //    to answer "is the brain getting smarter?" not just "is it busy?".
    let meta_row = sqlx::query_as::<_, MetacognitionRow>(
        r#"
        SELECT (input_snapshot->'snapshot'->'metacognition')::text AS metacognition
        FROM viryaos_autopilot_decisions
        WHERE workspace_id = $1
          AND input_snapshot ? 'snapshot'
          AND input_snapshot->'snapshot' ? 'metacognition'
        ORDER BY evaluated_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;

    let metacognition = meta_row
        .and_then(|row| row.metacognition)
        .and_then(|s| serde_json::from_str::<MetacognitionPayload>(&s).ok())
        .and_then(|m| m.state);

    // Counted from the durable belief ledger, not from the snapshot.
    //
    // `learning_cycles` and `improving_cycles_total` used to be read off the
    // serialized `metacognition` payload. The loader builds a fresh
    // `MetacognitionMonitor` every cycle and feeds it exactly one observation,
    // so both are only ever 0 or 1 — and it says so in a comment. Inside the
    // brain that is harmless, because nothing reads them: `sizing_multiplier`,
    // `exploration_boost` and `state` all depend on the assessed state alone.
    // On the operator scorecard it was a lie with a history-shaped name. A
    // system that had learned a thousand times and one that had never learned
    // both rendered `1`.
    //
    // Persisting the monitor is the wrong fix: this loader also serves the
    // read-only cycle preview, so the write would advance the brain's
    // self-history every time somebody opened the page.
    //
    // `viryaos_brain_belief_revisions` is the record that already exists and
    // already means this. A learning cycle is a day on which a belief actually
    // moved, and an improving one is a revision the module recorded as an
    // improvement. When nothing has been learned this reads 0, which is the
    // honest answer and the one the empty ledger deserves.
    let learning_row = sqlx::query_as::<_, LearningHistoryRow>(
        r#"
        SELECT
            count(DISTINCT recorded_at::date)::bigint AS learning_cycles,
            count(*)::bigint AS belief_revisions
        FROM viryaos_brain_belief_revisions
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;
    let learning_cycles = learning_row.learning_cycles;
    let improving_cycles_total = learning_row.belief_revisions;

    let evidence_row = sqlx::query_as::<_, EvidenceRow>(
        r#"
        SELECT
            count(*) FILTER (WHERE partial_resolution_count > 0 AND resolved_at IS NULL)::bigint AS evidence_partial,
            count(*) FILTER (WHERE resolved_at IS NOT NULL)::bigint AS evidence_resolved,
            count(*) FILTER (WHERE resolved_at IS NULL AND partial_resolution_count = 0)::bigint AS evidence_pending,
            max(last_partial_resolution_at) AS last_partial_resolution_at,
            max(resolved_at) AS last_full_resolution_at
        FROM viryaos_growth_evidence
        WHERE workspace_id = $1
          AND action_id IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    let horizon_rows = sqlx::query_as::<_, MeasurementHorizonRow>(
        r#"
        SELECT
            measurement_kind AS kind,
            count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
            count(*) FILTER (WHERE status = 'succeeded')::bigint AS succeeded,
            count(*) FILTER (WHERE status = 'failed')::bigint AS failed,
            min(due_at) FILTER (WHERE status = 'pending') AS next_due_at
        FROM viryaos_autopilot_measurements
        WHERE workspace_id = $1
        GROUP BY measurement_kind
        ORDER BY min(due_at) FILTER (WHERE status = 'pending') NULLS LAST
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    // The secondary-metric ledger: how many evidence rows carry observed
    // metric write-backs, and how wide the learned vocabulary is. The
    // lateral unnest keeps both counts in one scan — rows whose map is
    // empty drop out of the join, so `evidence_with_metrics` counts only
    // rows that actually recorded a metric.
    let metric_row = sqlx::query_as::<_, MetricCoverageRow>(
        r#"
        SELECT
            count(DISTINCT evidence.id)::bigint AS evidence_with_metrics,
            count(DISTINCT metric.key)::bigint AS metric_keys_observed
        FROM viryaos_growth_evidence AS evidence
        CROSS JOIN LATERAL jsonb_each(evidence.observed_metrics) AS metric
        WHERE evidence.workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // The learned revenue-per-fan rate, from the causal model's persisted
    // checkpoint. The brain refuses to price revenue until its own floors
    // are met — `exchange` is absent in exactly that state, matching the
    // `None` the portfolio sees.
    let exchange_row = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        r#"
        SELECT state -> 'value_exchange'
        FROM viryaos_brain_state
        WHERE workspace_id = $1 AND module = 'causal_model'
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;
    let exchange = exchange_row
        .flatten()
        .and_then(|value| serde_json::from_value::<PersistedExchange>(value).ok())
        .map(|persisted| ExchangeState {
            days_observed: persisted.days_observed,
            new_fans: persisted.new_fans,
            revenue_minor: persisted.revenue_minor,
            minor_per_fan: (persisted.days_observed >= EXCHANGE_MIN_DAYS
                && persisted.new_fans >= EXCHANGE_MIN_FANS
                && persisted.revenue_minor > 0.0)
                .then_some(persisted.revenue_minor / persisted.new_fans),
        });

    Ok(AgentScorecard {
        status: AgentStatus {
            agent_enabled: status_row.agent_enabled,
            dry_run: status_row.dry_run,
            posture: status_row.posture,
            live_capabilities: live_caps.into_iter().map(|r| r.capability).collect(),
            parked_capabilities: parked_caps.into_iter().map(|r| r.capability).collect(),
            last_decision_at: status_row.last_decision_at,
            last_action_at: status_row.last_action_at,
        },
        week: WeekSummary {
            executed: week_row.executed,
            succeeded: week_row.succeeded,
            failed: week_row.failed,
            parked: week_row.parked,
            awaiting_approval: week_row.awaiting_approval,
            unknown: week_row.unknown,
            success_rate_basis_points: success_rate,
        },
        track_record: TrackRecord {
            improved: track_row.improved,
            neutral: track_row.neutral,
            worsened: track_row.worsened,
            unmeasured: track_row.unmeasured,
            awaiting_measurement: track_row.awaiting_measurement,
            next_measurement_due_at: track_row.next_measurement_due_at,
            measurement_coverage_basis_points: coverage,
        },
        by_context: contexts,
        recent_results: recent,
        learning: LearningState {
            metacognition,
            learning_cycles,
            improving_cycles_total,
            evidence_partial: evidence_row.evidence_partial,
            evidence_resolved: evidence_row.evidence_resolved,
            evidence_pending: evidence_row.evidence_pending,
            last_partial_resolution_at: evidence_row.last_partial_resolution_at,
            last_full_resolution_at: evidence_row.last_full_resolution_at,
            measurements_by_horizon: horizon_rows.into_iter().map(|r| MeasurementHorizon {
                kind: r.kind,
                pending: r.pending,
                succeeded: r.succeeded,
                failed: r.failed,
                next_due_at: r.next_due_at,
            }).collect(),
            evidence_with_metrics: metric_row.evidence_with_metrics,
            metric_keys_observed: metric_row.metric_keys_observed,
            exchange,
        },
    })
}

pub async fn scorecard_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match load_agent_scorecard(
        &state,
        state.ops.workspace_id().into_uuid(),
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(scorecard) => private_json(StatusCode::OK, scorecard),
        Err(error) => {
            tracing::warn!(%error, "could not load agent scorecard");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
