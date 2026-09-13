// The watchdog's condition tests.
//
// `include!`d into `ops_watchdog.rs` so they share that module's scope, the
// same way `conditions.rs` is. Split out because the parent reached the 1200
// line source-size ratchet, and raising a baseline to get green is the one
// thing that file's own rules forbid.
//
// Every test here reads one question: given a snapshot, which conditions fire
// and at what severity. That is the part of the watchdog worth reviewing, so
// it is worth being able to read on its own.

#[cfg(test)]
mod tests {
    use super::{OpsSnapshot, conditions};
use crate::auto_post_platforms::{PublishingPosture, RedditPosture};

/// The posture of a workspace whose channels are all switched on.
///
/// Every test that is not about publishing uses this, so a condition about
/// unpublished drafts does not fire in the middle of an unrelated assertion.
fn publishing() -> PublishingPosture {
    PublishingPosture {
        platforms: crate::auto_post_platforms::AutoPostPlatforms {
            telegram: true,
            discord: true,
            social: true,
        },
        reddit: RedditPosture::Publishes,
    }
}

    fn healthy() -> OpsSnapshot {
        OpsSnapshot {
            executor_registered: 1,
            executor_active: 1,
            unknown_actions: 0,
            stale_unknown_actions: 0,
            contradicted_actions: 0,
            failing_platforms: 0,
            working_platforms: 2,
            stuck_ungeocoded_cities: 0,
            fans_awaiting_geocoding: 0,
            outcomes_rejected_unverified: 0,
            outcomes_accepted: 4,
            orphaned_publishing_actions: 0,
            orphaned_publishing_actions_all_time: 0,
            refused_growth_deliveries: 0,
            unscoreable_live_opportunities: 0,
            duplicate_community_drafts: 0,
            relentless_degraded_phases: None,
            outcome_rejection_reasons: None,
            off_platform_push_attempts: 0,
            reddit_drafts_waiting: 0,
            reddit_drafts_failed: None,
            reddit_posting_demand: 0,
            reddit_session_usable: 1,
            reddit_credential_status: Some("active".to_owned()),
            reddit_credential_error: None,
            // A workspace below the decision threshold, so the
            // never-updated-posterior condition stays quiet unless a test asks
            // for it.
            decisions_total: 0,
            causal_observations: Some(0),
        }
    }

    /// History alone must not hold the alarm open.
    ///
    /// An orphaned draft is never published later, so an unbounded count kept
    /// `publishing.orphaned_draft` active for good once one existed. The
    /// all-time count still travels in the details, so nothing is hidden.
    #[test]
    fn orphans_older_than_the_window_report_without_alarming() {
        let mut snapshot = healthy();
        snapshot.orphaned_publishing_actions_all_time = 2;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(
            raised.is_empty(),
            "orphans outside the window must not hold an alarm open: {raised:?}"
        );
        let orphan = conditions(&snapshot, publishing())
            .into_iter()
            .find(|condition| condition.key == "publishing.orphaned_draft")
            .expect("the orphaned-draft condition is always present");
        assert_eq!(orphan.details["orphaned_actions_all_time"], 2);
    }

    /// A denied live-opportunity decision must raise attention.
    ///
    /// Counted from the brain's own `deny` decisions rather than from a guess at
    /// the reason. The first version counted rows with no strategic value and no
    /// logistics; an import filled strategic value and the alarm went quiet while
    /// every opportunity stayed held on the confidence gate. The decision row is
    /// the one signal that cannot drift from what the brain did.
    #[test]
    fn an_unscoreable_live_opportunity_raises_attention_by_itself() {
        let mut snapshot = healthy();
        snapshot.unscoreable_live_opportunities = 430;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(
            raised,
            vec!["growth.unscoreable_live_opportunities"],
            "an unreachable score bar must fire without needing any other fault"
        );
    }

