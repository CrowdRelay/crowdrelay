//! Organic-funnel control over Growth Intelligence.
//!
//! The canonical funnel decides whether this cycle needs more attributable
//! reach or whether acquisition must stop while a downstream leak is repaired.
//! It never creates authority: candidates still pass their existing policy,
//! consent, cooldown and execution gates.

use super::*;

fn organic_funnel_template_rank(
    directive: OrganicFunnelDirective,
    template: WorkerTemplate,
) -> Option<usize> {
    if directive != OrganicFunnelDirective::ExpandReach {
        return None;
    }
    if template.can_acquire_new_fans() {
        return Some(0);
    }
    matches!(
        template,
        WorkerTemplate::RedditScanner
            | WorkerTemplate::TelegramScanner
            | WorkerTemplate::MetalArchivesScanner
            | WorkerTemplate::FanbaseScout
    )
    .then_some(1)
}

pub(crate) fn organic_funnel_control_summary(control: OrganicFunnelControl) -> String {
    format!(
        "organic funnel control: directive={} mature_links={} visitors={} signups={} confirmed={} activation={}/{} retention={}/{}",
        control.directive.as_str(),
        control.mature_links,
        control.unique_visitors,
        control.signups,
        control.confirmed,
        control.activated_mature,
        control.activation_mature,
        control.retained,
        control.retention_mature,
    )
}

pub(crate) fn apply_organic_funnel_control(
    candidates: &mut Vec<ScoredCandidate>,
    control: OrganicFunnelControl,
) {
    candidates.retain_mut(|candidate| {
        let AutopilotActionPayload::RequestAgentRun { template_id, .. } =
            &candidate.candidate.action
        else {
            return false;
        };
        let Some(template) = WorkerTemplate::parse(template_id) else {
            return false;
        };
        let Some(rank) = organic_funnel_template_rank(control.directive, template) else {
            return false;
        };
        if let Some(object) = candidate.candidate.input_snapshot.as_object_mut() {
            object.insert(
                "organic_funnel_control".to_owned(),
                serde_json::to_value(control).unwrap_or(serde_json::Value::Null),
            );
        }
        candidate.strategy_rank = rank;
        true
    });
}

#[cfg(test)]
mod organic_funnel_control_tests {
    use super::*;

    #[test]
    fn no_visitors_prioritizes_real_reach_before_more_research() {
        for template in [
            WorkerTemplate::SocialPost,
            WorkerTemplate::TelegramPoster,
            WorkerTemplate::DiscordPoster,
            WorkerTemplate::CommunityEngager,
        ] {
            assert_eq!(
                organic_funnel_template_rank(OrganicFunnelDirective::ExpandReach, template),
                Some(0),
                "{template:?} should be an outward reach action"
            );
        }
        for template in [
            WorkerTemplate::RedditScanner,
            WorkerTemplate::TelegramScanner,
            WorkerTemplate::MetalArchivesScanner,
            WorkerTemplate::FanbaseScout,
        ] {
            assert_eq!(
                organic_funnel_template_rank(OrganicFunnelDirective::ExpandReach, template),
                Some(1),
                "{template:?} should only replenish reach supply"
            );
        }
        for template in [
            WorkerTemplate::SignalInviter,
            WorkerTemplate::PressPitch,
            WorkerTemplate::GrowthStrategist,
            WorkerTemplate::StrategyConsult,
        ] {
            assert_eq!(
                organic_funnel_template_rank(OrganicFunnelDirective::ExpandReach, template),
                None,
                "{template:?} does not move attributable new-person reach"
            );
        }
    }

    #[test]
    fn join_ask_is_only_a_reach_or_direct_conversion_recovery() {
        assert!(OrganicFunnelDirective::ExpandReach.permits_join_ask());
        assert!(OrganicFunnelDirective::RepairConversion.permits_join_ask());
        assert!(!OrganicFunnelDirective::RepairConfirmation.permits_join_ask());
        assert!(!OrganicFunnelDirective::ActivateFans.permits_join_ask());
        assert!(!OrganicFunnelDirective::RetainFans.permits_join_ask());
    }

    #[test]
    fn downstream_leaks_stop_growth_intelligence_from_buying_more_top_of_funnel() {
        for directive in [
            OrganicFunnelDirective::RepairConversion,
            OrganicFunnelDirective::RepairConfirmation,
            OrganicFunnelDirective::ActivateFans,
            OrganicFunnelDirective::RetainFans,
        ] {
            for template in WorkerTemplate::active() {
                assert_eq!(
                    organic_funnel_template_rank(directive, template),
                    None,
                    "{directive:?} must hand the cycle to downstream recovery, not {template:?}"
                );
            }
        }
    }
}
