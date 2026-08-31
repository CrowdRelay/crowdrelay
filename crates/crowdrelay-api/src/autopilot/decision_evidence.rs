// Decision evidence + learning loop — two read-only endpoints that expose the
// real decision → action → outcome chain persisted in the autopilot tables.
//
// No new tables, no migrations, no fabricated data. Every field comes from an
// existing column in viryaos_autopilot_decisions, viryaos_autopilot_actions,
// or viryaos_autopilot_outcomes. Missing fields within a present stage are
// NOT fabricated — they surface as stage-specific `data_integrity` warnings
// so the operator can see corruption rather than being given a polished but
// false history. Action corruption does NOT imply outcome corruption.
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
/// "Not yet measured", never fabricated success. If an action or outcome row
/// exists but has missing required fields, the corresponding field in
/// `data_integrity` is set and the corrupt entity is surfaced as `None` —
/// distinguishing absence from corruption. Action corruption does NOT
/// imply outcome corruption, and vice versa.
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
    /// `None` for observe_only decisions, decisions that produced no action,
    /// or when an action row exists but has corrupt/missing required fields
    /// (see `data_integrity.action`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<LearningLoopAction>,
    /// The measured outcome of the action, if one was recorded.
    /// `None` when the action hasn't completed, no measurement exists,
    /// or when an outcome row exists but has corrupt/missing required fields
    /// (see `data_integrity.outcome`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LearningLoopOutcome>,
    /// Stage-specific integrity warnings. `action` is set when an action row
    /// exists but has missing required fields; `outcome` is set when an
    /// outcome row exists but has missing required fields. The two are
    /// independent — action corruption does NOT mark the outcome corrupt,
    /// and vice versa. Omitted entirely when both are absent.
    #[serde(skip_serializing_if = "DataIntegrityWarnings::is_empty")]
    pub data_integrity: DataIntegrityWarnings,
}

/// Stage-specific data integrity warnings for a learning loop entry.
/// Each field is independent: action corruption does not imply outcome
/// corruption, and vice versa. The frontend uses the per-stage field to
/// render "Data integrity issue" only for the stage that is actually
/// corrupt, never for a stage that is simply absent.
#[derive(Debug, Default, Serialize)]
pub struct DataIntegrityWarnings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

impl DataIntegrityWarnings {
    fn is_empty(&self) -> bool {
        self.action.is_none() && self.outcome.is_none()
    }
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
    match load_decision_evidence(&state.database, decision_id, state.ops.workspace_id().into_uuid())
        .await
    {
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
    workspace_id: Uuid,
) -> Result<Option<DecisionEvidence>, sqlx::Error> {
    let row = sqlx::query_as::<_, DecisionEvidenceRow>(
        r#"
        SELECT context, decision_kind, subject_kind, subject_id,
               confidence_basis_points, disposition, reason,
               input_snapshot, policy_snapshot, recommendation, evaluated_at
        FROM viryaos_autopilot_decisions
        WHERE id = $1 AND workspace_id = $2
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
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
/// If an action or outcome row exists but has missing required fields, the
/// entry carries stage-specific `data_integrity` warnings instead of
/// fabricating defaults.
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
            -- Action fields (nullable: observe_only decisions have no action).
            -- LATERAL LIMIT 1: multiple actions per decision is invalid/
            -- unsupported state; latest-row selection is a read-model safety
            -- net only, not semantic endorsement of 1:N cardinality.
            a.action_id,
            a.action_kind,
            a.action_status,
            a.action_finished_at,
            -- Outcome fields (nullable: not all actions have measured outcomes)
            o.effect_assessment AS outcome_effect_assessment,
            o.metric_key AS outcome_metric_key,
            o.delta_basis_points AS outcome_delta_basis_points,
            o.observed_at AS outcome_observed_at
        FROM viryaos_autopilot_decisions d
        LEFT JOIN LATERAL (
            SELECT id AS action_id, action_kind, status AS action_status, finished_at AS action_finished_at
            FROM viryaos_autopilot_actions
            WHERE workspace_id = $1 AND decision_id = d.id
            ORDER BY created_at DESC
            LIMIT 1
        ) a ON true
        LEFT JOIN LATERAL (
            SELECT effect_assessment, metric_key, delta_basis_points, observed_at
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = $1
              AND decision_id = d.id
              AND effect_assessment IS NOT NULL
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
        .map(|r| {
            let mut action_warning: Option<String> = None;
            let mut outcome_warning: Option<String> = None;

            // Action: required fields are action_kind and action_status.
            // If the row exists but either is NULL, that's corruption —
            // surface it, don't fabricate.
            let action = r.action_id.and_then(|id| {
                match (r.action_kind.as_ref(), r.action_status.as_ref()) {
                    (Some(kind), Some(status)) => Some(LearningLoopAction {
                        action_id: id,
                        action_kind: kind.clone(),
                        status: status.clone(),
                        finished_at: r.action_finished_at,
                    }),
                    _ => {
                        action_warning = Some(format!(
                            "action {id} exists but has missing required fields"
                        ));
                        None
                    }
                }
            });

            // Outcome: required fields are metric_key, delta_basis_points,
            // and observed_at. If the row exists but any are NULL, that's
            // corruption — surface it, don't fabricate.
            let outcome = r.outcome_effect_assessment.as_ref().and_then(|assessment| {
                match (
                    r.outcome_metric_key.as_ref(),
                    r.outcome_delta_basis_points,
                    r.outcome_observed_at,
                ) {
                    (Some(key), Some(delta), Some(observed)) => Some(LearningLoopOutcome {
                        effect_assessment: assessment.clone(),
                        metric_key: key.clone(),
                        delta_basis_points: delta,
                        observed_at: observed,
                    }),
                    _ => {
                        outcome_warning = Some(
                            "outcome exists but has missing required fields".to_string(),
                        );
                        None
                    }
                }
            });

            if action_warning.is_some() || outcome_warning.is_some() {
                let parts: Vec<&str> = [action_warning.as_deref(), outcome_warning.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                tracing::warn!(
                    decision_id = %r.decision_id,
                    warnings = parts.join("; "),
                    "data integrity issues in learning loop entry"
                );
            }

            LearningLoopEntry {
                decision_id: r.decision_id,
                context: r.context,
                decision_kind: r.decision_kind,
                subject_kind: r.subject_kind,
                subject_id: r.subject_id,
                confidence_basis_points: r.confidence_basis_points,
                disposition: r.disposition,
                reason: r.reason,
                evaluated_at: r.evaluated_at,
                action,
                outcome,
                data_integrity: DataIntegrityWarnings {
                    action: action_warning,
                    outcome: outcome_warning,
                },
            }
        })
        .collect())
}
