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
            referral_ask_ready_at: Some(datetime!(2026-10-10 12:00 UTC)),
            has_signal_install: false,
            last_event_interest_at: None,
            recent_checkin: None,
        }
    }

    fn policy(
        autonomy_level: AutonomyLevel,
    ) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
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
            template_key, show, ..
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
        assert!(
            !fired_recall,
            "a check-in past the window must not recall it"
        );
        Ok(())
    }

    #[test]
    fn a_ticket_thank_you_does_not_rearm_when_contacts_interest_or_policy_change()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = datetime!(2026-10-18 18:00 UTC);
        let mut fan = snapshot();
        fan.has_paid_ticket = true;
        fan.paid_ticket_count = 1;
        fan.last_paid_ticket_at = Some(now - time::Duration::hours(2));
        let mut policy = policy(AutonomyLevel::BoundedAuto)?;
        let first = lifecycle_candidate(fan.clone(), &policy, now)?.expect("first ticket");
        fan.last_marketing_touch_at = Some(now - time::Duration::minutes(1));
        fan.last_event_interest_at = Some(now - time::Duration::minutes(2));
        fan.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: now - time::Duration::minutes(3),
            event_slug: "other-night".to_owned(),
            event_title: "Other night".to_owned(),
        });
        policy.version += 1;
        let next = lifecycle_candidate(fan, &policy, now)?.expect("same milestone");
        assert_ne!(first.decision_key, next.decision_key);
        assert_eq!(first.action_idempotency_key, next.action_idempotency_key);
        assert_eq!(
            first.input_snapshot.get("lifecycle_episode"),
            next.input_snapshot.get("lifecycle_episode")
        );
        Ok(())
    }

    #[test]
    fn each_new_qualified_referral_can_receive_its_own_thanks()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = datetime!(2026-10-18 18:00 UTC);
        let policy = policy(AutonomyLevel::BoundedAuto)?;
        let mut fan = snapshot();
        fan.qualified_referrals = 1;
        fan.last_qualified_referral_at = Some(now - time::Duration::hours(3));
        let first = lifecycle_candidate(fan.clone(), &policy, now)?.expect("qualified referral");
        fan.last_marketing_touch_at = Some(now - time::Duration::hours(1));
        let repeat = lifecycle_candidate(fan.clone(), &policy, now)?.expect("same referral");
        assert_eq!(first.action_idempotency_key, repeat.action_idempotency_key);
        fan.qualified_referrals = 2;
        fan.last_qualified_referral_at = Some(now - time::Duration::minutes(30));
        let second = lifecycle_candidate(fan, &policy, now)?.expect("new referral");
        assert_ne!(first.action_idempotency_key, second.action_idempotency_key);
        Ok(())
    }
    #[test]
    fn a_new_welcome_names_v2_without_broadening_policy_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fan = snapshot();
        fan.last_marketing_touch_at = None;
        let c = lifecycle_candidate(
            fan.clone(),
            &policy(AutonomyLevel::BoundedAuto)?,
            datetime!(2026-10-18 18:00 UTC),
        )?
        .expect("welcome");
        assert!(
            matches!(&c.action,AutopilotActionPayload::RequestFanLifecycleMessage{template_key,..} if template_key=="crowdrelay.fan.welcome.v2")
        );
        let observe = lifecycle_candidate(
            fan,
            &policy(AutonomyLevel::Observe)?,
            datetime!(2026-10-18 18:00 UTC),
        )?
        .expect("observed welcome");
        assert_eq!(observe.disposition, PolicyDisposition::ObserveOnly);
        Ok(())
    }
}
