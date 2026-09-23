// Application-level join-ask mapping tests: the domain's eligibility rules
// live in `crowdrelay-domain/src/join_ask.rs`; these pin what the candidate
// carries out of them — verbatim text, the `/signal` CTA, the
// week-scoped keys, and the standing-approval clamp on the disposition.

#[cfg(test)]
mod join_ask_tests {
    use super::join_ask::evaluate_join_ask_candidates;
    use super::*;
    use crowdrelay_domain::{
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        content_engine::ContentStrategyPolicy,
        join_ask::JoinAskSnapshot,
    };
    use time::OffsetDateTime;
    use time::macros::datetime;

    fn snapshot() -> JoinAskSnapshot {
        JoinAskSnapshot {
            variants: vec!["join us".to_owned(), "come along".to_owned()],
            cadence_days: 7,
            platforms: vec!["facebook".to_owned()],
            member_site_base_url: Some("https://virya.music".to_owned()),
            social_auto_post: true,
            connected_platforms: vec!["facebook".to_owned()],
            posts: Vec::new(),
            instagram_photo_count: 0,
        }
    }

    fn policy(
        autonomy_level: AutonomyLevel,
    ) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        Ok(AutopilotPolicy {
            context: AutopilotContext::ContentStrategy,
            enabled: true,
            autonomy_level,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 6,
            config: AutopilotPolicyConfig::ContentStrategy(
                ContentStrategyPolicy::default(),
            ),
            version: 3,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    #[test]
    fn an_eligible_platform_emits_one_keyed_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(7));
        let now = datetime!(2026-09-23 10:00 UTC);
        let evaluation = evaluate_join_ask_candidates(
            &snapshot(),
            &policy(AutonomyLevel::BoundedAuto)?,
            workspace_id,
            now,
        )?;
        assert_eq!(evaluation.candidates.len(), 1);
        assert!(evaluation.held.is_empty());
        let candidate = &evaluation.candidates[0];
        // Standing approval on the channel + a bounded-auto policy → the
        // ask executes without a per-post click.
        assert_eq!(candidate.disposition, PolicyDisposition::AutoExecute);
        // The week is inside both keys — a second cycle the same week
        // conflicts rather than posting twice.
        assert_eq!(candidate.decision_key, "join_ask:facebook:2026-W39");
        assert_eq!(
            candidate.action_idempotency_key,
            "join_ask:facebook:2026-W39"
        );
        let AutopilotActionPayload::PublishJoinAsk {
            platform,
            variant_index,
            text,
            cta_url,
        } = &candidate.action
        else {
            return Err(std::io::Error::other(format!(
                "expected PublishJoinAsk, got {:?}",
                candidate.action
            ))
            .into());
        };
        assert_eq!(platform, "facebook");
        assert_eq!(*variant_index, 0);
        // The tenant's words, verbatim — the link belongs to the executor.
        assert_eq!(text, "join us");
        assert_eq!(
            cta_url,
            "https://virya.music/signal?utm_source=facebook&utm_medium=join_ask&utm_campaign=join_ask_w39"
        );
        Ok(())
    }

    #[test]
    fn no_standing_publish_approval_holds_the_candidate_for_a_person()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(7));
        let mut snapshot = snapshot();
        snapshot.social_auto_post = false;
        // The widest policy cannot lift a channel the operator never
        // approved for unattended publishing — the flag clamps, not grants.
        let evaluation = evaluate_join_ask_candidates(
            &snapshot,
            &policy(AutonomyLevel::BoundedAuto)?,
            workspace_id,
            OffsetDateTime::now_utc(),
        )?;
        assert_eq!(evaluation.candidates.len(), 1);
        assert_eq!(
            evaluation.candidates[0].disposition,
            PolicyDisposition::RequireApproval
        );
        Ok(())
    }

    #[test]
    fn a_held_platform_reports_its_reason_instead_of_a_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(7));
        let mut snapshot = snapshot();
        snapshot.connected_platforms.clear();
        let evaluation = evaluate_join_ask_candidates(
            &snapshot,
            &policy(AutonomyLevel::BoundedAuto)?,
            workspace_id,
            OffsetDateTime::now_utc(),
        )?;
        assert!(evaluation.candidates.is_empty());
        assert_eq!(
            evaluation.held,
            vec![(
                "facebook".to_owned(),
                crowdrelay_domain::join_ask::JoinAskHold::NotConnected
            )]
        );
        Ok(())
    }
}
