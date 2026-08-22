#[cfg(test)]
mod growth_metric_candidate_tests {
    use super::*;
    use crowdrelay_domain::{
        GrowthMetricSeriesId,
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        growth_metrics::{
            GrowthMetricPolicy, GrowthMetricSnapshot, MetricDirection, MetricPlatform, MetricPoint,
            MetricValueTier, compute_trend,
        },
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    /// A 22-day climb of `+100/day` followed by 7 days of `+5/day`: a stall
    /// against the series' own baseline.
    fn stalling_points(now: OffsetDateTime) -> Vec<MetricPoint> {
        (0..30)
            .map(|day| MetricPoint {
                captured_at: now - time::Duration::days(29 - day),
                value: if day <= 22 {
                    1_000 + day * 100
                } else {
                    3_200 + (day - 22) * 5
                },
            })
            .collect()
    }

    fn snapshot(now: OffsetDateTime) -> GrowthMetricSnapshot {
        let points = stalling_points(now);
        GrowthMetricSnapshot {
            series_id: GrowthMetricSeriesId::from_uuid(uuid::Uuid::from_u128(11)),
            platform: MetricPlatform::YouTube,
            metric_key: "subscribers".to_owned(),
            direction: MetricDirection::HigherIsBetter,
            value_tier: MetricValueTier::Intermediate,
            expected_interval_hours: 24,
            trend: compute_trend(&points, now).expect("trend from a full window"),
            hours_since_last_signal: None,
            stronger_tier_tracked: false,
        }
    }

    fn policy(autonomy_level: AutonomyLevel) -> Result<AutopilotPolicy, Box<dyn std::error::Error>>
    {
        Ok(AutopilotPolicy {
            context: AutopilotContext::GrowthMetrics,
            enabled: true,
            autonomy_level,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 12,
            config: AutopilotPolicyConfig::GrowthMetrics(GrowthMetricPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    #[test]
    fn recommend_policy_never_creates_auto_execute_disposition()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let candidate = growth_metric_candidate(&snapshot(now()), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
        assert_eq!(candidate.action.action_kind(), "growth.opportunity.raise");
        Ok(())
    }

    #[test]
    fn decision_key_changes_when_the_evidence_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let baseline = growth_metric_candidate(&snapshot(now()), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        let mut moved = snapshot(now());
        moved.trend.latest_value += 500;
        let moved = growth_metric_candidate(&moved, &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(baseline.decision_key, moved.decision_key);
        Ok(())
    }

    #[test]
    fn decision_key_changes_when_the_policy_version_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = policy(AutonomyLevel::Recommend)?;
        let mut second = policy(AutonomyLevel::Recommend)?;
        second.version = 2;

        let a = growth_metric_candidate(&snapshot(now()), &first, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let b = growth_metric_candidate(&snapshot(now()), &second, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(a.decision_key, b.decision_key);
        Ok(())
    }

    #[test]
    fn action_key_is_stable_inside_a_cooldown_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        let later = now() + time::Duration::hours(1);

        let first = growth_metric_candidate(&snapshot(now()), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let second = growth_metric_candidate(&snapshot(later), &policy, later)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(
            first.action_idempotency_key, second.action_idempotency_key,
            "re-detecting the same finding an hour later must not enqueue a second action"
        );
        Ok(())
    }

    #[test]
    fn action_key_differs_across_cooldown_windows() -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::Recommend)?;
        // The default cooldown is 168 hours; a finding that recurs well beyond
        // it is separate work, not a replay of the first one.
        let much_later = now() + time::Duration::days(30);

        let first = growth_metric_candidate(&snapshot(now()), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let second = growth_metric_candidate(&snapshot(much_later), &policy, much_later)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_ne!(
            first.action_idempotency_key, second.action_idempotency_key,
            "a finding recurring a month later must be raisable again"
        );
        Ok(())
    }

    #[test]
    fn a_mismatched_policy_config_yields_no_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut policy = policy(AutonomyLevel::Recommend)?;
        policy.config =
            AutopilotPolicyConfig::TicketYield(crowdrelay_domain::pricing::TicketYieldPolicy::default());

        assert!(growth_metric_candidate(&snapshot(now()), &policy, now())?.is_none());
        Ok(())
    }

    #[test]
    fn confidence_below_the_policy_floor_is_denied() -> Result<(), Box<dyn std::error::Error>> {
        let mut policy = policy(AutonomyLevel::BoundedAuto)?;
        policy.minimum_confidence = Confidence::MAX;

        let candidate = growth_metric_candidate(&snapshot(now()), &policy, now())?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.disposition, PolicyDisposition::Deny);
        Ok(())
    }
}
