pub(super) async fn record_execution_outcome(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let (metric_key, observed_value, baseline_value) = match payload {
        AutopilotActionPayload::ChangeTicketPrice {
            from_minor,
            to_minor,
            ..
        } => (
            "ticket_price_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::ChangeTicketCapacity {
            from_capacity,
            to_capacity,
            ..
        } => (
            "ticket_capacity",
            f64::from(*to_capacity),
            Some(f64::from(*from_capacity)),
        ),
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => {
            ("lifecycle_message_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestMerchReorder { quantity, .. } => {
            ("merch_reorder_quantity", f64::from(*quantity), None)
        }
        AutopilotActionPayload::ChangeMerchPrice {
            from_minor,
            to_minor,
            ..
        } => (
            "merch_price_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::RequestBookingOutreach { score, .. } => {
            ("booking_opportunity_score", f64::from(*score), None)
        }
        AutopilotActionPayload::RequestAudienceCampaign { .. } => {
            ("audience_campaign_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestSourceCampaign { .. } => {
            ("source_campaign_requested", 1.0, None)
        }
        // The count, not a flag: a night offered to three promoters and a
        // night offered to one are different asks, and 4G.5 scores replies
        // against how many people were written to.
        AutopilotActionPayload::RequestGigOutreach { recipients, .. } => (
            "gig_outreach_recipients",
            recipients.len() as f64,
            None,
        ),
        AutopilotActionPayload::RequestMerchBundle {
            bundle_price_minor, ..
        } => ("merch_bundle_price_minor", *bundle_price_minor as f64, None),
        AutopilotActionPayload::RequestOutreach { .. } => ("outreach_requested", 1.0, None),
        AutopilotActionPayload::RequestOutreachReply { .. } => ("outreach_reply_sent", 1.0, None),
        // One letter, one person. The measure worth having later is how many
        // of these turned into somebody who hears the dates, which is a fan
        // row appearing against an address the band already knew.
        AutopilotActionPayload::RequestLatarnikInvite { .. } => ("latarnik_invite_sent", 1.0, None),
        AutopilotActionPayload::RequestRepresentationApproach { .. } => {
            ("representation_approach_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestBookingAgentApproach { .. } => {
            ("booking_agent_approach_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestBookingAgentReply { .. } => {
            ("booking_agent_reply_sent", 1.0, None)
        }
        // The wave's measure is letters sent, not waves approved — the
        // funnel reads per agent either way.
        AutopilotActionPayload::RequestBookingAgentApproachWave { approaches, .. } => {
            (
                "booking_agent_approach_requested",
                approaches.len() as f64,
                None,
            )
        }
        AutopilotActionPayload::RequestBeaconDiscovery { .. } => ("beacon_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestOutreachDiscovery { .. } => ("outreach_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => ("booking_target_discovery_requested", 1.0, None),
        AutopilotActionPayload::RequestBeaconOutreach { .. } => ("beacon_outreach_requested", 1.0, None),
        AutopilotActionPayload::RequestBeaconInviteBatch { requested_count, .. } => {
            ("beacon_invite_batch_requested", f64::from(*requested_count), None)
        }
        AutopilotActionPayload::RaiseGrowthOpportunity {
            deviation_basis_points,
            ..
        } => (
            "growth_opportunity_raised",
            f64::from(*deviation_basis_points),
            None,
        ),
        AutopilotActionPayload::IssueReferralCode { .. } => ("referral_code_issued", 1.0, None),
        AutopilotActionPayload::RunPlayStep { step_index, .. } => {
            ("play_step_dispatched", f64::from(*step_index), None)
        }
        AutopilotActionPayload::SetEventTicketUrl { .. } => ("event_ticket_url_set", 1.0, None),
        AutopilotActionPayload::RaiseGrowthDebt {
            overdue_basis_points,
            ..
        } => (
            "growth_debt_raised",
            f64::from(*overdue_basis_points),
            None,
        ),
        AutopilotActionPayload::RunArchivePromoteWave {
            limit,
            staged_count,
            ..
        } => (
            // The wave size fired against the backlog it was raised on —
            // confirmations are measured downstream by the fan lifecycle,
            // not attributed to the approval click.
            "archive_promote_wave_run",
            *limit as f64,
            Some(*staged_count as f64),
        ),
        AutopilotActionPayload::RequestShowGrowth { .. } => ("show_growth_lever_requested", 1.0, None),
        AutopilotActionPayload::RequestContentArtifact { .. } => {
            ("content_artifact_requested", 1.0, None)
        }
        AutopilotActionPayload::AdjustExperiment { complete, .. } => (
            if *complete {
                "experiment_completed"
            } else {
                "experiment_allocation_changed"
            },
            1.0,
            None,
        ),
        AutopilotActionPayload::CompleteShowTask { .. } => ("show_task_completed", 1.0, None),
        AutopilotActionPayload::EscalateShowTask { .. } => ("show_task_escalated", 1.0, None),
        AutopilotActionPayload::RequestPromotionBudgetChange {
            from_minor,
            to_minor,
            ..
        } => (
            "promotion_daily_budget_minor",
            *to_minor as f64,
            Some(*from_minor as f64),
        ),
        AutopilotActionPayload::ExecuteReleaseMilestone { .. } => ("release_milestone_executed",1.0,None),
        AutopilotActionPayload::ApplyLiveOpportunity { score, .. } => ("live_opportunity_score",f64::from(*score),None),
        AutopilotActionPayload::VerifyPlaylistPlacement { checkpoint, .. } => {
            ("playlist_placement_checked", f64::from(*checkpoint), None)
        }
        AutopilotActionPayload::EscalateEditorialPitch { .. } => {
            ("editorial_pitch_escalated", 1.0, None)
        }
        // The number worth recording is the money asked for or taken. A round
        // count would say how hard the agent pushed and nothing about whether
        // the push was worth making.
        AutopilotActionPayload::CounterLiveOpportunityTerms { ask_minor, .. } => {
            ("live_opportunity_counter_minor", *ask_minor as f64, None)
        }
        AutopilotActionPayload::AcceptLiveOpportunityTerms { fee_minor, .. } => {
            ("live_opportunity_accepted_minor", *fee_minor as f64, None)
        }
        AutopilotActionPayload::IssueCounterpartyReport { .. } => {
            ("counterparty_report_issued", 1.0, None)
        }
        AutopilotActionPayload::PrepareFundingPackage { .. } => ("funding_package_requested",1.0,None),
        AutopilotActionPayload::SubmitFundingApplication { .. } => ("funding_submission_requested",1.0,None),
        AutopilotActionPayload::SendTeamAssignmentEmail { .. } => {
            ("team_assignment_email_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestAgentContent { .. } => {
            ("agent_content_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestOutreachTarget { .. } => {
            ("outreach_target_promoted", 1.0, None)
        }
        AutopilotActionPayload::RequestAgentRun { .. } => {
            ("agent_run_dispatched", 1.0, None)
        }
        AutopilotActionPayload::RequestCommunityEngagement { .. } => {
            ("community_engagement_requested", 1.0, None)
        }
        AutopilotActionPayload::RequestSignalPush { .. } => {
            ("signal_push_requested", 1.0, None)
        }
        // The action's own success is the post being *filed* — the social
        // executor's `social_posts` row is the publication receipt, and its
        // clicks land on the `content_link_clicks_7d` measurement.
        AutopilotActionPayload::PublishJoinAsk { .. } => ("join_ask_published", 1.0, None),
        AutopilotActionPayload::RaiseContentSuggestion { .. } => {
            ("content_suggestion_raised", 1.0, None)
        }
        AutopilotActionPayload::RaiseContentArc { .. } => ("content_arc_raised", 1.0, None),
        AutopilotActionPayload::RaiseDeclineAdvisory { posts_considered, .. } => {
            ("decline_advisory_raised", f64::from(*posts_considered), None)
        }
    };
    sqlx::query(
        r#"
        INSERT INTO autopilot_outcomes (
            workspace_id, decision_id, action_id, metric_key,
            observed_value, baseline_value, observed_at
        )
        SELECT $1, action.decision_id, action.id, $3, $4, $5, $6
        FROM autopilot_actions AS action
        WHERE action.workspace_id = $1 AND action.id = $2
        -- The arbiter predicate must mirror
        -- autopilot_outcomes_action_metric_uidx exactly; 0303 added
        -- `measurement_id IS NULL` to the index and this statement broke.
        ON CONFLICT (workspace_id, action_id, metric_key)
            WHERE action_id IS NOT NULL AND measurement_id IS NULL DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(metric_key)
    .bind(observed_value)
    .bind(baseline_value)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
