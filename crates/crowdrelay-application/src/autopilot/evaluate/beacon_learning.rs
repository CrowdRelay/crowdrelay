// Learned ordering for already-eligible Beacon outreach.
//
// This layer is deliberately downstream of `beacon_candidate`: the domain
// policy decides whether a relationship contact is due. The causal model may
// reorder due asks; it must never make an ineligible Beacon eligible.

struct RankedBeaconCandidate {
    candidate: DecisionCandidate,
    /// Relationship work keeps the calendar's urgency. Learning may reorder
    /// peers for the same event/time, never pull a later show ahead of an
    /// earlier one just because its historical Y30 is higher.
    event_starts_at: OffsetDateTime,
    original_order: usize,
    rank_value: f64,
}

fn rank_beacon_candidate(
    mut candidate: DecisionCandidate,
    event_starts_at: OffsetDateTime,
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

    // One ask spends one person's attention and this event's one
    // relationship slot with that partner. Neither converts to fans
    // honestly yet, so `units` stays zero — the dimensions are provenance
    // the review card shows, not a made-up fan penalty. What the choice
    // actually costs is recorded below as the foregone runner-up.
    let value = crowdrelay_brain::DecisionValue::from_stats(
        &stats,
        crowdrelay_brain::ResourceCost {
            audience_attention: Some(1.0),
            campaign_slots: Some(1.0),
            ..crowdrelay_brain::ResourceCost::default()
        },
        mode,
    )
    .with_harm_cost(&stats);
    let rank_value = value.total();

    // Granularity honesty: the numbers above shrink toward parent levels, so
    // `expected_incremental_y30` alone cannot say whose history produced it.
    // The card distinguishes this beacon's own outcomes from the template's
    // and from a bare prior — a learned τ on `beacon:…` is not the same
    // claim as the phase average wearing a target's name.
    let target_observations = causal_model.fans.target_confidence(target_key)
        + causal_model
            .treatment_effects
            .effects
            .target_confidence(target_key)
        + causal_model
            .treatment_effects_y30
            .effects
            .target_confidence(target_key);
    let template_observations = causal_model.fans.confidence(template_id)
        + causal_model.treatment_effects.observation_count(template_id)
        + causal_model
            .treatment_effects_y30
            .observation_count(template_id);
    let evidence_basis = if target_observations > 0 {
        "target_history"
    } else if template_observations > 0 {
        "template_prior"
    } else {
        "prior"
    };

    // Persist the exact decision-time belief beside the normal Beacon
    // snapshot. A later operator can tell whether ordering came from a cold
    // outcome prior, bridged Y14 evidence or direct durable Y30 evidence.
    if let Some(snapshot) = candidate.input_snapshot.as_object_mut() {
        snapshot.insert(
            "north_star_ranking".to_owned(),
            serde_json::json!({
                "template_id": template_id,
                "target_key": target_key,
                "event_starts_at": event_starts_at.to_string(),
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
                "evidence_basis": evidence_basis,
                "target_observations": target_observations,
                "template_observations": template_observations,
            }),
        );
    }

    RankedBeaconCandidate {
        candidate,
        event_starts_at,
        original_order,
        rank_value,
    }
}

