// Application-level lifecycle mapping tests: the domain's eligibility rules
// live in `crowdrelay-domain/src/audience_lifecycle.rs`; these pin what the
// candidate carries out of them — the template key, the forced approval on
// unproven outward copy, the frozen show context, and the re-arming keys.

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crowdrelay_domain::{
        audience_lifecycle::{FanLifecyclePolicy, FanLifecycleSnapshot, LifecycleCheckin},
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    };
    use time::macros::datetime;

    fn snapshot() -> FanLifecycleSnapshot {
        FanLifecycleSnapshot {
            fan_id: FanId::from_uuid(uuid::Uuid::from_u128(9)),
            active: true,
            marketing_consent: true,
            created_at: datetime!(2026-09-01 12:00 UTC),
            synesthesia_completed_at: None,
            // A touch already landed, so the recall's send is neither the
            // fan's first contact nor inside the cooldown.
            last_marketing_touch_at: Some(datetime!(2026-10-01 12:00 UTC)),
            has_paid_ticket: false,
            last_paid_ticket_at: None,
            paid_ticket_count: 0,
            qualified_referrals: 0,
            last_qualified_referral_at: None,
            has_referral_code: true,
            has_signal_install: false,
            last_event_interest_at: None,
            recent_checkin: None,
        }
    }

    fn policy(autonomy_level: AutonomyLevel) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        Ok(AutopilotPolicy {
            context: AutopilotContext::FanLifecycle,
            enabled: true,
            autonomy_level,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 6,
            config: AutopilotPolicyConfig::FanLifecycle(FanLifecyclePolicy::default()),
            version: 3,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    #[test]
    fn a_recent_check_in_emits_the_recall_with_the_frozen_show()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut snapshot = snapshot();
        snapshot.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: datetime!(2026-10-17 22:30 UTC),
            event_slug: "virya-furydate-impala-10-17".to_owned(),
            event_title: "Virya × Furydate × Impala".to_owned(),
        });
        let candidate = lifecycle_candidate(
            snapshot,
            &policy(AutonomyLevel::BoundedAuto)?,
            datetime!(2026-10-18 18:00 UTC),
        )?
        .expect("a check-in inside the window must propose");
        let AutopilotActionPayload::RequestFanLifecycleMessage {
            template_key,
            show,
            ..
        } = &candidate.action
        else {
            panic!("expected a lifecycle message, got {:?}", candidate.action);
        };
        assert_eq!(template_key, "crowdrelay.fan.show_recall.v1");
        // The night is frozen onto the action — a second check-in before
        // approval must not rewrite what the operator clicked send on.
        let show = show.as_ref().expect("the recall carries its night");
        assert_eq!(show.event_slug, "virya-furydate-impala-10-17");
        assert_eq!(show.event_title, "Virya × Furydate × Impala");
        // No linked install at decision time → the recall doubles as the ask.
        assert!(show.wants_install_url);
        // Unproven outward copy never auto-sends, even under bounded auto.
        assert_eq!(candidate.disposition, PolicyDisposition::RequireApproval);
        Ok(())
    }

    #[test]
    fn an_installed_fan_gets_the_thanks_without_the_install_ask()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut snapshot = snapshot();
        snapshot.has_signal_install = true;
        snapshot.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: datetime!(2026-10-17 22:30 UTC),
            event_slug: "virya-furydate-impala-10-17".to_owned(),
            event_title: "Virya × Furydate × Impala".to_owned(),
        });
        let candidate = lifecycle_candidate(
            snapshot,
            &policy(AutonomyLevel::BoundedAuto)?,
            datetime!(2026-10-18 18:00 UTC),
        )?
        .expect("a check-in inside the window must propose");
        let AutopilotActionPayload::RequestFanLifecycleMessage { show, .. } = &candidate.action
        else {
            panic!("expected a lifecycle message, got {:?}", candidate.action);
        };
        assert_eq!(show.as_ref().map(|s| s.wants_install_url), Some(false));
        Ok(())
    }

    #[test]
    fn a_second_show_re_arms_instead_of_colliding_with_the_first()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = policy(AutonomyLevel::BoundedAuto)?;
        let mut first = snapshot();
        first.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: datetime!(2026-10-17 22:30 UTC),
            event_slug: "virya-furydate-impala-10-17".to_owned(),
            event_title: "Virya × Furydate × Impala".to_owned(),
        });
        let first = lifecycle_candidate(first, &policy, datetime!(2026-10-18 18:00 UTC))?
            .expect("first show proposes");
        let mut second = snapshot();
        second.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: datetime!(2026-10-30 23:00 UTC),
            event_slug: "virya-10-30".to_owned(),
            event_title: "Virya".to_owned(),
        });
        let second = lifecycle_candidate(second, &policy, datetime!(2026-10-31 18:00 UTC))?
            .expect("second show proposes");
        // A pending recall for 10-17 must not swallow the 10-30 one: both
        // keys carry the check-in instant, so the new night is a new action.
        assert_ne!(first.decision_key, second.decision_key);
        assert_ne!(
            first.action_idempotency_key, second.action_idempotency_key,
            "a pending recall for one show must not dedupe the next show's"
        );
        Ok(())
    }

    #[test]
    fn a_stale_check_in_stays_silent() -> Result<(), Box<dyn std::error::Error>> {
        let mut snapshot = snapshot();
        snapshot.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: datetime!(2026-10-10 22:30 UTC),
            event_slug: "old-show".to_owned(),
            event_title: "Old show".to_owned(),
        });
        let candidate = lifecycle_candidate(
            snapshot,
            &policy(AutonomyLevel::BoundedAuto)?,
            // Sixty-one hours later: past the recall window, and the fan is
            // too old for the welcome — nothing is owed.
            datetime!(2026-10-13 12:00 UTC),
        )?;
        let fired_recall = candidate.as_ref().is_some_and(|candidate| {
            matches!(
                &candidate.action,
                AutopilotActionPayload::RequestFanLifecycleMessage { template_key, .. }
                    if template_key == "crowdrelay.fan.show_recall.v1"
            )
        });
        assert!(!fired_recall, "a check-in past the window must not recall it");
        Ok(())
    }
}
