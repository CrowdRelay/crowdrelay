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
    window_inputs: &BookingWindowInputSet,
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
    // §12-6: the proposed window rides on the selected target's room history
    // plus the shared own-calendar — `None` is a first-class answer, not a
    // fallback date.
    let proposed_window = window_inputs
        .targets
        .iter()
        .find(|input| input.target_id == target_id)
        .and_then(|input| {
            propose_booking_window(
                &BookingWindowInputs {
                    room_shows: input.room_shows.clone(),
                    own_shows: window_inputs.own_shows.clone(),
                    venue_coords: input.venue_coords,
                },
                now,
            )
        });
    // "Write to A, B and C" is one action — the next-ranked eligible targets
    // in the same city come along under the same approval. Two beyond the
    // anchor: enough to make the letter the city's booking desks actually
    // compare notes on, bounded so it can never become a mail-merge.
    let additional_recipients = additional_booking_recipients(
        snapshot.city_id,
        expected_attendance,
        targets,
        target_id,
        &target_policy,
        now,
        2,
    );
    let mut recipient_ids: Vec<String> = additional_recipients
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();
    recipient_ids.sort_unstable();
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
            "proposed_window": proposed_window,
            "additional_recipients": additional_recipients,
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
            proposed_window: proposed_window.clone(),
            additional_recipients,
            venue_evidence: target_snapshot.venue_evidence.clone(),
            // Composed when the action persists — the evaluator is pure and
            // the sender identity lives in Postgres.
            draft: crowdrelay_domain::booking_letter::BookingLetter::default(),
        },
        decision_key: format!(
            "decision:booking:v{}:{}:{}:tv{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
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
            // The proposal's dates and recipient set are decision inputs: a
            // changed window or a changed list is a different proposal and
            // must re-decide rather than ride on the old answer.
            proposed_window.as_ref().map_or(0, |window| {
                window.start.midnight().assume_utc().unix_timestamp()
            }),
            proposed_window.as_ref().map_or(0, |window| {
                window.end.midnight().assume_utc().unix_timestamp()
            }),
            recipient_ids.join(","),
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
            // A follow-up re-asks the question the initial send already
            // posed — it neither re-derives a window nor copies new
            // recipients onto a thread they were never part of.
            proposed_window: None,
            additional_recipients: Vec::new(),
            venue_evidence: target.venue_evidence.clone(),
            draft: crowdrelay_domain::booking_letter::BookingLetter::default(),
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

/// The deadline-driven ask on a festival target: the next edition's
/// application window closes inside `ask_within_days` and nobody has asked
/// for it. This is deliberately *not* the city-demand path — a festival slot
/// is the demand, and the close date, not a score, is what makes today the
/// day to write.
///
/// Both keys carry the close timestamp rather than the day countdown: the
/// countdown changes every morning and would re-decide the same edition
/// daily, while the timestamp changes only when a different edition becomes
/// next — which is exactly when a new decision is honest.
pub(super) fn festival_window_candidate(
    target: &BookingTargetSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let window_policy = FestivalWindowPolicy::default();
    let FestivalWindowDecision::Request { confidence } =
        evaluate_festival_window(target, window_policy, now)
    else {
        return Ok(None);
    };
    let Some(closes_at) = target.next_application_closes_at else {
        // The evaluator passed on the countdown, so a missing timestamp is a
        // read inconsistency, not a proposal — say nothing rather than guess
        // which edition this is.
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::City(target.city_id),
        decision_kind: "request_festival_window",
        confidence,
        disposition,
        reason: "the next edition's application window is closing and no ask is in flight",
        input_snapshot: serde_json::to_value(target)?,
        policy_snapshot: policy_evidence(
            policy,
            serde_json::json!({"opportunity":domain_policy,"festival_window":window_policy}),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: target.city_id,
            target_id: target.target_id,
            target_version: target.version,
            target_name: target.display_name.clone(),
            score: 0,
            phase: BookingOutreachPhase::Initial,
            // The edition's own dates are the ask's context; the letter
            // composes without a derived window line, and extra recipients
            // would mail-merge a deadline that belongs to one festival.
            proposed_window: None,
            additional_recipients: Vec::new(),
            venue_evidence: target.venue_evidence.clone(),
            draft: crowdrelay_domain::booking_letter::BookingLetter::default(),
        },
        decision_key: format!(
            "decision:festival-window:v{}:{}:tv{}:{}",
            policy.version,
            target.target_id,
            target.version,
            closes_at.unix_timestamp()
        ),
        action_idempotency_key: format!(
            "action:festival-window:{}:{}",
            target.target_id,
            closes_at.unix_timestamp()
        ),
    }))
}

/// The cold-start lane: where the city demand gate cannot score yet (a
/// 23-fan roster never reaches `minimum_score = 65`), a venue-linked
/// target's own room history proposes an initial letter instead. The room
/// demonstrably booking comparable acts is the demand signal the fan
/// density cannot yet supply.
///
/// Same `RequestBookingOutreach` action kind as the demand path — one
/// send lane, one hold window, one draft composer. The distinct
/// `decision_kind`/key prefix keeps the lanes separable in the trace and
/// lets a lapsed ask re-raise under the same bounded-ask contract.
pub(super) fn venue_fit_candidate(
    target: &BookingTargetSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let fit_policy = VenueFitPolicy::default();
    let VenueFitDecision::Request {
        confidence,
        evidence_score,
    } = evaluate_venue_fit(target, fit_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::City(target.city_id),
        decision_kind: "request_booking_venue_fit",
        confidence,
        disposition,
        reason: "the venue's own booking history shows it programs comparable acts",
        input_snapshot: serde_json::to_value(target)?,
        policy_snapshot: policy_evidence(
            policy,
            serde_json::json!({"opportunity":domain_policy,"venue_fit":fit_policy}),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: target.city_id,
            target_id: target.target_id,
            target_version: target.version,
            target_name: target.display_name.clone(),
            score: evidence_score,
            phase: BookingOutreachPhase::Initial,
            // No derived window — the room's marks are the pitch context,
            // and a proposed date read from a cold-start evidence row would
            // fabricate precision the lane does not have.
            proposed_window: None,
            additional_recipients: Vec::new(),
            venue_evidence: target.venue_evidence.clone(),
            draft: crowdrelay_domain::booking_letter::BookingLetter::default(),
        },
        decision_key: format!(
            "decision:venue-fit:v{}:{}:tv{}:{}:{}:{}",
            policy.version,
            target.target_id,
            target.version,
            target
                .last_outreach_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            target
                .venue_evidence
                .as_ref()
                .map_or(0, |evidence| evidence.shows_last_12m),
            target
                .venue_evidence
                .as_ref()
                .map_or(0, |evidence| evidence.comparable_acts),
        ),
        action_idempotency_key: format!(
            "action:booking-venuefit:{}:tv{}:{}",
            target.target_id,
            target.version,
            target.last_outreach_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

pub(super) fn campaign_lifecycle_candidate(
    snapshot: &EventCampaignSnapshot,
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
            // O.5: the size and the basis travel with the ask, so the approval
            // says who this reaches rather than naming a template key.
            audience_size: phase.audience_size(snapshot),
            audience_basis: phase.audience_basis().to_owned(),
            // O.3: the words travel too — the operator approves the sentences
            // the fans receive, and the mailer sends them verbatim.
            draft: phase.compose(snapshot),
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
