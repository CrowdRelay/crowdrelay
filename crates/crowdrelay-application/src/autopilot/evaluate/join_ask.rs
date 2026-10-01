//! Join-ask candidate mapping (§5). The domain's `evaluate_join_ask` already
//! applied every gate — connection, cadence, the Instagram photo, the site
//! URL — and returned the asks plus the holds. This module only turns each
//! ask into a durable, idempotent approval-queue action under the usual
//! authority gate, and hands the holds back for the cycle report.
//!
//! The disposition reads the channel's standing publish approval, the same
//! flag `AutoPostPlatforms::permits` consults for agent drafts: a channel
//! the operator already lets publish unattended auto-executes under a
//! bounded-auto policy; every other case parks for a person. The flag can
//! only ever hold a candidate back, never promote one past the policy.

use crowdrelay_domain::join_ask::{
    JoinAskAsk, JoinAskHold, JoinAskSnapshot, join_ask_channel_permits,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// What one join-ask evaluation produced: the candidates to persist and the
/// platforms held back with their reason, `(platform, hold)` — a held
/// platform is work waiting on a person, and the cycle report must say so
/// rather than read as "the evaluator never looked".
pub(super) struct JoinAskEvaluation {
    pub candidates: Vec<DecisionCandidate>,
    pub held: Vec<(String, JoinAskHold)>,
}

/// The ask is fixed — the tenant wrote the words, and the fan-outcome
/// selector chose which one to test/exploit this week. Confidence here answers
/// "is this worth doing at all", not "which wording wins": the followers
/// already on the page are the cheapest fans to win, so the ask is confident
/// whenever it is eligible. Variant choice stays auditable in the input
/// snapshot below.
const JOIN_ASK_CONFIDENCE_BASIS_POINTS: u16 = 7_000;

fn join_ask_candidate(
    ask: &JoinAskAsk,
    snapshot: &JoinAskSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &crowdrelay_domain::content_engine::ContentStrategyPolicy,
    workspace_id: WorkspaceId,
) -> Result<DecisionCandidate, serde_json::Error> {
    let confidence = Confidence::saturating_from_basis_points(JOIN_ASK_CONFIDENCE_BASIS_POINTS);
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    if !join_ask_channel_permits(snapshot, &ask.platform) {
        // The channel has no standing publish approval — the flag can only
        // ever lower the disposition, matching the rule the agent-draft
        // path applies: an auto-post flag is an approval already given, and
        // its absence means a person sees the post first.
        disposition = clamp_disposition(disposition, AutonomyLevel::RequireApproval);
    }
    Ok(DecisionCandidate {
        context: policy.context,
        // Per-platform subject: the inflight-subject index keys on
        // (workspace, context, action_kind, subject_id), and a workspace-wide
        // subject makes every platform's ask collide — one unanswered
        // Facebook ask would park Instagram's lane for its whole window.
        subject: social_channel_subject(workspace_id, &ask.platform),
        decision_kind: "publish_join_ask",
        confidence,
        disposition,
        reason: "weekly join ask; tenant-authored wording selected from first-party fan outcomes",
        input_snapshot: serde_json::json!({
            "capture_context": snapshot.capture_context,
            "platform": ask.platform,
            "week": ask.week_key,
            "variant_index": ask.variant_index,
            "variant_trials": ask.variant_trials,
            "variant_fans": ask.variant_fans,
            "variant_selection": ask.selection_reason,
            "text": ask.text,
            "cta_url": ask.cta_url,
            "image_url": ask.image_url,
            "cadence_days": snapshot.cadence_days,
            // The decision-time prediction: clicks and signups through the
            // tracked link within 7 days. No follower claim — reach.rs reads
            // followers correlational-only, and this ask's honest measure is
            // the link it carries.
            "prediction": "clicks and signups via the tracked link within 7 days; no follower claim",
        }),
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::PublishJoinAsk {
            platform: ask.platform.clone(),
            variant_index: ask.variant_index,
            text: ask.text.clone(),
            cta_url: ask.cta_url.clone(),
            image_url: ask.image_url.clone(),
        },
        // One ask per platform per ISO week, however many cycles run: the
        // week is inside both keys, so a second cycle in the same week
        // conflicts rather than doubling the post.
        decision_key: format!("join_ask:{}:{}", ask.platform, ask.week_key),
        action_idempotency_key: format!("join_ask:{}:{}", ask.platform, ask.week_key),
    })
}

pub(super) fn evaluate_join_ask_candidates(
    snapshot: &JoinAskSnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<JoinAskEvaluation, serde_json::Error> {
    let AutopilotPolicyConfig::ContentStrategy(domain_policy) = &policy.config else {
        return Ok(JoinAskEvaluation {
            candidates: Vec::new(),
            held: Vec::new(),
        });
    };
    let plan = crowdrelay_domain::join_ask::evaluate_join_ask(snapshot, now);
    let mut candidates = Vec::with_capacity(plan.asks.len());
    for ask in &plan.asks {
        candidates.push(join_ask_candidate(
            ask,
            snapshot,
            policy,
            domain_policy,
            workspace_id,
        )?);
    }
    Ok(JoinAskEvaluation {
        candidates,
        held: plan.held,
    })
}
