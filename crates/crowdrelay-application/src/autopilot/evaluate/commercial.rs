//! Funding, merch, booking and campaign candidate construction.

use super::*;

pub(super) fn funding_candidate(
    snapshot: FundingOpportunitySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Funding(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_funding(snapshot, *domain_policy, now);
    let (decision_kind, confidence, action, force_approval, key) = match decision {
        FundingDecision::Hold => return Ok(None),
        FundingDecision::PreparePackage { confidence } => (
            "prepare_funding_package",
            confidence,
            AutopilotActionPayload::PrepareFundingPackage {
                opportunity_id: snapshot.opportunity_id,
            },
            false,
            "prepare",
        ),
        FundingDecision::SubmitForApproval { confidence } => (
            "submit_funding_application",
            confidence,
            AutopilotActionPayload::SubmitFundingApplication {
                opportunity_id: snapshot.opportunity_id,
            },
            true,
            "submit",
        ),
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    if force_approval && matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::TeamOpportunity(snapshot.opportunity_id),
        decision_kind,
        confidence,
        disposition,
        reason: "eligible funding opportunity clears deterministic value and contribution gates",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        decision_key: format!(
            "decision:funding:v{}:{}:{key}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.deadline.unix_timestamp()
        ),
        action_idempotency_key: format!("action:funding:{}:{key}", snapshot.opportunity_id),
    }))
}

pub(super) fn merch_candidate(
    snapshot: MerchInventorySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Merchandising(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchReorderDecision::RequestReorder {
        quantity,
        confidence,
    } = evaluate_reorder(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::MerchVariant(snapshot.variant_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_merch_reorder",
        confidence,
        disposition,
        reason: "projected stock coverage is below bounded target",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestMerchReorder {
            variant_id: snapshot.variant_id,
            quantity,
        },
        decision_key: format!(
            "decision:merch:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.variant_id,
            snapshot.available_quantity,
            snapshot.sold_last_30d,
            snapshot
                .last_reorder_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:merch:{}:reorder:{}:{quantity}",
            snapshot.variant_id,
            snapshot.last_reorder_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string()
            )
        ),
    }))
}

pub(super) fn merch_price_candidate(
    snapshot: MerchPriceSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::MerchPricing(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchPriceDecision::ChangePrice {
        direction,
        to_minor,
        confidence,
    } = evaluate_merch_price(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let Ok(from_minor) = i64::try_from(snapshot.current_price_minor) else {
        return Ok(None);
    };
    let Ok(to_minor) = i64::try_from(to_minor) else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let reason = match direction {
        MerchPriceDirection::Increase => {
            "recent demand acceleration and scarce stock justify one bounded price step"
        }
        MerchPriceDirection::Decrease => {
            "stagnant demand and excess stock justify one margin-safe price step"
        }
    };
    let subject = ActionSubject::MerchProduct(snapshot.product_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "change_merch_price",
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::ChangeMerchPrice {
            product_id: snapshot.product_id,
            from_minor,
            to_minor,
            economics_version: snapshot.economics_version,
        },
        decision_key: format!(
            "decision:merch-price:v{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.product_id,
            snapshot.current_price_minor,
            snapshot.minimum_price_minor,
            snapshot.maximum_price_minor,
            snapshot.economics_version,
            snapshot.available_quantity,
            snapshot.sold_last_7d,
            snapshot.sold_last_30d,
            snapshot
                .last_price_change_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:merch-price:{}:{}:{}:ev{}:{}",
            snapshot.product_id,
            from_minor,
            to_minor,
            snapshot.economics_version,
            snapshot
                .last_price_change_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
    }))
}

pub(super) fn booking_candidate(
    snapshot: CityOpportunitySnapshot,
    targets: &[BookingTargetSnapshot],
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let BookingOpportunityDecision::RequestOutreach { score, confidence } =
        evaluate_booking_opportunity(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let target_policy = BookingTargetSelectionPolicy::default();
    let expected_attendance = estimated_attendance(snapshot);
    let BookingTargetDecision::Selected {
        target_id,
        target_version,
        selection_score,
    } = select_booking_target(
        snapshot.city_id,
        expected_attendance,
        targets,
        target_policy,
        now,
    )
    else {
        return Ok(None);
    };
    let Some(target_snapshot) = targets.iter().find(|target| target.target_id == target_id) else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::City(snapshot.city_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_booking_outreach",
        confidence,
        disposition,
        reason: "city demand exceeds threshold and a verified booking target is eligible",
        input_snapshot: serde_json::json!({
            "city": snapshot,
            "target": target_snapshot,
            "selection_score": selection_score,
            "expected_attendance": expected_attendance,
        }),
        policy_snapshot: policy_evidence(
            policy,
            serde_json::json!({
                "opportunity": domain_policy,
                "target_selection": target_policy,
            }),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: snapshot.city_id,
            target_id,
            target_version,
            target_name: target_snapshot.display_name.clone(),
            score,
            phase: BookingOutreachPhase::Initial,
        },
        decision_key: format!(
            "decision:booking:v{}:{}:{}:tv{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.city_id,
            target_id,
            target_version,
            snapshot.active_fans,
            snapshot.new_fans_30d,
            snapshot.event_interests,
            snapshot.area_claims,
            snapshot
                .market_evidence
                .map_or(0, |value| value.score_basis_points),
            snapshot
                .market_evidence
                .map_or(0, |value| value.confidence.basis_points()),
            expected_attendance,
            selection_score,
            snapshot
                .last_outreach_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:booking:{}:{}:tv{}:{}",
            snapshot.city_id,
            target_id,
            target_version,
            snapshot.last_outreach_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

pub(super) fn booking_followup_candidate(
    target: &BookingTargetSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let followup_policy = BookingFollowUpPolicy::default();
    let BookingFollowUpDecision::Request { confidence } =
        evaluate_booking_followup(target, followup_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::City(target.city_id),
        decision_kind: "request_booking_followup",
        confidence,
        disposition,
        reason: "verified booking target has not replied and the bounded follow-up window is due",
        input_snapshot: serde_json::to_value(target)?,
        policy_snapshot: policy_evidence(
            policy,
            serde_json::json!({"opportunity":domain_policy,"followup":followup_policy}),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: target.city_id,
            target_id: target.target_id,
            target_version: target.version,
            target_name: target.display_name.clone(),
            score: 0,
            phase: BookingOutreachPhase::FollowUp,
        },
        decision_key: format!(
            "decision:booking-followup:v{}:{}:tv{}:{}:{}",
            policy.version,
            target.target_id,
            target.version,
            target.followup_count,
            target
                .last_outreach_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: format!(
            "action:booking-followup:{}:tv{}:{}",
            target.target_id,
            target.version,
            target.followup_count.saturating_add(1)
        ),
    }))
}

pub(super) fn campaign_lifecycle_candidate(
    snapshot: EventCampaignSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::CampaignLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let EventCampaignDecision::Request { phase, confidence } =
        evaluate_event_campaign(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: "request_event_campaign",
        confidence,
        disposition,
        reason: "event lifecycle phase is due for a consented first-party audience",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestAudienceCampaign {
            event_id: snapshot.event_id,
            phase,
            template_key: phase.template_key().to_owned(),
        },
        decision_key: format!(
            "decision:event-campaign:v{}:{}:{:?}:{}:{}:{}",
            policy.version,
            snapshot.event_id,
            phase,
            snapshot.interested_fans,
            snapshot.paid_buyers,
            snapshot.attendees
        ),
        action_idempotency_key: format!("action:event-campaign:{}:{:?}", snapshot.event_id, phase),
    }))
}
