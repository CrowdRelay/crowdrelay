// Learned ordering for already-eligible Beacon outreach.
//
// This layer is deliberately downstream of `beacon_candidate`: the domain
// policy decides whether a relationship contact is due. The causal model may
// reorder due asks; it must never make an ineligible Beacon eligible.

struct RankedBeaconCandidate {
    candidate: DecisionCandidate,
    original_order: usize,
    rank_value: f64,
}

fn rank_beacon_candidate(
    mut candidate: DecisionCandidate,
    original_order: usize,
    template_id: &str,
    target_key: &str,
    causal_model: &crowdrelay_brain::CausalModel,
) -> RankedBeaconCandidate {
    // The dispatch envelope for Beacon outreach is written with the default
    // context and the same template/target identity. Reading with anything
    // else would split evidence from the decision it is supposed to teach.
    let context = crowdrelay_brain::DispatchContext::default();
    let stats = causal_model.predict_stats_with_treatment_for_target(
        template_id,
        Some(target_key),
        &context,
    );
    let mode = if stats.use_treatment_effect {
        crowdrelay_brain::DecisionMode::Exploit
    } else {
        crowdrelay_brain::DecisionMode::Explore
    };

    // Resource cost is deliberately absent from this ordering. The domain
    // policy, class ceiling and attention envelope already decide whether a
    // relationship ask may happen; this comparison only answers which of the
    // already-due asks has stronger North-Star evidence. Harm that has an
    // honest fan-equivalent conversion remains part of intrinsic value.
    let value = crowdrelay_brain::DecisionValue::from_stats(
        &stats,
        crowdrelay_brain::ResourceCost::default(),
        mode,
    )
    .with_harm_cost(&stats);
    let rank_value = value.total();

    // Persist the exact decision-time belief beside the normal Beacon
    // snapshot. A later operator can tell whether ordering came from a cold
    // outcome prior, bridged Y14 evidence or direct durable Y30 evidence.
    if let Some(snapshot) = candidate.input_snapshot.as_object_mut() {
        snapshot.insert(
            "north_star_ranking".to_owned(),
            serde_json::json!({
                "template_id": template_id,
                "target_key": target_key,
                "rank_value_y30_fans": rank_value,
                "expected_incremental_y30": value.expected_incremental_y30,
                "uncertainty": value.uncertainty,
                "p_meaningful_effect": value.p_meaningful_effect,
                "estimation_regime": value.estimation_regime.as_str(),
                "sample_size": value.sample_size,
                "uses_y30": value.uses_y30,
                "bridge_confidence": value.bridge_confidence,
                "bridge_is_reliable": value.bridge_is_reliable,
                "harm_fans": value.harm_fans,
            }),
        );
    }

    RankedBeaconCandidate {
        candidate,
        original_order,
        rank_value,
    }
}

fn order_ranked_beacon_candidates(
    mut candidates: Vec<RankedBeaconCandidate>,
) -> (Vec<DecisionCandidate>, bool) {
    let original: Vec<usize> = candidates.iter().map(|item| item.original_order).collect();

    // Highest expected fan value first. original_order is an explicit tie
    // breaker so a cold model whose priors are equal preserves the repository's
    // deterministic ordering exactly.
    candidates.sort_by(|left, right| {
        right
            .rank_value
            .total_cmp(&left.rank_value)
            .then_with(|| left.original_order.cmp(&right.original_order))
    });

    let reordered = candidates
        .iter()
        .map(|item| item.original_order)
        .ne(original.iter().copied());

    (
        candidates.into_iter().map(|item| item.candidate).collect(),
        reordered,
    )
}

impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn evaluate_beacon_campaigns(
        &self,
        policy: &AutopilotPolicy,
        loaded_model: Option<&LoadedCausalModel>,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let snapshots = self
            .repository
            .load_beacon_campaign_snapshots(self.workspace_id, now)
            .await?;

        let Some(loaded_model) = loaded_model else {
            for snapshot in snapshots {
                if let Some(candidate) = beacon_candidate(snapshot, policy, now)? {
                    self.persist(&candidate, limits, report).await?;
                }
            }
            return Ok(());
        };

        let mut ranked = Vec::with_capacity(snapshots.len());
        for (original_order, snapshot) in snapshots.into_iter().enumerate() {
            if let Some(candidate) = beacon_candidate(snapshot, policy, now)? {
                let (template_id, target_key) = match &candidate.action {
                    AutopilotActionPayload::RequestBeaconOutreach {
                        beacon_id,
                        template_key,
                        ..
                    } => (template_key.clone(), format!("beacon:{beacon_id}")),
                    _ => return Err(RepositoryError::Unexpected.into()),
                };
                ranked.push(rank_beacon_candidate(
                    candidate,
                    original_order,
                    &template_id,
                    &target_key,
                    &loaded_model.model,
                ));
            }
        }

        let (ordered, reordered) = order_ranked_beacon_candidates(ranked);
        if reordered {
            report.gi_dispatch_log.push(
                "brain decision influenced by learning: Beacon due-ask order changed by North-Star value"
                    .into(),
            );
        }
        for candidate in ordered {
            self.persist(&candidate, limits, report).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod beacon_learning_tests {
    use super::*;

    fn candidate(key: &str) -> DecisionCandidate {
        DecisionCandidate {
            context: AutopilotContext::Beacon,
            subject: ActionSubject::Workspace(WorkspaceId::from_uuid(Uuid::nil())),
            decision_kind: "fixture",
            confidence: Confidence::saturating_from_basis_points(9_000),
            disposition: PolicyDisposition::RecommendOnly,
            reason: "fixture",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::RequestAgentRun {
                template_id: "fixture".to_owned(),
                prompt: String::new(),
                priority: 1,
                tier: crowdrelay_brain::AgentTier::Basic,
            },
            decision_key: key.to_owned(),
            action_idempotency_key: key.to_owned(),
        }
    }

    #[test]
    fn equal_priors_preserve_repository_order() {
        let ranked = vec![
            RankedBeaconCandidate {
                candidate: candidate("a"),
                original_order: 0,
                rank_value: 2.0,
            },
            RankedBeaconCandidate {
                candidate: candidate("b"),
                original_order: 1,
                rank_value: 2.0,
            },
        ];
        let (ordered, changed) = order_ranked_beacon_candidates(ranked);
        assert!(!changed);
        assert_eq!(ordered[0].decision_key, "a");
        assert_eq!(ordered[1].decision_key, "b");
    }

    #[test]
    fn learned_fan_value_reorders_only_due_candidates() {
        let ranked = vec![
            RankedBeaconCandidate {
                candidate: candidate("lower"),
                original_order: 0,
                rank_value: 0.4,
            },
            RankedBeaconCandidate {
                candidate: candidate("higher"),
                original_order: 1,
                rank_value: 1.7,
            },
        ];
        let (ordered, changed) = order_ranked_beacon_candidates(ranked);
        assert!(changed);
        assert_eq!(ordered[0].decision_key, "higher");
        assert_eq!(ordered[1].decision_key, "lower");
    }
}
