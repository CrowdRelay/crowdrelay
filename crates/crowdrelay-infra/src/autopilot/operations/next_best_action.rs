//! The cross-context Next Best Action queue.
//!
//! One query assembles candidates from rows that already exist — a decision,
//! the action it produced, and the subject's own date — and the domain ranks
//! them. Nothing is stored: a denormalized queue table would start disagreeing
//! with its own evidence the moment an action succeeded.
//!
//! The SQL selects a bounded candidate window and does no ordering that
//! matters. Ranking lives in `crowdrelay_domain::next_best_action` so the
//! ordering an operator sees is the one covered by unit tests, not a second
//! `ORDER BY` that drifts from it.

use super::*;
use crowdrelay_application::autopilot::{AutopilotActionPayload, NextBestAction};
use crowdrelay_domain::plays::PlayKind;
use crowdrelay_domain::{
    growth_metrics::MetricValueTier,
    next_best_action::{AuthorityState, QueueCandidate, rank_next_best_actions},
};

/// Candidate window fed to the ranker. Wider than the ten entries that survive,
/// because the top ten by rank are not the ten most recent — and far narrower
/// than the full decision history, which would be an unbounded scan.
const MAX_QUEUE_CANDIDATES: i64 = 200;

#[derive(Debug, FromRow)]
struct QueueRow {
    decision_id: Uuid,
    action_id: Option<Uuid>,
    context: String,
    decision_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    confidence_basis_points: i32,
    reason: String,
    disposition: String,
    payload: Option<Value>,
    action_status: Option<String>,
    due_at: Option<OffsetDateTime>,
}

/// Pulls the two ranking inputs the payload can supply.
///
/// Returns `(value_tier, deviation_basis_points, recommended_action)`. A
/// payload that carries none of them yields `None`s rather than a default: an
/// invented tier would let an unmeasured action outrank a measured one.
fn payload_signals(
    payload: Option<&Value>,
) -> (Option<MetricValueTier>, Option<u32>, Option<String>) {
    let Some(payload) = payload else {
        return (None, None, None);
    };
    let Ok(payload) = serde_json::from_value::<AutopilotActionPayload>(payload.clone()) else {
        // A payload shape this build does not understand is evidence we cannot
        // read, not evidence of zero.
        return (None, None, None);
    };
    match payload {
        AutopilotActionPayload::RaiseGrowthOpportunity {
            deviation_basis_points,
            recommended_action,
            ..
        } => (None, Some(deviation_basis_points), Some(recommended_action)),
        AutopilotActionPayload::RaiseGrowthDebt {
            debt_kind,
            overdue_basis_points,
            recommended_action,
            ..
        } => (
            Some(debt_kind.value_tier()),
            // Overdue is expressed against a horizon where `10_000` is "exactly
            // due", so the comparable magnitude is how far past it went.
            Some(overdue_basis_points.saturating_sub(10_000)),
            Some(recommended_action),
        ),
        other => (None, None, Some(other.action_kind().to_owned())),
    }
}

