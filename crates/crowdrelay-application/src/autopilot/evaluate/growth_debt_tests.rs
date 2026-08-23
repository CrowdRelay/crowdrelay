#[cfg(test)]
mod growth_debt_candidate_tests {
    use super::*;
    use crowdrelay_domain::{
        BookingTargetId, EventId,
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        growth_debt::{GrowthDebtKind, GrowthDebtObservation, GrowthDebtPolicy, GrowthDebtSubject},
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    /// A warm booking relationship idle for well over twice its horizon.
    fn observation() -> GrowthDebtObservation {
        GrowthDebtObservation {
            kind: GrowthDebtKind::RelationshipQuiet,
            subject: GrowthDebtSubject::BookingTarget(BookingTargetId::from_uuid(
                uuid::Uuid::from_u128(21),
            )),
            idle_hours: 3_600,
            outstanding_items: 1,
            tracked_items: 1,
            relationship_score: Some(85),
            hours_until_deadline: None,
            hours_since_last_signal: None,
        }
    }

    fn policy(autonomy_level: AutonomyLevel) -> Result<AutopilotPolicy, Box<dyn std::error::Error>>
    {
        Ok(AutopilotPolicy {
            context: AutopilotContext::GrowthDebt,
            enabled: true,
            autonomy_level,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::GrowthDebt(GrowthDebtPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    #[test]
    fn recommend_policy_never_creates_auto_execute_disposition()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let candidate = growth_debt_candidate(&observation(), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
        assert_eq!(candidate.action.action_kind(), "growth.debt.raise");
        Ok(())
    }

    #[test]
    fn the_action_carries_the_denominator_alongside_the_count()
    -> Result<(), Box<dyn std::error::Error>> {
        // "6 outstanding" means nothing without "of 10 tracked". An operator
        // ranking debt against debt needs the share, not a bare count.
        let policy = policy(AutonomyLevel::Recommend)?;
        let candidate = growth_debt_candidate(
            &GrowthDebtObservation {
                kind: GrowthDebtKind::EventLeversSkipped,
                subject: GrowthDebtSubject::Event(EventId::from_uuid(uuid::Uuid::from_u128(22))),
                idle_hours: 1_000,
                outstanding_items: 6,
                tracked_items: 10,
                relationship_score: None,
                hours_until_deadline: Some(240),
                hours_since_last_signal: None,
            },
            &policy,
            now(),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        let AutopilotActionPayload::RaiseGrowthDebt {
            outstanding_items,
            tracked_items,
            subject_kind,
            ..
        } = candidate.action
        else {
            return Err(Box::new(std::io::Error::other("wrong payload")));
        };
        assert_eq!(outstanding_items, 6);
        assert_eq!(tracked_items, 10);
        assert_eq!(subject_kind, "event");
        Ok(())
    }

    #[test]
    fn the_decision_key_changes_with_the_evidence() -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let baseline = growth_debt_candidate(&observation(), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let worse = growth_debt_candidate(
            &GrowthDebtObservation {
                idle_hours: 20_000,
                ..observation()
            },
            &policy,
            now(),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(baseline.decision_key, worse.decision_key);
        Ok(())
    }

    #[test]
    fn the_decision_key_changes_with_the_policy_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let baseline = growth_debt_candidate(&observation(), &policy(AutonomyLevel::Recommend)?, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let revised = growth_debt_candidate(
            &observation(),
            &AutopilotPolicy {
                version: 2,
                ..policy(AutonomyLevel::Recommend)?
            },
            now(),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(baseline.decision_key, revised.decision_key);
        Ok(())
    }

    #[test]
    fn the_action_key_is_stable_inside_a_cooldown_and_differs_across_windows()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let cooldown = i64::from(GrowthDebtPolicy::default().cooldown_hours);

        let first = growth_debt_candidate(&observation(), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        // Same window, worse evidence: the operator queue must not gain a
        // second copy just because the debt aged an hour.
        let same_window = growth_debt_candidate(
            &GrowthDebtObservation {
                idle_hours: 4_000,
                ..observation()
            },
            &policy,
            now() + time::Duration::hours(1),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let later_window = growth_debt_candidate(
            &observation(),
            &policy,
            now() + time::Duration::hours(cooldown * 2),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(first.action_idempotency_key, same_window.action_idempotency_key);
        assert_ne!(first.action_idempotency_key, later_window.action_idempotency_key);
        Ok(())
    }

    #[test]
    fn two_debt_kinds_on_one_subject_do_not_collide()
    -> Result<(), Box<dyn std::error::Error>> {
        // An event can owe both skipped levers and a stalled release plan.
        // Those are separate findings, not one finding to overwrite.
        let policy = policy(AutonomyLevel::Recommend)?;
        let event = GrowthDebtSubject::Event(EventId::from_uuid(uuid::Uuid::from_u128(23)));
        let levers = growth_debt_candidate(
            &GrowthDebtObservation {
                kind: GrowthDebtKind::EventLeversSkipped,
                subject: event,
                idle_hours: 1_000,
                outstanding_items: 6,
                tracked_items: 10,
                relationship_score: None,
                hours_until_deadline: Some(240),
                hours_since_last_signal: None,
            },
            &policy,
            now(),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let milestones = growth_debt_candidate(
            &GrowthDebtObservation {
                kind: GrowthDebtKind::ReleaseMilestonesMissed,
                subject: event,
                idle_hours: 1_000,
                outstanding_items: 6,
                tracked_items: 10,
                relationship_score: None,
                hours_until_deadline: Some(240),
                hours_since_last_signal: None,
            },
            &policy,
            now(),
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(levers.decision_key, milestones.decision_key);
        assert_ne!(
            levers.action_idempotency_key,
            milestones.action_idempotency_key
        );
        Ok(())
    }

    #[test]
    fn a_mismatched_policy_config_yields_no_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = AutopilotPolicy {
            config: AutopilotPolicyConfig::GrowthMetrics(
                crowdrelay_domain::growth_metrics::GrowthMetricPolicy::default(),
            ),
            ..policy(AutonomyLevel::Recommend)?
        };
        assert!(growth_debt_candidate(&observation(), &policy, now())?.is_none());
        Ok(())
    }

    #[test]
    fn confidence_below_the_policy_floor_is_denied() -> Result<(), Box<dyn std::error::Error>> {
        let policy = AutopilotPolicy {
            minimum_confidence: Confidence::MAX,
            ..policy(AutonomyLevel::BoundedAuto)?
        };
        let candidate = growth_debt_candidate(&observation(), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.disposition, PolicyDisposition::Deny);
        Ok(())
    }

    #[test]
    fn a_subject_the_domain_holds_produces_no_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        // The evaluator must not second-guess the rule's refusals.
        let policy = policy(AutonomyLevel::Recommend)?;
        let cold = GrowthDebtObservation {
            relationship_score: Some(5),
            ..observation()
        };
        assert!(growth_debt_candidate(&cold, &policy, now())?.is_none());
        Ok(())
    }
}
