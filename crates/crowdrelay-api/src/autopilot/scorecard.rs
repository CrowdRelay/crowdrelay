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
    pub executed: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub parked: i64,
    pub awaiting_approval: i64,
    /// Share of executed actions that succeeded, in basis points.
    /// None when there are no executed actions.
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
}

#[derive(Debug, FromRow)]
struct TrackRecordRow {
    improved: i64,
    neutral: i64,
    worsened: i64,
    unmeasured: i64,
}

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
    // over an hour — we infer the capability from the action_kind.
    let parked_caps = sqlx::query_as::<_, ParkedCapabilityRow>(
        r#"
        WITH parked_payloads AS (
            SELECT payload
            FROM viryaos_autopilot_actions
            WHERE workspace_id = $1
              AND status = 'queued'
              AND available_at < $2 - INTERVAL '1 hour'
        )
        SELECT DISTINCT
            CASE
                WHEN payload->>'action_kind' LIKE 'booking.%' THEN 'booking.outreach'
                WHEN payload->>'action_kind' LIKE 'outreach.%' THEN 'outreach.send'
                WHEN payload->>'action_kind' LIKE 'beacon.%' THEN 'beacon.outreach'
                WHEN payload->>'action_kind' LIKE 'content.%' THEN 'content.artifact'
                WHEN payload->>'action_kind' LIKE 'show_growth.%' THEN 'show.growth'
                WHEN payload->>'action_kind' LIKE 'fan.%' THEN 'fan.lifecycle.message'
                WHEN payload->>'action_kind' LIKE 'play.%' THEN 'play.execute'
                WHEN payload->>'action_kind' LIKE 'funding.%' THEN 'funding.submit'
                WHEN payload->>'action_kind' LIKE 'opportunity.%' THEN 'opportunity.application'
                ELSE payload->>'action_kind'
            END AS capability
        FROM parked_payloads
        WHERE payload->>'action_kind' IS NOT NULL
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
            )::bigint AS awaiting_approval
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
            count(*) FILTER (WHERE outcome.action_id IS NULL)::bigint AS unmeasured
        FROM executed_actions AS action
        LEFT JOIN LATERAL (
            SELECT effect_assessment, action_id
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = $1
              AND action_id = action.id
            LIMIT 1
        ) AS outcome ON true
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    let total_executed =
        track_row.improved + track_row.neutral + track_row.worsened + track_row.unmeasured;
    let measured = track_row.improved + track_row.neutral + track_row.worsened;
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
            success_rate_basis_points: success_rate,
        },
        track_record: TrackRecord {
            improved: track_row.improved,
            neutral: track_row.neutral,
            worsened: track_row.worsened,
            unmeasured: track_row.unmeasured,
            measurement_coverage_basis_points: coverage,
        },
        by_context: contexts,
        recent_results: recent,
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
