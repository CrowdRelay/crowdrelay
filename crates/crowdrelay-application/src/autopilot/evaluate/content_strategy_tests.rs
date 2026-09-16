#[cfg(test)]
mod content_strategy_candidate_tests {
    use super::*;
    use crowdrelay_domain::{
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        content_engine::{
            Arc, ArcStatus, ContentStrategyPolicy, ContentSuggestion, Effort, SuggestionStatus,
        },
    };
    use serde_json::json;
    use time::OffsetDateTime;

    fn suggestion() -> ContentSuggestion {
        ContentSuggestion {
            id: crowdrelay_domain::ContentSuggestionId::from_uuid(uuid::Uuid::from_u128(77)),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(7)),
            arc_id: None,
            format_key: Some("playthrough".to_owned()),
            concept: "Playthrough".to_owned(),
            reason: "Peers ride single releases; your fans react to two of them.".to_owned(),
            evidence: json!({
                "trend_ids": [uuid::Uuid::from_u128(9)],
                "arc_format_key_hit": false,
                "covered_by_production": true,
                "efe_score": 0.42,
            }),
            suggested_after: None,
            suggested_before: None,
            effort: Some(Effort::Low),
            proposed_assignee_member_id: None,
            distribution_promise: json!({"communities": ["r/Metal"], "consented_fans": 340}),
            status: SuggestionStatus::Raised,
            expires_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn policy(
        autonomy_level: AutonomyLevel,
        config: ContentStrategyPolicy,
    ) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        Ok(AutopilotPolicy {
            context: AutopilotContext::ContentStrategy,
            enabled: true,
            autonomy_level,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 6,
            config: AutopilotPolicyConfig::ContentStrategy(config),
            version: 3,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    #[test]
    fn a_raised_suggestion_lands_as_one_queue_entry_with_its_evidence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let suggestion = suggestion();
        let candidate = content_strategy_candidate(
            &suggestion,
            &policy(AutonomyLevel::RequireApproval, ContentStrategyPolicy::default())?,
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.context, AutopilotContext::ContentStrategy);
        assert_eq!(candidate.disposition, PolicyDisposition::RequireApproval);
        assert_eq!(candidate.decision_kind, "raise_content_suggestion");
        // One action per suggestion, ever — the band's answer must not be
        // argued with by the next cycle.
        assert_eq!(
            candidate.action_idempotency_key,
            format!("action:content-suggestion:{}", suggestion.id.into_uuid())
        );
        let AutopilotActionPayload::RaiseContentSuggestion {
            suggestion_id,
            reason,
            distribution_promise,
            ..
        } = candidate.action
        else {
            return Err(std::io::Error::other("wrong payload").into());
        };
        assert_eq!(suggestion_id, suggestion.id);
        assert_eq!(reason, suggestion.reason);
        // The promise rides in the payload — the evidence panel reads it
        // without a join back to the suggestion row.
        assert_eq!(distribution_promise["consented_fans"], 340);
        Ok(())
    }

    #[test]
    fn corroborated_evidence_carries_more_confidence_than_a_bare_raise(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let policy =
            policy(AutonomyLevel::RequireApproval, ContentStrategyPolicy::default())?;
        let bare = ContentSuggestion {
            evidence: json!({}),
            ..suggestion()
        };
        let corroborated = suggestion();
        let bare_candidate = content_strategy_candidate(&bare, &policy)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        let corroborated_candidate = content_strategy_candidate(&corroborated, &policy)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert!(
            corroborated_candidate.confidence.basis_points()
                > bare_candidate.confidence.basis_points()
        );
        Ok(())
    }

    #[test]
    fn a_configured_floor_drops_thin_scores_and_unscored_rows(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let floor_policy = policy(
            AutonomyLevel::RequireApproval,
            ContentStrategyPolicy {
                minimum_efe_score: Some(0.5),
            },
        )?;
        // 0.42 < 0.5: below the floor, never queued.
        assert!(content_strategy_candidate(&suggestion(), &floor_policy)?.is_none());
        // A row with no score at all cannot be checked against the floor —
        // fail closed rather than wave it through.
        let unscored = ContentSuggestion {
            evidence: json!({}),
            ..suggestion()
        };
        assert!(content_strategy_candidate(&unscored, &floor_policy)?.is_none());
        Ok(())
    }

    #[test]
    fn observe_and_recommend_never_enqueue_an_action(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for level in [AutonomyLevel::Observe, AutonomyLevel::Recommend] {
            let candidate = content_strategy_candidate(
                &suggestion(),
                &policy(level, ContentStrategyPolicy::default())?,
            )?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
            assert!(matches!(
                candidate.disposition,
                PolicyDisposition::ObserveOnly | PolicyDisposition::RecommendOnly
            ));
        }
        Ok(())
    }

    fn arc() -> Arc {
        Arc {
            id: crowdrelay_domain::ArcId::from_uuid(uuid::Uuid::from_u128(88)),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(7)),
            title: "Single: release".to_owned(),
            summary: "A 4-beat run at Single — 2026-10-02 to 2026-11-06.".to_owned(),
            horizon_start: Some(time::Date::from_calendar_date(2026, time::Month::October, 2).unwrap()),
            horizon_end: Some(time::Date::from_calendar_date(2026, time::Month::November, 6).unwrap()),
            spine: json!([
                {"at": "2026-10-10", "beat": "Playthrough", "format_key": "playthrough"},
                {"at": "2026-10-20", "beat": "Making of", "format_key": "making_of"}
            ]),
            evidence: json!({
                "anchor": {"kind": "release", "id": "rel-1", "name": "Single", "date": "2026-10-30"},
                "trend_ids": [uuid::Uuid::from_u128(9)],
                "format_keys": ["playthrough", "making_of"]
            }),
            status: ArcStatus::Proposed,
            approved_at: None,
            approved_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_proposed_arc_lands_as_one_queue_entry_with_its_plan(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let arc = arc();
        let candidate = content_arc_candidate(
            &arc,
            &policy(AutonomyLevel::RequireApproval, ContentStrategyPolicy::default())?,
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;

        assert_eq!(candidate.context, AutopilotContext::ContentStrategy);
        assert_eq!(candidate.disposition, PolicyDisposition::RequireApproval);
        assert_eq!(candidate.decision_kind, "raise_content_arc");
        assert_eq!(
            candidate.subject,
            ActionSubject::ContentArc(arc.id),
            "the arc, not a beat inside it, is what the band approves"
        );
        // One ask per arc, ever — a declined season is a taste signal the
        // cooldown remembers, not a question to re-ask.
        assert_eq!(
            candidate.action_idempotency_key,
            format!("action:content-arc:{}", arc.id.into_uuid())
        );
        let AutopilotActionPayload::RaiseContentArc {
            arc_id,
            title,
            beats,
            ..
        } = candidate.action
        else {
            return Err(std::io::Error::other("wrong payload").into());
        };
        assert_eq!(arc_id, arc.id);
        assert_eq!(title, arc.title);
        assert_eq!(beats, 2, "the spine's size rides the ask");
        Ok(())
    }

    #[test]
    fn a_corroborated_arc_carries_more_confidence_than_a_bare_anchor(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let policy =
            policy(AutonomyLevel::RequireApproval, ContentStrategyPolicy::default())?;
        let bare = Arc {
            evidence: json!({"anchor": {"kind": "release", "id": "rel-1"}, "trend_ids": []}),
            ..arc()
        };
        let corroborated = arc();
        let bare_bp = content_arc_candidate(&bare, &policy)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?
            .confidence
            .basis_points();
        let corroborated_bp = content_arc_candidate(&corroborated, &policy)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?
            .confidence
            .basis_points();
        assert!(
            corroborated_bp > bare_bp,
            "trend evidence must move the ask: {corroborated_bp} vs {bare_bp}"
        );
        Ok(())
    }
}
