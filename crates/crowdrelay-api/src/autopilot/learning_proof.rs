// The learning proof — one endpoint that answers "what did the brain change
// because of what happened".
//
// `learning-loop` shows decision → action → outcome, which proves the brain
// acts and that its actions get measured. It cannot show the fourth link:
// that a later decision is different *because* of an earlier outcome. That
// link lives in the belief-revision ledger (migration 0252), which records
// each belief that moved and the actions whose measured outcomes moved it.
//
// One entry here is a complete chain:
//
//   caused_by      the actions that ran, their decisions, and what was
//                  measured about them
//   revision       the belief that moved, from what to what
//   then_influenced  the decisions taken afterwards that acted on the moved
//                  belief, and whether learning is what chose them
//
// Nothing is inferred from timestamps alone. `caused_by` is the ledger's own
// citation, written by the code that performed the update. `then_influenced`
// is matched on the strategy or template the decision recorded in its
// `input_snapshot.learning` block at decision time — not re-derived now,
// against beliefs that have since moved again.

/// The number of revisions returned. The operator reads this newest-first and
/// stops at the first chain that explains what they came to find out.
const REVISION_LIMIT: i64 = 20;

/// Decisions considered as candidates for `then_influenced`, across all
/// revisions in the response. Bounded because a busy tenant produces many
/// decisions per revision and only the first few after a revision say
/// anything about it.
const INFLUENCED_SCAN_LIMIT: i64 = 500;

/// Decisions reported per revision.
const INFLUENCED_PER_REVISION: usize = 5;

#[derive(Debug, Serialize)]
pub struct LearningProof {
    pub entries: Vec<LearningProofEntry>,
    /// True when the ledger holds nothing yet. Distinguishes "the brain has
    /// not changed a belief since this was deployed" from "the endpoint is
    /// broken" — the two look identical in an empty array.
    pub no_revisions_recorded: bool,
}

#[derive(Debug, Serialize)]
pub struct LearningProofEntry {
    pub revision_id: Uuid,
    /// `strategy_posterior` or `hypothesis_state`.
    pub module: String,
    /// For `strategy_posterior`, the posterior cell
    /// `strategy:growth_trend:event_proximity`. For `hypothesis_state`, the
    /// template_id.
    pub belief_key: String,
    pub change_summary: String,
    pub previous_value: serde_json::Value,
    pub current_value: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// What happened, that moved the belief.
    pub caused_by: Vec<ProofCause>,
    /// What the brain did afterwards while holding the moved belief.
    pub then_influenced: Vec<ProofInfluence>,
    /// True when at least one influenced decision recorded that learning, and
    /// not the operator's rules, chose its strategy. This is the whole claim
    /// of the endpoint, and it is deliberately a field rather than a
    /// presentation detail: an entry where it is false is a belief that moved
    /// and has not yet changed a decision, which is a real and different
    /// state.
    pub changed_a_decision: bool,
}

/// One action whose measured outcome moved the belief.
#[derive(Debug, Serialize)]
pub struct ProofCause {
    pub action_id: Uuid,
    pub action_kind: Option<String>,
    pub decision_id: Option<Uuid>,
    pub trace_id: Option<Uuid>,
    /// Why the brain took the action in the first place.
    pub decision_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
    /// What was measured about it. `None` when the evidence row resolved from
    /// a measurement that wrote no assessed outcome row — the belief still
    /// moved, and saying otherwise would invent an assessment.
    pub effect_assessment: Option<String>,
    pub metric_key: Option<String>,
    pub delta_basis_points: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub observed_at: Option<OffsetDateTime>,
}

