// ContentSupply delivery-lane routing.
//
// The lane ledger is delivery truth, not just an ops screen. This gate keeps a
// cycle from piling fresh acquisition work behind a lane that is already busy
// or stuck. A quiet lane gets exactly one probe per cycle: otherwise a new
// channel could never become measured at all.

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeliveryLaneKey {
    scope: crowdrelay_domain::lane_ledger::LaneScope,
    lane: String,
}

struct DeliveryLaneGate {
    verdicts: std::collections::BTreeMap<
        DeliveryLaneKey,
        crowdrelay_domain::lane_ledger::Verdict,
    >,
    probed: std::collections::BTreeSet<DeliveryLaneKey>,
    held: std::collections::BTreeSet<String>,
}

impl DeliveryLaneGate {
    fn new(
        verdicts: Vec<(
            crowdrelay_domain::lane_ledger::LaneScope,
            String,
            crowdrelay_domain::lane_ledger::Verdict,
        )>,
    ) -> Self {
        Self {
            verdicts: verdicts
                .into_iter()
                .map(|(scope, lane, verdict)| (DeliveryLaneKey { scope, lane }, verdict))
                .collect(),
            probed: std::collections::BTreeSet::new(),
            held: std::collections::BTreeSet::new(),
        }
    }

    fn allows_candidate(
        &mut self,
        candidate: &DecisionCandidate,
        communities: &[CommunityRelayTarget],
    ) -> bool {
        let Some(key) = delivery_lane_for_candidate(candidate, communities) else {
            // Email and Signal have their own send/receipt paths and are not in
            // the post-table lane ledger. Do not invent health for them.
            return true;
        };
        let verdict = self
            .verdicts
            .get(&key)
            .copied()
            .unwrap_or(crowdrelay_domain::lane_ledger::Verdict::Quiet);
        match verdict.planning_availability() {
            crowdrelay_domain::lane_ledger::PlanningAvailability::Open => true,
            crowdrelay_domain::lane_ledger::PlanningAvailability::Probe => {
                if self.probed.insert(key.clone()) {
                    true
                } else {
                    self.held
                        .insert(format!("{}/{}=quiet_probe_already_scheduled", key.scope.as_str(), key.lane));
                    false
                }
            }
            crowdrelay_domain::lane_ledger::PlanningAvailability::Busy
            | crowdrelay_domain::lane_ledger::PlanningAvailability::Blocked => {
                self.held.insert(format!(
                    "{}/{}={}",
                    key.scope.as_str(),
                    key.lane,
                    lane_verdict_name(verdict)
                ));
                false
            }
        }
    }

