#[cfg(test)]
mod plays_tests {
    use super::*;
    use crowdrelay_domain::{
        EventId, FanId, PlayId,
        action_class::ActionClass,
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        plays::{PlayPolicy, PlayStepKind, PlayStepState},
    };

    fn policy(level: AutonomyLevel) -> AutopilotPolicy {
        AutopilotPolicy {
            context: AutopilotContext::Plays,
            enabled: true,
            autonomy_level: level,
            minimum_confidence: Confidence::MIN,
            max_actions_24h: 40,
            config: AutopilotPolicyConfig::Plays(PlayPolicy::default()),
            version: 7,
            guarded_until: None,
            guardrail_reason: None,
        }
    }

    fn anchor_at() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    fn running(audience: PlayAudience) -> PlayRunSnapshot {
        let steps = PlayKind::TrackUsAsk
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor_at());
                PlayStepState {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                    settled: false,
                    recipients_emitted: 0,
                }
            })
            .collect();
        PlayRunSnapshot {
            play_id: PlayId::new(),
            kind: PlayKind::TrackUsAsk,
            event_id: EventId::new(),
            anchor_at: anchor_at(),
            anchor_active: true,
            steps,
            audience,
        }
    }

    fn due_now(snapshot: &PlayRunSnapshot) -> OffsetDateTime {
        snapshot
            .steps
            .first()
            .map_or_else(anchor_at, |step| step.due_at + time::Duration::hours(1))
    }

    #[test]
    fn a_show_too_close_to_run_its_first_step_never_starts_a_play() {
        let policy = policy(AutonomyLevel::BoundedAuto);
        let near = PlayAnchor {
            event_id: EventId::new(),
            anchor_at: anchor_at(),
            active: true,
            hours_until: 24,
        };
        assert!(play_start(PlayKind::TrackUsAsk, near, &policy).is_none());
        let far = PlayAnchor {
            hours_until: 30 * 24,
            ..near
        };
        let start = play_start(PlayKind::TrackUsAsk, far, &policy)
            .expect("a show a month out can carry the whole schedule");
        assert_eq!(start.steps.len(), PlayKind::TrackUsAsk.steps().len());
        // The claim is frozen at start, not reconstructed later from whatever
        // the code says by then.
        assert_eq!(start.hypothesis, PlayKind::TrackUsAsk.hypothesis());
        assert_eq!(
            (start.success_metric_platform, start.success_metric_key),
            PlayKind::TrackUsAsk.success_metric()
        );
    }

    #[test]
    fn a_cancelled_show_is_never_a_reason_to_start_a_play() {
        let anchor = PlayAnchor {
            event_id: EventId::new(),
            anchor_at: anchor_at(),
            active: false,
            hours_until: 30 * 24,
        };
        assert!(play_start(PlayKind::TrackUsAsk, anchor, &policy(AutonomyLevel::BoundedAuto)).is_none());
    }

    #[test]
    fn every_step_of_a_started_play_has_a_window_derived_from_the_anchor() {
        let anchor = PlayAnchor {
            event_id: EventId::new(),
            anchor_at: anchor_at(),
            active: true,
            hours_until: 30 * 24,
        };
        let start = play_start(PlayKind::TrackUsAsk, anchor, &policy(AutonomyLevel::BoundedAuto))
            .expect("startable");
        for (step, spec) in start.steps.iter().zip(PlayKind::TrackUsAsk.steps()) {
            let (due_at, expires_at) = step_schedule(*spec, anchor_at());
            assert_eq!((step.due_at, step.expires_at), (due_at, expires_at));
            assert!(step.expires_at > step.due_at, "a window has to be open");
        }
    }

    #[test]
    fn a_send_is_addressed_to_the_fan_so_the_cooldown_can_see_it() {
        // Subjecting the action to the play instead would let one campaign
        // message the same person every cycle without ever touching the
        // envelope's per-contact cooldown.
        let fan_id = FanId::new();
        let snapshot = running(PlayAudience::Next {
            fan_id,
            remaining: 40,
        });
        let decision = play_decision(&snapshot, &policy(AutonomyLevel::BoundedAuto), due_now(&snapshot))
            .expect("a plays policy decides");
        let candidate = play_step_candidate(&snapshot, decision, &policy(AutonomyLevel::BoundedAuto))
            .expect("serializable")
            .expect("an open step with an audience sends");
        assert_eq!(candidate.subject, ActionSubject::Fan(fan_id));
        assert!(candidate.subject.is_contactable_person());
        assert_eq!(
            candidate.action.action_class(),
            ActionClass::OwnedAudience,
            "a fan ask is owned audience, whatever the play is called"
        );
    }

    #[test]
    fn a_step_send_is_idempotent_on_the_fan_for_ever() {
        // Unlike the detector keys, this one carries no time component. There
        // is no later occasion on which the same ask about the same show
        // becomes a second legitimate message.
        let fan_id = FanId::new();
        let snapshot = running(PlayAudience::Next {
            fan_id,
            remaining: 40,
        });
        let now = due_now(&snapshot);
        let decision =
            play_decision(&snapshot, &policy(AutonomyLevel::BoundedAuto), now).expect("decided");
        let candidate = play_step_candidate(&snapshot, decision, &policy(AutonomyLevel::BoundedAuto))
            .expect("serializable")
            .expect("sends");
        assert_eq!(
            candidate.action_idempotency_key,
            format!("action:play-step:{}:0:{fan_id}", snapshot.play_id)
        );
        let later = play_decision(
            &snapshot,
            &policy(AutonomyLevel::BoundedAuto),
            now + time::Duration::days(3),
        )
        .expect("decided");
        let repeat = play_step_candidate(&snapshot, later, &policy(AutonomyLevel::BoundedAuto))
            .expect("serializable")
            .expect("sends");
        assert_eq!(
            repeat.action_idempotency_key, candidate.action_idempotency_key,
            "the same fan and step must not mint a second send later in the window"
        );
    }

    #[test]
    fn an_empty_audience_produces_a_skip_rather_than_a_send() {
        let snapshot = running(PlayAudience::Exhausted);
        let decision = play_decision(&snapshot, &policy(AutonomyLevel::BoundedAuto), due_now(&snapshot))
            .expect("decided");
        assert!(matches!(
            decision,
            PlayDecision::SkipStep {
                reason: crowdrelay_domain::plays::StepSkipReason::NoEligibleRecipients,
                ..
            }
        ));
        assert!(
            play_step_candidate(&snapshot, decision, &policy(AutonomyLevel::BoundedAuto))
                .expect("serializable")
                .is_none(),
            "a skip is a settled row, never an action"
        );
    }

    #[test]
    fn a_recommend_policy_never_sends_a_step_unattended() {
        let snapshot = running(PlayAudience::Next {
            fan_id: FanId::new(),
            remaining: 40,
        });
        let policy = policy(AutonomyLevel::Recommend);
        let decision = play_decision(&snapshot, &policy, due_now(&snapshot)).expect("decided");
        let candidate = play_step_candidate(&snapshot, decision, &policy)
            .expect("serializable")
            .expect("sends");
        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
    }

    #[test]
    fn the_post_show_step_is_reached_only_after_the_announce_one_settles() {
        // The property that keeps a gated or missed step from stalling the
        // campaign behind it.
        let mut snapshot = running(PlayAudience::Next {
            fan_id: FanId::new(),
            remaining: 12,
        });
        let post_show_due = snapshot
            .steps
            .get(1)
            .map(|step| step.due_at + time::Duration::hours(1))
            .expect("two steps");
        let blocked = play_decision(&snapshot, &policy(AutonomyLevel::BoundedAuto), post_show_due)
            .expect("decided");
        assert!(
            matches!(blocked, PlayDecision::SkipStep { index: 0, .. }),
            "the earlier step settles first, as a recorded skip"
        );
        if let Some(step) = snapshot.steps.first_mut() {
            step.settled = true;
        }
        let decision = play_decision(&snapshot, &policy(AutonomyLevel::BoundedAuto), post_show_due)
            .expect("decided");
        let candidate = play_step_candidate(&snapshot, decision, &policy(AutonomyLevel::BoundedAuto))
            .expect("serializable")
            .expect("sends");
        assert!(matches!(
            candidate.action,
            AutopilotActionPayload::RunPlayStep {
                step_index: 1,
                step_kind: PlayStepKind::PostShowAsk,
                ..
            }
        ));
    }
}
