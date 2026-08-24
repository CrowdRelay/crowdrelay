//! Turning a play's state machine into the two things the rest of the system
//! already understands: a durable decision, and a settled row.
//!
//! Everything here is a translation. The judgement — whether a step is due,
//! whether it may still be sent, whether the play is finished — belongs to
//! `crowdrelay_domain::plays` and is not repeated. What this module owns is the
//! part the domain cannot see: which anchor is worth a play at all, and which
//! recipient the next send is for.

use super::*;

/// Rebuilds the domain's view from the adapter's.
///
/// The audience is one value in this crate and two fields in the domain, so the
/// conversion happens exactly here. Doing it at the call site is how a count
/// and a recipient end up disagreeing.
fn domain_snapshot(snapshot: &PlayRunSnapshot) -> PlaySnapshot {
    PlaySnapshot {
        play_id: snapshot.play_id,
        kind: snapshot.kind,
        anchor_at: snapshot.anchor_at,
        anchor_active: snapshot.anchor_active,
        steps: snapshot.steps.clone(),
        eligible_recipients: snapshot.audience.remaining(),
    }
}

/// What the play wants to do next, under the operator's configured policy.
pub(super) fn play_decision(
    snapshot: &PlayRunSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Option<PlayDecision> {
    let AutopilotPolicyConfig::Plays(domain_policy) = policy.config else {
        return None;
    };
    Some(evaluate_play(
        &domain_snapshot(snapshot),
        domain_policy,
        now,
    ))
}

/// The play to start for this anchor, or `None` when the anchor cannot carry
/// one.
///
/// The schedule is resolved here and then stored, never recomputed: a play
/// whose windows moved when the offsets in the code changed would reschedule a
/// campaign that is already running.
pub(super) fn play_start(
    kind: PlayKind,
    anchor: PlayAnchor,
    policy: &AutopilotPolicy,
) -> Option<PlayStart> {
    let AutopilotPolicyConfig::Plays(domain_policy) = policy.config else {
        return None;
    };
    if !play_is_worth_starting(kind, anchor.active, anchor.hours_until, domain_policy) {
        return None;
    }
    let (platform, metric_key) = kind.success_metric();
    let steps: Vec<PlayStepPlan> = kind
        .steps()
        .iter()
        .map(|spec| {
            let (due_at, expires_at) = step_schedule(*spec, anchor.anchor_at);
            PlayStepPlan {
                index: spec.index,
                kind: spec.kind,
                class: spec.class,
                due_at,
                expires_at,
            }
        })
        .collect();
    // The window closes after the *last* step, whichever that is, plus the
    // settle period. Taking the first step's expiry would read the series while
    // the campaign was still running and call the result the campaign's effect.
    let last_expiry = steps.iter().map(|step| step.expires_at).max()?;
    Some(PlayStart {
        kind,
        event_id: anchor.event_id,
        anchor_at: anchor.anchor_at,
        hypothesis: kind.hypothesis(),
        success_metric_platform: platform,
        success_metric_key: metric_key,
        measurement_window_end: measurement_due_at(last_expiry, domain_policy.measurement),
        steps,
    })
}

/// One send of one step to one fan, as a durable candidate.
///
/// The subject is the fan rather than the play on purpose. The envelope's
/// cooldown exists so nobody hears from the agent twice in a week, and it only
/// means anything when the subject is the person being contacted — keyed on the
/// play, a campaign could message the same fan every cycle without ever
/// touching the cooldown.
pub(super) fn play_step_candidate(
    snapshot: &PlayRunSnapshot,
    decision: PlayDecision,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Plays(domain_policy) = policy.config else {
        return Ok(None);
    };
    let PlayDecision::RunStep {
        index,
        kind: step_kind,
        class: _,
        confidence,
    } = decision
    else {
        return Ok(None);
    };
    // A step that wants fans must have one. The domain only reaches `RunStep`
    // with a non-empty audience, so this is unreachable through the state
    // machine; it is still a check rather than an unwrap, because a send with
    // no recipient is the one mistake an owned-audience play must never make
    // and holding is always safe.
    let fan_id = snapshot.audience.fan_id();
    if matches!(step_kind.audience(), StepAudience::Fans) && fan_id.is_none() {
        return Ok(None);
    }
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        // The fan when there is one, and otherwise the show. The subject is
        // what the envelope's cooldown is keyed on, and a listing sweep must
        // not spend a person's weekly contact budget on work that reaches
        // nobody.
        subject: fan_id.map_or(ActionSubject::Event(snapshot.event_id), ActionSubject::Fan),
        decision_kind: "run_play_step",
        confidence,
        disposition,
        reason: step_kind.reason(),
        input_snapshot: serde_json::json!({
            "play": serde_json::to_value(domain_snapshot(snapshot))?,
            "event_id": snapshot.event_id,
            "recipient_fan_id": fan_id,
        }),
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RunPlayStep {
            play_id: snapshot.play_id,
            play_kind: snapshot.kind,
            step_index: index,
            step_kind,
            event_id: snapshot.event_id,
            fan_id,
            template_key: step_kind.template_key().to_owned(),
        },
        decision_key: format!(
            "decision:play-step:v{}:{}:{}:{}",
            policy.version,
            snapshot.play_id,
            index,
            recipient_key(fan_id)
        ),
        // Permanently stable, unlike the detector keys that carry a cooldown
        // window. A fan receives a given step of a given play once and never
        // again — there is no later occasion on which the same ask about the
        // same show becomes a second, legitimate message.
        action_idempotency_key: format!(
            "action:play-step:{}:{}:{}",
            snapshot.play_id,
            index,
            recipient_key(fan_id)
        ),
    }))
}

/// The recipient component of a play step's keys.
///
/// A step with no audience uses a fixed word rather than an empty string: an
/// empty component would make two different keys collide the first time
/// anything else was left out of one.
fn recipient_key(fan_id: Option<FanId>) -> String {
    fan_id.map_or_else(|| "anchor".to_owned(), |fan_id| fan_id.to_string())
}