    fn wait_reason(&self) -> Option<String> {
        (!self.held.is_empty()).then(|| {
            format!(
                "delivery lane routing held new work: {}",
                self.held.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

fn lane_verdict_name(verdict: crowdrelay_domain::lane_ledger::Verdict) -> &'static str {
    use crowdrelay_domain::lane_ledger::Verdict;
    match verdict {
        Verdict::Quiet => "quiet",
        Verdict::Delivering => "delivering",
        Verdict::DeliveringPartly => "delivering_partly",
        Verdict::HeldForPerson => "held_for_person",
        Verdict::RateLimited => "rate_limited",
        Verdict::Failing => "failing",
        Verdict::Queued => "queued",
    }
}

fn delivery_lane_for_candidate(
    candidate: &DecisionCandidate,
    communities: &[CommunityRelayTarget],
) -> Option<DeliveryLaneKey> {
    use crowdrelay_domain::lane_ledger::LaneScope;

    if !matches!(
        candidate.decision_kind,
        "drop_surge_fanout" | "relay_owned_post"
    ) {
        return None;
    }

    if let ActionSubject::TargetCommunity(target_id) = candidate.subject {
        let target = communities
            .iter()
            .find(|target| target.target_id.into_uuid() == target_id)?;
        return Some(DeliveryLaneKey {
            scope: LaneScope::Community,
            lane: target.platform.clone(),
        });
    }

    if candidate.decision_kind == "drop_surge_fanout"
        && let AutopilotActionPayload::RequestAgentContent { draft, .. } = &candidate.action
    {
        let platform = draft.get("platform")?.as_str()?;
        let lane = if platform == "discord" {
            "discord_channel"
        } else {
            platform
        };
        return Some(DeliveryLaneKey {
            scope: LaneScope::Owned,
            lane: lane.to_owned(),
        });
    }

    None
}

#[cfg(test)]
mod lane_routing_tests {
    use super::*;
    use crowdrelay_domain::lane_ledger::{LaneScope, Verdict};

    fn gate(scope: LaneScope, lane: &str, verdict: Verdict) -> DeliveryLaneGate {
        DeliveryLaneGate::new(vec![(scope, lane.to_owned(), verdict)])
    }

    #[test]
    fn quiet_is_one_probe_not_health() {
        let key = DeliveryLaneKey {
            scope: LaneScope::Owned,
            lane: "instagram".to_owned(),
        };
        let mut gate = DeliveryLaneGate::new(Vec::new());
        let verdict = gate
            .verdicts
            .get(&key)
            .copied()
            .unwrap_or(Verdict::Quiet);
        assert_eq!(
            verdict.planning_availability(),
            crowdrelay_domain::lane_ledger::PlanningAvailability::Probe
        );
        assert!(gate.probed.insert(key.clone()));
        assert!(!gate.probed.insert(key));
    }

    #[test]
    fn delivering_is_open_while_busy_or_stuck_is_not() {
        use crowdrelay_domain::lane_ledger::PlanningAvailability;
        for (verdict, expected) in [
            (Verdict::Delivering, PlanningAvailability::Open),
            (Verdict::Quiet, PlanningAvailability::Probe),
            (Verdict::Queued, PlanningAvailability::Busy),
            (Verdict::DeliveringPartly, PlanningAvailability::Blocked),
            (Verdict::HeldForPerson, PlanningAvailability::Blocked),
            (Verdict::RateLimited, PlanningAvailability::Blocked),
            (Verdict::Failing, PlanningAvailability::Blocked),
        ] {
            assert_eq!(verdict.planning_availability(), expected);
        }
    }

    fn direct_post_candidate(platform: &str) -> DecisionCandidate {
        DecisionCandidate {
            context: AutopilotContext::ContentSupply,
            subject: ActionSubject::DropSurgeLane(Uuid::now_v7()),
            decision_kind: "drop_surge_fanout",
            confidence: crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(9_500),
            disposition: crowdrelay_domain::autonomy::PolicyDisposition::RequireApproval,
            reason: "test",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::RequestAgentContent {
                template_id: None,
                task_id: Uuid::now_v7(),
                draft: serde_json::json!({"platform": platform}),
                recipient_email: None,
                recipient_name: None,
                recipient_target_id: None,
            },
            decision_key: "test-decision".to_owned(),
            action_idempotency_key: "test-action".to_owned(),
        }
    }

    #[test]
    fn owned_discord_uses_discord_channel_and_not_the_community_discord_lane() {
        let candidate = direct_post_candidate("discord");
        let mut owned_blocked =
            gate(LaneScope::Owned, "discord_channel", Verdict::HeldForPerson);
        assert!(
            !owned_blocked.allows_candidate(&candidate, &[]),
            "owned Discord backlog must stop another owned Discord post"
        );

        let mut community_blocked =
            gate(LaneScope::Community, "discord", Verdict::HeldForPerson);
        assert!(
            community_blocked.allows_candidate(&candidate, &[]),
            "a community Discord hold must not poison the band's owned Discord channel"
        );
    }

    #[test]
    fn owned_and_community_telegram_are_different_keys() {
        let owned = DeliveryLaneKey {
            scope: LaneScope::Owned,
            lane: "telegram".to_owned(),
        };
        let community = DeliveryLaneKey {
            scope: LaneScope::Community,
            lane: "telegram".to_owned(),
        };
        assert_ne!(owned, community);

        let owned_gate = gate(LaneScope::Owned, "telegram", Verdict::Delivering);
        assert_eq!(owned_gate.verdicts.get(&owned), Some(&Verdict::Delivering));
        assert_eq!(owned_gate.verdicts.get(&community), None);
    }
}
