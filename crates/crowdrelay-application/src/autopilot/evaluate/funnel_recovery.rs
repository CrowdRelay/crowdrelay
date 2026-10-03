// Organic-funnel recovery orchestration for FanLifecycle.
//
// The funnel can narrow *which* already-authorized lifecycle work gets scarce
// attention first; it never bypasses the lifecycle's consent, cooldown,
// policy or envelope gates. Confirmation recovery is the one extra candidate:
// it exists only after the canonical delivery ledger proves the original
// double-opt-in route failed.

fn funnel_context_rank(
    context: AutopilotContext,
    control: Option<OrganicFunnelControl>,
) -> u8 {
    let Some(control) = control else {
        return 10;
    };
    match control.directive {
        OrganicFunnelDirective::RepairConfirmation
        | OrganicFunnelDirective::ActivateFans
        | OrganicFunnelDirective::RetainFans => {
            if context == AutopilotContext::FanLifecycle { 0 } else { 10 }
        }
        OrganicFunnelDirective::RepairConversion => {
            if context == AutopilotContext::ContentStrategy { 0 } else { 10 }
        }
        OrganicFunnelDirective::ExpandReach => 10,
    }
}

fn content_supply_public_reach(candidate: &DecisionCandidate) -> bool {
    if !matches!(
        candidate.decision_kind,
        "relay_owned_post" | "drop_surge_fanout"
    ) {
        return false;
    }
    match &candidate.action {
        AutopilotActionPayload::RequestAgentRun { template_id, .. } => matches!(
            template_id.as_str(),
            "community-engager" | "community-repost"
        ),
        AutopilotActionPayload::RequestAgentContent { template_id, .. } => template_id
            .as_deref()
            .is_some_and(|template| {
                matches!(
                    template,
                    "social-post" | "telegram-poster" | "discord-poster"
                )
            }),
        AutopilotActionPayload::RequestCommunityEngagement { .. } => true,
        _ => false,
    }
}

fn funnel_allows_content_supply(
    candidate: &DecisionCandidate,
    control: Option<OrganicFunnelControl>,
) -> bool {
    control.is_none_or(|control| {
        control.directive == OrganicFunnelDirective::ExpandReach
            || !content_supply_public_reach(candidate)
    })
}

fn attach_organic_funnel_control(
    candidate: &mut DecisionCandidate,
    control: OrganicFunnelControl,
) {
    if let Some(object) = candidate.input_snapshot.as_object_mut() {
        object.insert(
            "organic_funnel_control".to_owned(),
            serde_json::to_value(control).unwrap_or(serde_json::Value::Null),
        );
    }
}

fn confirmation_recovery_candidate(
    snapshot: crate::autopilot::ConfirmationRecoverySnapshot,
    policy: &AutopilotPolicy,
    control: OrganicFunnelControl,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::FanLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let confidence = Confidence::saturating_from_basis_points(9_900);
    let mut input_snapshot = serde_json::json!({
        "confirmation_recovery": snapshot,
        "organic_funnel_control": control,
    });
    // Keep the causal input flat enough for the existing evidence UI to show
    // the failed event identity without knowing this new recovery type.
    if let Some(object) = input_snapshot.as_object_mut() {
        object.insert(
            "recovery_reason".to_owned(),
            serde_json::Value::String(
                "latest double-opt-in delivery definitively failed; one bounded transactional retry"
                    .to_owned(),
            ),
        );
    }
    let failed_event = snapshot.failed_outbox_event_id;
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Fan(snapshot.fan_id),
        decision_kind: "recover_failed_fan_confirmation",
        confidence,
        disposition: disposition(
            policy.autonomy_level,
            confidence,
            policy.minimum_confidence,
        ),
        reason: "attributed signup is stuck because its latest double-opt-in delivery definitively failed",
        input_snapshot,
        policy_snapshot: policy_evidence(policy, *domain_policy)?,
        action: AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: snapshot.fan_id,
            template_key: crate::autopilot::CONFIRMATION_RECOVERY_TEMPLATE.to_owned(),
            show: None,
        },
        decision_key: format!(
            "decision:confirmation-recovery:v{}:{}:{}",
            policy.version, snapshot.fan_id, failed_event
        ),
        action_idempotency_key: format!(
            "action:confirmation-recovery:{}:{}",
            snapshot.fan_id, failed_event
        ),
    }))
}

