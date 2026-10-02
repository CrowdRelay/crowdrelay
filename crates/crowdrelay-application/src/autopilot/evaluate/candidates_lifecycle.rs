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


/// Internal research for one warm relationship that is otherwise ready for a
/// thoughtful next touch. This action reaches nobody; its result must still
/// pass the agent provenance/grounding gate before it can become durable
/// relationship intelligence.
fn relationship_research_candidate(
    snapshot: RelationshipResearchSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::FanLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let confidence = Confidence::MAX;
    let disposition = crowdrelay_domain::autonomy::internal_work_disposition(disposition(
        policy.autonomy_level,
        confidence,
        policy.minimum_confidence,
    ));
    if matches!(
        disposition,
        PolicyDisposition::Deny
            | PolicyDisposition::ObserveOnly
            | PolicyDisposition::RecommendOnly
    ) {
        return Ok(None);
    }

    let bucket = now.unix_timestamp().div_euclid(86_400);
    let prompt = format!(
        "Research exactly this existing relationship before any new outward ask. \
         The task metadata pins beacon_id={}. Find at most one recent, dated, \
         public-source thing they actually did that a thoughtful colleague could \
         mention. Do not draft a message and do not infer consent. If the source \
         is weak or undated, return no item.\n\nName: {}\nRole: {}\nCity: {}\n\
         Relationship score: {}\nHas replied: {}\nDays since last contact: {}",
        snapshot.beacon_id,
        snapshot.display_name,
        snapshot.role,
        snapshot.city.as_deref().unwrap_or("unknown"),
        snapshot.relationship_score,
        snapshot.has_replied,
        snapshot
            .days_since_last_contact
            .map_or_else(|| "unknown".to_owned(), |days| days.to_string()),
    );

    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Beacon(snapshot.beacon_id),
        decision_kind: "request_relationship_research",
        confidence,
        disposition,
        reason: "a warm relationship is eligible for a future value-led touch but lacks recent sourced context",
        input_snapshot: serde_json::to_value(&snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestAgentRun {
            template_id: "contact-researcher".to_owned(),
            prompt,
            priority: 9,
            tier: crowdrelay_brain::AgentTier::Premium,
        },
        decision_key: format!(
            "decision:relationship-research:v{}:{}:{}",
            policy.version, snapshot.beacon_id, bucket
        ),
        action_idempotency_key: format!(
            "action:relationship-research:{}:{}",
            snapshot.beacon_id, bucket
        ),
    }))
}


impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    async fn evaluate_relationship_research(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
    ) -> Result<(), AutopilotError> {
        let research = self
            .repository
            .load_relationship_research_snapshots(self.workspace_id, now)
            .await?;
        for snapshot in research {
            if let Some(candidate) =
                relationship_research_candidate(snapshot, policy, now)?
            {
                self.persist(&candidate, limits, report).await?;
            }
        }
        Ok(())
    }
}
