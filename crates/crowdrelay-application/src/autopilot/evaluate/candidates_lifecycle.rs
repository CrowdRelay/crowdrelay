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
    let (template, confidence) = match evaluate_fan_lifecycle(snapshot.clone(), *domain_policy, now)
    {
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
                input_snapshot: serde_json::to_value(&snapshot)?,
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
        LifecycleTemplate::ShowRecall => "crowdrelay.fan.show_recall.v1",
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // The install ask and the show recall are new outward surfaces and their
    // copy is unproven: every send of either waits for a person until a later
    // revision earns the trust the welcome and thank-yous carry. Only ever
    // tightens — Observe and Recommend keep their answer.
    if matches!(
        template,
        LifecycleTemplate::SignalInstallAsk | LifecycleTemplate::ShowRecall
    ) && matches!(disposition, PolicyDisposition::AutoExecute)
    {
        disposition = PolicyDisposition::RequireApproval;
    }
    let subject = ActionSubject::Fan(snapshot.fan_id);
    // The recall names a specific night, so the night rides the action: the
    // snapshot's check-in fields freeze into the payload, and a fan with no
    // linked install additionally gets the tracked Signal CTA minted at
    // execution — the recall then doubles as their install ask.
    let show = if template == LifecycleTemplate::ShowRecall {
        snapshot
            .recent_checkin
            .as_ref()
            .map(|checkin| crate::autopilot::model::LifecycleShowContext {
                event_slug: checkin.event_slug.clone(),
                event_title: checkin.event_title.clone(),
                wants_install_url: !snapshot.has_signal_install,
            })
    } else {
        None
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_lifecycle_message",
        confidence,
        disposition,
        reason: "consented fan lifecycle has a deterministic communication step due",
        input_snapshot: serde_json::to_value(&snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: snapshot.fan_id,
            template_key: template_key.to_owned(),
            show,
        },
        // The check-in timestamp joins the key so a second show re-arms the
        // recall instead of colliding with the pending decision for the
        // first one — same night, same key; new night, new ask.
        decision_key: format!(
            "decision:lifecycle:v{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.fan_id,
            template_key,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            snapshot
                .last_event_interest_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            snapshot
                .recent_checkin
                .as_ref()
                .map_or(0, |checkin| checkin.checked_in_at.unix_timestamp()),
        ),
        action_idempotency_key: format!(
            "action:lifecycle:{}:{template_key}:{}:{}",
            snapshot.fan_id,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            // The check-in joins the action key for the same reason it joins
            // the decision key: a second show while the first recall is still
            // pending is a new action, not a duplicate of the old one.
            snapshot
                .recent_checkin
                .as_ref()
                .map_or(0, |checkin| checkin.checked_in_at.unix_timestamp()),
        ),
    }))
}
