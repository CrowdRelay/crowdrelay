#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::{
        TeamOpportunityId, TicketTypeId,
        live_opportunities::{
            LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot,
            live_opportunity_score,
        },
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        pricing::TicketYieldPolicy,
    };

    #[test]
    fn recommend_policy_never_creates_auto_execute_disposition()
    -> Result<(), Box<dyn std::error::Error>> {
        let minimum = Confidence::from_basis_points(8_000)?;
        let policy = AutopilotPolicy {
            context: AutopilotContext::TicketYield,
            enabled: true,
            autonomy_level: AutonomyLevel::Recommend,
            minimum_confidence: minimum,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::TicketYield(TicketYieldPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let candidate = ticket_candidate(
            TicketYieldSnapshot {
                ticket_type_id: TicketTypeId::new(),
                current_price_minor: 3_000,
                paid_quantity: 80,
                capacity: 100,
                sale_capacity: 100,
                paid_last_72h: 8,
                days_to_event: 21,
                last_price_change_at: None,
                last_capacity_change_at: None,
                allocation_guardrail: None,
            },
            &policy,
            now,
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
        Ok(())
    }
    /// A Landmark festival whose score lands in the gap between the configured
    /// floor and the real one.
    ///
    /// Strategic value 85% makes it Landmark (21 of 25 points), fit 70% gives 21
    /// of 30, reputation 60% gives 9 of 15, evidence 70% gives 10 of 15, and a
    /// bounded loss gives 6 of 15 economics — 67. With the default
    /// `minimum_score` of 65 the domain gate passes it; confidence is
    /// `7_500 + (67 - 65) * 100 = 7_700`, which is under the 8000 minimum, so the
    /// authority gate used to deny it.
    fn landmark_scoring_67() -> LiveOpportunitySnapshot {
        LiveOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            kind: LiveOpportunityKind::Festival,
            active: true,
            verified_destination: true,
            auto_submission_capable: true,
            fit_basis_points: 7_000,
            reputation_basis_points: 6_000,
            evidence_confidence: Confidence::from_basis_points(7_000)
                .expect("a valid confidence"),
            expected_fee_minor: 70_000,
            estimated_cost_minor: 90_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: None,
            event_starts_at: None,
            travel_band: None,
            costed_from_logistics: true,
            committed_shows_year: 0,
            pipeline_shows_year: 0,
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            far_shot_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: false,
            already_applied: false,
            strategic_value_basis_points: 8_500,
        }
    }

    fn live_policy(level: AutonomyLevel) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        Ok(AutopilotPolicy {
            context: AutopilotContext::LiveOpportunity,
            enabled: true,
            autonomy_level: level,
            minimum_confidence: Confidence::from_basis_points(8_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::LiveOpportunity(LiveOpportunityPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    /// The confidence gate must not decide that nobody sees a decision the
    /// domain gate sent to a person.
    ///
    /// `Deny` writes the decision to the ledger and creates no action row, so the
    /// opportunity never enters `awaiting_approval` — the queue `ops/attention`
    /// reads and the only one the operator looks at. Finding it afterwards means
    /// querying for `disposition = 'deny'`.
    #[test]
    fn a_score_between_the_configured_floor_and_the_real_one_still_reaches_a_human()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = live_policy(AutonomyLevel::RequireApproval)?;
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        assert_eq!(
            live_opportunity_score(snapshot),
            67,
            "the fixture must land in the gap this test is about"
        );
        let candidate = live_opportunity_candidate(snapshot, &policy, now)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(
            candidate.disposition,
            PolicyDisposition::RequireApproval,
            "a verified opportunity above the configured minimum_score must reach \
             the approval queue rather than being denied by the confidence gate"
        );
        Ok(())
    }

    /// The lift obeys the autonomy level, which is the trap in fixing this.
    ///
    /// `disposition` tests confidence before the level, so a low-confidence
    /// decision returns `Deny` on an `Observe` workspace too. Rewriting that to
    /// `RequireApproval` would put approval requests in front of an operator who
    /// asked the autopilot to observe and nothing else.
    #[test]
    fn the_lift_does_not_promote_an_observe_only_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        for (level, expected) in [
            (AutonomyLevel::Observe, PolicyDisposition::ObserveOnly),
            (AutonomyLevel::Recommend, PolicyDisposition::RecommendOnly),
        ] {
            let policy = live_policy(level)?;
            let candidate = live_opportunity_candidate(snapshot, &policy, now)?
                .ok_or_else(|| std::io::Error::other("candidate expected"))?;
            assert_eq!(
                candidate.disposition, expected,
                "{level:?} must keep its own disposition, not be promoted to \
                 approval by the Deny lift"
            );
        }
        Ok(())
    }

    /// And it still never widens what the machine may do unattended.
    #[test]
    fn a_forced_approval_decision_never_auto_executes()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = live_policy(AutonomyLevel::BoundedAuto)?;
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        let candidate = live_opportunity_candidate(snapshot, &policy, now)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(
            candidate.disposition,
            PolicyDisposition::RequireApproval,
            "the domain routed this to a human; bounded-auto does not override that"
        );
        Ok(())
    }

    /// Finds the real confidence floor by asking the production evaluator.
    ///
    /// `minimum_confidence` is a score floor wearing different units. Live
    /// opportunity confidence is `7_500 + (score - minimum_score) * 100`, so with
    /// a 8000 minimum the first score to clear it is `minimum_score + 5` — an
    /// operator who sets `minimum_score` to 65 has really set 70, and the two
    /// numbers live in different crates with neither mentioning the other.
    ///
    /// Nothing is lost to it now: a decision the domain routed to a human is
    /// lifted past the confidence gate, because that gate decides whether the
    /// *machine* may act alone. The gap still governs whether an opportunity can
    /// ever auto-submit, so it is worth having written down.
    ///
    /// The first version of this test computed the formula inline and asserted it
    /// against itself. Moving the real base from 7_500 to 8_000 did not fail it.
    /// This one sweeps `fit_basis_points` through `evaluate_live_opportunity` and
    /// reads the confidence the production path actually produced, so the
    /// assertion is about the code rather than about a copy of it.
    #[test]
    fn the_confidence_floor_is_a_second_score_floor() -> Result<(), Box<dyn std::error::Error>> {
        let minimum = Confidence::from_basis_points(8_000)?;
        let policy = LiveOpportunityPolicy::default();
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);

        let mut lowest_clearing: Option<u16> = None;
        let mut highest_denied: Option<u16> = None;
        for fit in (0..=10_000_u16).step_by(100) {
            let mut snapshot = landmark_scoring_67();
            snapshot.fit_basis_points = fit;
            let score = live_opportunity_score(snapshot);
            let confidence = match evaluate_live_opportunity(snapshot, policy, now) {
                LiveOpportunityDecision::PrepareForApproval { confidence, .. }
                | LiveOpportunityDecision::SubmitAutomatically { confidence, .. }
                | LiveOpportunityDecision::EscalateLandmark { confidence, .. } => confidence,
                // Below `minimum_score`, or refused for another reason. The
                // confidence gate is not what is being measured there.
                LiveOpportunityDecision::Hold => continue,
            };
            if confidence.basis_points() >= minimum.basis_points() {
                lowest_clearing = Some(lowest_clearing.map_or(score, |best| best.min(score)));
            } else {
                highest_denied = Some(highest_denied.map_or(score, |worst| worst.max(score)));
            }
        }

        let lowest_clearing = lowest_clearing.expect("some score must clear the floor");
        let highest_denied = highest_denied.expect("some score must fall under it");
        assert_eq!(
            lowest_clearing,
            policy.minimum_score + 5,
            "the evaluator's own confidence clears {} five points above the \
             configured minimum_score of {}",
            minimum.basis_points(),
            policy.minimum_score
        );
        assert_eq!(
            highest_denied,
            policy.minimum_score + 4,
            "and the band the confidence gate rejects runs right up to it, so \
             minimum_score is not the floor an operator gets"
        );
        Ok(())
    }
}
