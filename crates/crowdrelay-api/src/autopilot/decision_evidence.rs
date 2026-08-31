// Decision evidence + learning loop — two read-only endpoints that expose the
// real decision → action → outcome chain persisted in the autopilot tables.
//
// No new tables, no migrations, no fabricated data. Every field comes from an
// existing column in viryaos_autopilot_decisions, viryaos_autopilot_actions,
// or viryaos_autopilot_outcomes.
//
// The ranking is lexicographic, NOT a weighted score. The evidence endpoint
// returns the raw input_snapshot and policy_snapshot as-is — the frontend
// renders them as key/value evidence, never as invented numeric contributions.

/// Structured evidence for a single decision — the "Why this decision" data.
///
/// Every field is read directly from `viryaos_autopilot_decisions`.
/// `input_snapshot` and `policy_snapshot` are raw JSON passed through as-is;
/// the frontend renders them as evidence, not as invented explanations.
#[derive(Debug, Serialize)]
pub struct DecisionEvidence {
    pub decision_id: Uuid,
    pub context: String,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub confidence_basis_points: i32,
    pub disposition: String,
    pub reason: String,
    /// The raw signals that fed the decision (metric snapshot, deviation,
    /// trend, etc.). Passed through as-is — never invented or interpreted.
    pub input_snapshot: serde_json::Value,
    /// The policy that governed the decision. Passed through as-is.
    pub policy_snapshot: serde_json::Value,
    /// The recommended action payload. Passed through as-is.
    pub recommendation: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct DecisionEvidenceRow {
    context: String,
    decision_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    confidence_basis_points: i32,
    disposition: String,
    reason: String,
    input_snapshot: serde_json::Value,
    policy_snapshot: serde_json::Value,
    recommendation: serde_json::Value,
    evaluated_at: OffsetDateTime,
}

/// One entry in the learning loop: a decision with its action and outcome
/// where they exist. Missing stages are `None` — the frontend shows
/// "Not yet measured", never fabricated success.
#[derive(Debug, Serialize)]
pub struct LearningLoopEntry {
    pub decision_id: Uuid,
    pub context: String,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub confidence_basis_points: i32,
    pub disposition: String,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
    /// The action that resulted from this decision, if one was created.
    /// `None` for observe_only decisions or decisions that produced no action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<LearningLoopAction>,
    /// The measured outcome of the action, if one was recorded.
    /// `None` when the action hasn't completed or no measurement exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LearningLoopOutcome>,
}

#[derive(Debug, Serialize)]
pub struct LearningLoopAction {
    pub action_id: Uuid,
    pub action_kind: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct LearningLoopOutcome {
    pub effect_assessment: String,
    pub metric_key: String,
    pub delta_basis_points: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct LearningLoopRow {
    decision_id: Uuid,
    context: String,
    decision_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    confidence_basis_points: i32,
    disposition: String,
    reason: String,
    evaluated_at: OffsetDateTime,
    // Action fields — nullable
    action_id: Option<Uuid>,
    action_kind: Option<String>,
    action_status: Option<String>,
    action_finished_at: Option<OffsetDateTime>,
    // Outcome fields — nullable
    outcome_effect_assessment: Option<String>,
    outcome_metric_key: Option<String>,
    outcome_delta_basis_points: Option<i32>,
    outcome_observed_at: Option<OffsetDateTime>,
}

/// `GET /v1/control-plane/autopilot/decisions/{decision_id}/evidence`
///
/// Returns the structured evidence for a single decision. The decision_id
/// comes from the next-best-actions queue (`OpportunityBoardEntry.decision_id`
/// in the frontend), so the operator can inspect exactly why a queued
/// opportunity was raised.
pub async fn decision_evidence(
    State(state): State<AppState>,
    Path(decision_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    match load_decision_evidence(&state.database, decision_id).await {
        Ok(Some(evidence)) => private_json(StatusCode::OK, evidence),
        Ok(None) => Problem::not_found(request_id(&headers)).into_response(),
        Err(error) => {
            tracing::warn!(%error, "could not load decision evidence");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

async fn load_decision_evidence(
    pool: &sqlx::PgPool,
    decision_id: Uuid,
) -> Result<Option<DecisionEvidence>, sqlx::Error> {
    let row = sqlx::query_as::<_, DecisionEvidenceRow>(
        r#"
        SELECT context, decision_kind, subject_kind, subject_id,
               confidence_basis_points, disposition, reason,
               input_snapshot, policy_snapshot, recommendation, evaluated_at
        FROM viryaos_autopilot_decisions
        WHERE id = $1
        "#,
    )
    .bind(decision_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| DecisionEvidence {
        decision_id,
        context: r.context,
        decision_kind: r.decision_kind,
        subject_kind: r.subject_kind,
        subject_id: r.subject_id,
        confidence_basis_points: r.confidence_basis_points,
        disposition: r.disposition,
        reason: r.reason,
        input_snapshot: r.input_snapshot,
        policy_snapshot: r.policy_snapshot,
        recommendation: r.recommendation,
        evaluated_at: r.evaluated_at,
    }))
}

/// `GET /v1/control-plane/autopilot/learning-loop`
///
/// Returns the last 20 decisions with their associated actions and outcomes.
/// The chain is real and traceable: decision → action (via decision_id FK) →
/// outcome (via action_id FK). Missing stages are `None`, not fabricated.
pub async fn learning_loop(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match load_learning_loop(&state.database, state.ops.workspace_id().into_uuid()).await {
        Ok(entries) => private_json(StatusCode::OK, entries),
        Err(error) => {
            tracing::warn!(%error, "could not load learning loop");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

async fn load_learning_loop(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<Vec<LearningLoopEntry>, sqlx::Error> {
    let rows = sqlx::query_as::<_, LearningLoopRow>(
        r#"
        SELECT
            d.id AS decision_id,
            d.context,
            d.decision_kind,
            d.subject_kind,
            d.subject_id,
            d.confidence_basis_points,
            d.disposition,
            d.reason,
            d.evaluated_at,
            -- Action fields (nullable: observe_only decisions have no action)
            a.id AS action_id,
            a.action_kind,
            a.status AS action_status,
            a.finished_at AS action_finished_at,
            -- Outcome fields (nullable: not all actions have measured outcomes)
            o.effect_assessment AS outcome_effect_assessment,
            o.metric_key AS outcome_metric_key,
            o.delta_basis_points AS outcome_delta_basis_points,
            o.observed_at AS outcome_observed_at
        FROM viryaos_autopilot_decisions d
        LEFT JOIN viryaos_autopilot_actions a
          ON a.workspace_id = d.workspace_id AND a.decision_id = d.id
        LEFT JOIN LATERAL (
            SELECT effect_assessment, metric_key, delta_basis_points, observed_at
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = $1
              AND decision_id = d.id
            ORDER BY observed_at DESC
            LIMIT 1
        ) o ON true
        WHERE d.workspace_id = $1
        ORDER BY d.evaluated_at DESC
        LIMIT 20
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| LearningLoopEntry {
            decision_id: r.decision_id,
            context: r.context,
            decision_kind: r.decision_kind,
            subject_kind: r.subject_kind,
            subject_id: r.subject_id,
            confidence_basis_points: r.confidence_basis_points,
            disposition: r.disposition,
            reason: r.reason,
            evaluated_at: r.evaluated_at,
            action: r.action_id.map(|id| LearningLoopAction {
                action_id: id,
                action_kind: r.action_kind.unwrap_or_default(),
                status: r.action_status.unwrap_or_default(),
                finished_at: r.action_finished_at,
            }),
            outcome: r
                .outcome_effect_assessment
                .map(|assessment| LearningLoopOutcome {
                    effect_assessment: assessment,
                    metric_key: r.outcome_metric_key.unwrap_or_default(),
                    delta_basis_points: r.outcome_delta_basis_points.unwrap_or(0),
                    observed_at: r.outcome_observed_at.unwrap_or_else(OffsetDateTime::now_utc),
                }),
        })
        .collect())
}