pub(in crate::autopilot) async fn load_next_best_actions(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<NextBestAction>, RepositoryError> {
    let rows = sqlx::query_as::<_, QueueRow>(
        r#"
        SELECT
            decision.id AS decision_id,
            action.action_id,
            decision.context,
            decision.decision_kind,
            decision.subject_kind,
            decision.subject_id,
            decision.confidence_basis_points,
            decision.reason,
            decision.disposition,
            action.payload,
            action.action_status,
            deadline.due_at
        FROM viryaos_autopilot_decisions AS decision
        LEFT JOIN LATERAL (
            SELECT candidate.id AS action_id, candidate.payload, candidate.approval_expires_at,
                   candidate.status AS action_status
            FROM viryaos_autopilot_actions AS candidate
            WHERE candidate.workspace_id = decision.workspace_id
              AND candidate.decision_id = decision.id
            ORDER BY candidate.created_at DESC, candidate.id DESC
            LIMIT 1
        ) AS action ON true
        LEFT JOIN LATERAL (
            -- Only dates that already exist on the subject itself. There is no
            -- fallback: a finding with no real deadline reports none rather
            -- than borrowing one from somewhere plausible.
            SELECT min(due_at) AS due_at FROM (
                SELECT event.starts_at AS due_at
                FROM events AS event
                WHERE decision.subject_kind = 'event'
                  AND event.workspace_id = decision.workspace_id
                  AND event.id = decision.subject_id
                UNION ALL
                SELECT plan.release_at
                FROM viryaos_release_plans AS plan
                WHERE decision.subject_kind = 'release_plan'
                  AND plan.workspace_id = decision.workspace_id
                  AND plan.id = decision.subject_id
                UNION ALL
                SELECT opportunity.deadline
                FROM viryaos_team_opportunities AS opportunity
                WHERE decision.subject_kind = 'team_opportunity'
                  AND opportunity.workspace_id = decision.workspace_id
                  AND opportunity.id = decision.subject_id
                UNION ALL
                SELECT action.approval_expires_at
                WHERE action.approval_expires_at IS NOT NULL
            ) AS dates
        ) AS deadline ON true
        WHERE decision.workspace_id = $1
          AND decision.evaluated_at >= $2 - INTERVAL '7 days'
          -- 'deny' is excluded: the gate refused it, and listing it as work to
          -- do would invite someone to override a policy from a list view.
          AND decision.disposition <> 'deny'
          -- A finding a human says they handled themselves is done, not
          -- pending. The ledger row is written by the handled-externally
          -- endpoint; without this exclusion the queue would keep proposing
          -- work somebody already did.
          AND NOT EXISTS (
              SELECT 1 FROM operator_actions AS handled
              WHERE handled.workspace_id = decision.workspace_id
                AND handled.action = 'handle_autopilot_decision_externally'
                AND handled.target_type = 'autopilot_decision'
                AND handled.target_id = decision.id
          )
          -- Work whose newest action already left the queue is not next. The
          -- lateral join above carries that action's status: a `failed` action
          -- is as terminal as a succeeded or cancelled one (the executor
          -- claims only `queued` or stale `processing`), so keeping such a
          -- finding listed would park a dead button in front of the operator,
          -- whose every click comes back as a conflict.
          AND (action.action_status IS NULL
               OR action.action_status NOT IN ('succeeded', 'cancelled', 'failed'))
          -- Only the newest decision per subject and kind. An evidence refresh
          -- writes a new decision row, and the queue must show the finding
          -- once, not once per cycle it survived.
          AND NOT EXISTS (
              SELECT 1 FROM viryaos_autopilot_decisions AS newer
              WHERE newer.workspace_id = decision.workspace_id
                AND newer.subject_kind = decision.subject_kind
                AND newer.subject_id = decision.subject_id
                AND newer.decision_kind = decision.decision_kind
                AND (newer.evaluated_at, newer.id) > (decision.evaluated_at, decision.id)
          )
        ORDER BY decision.evaluated_at DESC, decision.id DESC
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_QUEUE_CANDIDATES)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    // The series an operator has live targets on. Loaded once: an objective is
    // a workspace-level fact and re-reading it per candidate would be a query
    // per queue entry for an answer that cannot change mid-read.
    let targeted: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT platform, metric_key
        FROM viryaos_growth_objectives
        WHERE workspace_id = $1
          AND retired_at IS NULL
          -- A deadline that has passed is history. History must not keep
          -- promoting work up the queue.
          AND deadline > $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    let mut candidates = Vec::with_capacity(rows.len());
    let mut identity: HashMap<(Uuid, String), (Uuid, Option<Uuid>)> =
        HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(authority) = AuthorityState::from_disposition(&row.disposition) else {
            continue;
        };
        // The disposition says what was decided; the action row says where the
        // work actually is. An approved-and-queued finding must not keep
        // presenting itself as blocked on a human: the operator would click
        // approve again and collect a conflict for a decision already made.
        let authority = match (row.action_status.as_deref(), authority) {
            (Some("queued" | "processing"), AuthorityState::AwaitingApproval) => {
                AuthorityState::AutoExecuting
            }
            (_, other) => other,
        };
        let context = super::parse_context(&row.context)?;
        let (value_tier, deviation_basis_points, recommended_action) =
            payload_signals(row.payload.as_ref());
        // The queue dedupes to one row per (subject, kind), so this key
        // identifies the finding the operator is looking at.
        identity.insert(
            (row.subject_id, row.decision_kind.clone()),
            (row.decision_id, row.action_id),
        );
        candidates.push((
            row.due_at,
            QueueCandidate {
                context: context.as_str(),
                decision_kind: row.decision_kind,
                subject_kind: row.subject_kind,
                subject_id: row.subject_id,
                authority,
                confidence: super::parse_confidence(row.confidence_basis_points)?,
                reason: row.reason,
                recommended_action: recommended_action.unwrap_or_default(),
                hours_until_deadline: row.due_at.map(|due_at| (due_at - now).whole_hours()),
                value_tier,
                deviation_basis_points,
                // Phase 5 fills this. Reading it as "no measured record" is
                // correct today and stays correct once measurements exist.
                measured_effect: None,
                contributes_to_objective: payload_series(row.payload.as_ref()).is_some_and(
                    |(platform, metric_key)| {
                        targeted
                            .iter()
                            .any(|(target, key)| *target == platform && *key == metric_key)
                    },
                ),
            },
        ));
    }

    let due_at_by_subject: HashMap<(Uuid, String), OffsetDateTime> = candidates
        .iter()
        .filter_map(|(due_at, candidate)| {
            due_at.map(|at| ((candidate.subject_id, candidate.decision_kind.clone()), at))
        })
        .collect();

    let ranked = rank_next_best_actions(
        candidates
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect(),
    );

    ranked
        .into_iter()
        .map(|entry| {
            let due_at = due_at_by_subject
                .get(&(
                    entry.candidate.subject_id,
                    entry.candidate.decision_kind.clone(),
                ))
                .copied();
            // Every surviving entry came from the identity map, so a miss
            // here is impossible; falling back would misaddress an operator's
            // "we did this ourselves" click, which is worse than failing.
            let (decision_id, action_id) = identity
                .get(&(
                    entry.candidate.subject_id,
                    entry.candidate.decision_kind.clone(),
                ))
                .copied()
                .ok_or(RepositoryError::Unexpected)?;
            Ok(NextBestAction {
                position: entry.position,
                decision_id,
                action_id,
                context: super::parse_context(entry.candidate.context)?,
                decision_kind: entry.candidate.decision_kind,
                subject_kind: entry.candidate.subject_kind,
                subject_id: entry.candidate.subject_id,
                authority: entry.candidate.authority,
                confidence: entry.candidate.confidence,
                reason: entry.candidate.reason,
                recommended_action: entry.candidate.recommended_action,
                ranked_by: entry.ranked_by,
                consequence: entry.consequence.to_owned(),
                due_at,
                value_tier: entry.candidate.value_tier,
                deviation_basis_points: entry.candidate.deviation_basis_points,
            })
        })
        .collect()
}

/// The series a finding moves, when it names one.
///
/// Two payloads can say: a growth-metric opportunity carries the series
/// directly, and a play step carries the play whose success metric is declared
/// in the domain. Everything else moves a number nobody can name from the
/// payload alone, and guessing one would let an unrelated finding ride an
/// objective up the queue.
fn payload_series(payload: Option<&Value>) -> Option<(String, String)> {
    let payload = payload?;
    match payload.get("kind").and_then(Value::as_str)? {
        "raise_growth_opportunity" => Some((
            payload.get("platform")?.as_str()?.to_owned(),
            payload.get("metric_key")?.as_str()?.to_owned(),
        )),
        "run_play_step" => {
            let kind = PlayKind::parse(payload.get("play_kind")?.as_str()?)?;
            let (platform, metric_key) = kind.success_metric();
            Some((platform.to_owned(), metric_key.to_owned()))
        }
        _ => None,
    }
}