fn order_ranked_beacon_candidates(
    mut candidates: Vec<RankedBeaconCandidate>,
) -> (Vec<DecisionCandidate>, bool) {
    let original: Vec<usize> = candidates.iter().map(|item| item.original_order).collect();

    // Calendar urgency first, learned North-Star value second. The repository
    // already orders Beacon work by event time; preserving that first key stops
    // a strong historical posterior for a six-weeks-away show from consuming
    // scarce third-party/approval budget ahead of today's relationship work.
    // Within the same event time, higher expected Y30 wins. Cold/equal priors
    // retain the repository's deterministic order.
    candidates.sort_by(|left, right| {
        left.event_starts_at
            .cmp(&right.event_starts_at)
            .then_with(|| right.rank_value.total_cmp(&left.rank_value))
            .then_with(|| left.original_order.cmp(&right.original_order))
    });

    // The budget-aware read: what choosing this ask foregoes. Each card
    // records the value of the ask the budget would have bought instead —
    // the top of the ordering for everyone below it, the runner-up for the
    // top itself. It stays out of `total()`: subtracting the same constant
    // from every non-top candidate changes nothing about the order and
    // would only dress a tie up as a loss.
    let top_value = candidates.first().map(|item| item.rank_value);
    let runner_up_value = candidates.get(1).map(|item| item.rank_value);
    for (index, item) in candidates.iter_mut().enumerate() {
        let foregone = if index == 0 { runner_up_value } else { top_value };
        let opportunity_cost = foregone.map_or(0.0, |value| -value);
        if let Some(snapshot) = item.candidate.input_snapshot.as_object_mut()
            && let Some(ranking) = snapshot
                .get_mut("north_star_ranking")
                .and_then(serde_json::Value::as_object_mut)
        {
            ranking.insert(
                "opportunity_cost_fans".to_owned(),
                serde_json::json!(opportunity_cost),
            );
        }
    }

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
            let event_starts_at = snapshot.event_starts_at;
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
                    event_starts_at,
                    original_order,
                    &template_id,
                    &target_key,
                    &loaded_model.model,
                ));
            }
        }

        let (ordered, reordered) = order_ranked_beacon_candidates(ranked);
        if reordered {
            // Which learned belief actually moved the ask: the outcome→choice
            // proof the spec asks for lives in `north_star_ranking`, and this
            // line names the winner so the log alone answers "why this one".
            let winner = ordered
                .first()
                .map(|candidate| candidate.decision_key.as_str())
                .unwrap_or("none");
            report.gi_dispatch_log.push(format!(
                "brain decision influenced by learning: Beacon due-ask order changed by North-Star value (top: {winner})"
            ));
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

    fn ranked(key: &str, order: usize, fan_value: f64, event_day: i64) -> RankedBeaconCandidate {
        RankedBeaconCandidate {
            candidate: candidate(key),
            event_starts_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(event_day),
            original_order: order,
            rank_value: fan_value,
        }
    }

    #[test]
    fn equal_priors_preserve_repository_order() {
        let ranked = vec![ranked("a", 0, 2.0, 10), ranked("b", 1, 2.0, 10)];
        let (ordered, changed) = order_ranked_beacon_candidates(ranked);
        assert!(!changed);
        assert_eq!(ordered[0].decision_key, "a");
        assert_eq!(ordered[1].decision_key, "b");
    }

    #[test]
    fn learned_fan_value_reorders_peers_for_the_same_event_time() {
        let ranked = vec![
            ranked("lower", 0, 0.4, 10),
            ranked("higher", 1, 1.7, 10),
        ];
        let (ordered, changed) = order_ranked_beacon_candidates(ranked);
        assert!(changed);
        assert_eq!(ordered[0].decision_key, "higher");
        assert_eq!(ordered[1].decision_key, "lower");
    }

    #[test]
    fn later_show_never_jumps_urgent_relationship_work_for_higher_y30() {
        let ranked = vec![
            ranked("urgent", 0, 0.1, 10),
            ranked("later-high-y30", 1, 50.0, 40),
        ];
        let (ordered, changed) = order_ranked_beacon_candidates(ranked);
        assert!(!changed, "learning must not reorder across event urgency");
        assert_eq!(ordered[0].decision_key, "urgent");
        assert_eq!(ordered[1].decision_key, "later-high-y30");
    }

    fn opportunity_cost(candidate: &DecisionCandidate) -> Option<f64> {
        candidate
            .input_snapshot
            .get("north_star_ranking")
            .and_then(|ranking| ranking.get("opportunity_cost_fans"))
            .and_then(serde_json::Value::as_f64)
    }

    #[test]
    fn opportunity_cost_records_the_foregone_runner_up() {
        // Same-day asks, so the touch budget buys the stronger one; picking
        // it costs the runner-up's value. The winner's card shows -0.4 (it
        // displaced a 0.4 ask), the loser's shows -1.7 (the ask it lost to).
        let mut first = ranked("first", 0, 1.7, 10);
        first.candidate.input_snapshot =
            serde_json::json!({"north_star_ranking": {}});
        let mut second = ranked("second", 1, 0.4, 10);
        second.candidate.input_snapshot =
            serde_json::json!({"north_star_ranking": {}});
        let (ordered, _) = order_ranked_beacon_candidates(vec![first, second]);
        assert_eq!(opportunity_cost(&ordered[0]), Some(-0.4));
        assert_eq!(opportunity_cost(&ordered[1]), Some(-1.7));
    }

    #[test]
    fn sole_candidate_foregoes_nothing() {
        // With no runner-up there is no foregone ask — the honest number is
        // zero, not a fabricated loss against an empty budget.
        let mut only = ranked("only", 0, 3.2, 10);
        only.candidate.input_snapshot =
            serde_json::json!({"north_star_ranking": {}});
        let (ordered, changed) = order_ranked_beacon_candidates(vec![only]);
        assert!(!changed);
        assert_eq!(opportunity_cost(&ordered[0]), Some(0.0));
    }
}