fn lifecycle_recovery_rank(
    candidate: &DecisionCandidate,
    control: Option<OrganicFunnelControl>,
) -> u8 {
    let Some(control) = control else {
        return 10;
    };
    let AutopilotActionPayload::RequestFanLifecycleMessage { template_key, .. } = &candidate.action
    else {
        return 20;
    };
    match control.directive {
        OrganicFunnelDirective::RepairConfirmation => {
            if template_key == crate::autopilot::CONFIRMATION_RECOVERY_TEMPLATE {
                0
            } else {
                10
            }
        }
        OrganicFunnelDirective::ActivateFans => match template_key.as_str() {
            "crowdrelay.fan.welcome.v2" => 0,
            "crowdrelay.fan.signal_install_ask.v1" | "crowdrelay.fan.show_recall.v1" => 1,
            _ => 10,
        },
        OrganicFunnelDirective::RetainFans => {
            if template_key == "crowdrelay.fan.reactivation.v1" {
                0
            } else {
                10
            }
        }
        OrganicFunnelDirective::ExpandReach | OrganicFunnelDirective::RepairConversion => 10,
    }
}

impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    async fn cycle_policies_and_funnel(
        &self,
        now: OffsetDateTime,
    ) -> Result<(Vec<AutopilotPolicy>, Option<OrganicFunnelControl>), AutopilotError> {
        let mut policies = self.repository.load_policies(self.workspace_id).await?;
        let control = self
            .repository
            .load_organic_funnel_control(self.workspace_id, now)
            .await?;
        policies.sort_by_key(|policy| funnel_context_rank(policy.context, control));
        Ok((policies, control))
    }

    fn prepare_content_candidate_for_funnel(
        candidate: &mut DecisionCandidate,
        control: Option<OrganicFunnelControl>,
        report: &mut AutopilotCycleReport,
    ) -> bool {
        if let Some(control) = control {
            attach_organic_funnel_control(candidate, control);
        }
        if funnel_allows_content_supply(candidate, control) {
            return true;
        }
        if let Some(control) = control {
            report.gi_dispatch_log.push(format!(
                "organic funnel control: held content-supply public reach decision={} while directive={}",
                candidate.decision_kind,
                control.directive.as_str()
            ));
        }
        false
    }

    async fn evaluate_fan_lifecycle_with_funnel(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        control: Option<OrganicFunnelControl>,
    ) -> Result<(), AutopilotError> {
        let mut candidates = Vec::new();

        if let Some(control) = control
            && control.directive == OrganicFunnelDirective::RepairConfirmation
        {
            for snapshot in self
                .repository
                .load_confirmation_recovery_snapshots(self.workspace_id, now)
                .await?
            {
                if let Some(candidate) =
                    confirmation_recovery_candidate(snapshot, policy, control)?
                {
                    candidates.push(candidate);
                }
            }
        }

        for snapshot in self
            .repository
            .load_fan_lifecycle_snapshots(self.workspace_id, now)
            .await?
        {
            if let Some(mut candidate) = lifecycle_candidate(snapshot, policy, now)? {
                if let Some(control) = control {
                    attach_organic_funnel_control(&mut candidate, control);
                }
                candidates.push(candidate);
            }
        }

        // Stable sort: equal-priority lifecycle work keeps the repository's
        // deterministic order; only the stage-specific recovery lane moves.
        candidates.sort_by_key(|candidate| lifecycle_recovery_rank(candidate, control));

        if let Some(control) = control {
            let eligible_recovery = candidates
                .iter()
                .filter(|candidate| lifecycle_recovery_rank(candidate, Some(control)) < 10)
                .count();
            report.gi_dispatch_log.push(format!(
                "organic funnel lifecycle recovery: directive={} eligible_actions={eligible_recovery}",
                control.directive.as_str()
            ));
        }

        for candidate in &candidates {
            self.persist(candidate, limits, report).await?;
        }

        self.evaluate_relationship_research(policy, now, limits, report)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod funnel_recovery_tests {
    use super::*;

    fn candidate(template: &str) -> DecisionCandidate {
        DecisionCandidate {
            context: AutopilotContext::FanLifecycle,
            subject: ActionSubject::Fan(FanId::new()),
            decision_kind: "test",
            confidence: Confidence::MAX,
            disposition: PolicyDisposition::AutoExecute,
            reason: "test",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::RequestFanLifecycleMessage {
                fan_id: FanId::new(),
                template_key: template.to_owned(),
                show: None,
            },
            decision_key: "d".to_owned(),
            action_idempotency_key: "a".to_owned(),
        }
    }

    fn control(directive: OrganicFunnelDirective) -> OrganicFunnelControl {
        OrganicFunnelControl {
            directive,
            mature_links: 1,
            unique_visitors: 1,
            signups: 1,
            confirmed: 1,
            activation_mature: 1,
            activated_mature: 0,
            retention_mature: 0,
            retained: 0,
        }
    }

    #[test]
    fn downstream_recovery_context_runs_before_other_contexts() {
        for directive in [
            OrganicFunnelDirective::RepairConfirmation,
            OrganicFunnelDirective::ActivateFans,
            OrganicFunnelDirective::RetainFans,
        ] {
            let c = control(directive);
            assert_eq!(funnel_context_rank(AutopilotContext::FanLifecycle, Some(c)), 0);
            assert_eq!(funnel_context_rank(AutopilotContext::ContentSupply, Some(c)), 10);
        }
        let c = control(OrganicFunnelDirective::RepairConversion);
        assert_eq!(funnel_context_rank(AutopilotContext::ContentStrategy, Some(c)), 0);
        assert_eq!(funnel_context_rank(AutopilotContext::FanLifecycle, Some(c)), 10);
    }

    #[test]
    fn downstream_leak_holds_public_content_reach_but_not_owned_fan_delivery() {
        let public = candidate("crowdrelay.fan.welcome.v2");
        let mut public = DecisionCandidate {
            decision_kind: "drop_surge_fanout",
            action: AutopilotActionPayload::RequestAgentContent {
                template_id: Some("social-post".to_owned()),
                task_id: uuid::Uuid::now_v7(),
                draft: serde_json::json!({}),
                recipient_email: None,
                recipient_name: None,
                recipient_target_id: None,
            },
            ..public
        };
        let owned = DecisionCandidate {
            decision_kind: "drop_surge_fanout",
            action: AutopilotActionPayload::RequestSignalPush {
                task_id: uuid::Uuid::now_v7(),
                title: "new".to_owned(),
                body: "new".to_owned(),
                target_path: Some("/l/x".to_owned()),
                event_id: None,
                segment: None,
                audience_size: Some(1),
                audience_basis: "consented fans".to_owned(),
                drop_surge_lane: Some("signal_push".to_owned()),
            },
            ..candidate("crowdrelay.fan.welcome.v2")
        };
        let downstream = Some(control(OrganicFunnelDirective::ActivateFans));
        assert!(!funnel_allows_content_supply(&public, downstream));
        assert!(funnel_allows_content_supply(&owned, downstream));
        assert!(funnel_allows_content_supply(
            &public,
            Some(control(OrganicFunnelDirective::ExpandReach))
        ));
        attach_organic_funnel_control(&mut public, control(OrganicFunnelDirective::ExpandReach));
        assert!(public.input_snapshot.get("organic_funnel_control").is_some());
    }

    #[test]
    fn activation_recovery_beats_thank_yous_and_referrals() {
        let c = control(OrganicFunnelDirective::ActivateFans);
        assert!(
            lifecycle_recovery_rank(&candidate("crowdrelay.fan.welcome.v2"), Some(c))
                < lifecycle_recovery_rank(&candidate("crowdrelay.fan.referral_invite.v1"), Some(c))
        );
        assert!(
            lifecycle_recovery_rank(
                &candidate("crowdrelay.fan.signal_install_ask.v1"),
                Some(c)
            ) < lifecycle_recovery_rank(
                &candidate("crowdrelay.fan.first_ticket_thanks.v1"),
                Some(c)
            )
        );
    }

    #[test]
    fn retention_recovery_prefers_real_dormant_reactivation_only() {
        let c = control(OrganicFunnelDirective::RetainFans);
        assert_eq!(
            lifecycle_recovery_rank(&candidate("crowdrelay.fan.reactivation.v1"), Some(c)),
            0
        );
        assert_eq!(
            lifecycle_recovery_rank(&candidate("crowdrelay.fan.referral_invite.v1"), Some(c)),
            10
        );
    }

    #[test]
    fn confirmation_recovery_is_the_only_confirmation_priority() {
        let c = control(OrganicFunnelDirective::RepairConfirmation);
        assert_eq!(
            lifecycle_recovery_rank(
                &candidate(crate::autopilot::CONFIRMATION_RECOVERY_TEMPLATE),
                Some(c)
            ),
            0
        );
        assert_eq!(
            lifecycle_recovery_rank(&candidate("crowdrelay.fan.welcome.v2"), Some(c)),
            10
        );
    }
}
