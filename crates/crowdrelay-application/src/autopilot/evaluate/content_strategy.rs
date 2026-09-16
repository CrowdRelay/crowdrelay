//! Content-strategy candidate mapping. The engine already ranked and
//! filtered; this module only turns each `raised` suggestion or `proposed`
//! arc into a durable, idempotent approval-queue action under the usual
//! authority gate.

use crowdrelay_domain::content_engine::{Arc, ContentSuggestion};
use serde_json::Value;

use super::{policy_evidence, *};

/// Confidence is not the rank score: the EFE is an ordering, not a
/// probability. What a queue entry can honestly claim is the tier of
/// evidence behind it — the capability filter passed, the promise is
/// real, and then each corroborating clause is worth something. The base
/// clears an unremarkable floor; the bonuses say how much evidence the
/// suggestion is standing on.
fn suggestion_confidence(suggestion: &ContentSuggestion) -> Confidence {
    let mut basis_points: u32 = 6_000;
    let evidence = &suggestion.evidence;
    if evidence
        .get("trend_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| !ids.is_empty())
    {
        basis_points = basis_points.saturating_add(1_500);
    }
    if evidence.get("arc_format_key_hit").and_then(Value::as_bool) == Some(true) {
        basis_points = basis_points.saturating_add(1_000);
    }
    if evidence
        .get("covered_by_production")
        .and_then(Value::as_bool)
        == Some(true)
    {
        basis_points = basis_points.saturating_add(1_000);
    }
    Confidence::saturating_from_basis_points(u16::try_from(basis_points).unwrap_or(u16::MAX))
}

pub(super) fn content_strategy_candidate(
    suggestion: &ContentSuggestion,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ContentStrategy(domain_policy) = &policy.config else {
        return Ok(None);
    };
    // The operator's floor reads the score the engine wrote into the row.
    // A suggestion without one is corrupt — a configured floor cannot be
    // checked against a missing number, so it does not reach the queue.
    if let Some(floor) = domain_policy.minimum_efe_score {
        let Some(score) = suggestion.evidence.get("efe_score").and_then(Value::as_f64) else {
            return Ok(None);
        };
        if score < floor {
            return Ok(None);
        }
    }
    let confidence = suggestion_confidence(suggestion);
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::ContentSuggestion(suggestion.id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "raise_content_suggestion",
        confidence,
        disposition,
        reason: "a feasible beat with a real distribution promise",
        // The whole row, spelled out: a queue entry must carry what it is
        // asking about, not an id somebody has to join back to read.
        input_snapshot: serde_json::json!({
            "suggestion_id": suggestion.id.into_uuid(),
            "arc_id": suggestion.arc_id.map(|id| id.into_uuid()),
            "format_key": suggestion.format_key,
            "concept": suggestion.concept,
            "reason": suggestion.reason,
            "evidence": suggestion.evidence,
            "suggested_after": suggestion.suggested_after.map(|date| date.to_string()),
            "suggested_before": suggestion.suggested_before.map(|date| date.to_string()),
            "effort": suggestion.effort.map(|effort| effort.as_str()),
            "proposed_assignee_member_id": suggestion
                .proposed_assignee_member_id
                .map(|id| id.into_uuid()),
            "distribution_promise": suggestion.distribution_promise,
            "expires_at": suggestion.expires_at.map(|at| at.to_string()),
        }),
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RaiseContentSuggestion {
            suggestion_id: suggestion.id,
            format_key: suggestion.format_key.clone(),
            concept: suggestion.concept.clone(),
            reason: suggestion.reason.clone(),
            distribution_promise: suggestion.distribution_promise.clone(),
        },
        // The policy version salts the key the way every other context's
        // does: an operator tightening the floor gets fresh decisions rather
        // than rows still wearing the old evidence.
        decision_key: format!(
            "decision:content-strategy:v{}:{}",
            policy.version,
            suggestion.id.into_uuid()
        ),
        // One queue entry per suggestion, ever. A cancelled ask is the
        // band's answer — re-raising the same row would be the engine
        // arguing with the person who already said no.
        action_idempotency_key: format!("action:content-suggestion:{}", suggestion.id.into_uuid()),
    }))
}

/// The arc ask sits above the beat asks: approving it is the season's one
/// creative decision, so its confidence reads what the proposal rests on —
/// a real dated anchor is the base, corroborated trends are the bonus.
fn arc_confidence(arc: &Arc) -> Confidence {
    let mut basis_points: u32 = 6_500;
    if arc
        .evidence
        .get("trend_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| !ids.is_empty())
    {
        basis_points = basis_points.saturating_add(1_500);
    }
    Confidence::saturating_from_basis_points(u16::try_from(basis_points).unwrap_or(u16::MAX))
}

pub(super) fn content_arc_candidate(
    arc: &Arc,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ContentStrategy(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let confidence = arc_confidence(arc);
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ContentArc(arc.id),
        decision_kind: "raise_content_arc",
        confidence,
        disposition,
        reason: "a season's shape built around real dated material",
        input_snapshot: serde_json::json!({
            "arc_id": arc.id.into_uuid(),
            "title": arc.title,
            "summary": arc.summary,
            "horizon_start": arc.horizon_start.map(|date| date.to_string()),
            "horizon_end": arc.horizon_end.map(|date| date.to_string()),
            "spine": arc.spine,
            "evidence": arc.evidence,
        }),
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RaiseContentArc {
            arc_id: arc.id,
            title: arc.title.clone(),
            summary: arc.summary.clone(),
            horizon_start: arc.horizon_start.map(|date| date.to_string()),
            horizon_end: arc.horizon_end.map(|date| date.to_string()),
            beats: u32::try_from(arc.spine.as_array().map_or(0, std::vec::Vec::len))
                .unwrap_or(u32::MAX),
        },
        decision_key: format!(
            "decision:content-strategy:v{}:arc:{}",
            policy.version,
            arc.id.into_uuid()
        ),
        // One ask per arc, ever — a declined arc retires and its anchor
        // cools down; re-raising it would argue with the band's answer.
        action_idempotency_key: format!("action:content-arc:{}", arc.id.into_uuid()),
    }))
}