    /// And it must be a warning, not critical.
    ///
    /// Nothing is corrupted and no wrong lesson is learned — the work is simply
    /// sat on. Every fix is judgement rather than code: assert the festival's
    /// standing, or move one of the two thresholds that interact to produce the
    /// real floor.
    #[test]
    fn an_unscoreable_live_opportunity_is_a_warning() {
        let mut snapshot = healthy();
        snapshot.unscoreable_live_opportunities = 1;
        let condition = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "growth.unscoreable_live_opportunities")
            .expect("condition should exist");
        assert_eq!(condition.severity, "warning");
        // The arithmetic belongs in the alert: an operator deciding whether to
        // lower the bar needs to see that 40 of the 100 points are unreachable.
        // The arithmetic belongs in the alert: an operator deciding whether to move
        // a threshold needs to see that two of them interact.
        let details = condition.details.to_string();
        assert!(details.contains("minimum_confidence"), "{details}");
        assert!(details.contains("real floor is 70"), "{details}");
    }

    /// Two queued drafts for one community must raise attention.
    ///
    /// Reddit is case-insensitive, so `r/MetalMemes` and `r/metalmemes` are one
    /// place. Duplicate `discovery_places` rows drafted one post each and
    /// migration 0259 collapsed the places while deliberately leaving the drafts,
    /// which carry different text the band wrote. Publishing both is posting twice
    /// under its name.
    #[test]
    fn duplicate_community_drafts_raise_attention_by_themselves() {
        let mut snapshot = healthy();
        snapshot.duplicate_community_drafts = 2;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(
            raised,
            vec!["publishing.duplicate_community_draft"],
            "a queued double-post must fire without needing any other fault"
        );
    }

    /// The brain acting on a belief nothing has ever tested must be reported.
    ///
    /// Measured production state: 733 decisions, 0 resolved evidence, empty
    /// strategy posterior, 0 belief revisions, and 22 fans none of which any
    /// action can claim. Every other signal looked healthy the whole time —
    /// cycles succeeded, decisions were written, actions were created. Nothing
    /// said the prior had never been checked against this tenant.
    #[test]
    fn a_posterior_no_observation_has_touched_raises_attention() {
        let mut snapshot = healthy();
        snapshot.decisions_total = 733;
        snapshot.causal_observations = Some(0);
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(raised, vec!["learning.posterior_never_updated"]);
    }

    /// A missing checkpoint means the same thing as zero observations.
    ///
    /// A workspace that has decided hundreds of times and written no causal
    /// checkpoint has not learned either — the absence is the finding, not a
    /// reason to stay quiet.
    #[test]
    fn a_missing_checkpoint_counts_as_never_updated() {
        let mut snapshot = healthy();
        snapshot.decisions_total = 733;
        snapshot.causal_observations = None;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(raised, vec!["learning.posterior_never_updated"]);
    }

    /// One observation is enough to stop the alarm.
    ///
    /// This reports that learning never started, not that it is slow. The moment
    /// evidence reaches the posterior the condition has nothing to say, and the
    /// quality of what was learned is a different question with different
    /// signals.
    #[test]
    fn one_observation_silences_it() {
        let mut snapshot = healthy();
        snapshot.decisions_total = 733;
        snapshot.causal_observations = Some(1);
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            !raised.contains(&"learning.posterior_never_updated"),
            "an observation has reached the posterior; learning started"
        );
    }

    /// A young workspace is not a broken one.
    ///
    /// No observations after three decisions is a system that has not run yet.
    /// Reporting it would train an operator to ignore the alarm before it ever
    /// meant anything.
    #[test]
    fn a_young_workspace_is_not_reported() {
        let mut snapshot = healthy();
        snapshot.decisions_total = 3;
        snapshot.causal_observations = Some(0);
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(raised.is_empty(), "a system that has not run yet is not a fault");
    }

    /// And it must be critical.
    ///
    /// Every other alarm reports a thing that broke. This one reports a thing
    /// that never started, while the brain spends its action budget ranking by
    /// an assumption.
    #[test]
    fn a_never_updated_posterior_is_critical() {
        let mut snapshot = healthy();
        snapshot.decisions_total = 733;
        snapshot.causal_observations = Some(0);
        let severity = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "learning.posterior_never_updated")
            .map(|c| c.severity);
        assert_eq!(severity, Some("critical"));
    }

    /// A failed draft must be reported with its reason.
    ///
    /// The content is still in the row, so this is recoverable — but only by a
    /// person, and only if they know. The brain cannot: the parent action is
    /// terminal and the seven-day cooldown stops it drafting that community
    /// again.
    #[test]
    fn failed_drafts_are_reported_with_their_reasons() {
        let mut snapshot = healthy();
        snapshot.reddit_drafts_failed = Some("5×error sending request; 2×403".to_owned());
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(raised, vec!["publishing.drafts_failed"]);
        let reasons = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "publishing.drafts_failed")
            .and_then(|c| c.details.get("reasons").cloned());
        assert_eq!(
            reasons,
            Some(serde_json::json!("5×error sending request; 2×403")),
            "the reason decides whether the content can be requeued; a count \
             cannot say that"
        );
    }

    /// The condition this whole surface exists for.
    ///
    /// An operator approved every suggestion, believed they had set autopilot
    /// everywhere, and saw nothing publish. Reddit needs three switches, the
    /// write switch overrides the other two, and none of that was readable
    /// anywhere — the readiness log reported the community executor as enabled
    /// because the worker had been constructed, not because it would post.
    #[test]
    fn drafts_with_nothing_to_publish_them_raise_attention() {
        let mut snapshot = healthy();
        snapshot.reddit_drafts_waiting = 5;
        let drafting = PublishingPosture {
            platforms: crate::auto_post_platforms::AutoPostPlatforms::default(),
            reddit: RedditPosture::Drafts {
                missing: "CROWDRELAY_REDDIT_WRITE_ENABLED",
            },
        };
        let raised = conditions(&snapshot, drafting)
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(raised, vec!["publishing.drafts_with_no_publisher"]);
    }

    /// And it must name the switch, not merely report the count.
    ///
    /// A count of waiting drafts is what the operator could already see. The
    /// switch is what they could not.
    #[test]
    fn the_waiting_drafts_condition_names_the_missing_switch() {
        let mut snapshot = healthy();
        snapshot.reddit_drafts_waiting = 5;
        let drafting = PublishingPosture {
            platforms: crate::auto_post_platforms::AutoPostPlatforms::default(),
            reddit: RedditPosture::Drafts {
                missing: "CROWDRELAY_COMMUNITY_AUTO_POST",
            },
        };
        let named = conditions(&snapshot, drafting)
            .into_iter()
            .find(|c| c.key == "publishing.drafts_with_no_publisher")
            .and_then(|c| c.details.get("missing_switch").cloned());
        assert_eq!(
            named,
            Some(serde_json::json!("CROWDRELAY_COMMUNITY_AUTO_POST")),
        );
    }

    /// Publishing on, drafts waiting: that is a queue, not a fault.
    ///
    /// The executor claims them on its own schedule, bounded to one post per
    /// subreddit per 7 days.
    #[test]
    fn drafts_waiting_while_publishing_is_on_is_not_a_fault() {
        let mut snapshot = healthy();
        snapshot.reddit_drafts_waiting = 5;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(raised.is_empty(), "a queue being worked is not an alarm");
    }

    /// Publishing off with nothing waiting is a setting, not a fault.
    #[test]
    fn manual_mode_with_an_empty_queue_reports_nothing() {
        let drafting = PublishingPosture {
            platforms: crate::auto_post_platforms::AutoPostPlatforms::default(),
            reddit: RedditPosture::Drafts {
                missing: "CROWDRELAY_REDDIT_WRITE_ENABLED",
            },
        };
        let raised = conditions(&healthy(), drafting)
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            raised.is_empty(),
            "a manual channel with nothing queued is a choice, not a problem"
        );
    }

    /// A refused off-platform push must fire on its own.
    ///
    /// The guard refused it, so nothing was sent — which is precisely why this
    /// would otherwise be invisible. A model proposing to send the whole fanbase
    /// to an unapproved destination is a fact about the prompt, not a transient
    /// error, and the approval click shows the copy rather than the link so a
    /// human reviewer would not catch it either.
    #[test]
    fn a_refused_off_platform_push_raises_attention_by_itself() {
        let mut snapshot = healthy();
        snapshot.off_platform_push_attempts = 1;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(
            raised,
            vec!["safety.off_platform_push_proposed"],
            "one refused attempt must fire without needing another fault"
        );
    }

    /// And critical, not a warning.
    ///
    /// Nothing was sent, so the temptation is to call it a warning. The guard
    /// being the only thing between that proposal and the audience is the reason
    /// it is not.
    #[test]
    fn a_refused_off_platform_push_is_critical() {
        let mut snapshot = healthy();
        snapshot.off_platform_push_attempts = 2;
        let severity = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "safety.off_platform_push_proposed")
            .map(|c| c.severity);
        assert_eq!(severity, Some("critical"));
    }

    /// The guard breakdown must travel even when nothing is wrong.
    ///
    /// Four refusals beside twenty-one accepted outcomes is the guard working,
    /// not a fault, so it must not raise an alarm. It must still be readable:
    /// `rejection_reason` was written on every refused row and read by nothing,
    /// so a count could not be told apart from a dead connector or a broken
    /// verifier.
    #[test]
    fn guard_reasons_travel_without_raising_an_alarm() {
        let mut snapshot = healthy();
        snapshot.outcome_rejection_reasons = Some("MISSING_TARGET_IDENTITY=4".to_owned());
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            raised.is_empty(),
            "a guard doing its job is not an alarm; it is a reading"
        );
        let reported = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "learning.outcomes_unverified")
            .and_then(|c| c.details.get("guards_fired").cloned());
        assert_eq!(
            reported,
            Some(serde_json::json!("MISSING_TARGET_IDENTITY=4")),
            "the breakdown must reach the operator's view"
        );
    }

    /// A phase failing every cycle must fire on its own.
    ///
    /// This is the reading that separates "isolation absorbing a transient
    /// error", which is the design working, from "that part of the brain has
    /// stopped". Nothing else on the list can tell an operator which they have.
    #[test]
    fn a_relentlessly_failing_phase_raises_attention_by_itself() {
        let mut snapshot = healthy();
        snapshot.relentless_degraded_phases = Some("action_claim".to_owned());
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(
            raised,
            vec!["brain.phase_failing_every_cycle"],
            "a phase broken every cycle must fire without needing another fault"
        );
    }

    /// And it must be critical rather than a warning.
    ///
    /// Every cycle since the phase broke has done less than it reported, and
    /// `outcome = 'degraded'` is the same word a healthy cycle absorbing one
    /// transient error carries. A warning here would read as that.
    #[test]
    fn a_relentlessly_failing_phase_is_critical() {
        let mut snapshot = healthy();
        snapshot.relentless_degraded_phases = Some("evaluation,reply_triage_claim".to_owned());
        let severity = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "brain.phase_failing_every_cycle")
            .map(|c| c.severity);
        assert_eq!(severity, Some("critical"));
    }

    /// An occasional degraded cycle must stay quiet.
    ///
    /// The phases are isolated precisely so one can fail without stopping the
    /// rest, and reporting that would teach an operator to ignore the alarm —
    /// the same reason the cycle records `degraded` rather than `failed`.
    #[test]
    fn occasional_degradation_does_not_raise_attention() {
        let snapshot = healthy();
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            !raised.contains(&"brain.phase_failing_every_cycle"),
            "a cycle that degraded now and then is isolation working, not a fault"
        );
    }

    /// And it must be a warning, because nothing has gone out yet.
    ///
    /// Reddit posting is manual, so the queue is the last point where this is
    /// still cheap to fix. Publishing both would be the fault; this is the warning
    /// before it.
    #[test]
    fn duplicate_community_drafts_are_a_warning() {
        let mut snapshot = healthy();
        snapshot.duplicate_community_drafts = 1;
        let condition = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "publishing.duplicate_community_draft")
            .expect("condition should exist");
        assert_eq!(condition.severity, "warning");
    }

    /// A permanently refused growth event must raise attention on its own.
    ///
    /// It was silent in production for ten days. A 4xx is
    /// `http_permanent_status`, so the outbox stops retrying and the delivery is
    /// `cancelled` rather than `dead` — and `ops/attention` reports dead
    /// deliveries plus a bare `cancelled` count, so four refused press pitches
    /// were one increment in a number that also held 39 stale
    /// `ops.status_changed` refusals from August.
    #[test]
    fn a_refused_growth_event_raises_attention_by_itself() {
        let mut snapshot = healthy();
        snapshot.refused_growth_deliveries = 4;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert_eq!(
            raised,
            vec!["delivery.growth_event_refused"],
            "a refused growth event must fire without needing any other fault"
        );
    }

    /// And it must be critical, not a warning.
    ///
    /// `publishing.orphaned_draft` is a warning because the draft was never
    /// spent. This one has already spent everything: verified outcome, created
    /// action, event built with its recipient, handed to the outbox — and then
    /// refused. The work is complete and reaches nobody.
    #[test]
    fn a_refused_growth_event_is_critical() {
        let mut snapshot = healthy();
        snapshot.refused_growth_deliveries = 1;
        let condition = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "delivery.growth_event_refused")
            .expect("condition should exist");
        assert_eq!(condition.severity, "critical");
    }

    /// A healthy delivery path must not raise it.
    #[test]
    fn no_refused_growth_event_stays_quiet() {
        let raised = conditions(&healthy(), publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            !raised.contains(&"delivery.growth_event_refused"),
            "raised on a healthy snapshot: {raised:?}"
        );
    }

    /// The condition must fire when actionable outcomes are all refused, even
    /// while observations keep flowing.
    ///
    /// This is the shape production was actually in: the grounding gate covers
    /// `require_approval` kinds only, so insights and segments kept arriving
    /// and kept being accepted. Comparing refusals against *every* accepted
    /// kind made the alarm unfirable in the one state it exists to report, and
    /// it stayed silent on the first cycle after deploy with 94 refusals
    /// behind it.
    #[test]
    fn refused_actionable_outcomes_raise_attention_even_while_insights_flow() {
        let mut snapshot = healthy();
        snapshot.outcomes_rejected_unverified = 7;
        snapshot.outcomes_accepted = 0;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            raised.contains(&"learning.outcomes_unverified"),
            "expected the learning alarm, raised: {raised:?}"
        );
    }

    #[test]
    fn a_few_refusals_beside_healthy_actionable_traffic_are_not_an_alarm() {
        let mut snapshot = healthy();
        snapshot.outcomes_rejected_unverified = 2;
        snapshot.outcomes_accepted = 9;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|c| c.active)
            .map(|c| c.key)
            .collect::<Vec<_>>();
        assert!(
            !raised.contains(&"learning.outcomes_unverified"),
            "a verifier doing its job is not an outage, raised: {raised:?}"
        );
    }

    #[test]
    fn healthy_runtime_does_not_raise_attention() {
        assert!(
            conditions(&healthy(), publishing())
                .iter()
                .all(|condition| !condition.active)
        );
    }

    #[test]
    fn executor_offline_is_detected() {
        let mut snapshot = healthy();
        snapshot.executor_active = 0;
        let active = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"executor.offline"));
    }

    #[test]
    fn transient_unknown_does_not_alert() {
        // Unknown actions that are within the alert age threshold
        // (stale_unknown_actions = 0) should NOT trigger the alert —
        // the reconciliation sweep may still resolve them.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 2;
        snapshot.stale_unknown_actions = 0;
        let active = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(!active.contains(&"execution.unknown_outcome"));
    }

    #[test]
    fn stale_unknown_action_outcomes_are_detected() {
        // Unknown actions whose unknown_age exceeds the threshold
        // should trigger the alert.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 2;
        snapshot.stale_unknown_actions = 1;
        let active = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"execution.unknown_outcome"));
        assert!(!active.contains(&"executor.offline"));
    }

    #[test]
    fn contradicted_outcomes_are_detected_with_no_age_grace() {
        // A contradiction has no sweep behind it — unlike `unknown`, nothing
        // will resolve it on its own — so a single one alerts immediately
        // rather than waiting out a staleness threshold.
        let mut snapshot = healthy();
        snapshot.contradicted_actions = 1;
        let active = conditions(&snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"execution.contradicted_outcome"));
        assert!(!active.contains(&"execution.unknown_outcome"));
    }

    #[test]
    fn a_resolved_action_with_older_receipts_is_not_a_contradiction() {
        // The snapshot query compares only the *newest* terminal receipt, so
        // an ordinary failure-then-success history contributes nothing here.
        // Guard the condition side of that: zero means silent.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 3;
        snapshot.contradicted_actions = 0;
        assert!(conditions(&snapshot, publishing()).iter().all(|condition| condition.key
            != "execution.contradicted_outcome"
            || !condition.active));
    }

    /// The keys `conditions` reports as active for a snapshot.
    fn active_keys(snapshot: &OpsSnapshot) -> Vec<&'static str> {
        conditions(snapshot, publishing())
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect()
    }

    #[test]
    fn one_failing_feed_beside_a_working_one_is_a_warning() {
        let mut snapshot = healthy();
        snapshot.failing_platforms = 1;
        snapshot.working_platforms = 2;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.feed_failing"));
        assert!(!active.contains(&"growth.all_feeds_failing"));
    }

    #[test]
    fn every_feed_failing_is_critical_and_not_also_a_warning() {
        // Production's standing state: every Reddit connection failing on an
        // invalid credential, for weeks, with nothing on the watchdog saying
        // so. The brain will not plan discovery through silent feeds, so this
        // is the whole acquisition side of the North Star stopped until a
        // person restores a credential -- and nothing recovers it on its own.
        //
        // The two conditions are mutually exclusive on purpose: raising both
        // would put the same fact on the operator's list twice, and an
        // exception list that repeats itself stops being read.
        let mut snapshot = healthy();
        snapshot.failing_platforms = 1;
        snapshot.working_platforms = 0;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.all_feeds_failing"));
        assert!(!active.contains(&"growth.feed_failing"));
    }

    #[test]
    fn a_tenant_with_no_feeds_at_all_raises_nothing() {
        // Nothing connected is not a failure. A tenant who has not connected a
        // feed yet has none that could be broken, and alerting on that would
        // fire on every new workspace from its first cycle.
        let mut snapshot = healthy();
        snapshot.failing_platforms = 0;
        snapshot.working_platforms = 0;
        let active = active_keys(&snapshot);
        assert!(!active.contains(&"growth.feed_failing"));
        assert!(!active.contains(&"growth.all_feeds_failing"));
    }

    #[test]
    fn stuck_ungeocoded_cities_are_detected() {
        // Cities that fans requested but the geocoder gave up on. Every fan in
        // one is unreachable by the nearby-show loop until a human fixes the
        // name or enters coordinates by hand.
        let mut snapshot = healthy();
        snapshot.stuck_ungeocoded_cities = 3;
        snapshot.fans_awaiting_geocoding = 7;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.stuck_ungeocoded_cities"));
    }

    /// The count now comes from a query that requires a waiting fan, so a
    /// stuck city nobody selected never reaches this snapshot at all.
    ///
    /// Production raised this warning for a day over two test rows --
    /// "Example City, Example Region" and "Tes5, Test" -- which the geocoder
    /// correctly refused five times because neither place exists, while the
    /// summary told the operator that fans there were unreachable. Nobody had
    /// ever selected either. The finding was true about rows and false about
    /// people, and only the second reading is worth waking anyone for.
    #[test]
    fn a_stuck_city_with_nobody_waiting_is_not_an_alarm() {
        let snapshot = healthy();
        assert_eq!(snapshot.stuck_ungeocoded_cities, 0);
        assert_eq!(snapshot.fans_awaiting_geocoding, 0);
        assert!(!active_keys(&snapshot).contains(&"growth.stuck_ungeocoded_cities"));
    }

    #[test]
    fn no_stuck_cities_raises_nothing() {
        // A tenant with no stuck cities (either no requests, or all resolved,
        // or still being retried) should not trigger the alert.
        let snapshot = healthy();
        let active = active_keys(&snapshot);
        assert!(!active.contains(&"growth.stuck_ungeocoded_cities"));
    }
}