/// One decision taken after the revision, that acted on the moved belief.
#[derive(Debug, Serialize)]
pub struct ProofInfluence {
    pub decision_id: Uuid,
    pub decision_kind: String,
    pub context: String,
    pub trace_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
    /// The strategy the operator's rules alone would have chosen.
    pub strategy_prior: Option<String>,
    /// The strategy the brain acted on.
    pub strategy_applied: Option<String>,
    /// `posterior` when learned evidence overrode the rules, `prior` when
    /// they agreed, `default` when no world model was available to derive a
    /// strategy from.
    pub strategy_source: Option<String>,
    /// The template the decision dispatched, where it dispatched one.
    pub template_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct RevisionRow {
    id: Uuid,
    module: String,
    belief_key: String,
    previous_value: serde_json::Value,
    current_value: serde_json::Value,
    change_summary: String,
    caused_by_action_ids: Vec<Uuid>,
    recorded_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct CauseRow {
    action_id: Uuid,
    action_kind: Option<String>,
    decision_id: Option<Uuid>,
    trace_id: Option<Uuid>,
    decision_reason: Option<String>,
    decided_at: Option<OffsetDateTime>,
    effect_assessment: Option<String>,
    metric_key: Option<String>,
    delta_basis_points: Option<i32>,
    observed_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct InfluenceRow {
    decision_id: Uuid,
    decision_kind: String,
    context: String,
    trace_id: Option<Uuid>,
    evaluated_at: OffsetDateTime,
    strategy_prior: Option<String>,
    strategy_applied: Option<String>,
    strategy_source: Option<String>,
    template_id: Option<String>,
}

/// `GET /v1/control-plane/autopilot/learning-proof`
///
/// Returns the last 20 belief revisions, each with the actions that caused it
/// and the decisions taken while holding it.
pub async fn learning_proof(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match load_learning_proof(&state.database, state.ops.workspace_id().into_uuid()).await {
        Ok(proof) => private_json(StatusCode::OK, proof),
        Err(error) => {
            tracing::warn!(%error, "could not load the learning proof");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

async fn load_learning_proof(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<LearningProof, sqlx::Error> {
    let revisions = sqlx::query_as::<_, RevisionRow>(
        r#"
        SELECT id, module, belief_key, previous_value, current_value,
               change_summary, caused_by_action_ids, recorded_at
        FROM viryaos_brain_belief_revisions
        WHERE workspace_id = $1
        ORDER BY recorded_at DESC
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(REVISION_LIMIT)
    .fetch_all(pool)
    .await?;

    if revisions.is_empty() {
        return Ok(LearningProof {
            entries: Vec::new(),
            no_revisions_recorded: true,
        });
    }

    let cited: Vec<Uuid> = {
        let mut cited: Vec<Uuid> = revisions
            .iter()
            .flat_map(|revision| revision.caused_by_action_ids.iter().copied())
            .collect();
        cited.sort_unstable();
        cited.dedup();
        cited
    };
    let causes = load_causes(pool, workspace_id, &cited).await?;

    // The oldest revision in the page bounds the decision scan: nothing before
    // it can have been influenced by any revision on this page.
    let earliest = revisions
        .last()
        .map_or_else(OffsetDateTime::now_utc, |revision| revision.recorded_at);
    let influences = load_influences(pool, workspace_id, earliest).await?;

    let entries = revisions
        .into_iter()
        .map(|revision| {
            let caused_by = revision
                .caused_by_action_ids
                .iter()
                .filter_map(|action_id| causes.get(action_id))
                .map(cause_from_row)
                .collect();
            let then_influenced = influences_for(&revision, &influences);
            let changed_a_decision = then_influenced
                .iter()
                .any(|influence| influence.strategy_source.as_deref() == Some("posterior"));
            LearningProofEntry {
                revision_id: revision.id,
                module: revision.module,
                belief_key: revision.belief_key,
                change_summary: revision.change_summary,
                previous_value: revision.previous_value,
                current_value: revision.current_value,
                recorded_at: revision.recorded_at,
                caused_by,
                then_influenced,
                changed_a_decision,
            }
        })
        .collect();

    Ok(LearningProof {
        entries,
        no_revisions_recorded: false,
    })
}

async fn load_causes(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    action_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, CauseRow>, sqlx::Error> {
    if action_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query_as::<_, CauseRow>(
        r#"
        SELECT
            a.id AS action_id,
            a.action_kind,
            a.decision_id,
            d.trace_id,
            d.reason AS decision_reason,
            d.evaluated_at AS decided_at,
            o.effect_assessment,
            o.metric_key,
            o.delta_basis_points,
            o.observed_at
        FROM viryaos_autopilot_actions a
        LEFT JOIN viryaos_autopilot_decisions d
          ON d.workspace_id = a.workspace_id AND d.id = a.decision_id
        -- The outcome the measurement assessed, newest first. An action can
        -- carry several horizons; the belief moved on the incremental one,
        -- and showing every horizon here would bury it.
        LEFT JOIN LATERAL (
            SELECT effect_assessment, metric_key, delta_basis_points, observed_at
            FROM viryaos_autopilot_outcomes
            WHERE workspace_id = a.workspace_id
              AND action_id = a.id
              AND effect_assessment IS NOT NULL
            ORDER BY observed_at DESC
            LIMIT 1
        ) o ON true
        WHERE a.workspace_id = $1 AND a.id = ANY($2)
        "#,
    )
    .bind(workspace_id)
    .bind(action_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|row| (row.action_id, row)).collect())
}

async fn load_influences(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    since: OffsetDateTime,
) -> Result<Vec<InfluenceRow>, sqlx::Error> {
    sqlx::query_as::<_, InfluenceRow>(
        r#"
        SELECT
            id AS decision_id,
            decision_kind,
            context,
            trace_id,
            evaluated_at,
            input_snapshot -> 'learning' ->> 'strategy_prior' AS strategy_prior,
            input_snapshot -> 'learning' ->> 'strategy_applied' AS strategy_applied,
            input_snapshot -> 'learning' ->> 'strategy_source' AS strategy_source,
            recommendation ->> 'template_id' AS template_id
        FROM viryaos_autopilot_decisions
        WHERE workspace_id = $1
          AND evaluated_at >= $2
          -- Only decisions that recorded what learning did to them. A decision
          -- from a build before that block existed cannot say whether the
          -- belief influenced it, and guessing would be the fabrication this
          -- endpoint exists to avoid.
          AND input_snapshot -> 'learning' IS NOT NULL
        ORDER BY evaluated_at ASC
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(INFLUENCED_SCAN_LIMIT)
    .fetch_all(pool)
    .await
}

fn cause_from_row(row: &CauseRow) -> ProofCause {
    ProofCause {
        action_id: row.action_id,
        action_kind: row.action_kind.clone(),
        decision_id: row.decision_id,
        trace_id: row.trace_id,
        decision_reason: row.decision_reason.clone(),
        decided_at: row.decided_at,
        effect_assessment: row.effect_assessment.clone(),
        metric_key: row.metric_key.clone(),
        delta_basis_points: row.delta_basis_points,
        observed_at: row.observed_at,
    }
}

/// The decisions taken after a revision that acted on the belief it moved.
///
/// Matching is by what the decision itself recorded, never by time alone:
/// for a strategy cell, the decision must have applied that strategy; for a
/// hypothesis, it must have dispatched that template. A decision that came
/// after the revision and touched neither is not evidence of anything.
fn influences_for(revision: &RevisionRow, influences: &[InfluenceRow]) -> Vec<ProofInfluence> {
    let subject = match revision.module.as_str() {
        // `strategy:growth_trend:event_proximity` — the strategy is the part
        // a decision records.
        "strategy_posterior" => revision.belief_key.split(':').next().unwrap_or_default(),
        "hypothesis_state" => revision.belief_key.as_str(),
        _ => return Vec::new(),
    };
    influences
        .iter()
        .filter(|influence| influence.evaluated_at > revision.recorded_at)
        .filter(|influence| match revision.module.as_str() {
            "strategy_posterior" => influence.strategy_applied.as_deref() == Some(subject),
            "hypothesis_state" => influence.template_id.as_deref() == Some(subject),
            _ => false,
        })
        .take(INFLUENCED_PER_REVISION)
        .map(|influence| ProofInfluence {
            decision_id: influence.decision_id,
            decision_kind: influence.decision_kind.clone(),
            context: influence.context.clone(),
            trace_id: influence.trace_id,
            evaluated_at: influence.evaluated_at,
            strategy_prior: influence.strategy_prior.clone(),
            strategy_applied: influence.strategy_applied.clone(),
            strategy_source: influence.strategy_source.clone(),
            template_id: influence.template_id.clone(),
        })
        .collect()
}
