// The consented-fan lifecycle turns: welcome, follow-ups, the Signal
// install ask, referral invite. Split from `candidates.rs` when the chunk
// cap made the file stop compiling in review — not for a conceptual reason
// beyond "everything the fan lifecycle can propose".

fn lifecycle_candidate(
    snapshot: FanLifecycleSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::FanLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let (template, confidence) = match evaluate_fan_lifecycle(snapshot, *domain_policy, now) {
        FanLifecycleDecision::RequestMessage {
            template,
            confidence,
        } => (template, confidence),
        // A code before any message that might carry an invite. Costs nothing,
        // reaches nobody, and is the precondition for the one growth loop that
        // compounds without the band doing more work.
        FanLifecycleDecision::IssueReferralCode { confidence } => {
            return Ok(Some(DecisionCandidate {
                context: policy.context,
                subject: ActionSubject::Fan(snapshot.fan_id),
                decision_kind: "issue_referral_code",
                confidence,
                disposition: disposition(
                    policy.autonomy_level,
                    confidence,
                    policy.minimum_confidence,
                ),
                reason: "a consented fan has no referral code, so no invite can be tracked",
                input_snapshot: serde_json::to_value(snapshot)?,
                policy_snapshot: policy_evidence(policy, *domain_policy)?,
                action: AutopilotActionPayload::IssueReferralCode {
                    fan_id: snapshot.fan_id,
                },
                decision_key: format!(
                    "decision:referral-code:v{}:{}",
                    policy.version, snapshot.fan_id
                ),
                // One code per fan, forever. Not windowed like a message: a
                // second code would split a fan's referrals across two
                // identities and make the ledger wrong.
                action_idempotency_key: format!("action:referral-code:{}", snapshot.fan_id),
            }));
        }
        FanLifecycleDecision::Hold(_) => return Ok(None),
    };
    let template_key = match template {
        LifecycleTemplate::Welcome => "crowdrelay.fan.welcome.v1",
        LifecycleTemplate::SynesthesiaFollowUp => "crowdrelay.synesthesia.follow_up.v1",
        LifecycleTemplate::DormantReactivation => "crowdrelay.fan.reactivation.v1",
        LifecycleTemplate::FirstTicketThankYou => "crowdrelay.fan.first_ticket_thanks.v1",
        LifecycleTemplate::ReturningFanThankYou => "crowdrelay.fan.returning_thanks.v1",
        LifecycleTemplate::ReferralThankYou => "crowdrelay.fan.referral_thanks.v1",
        LifecycleTemplate::ReferralInvite => "crowdrelay.fan.referral_invite.v1",
        LifecycleTemplate::SignalInstallAsk => "crowdrelay.fan.signal_install_ask.v1",
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // The install ask is a new outward surface and its copy is unproven: every
    // send of it waits for a person until a later revision earns the same
    // trust the welcome and thank-yous carry. Only ever tightens — Observe and
    // Recommend keep their answer.
    if template == LifecycleTemplate::SignalInstallAsk
        && matches!(disposition, PolicyDisposition::AutoExecute)
    {
        disposition = PolicyDisposition::RequireApproval;
    }
    let subject = ActionSubject::Fan(snapshot.fan_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_lifecycle_message",
        confidence,
        disposition,
        reason: "consented fan lifecycle has a deterministic communication step due",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: snapshot.fan_id,
            template_key: template_key.to_owned(),
        },
        decision_key: format!(
            "decision:lifecycle:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.fan_id,
            template_key,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            snapshot
                .last_event_interest_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:lifecycle:{}:{template_key}:{}",
            snapshot.fan_id,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
    }))
}
