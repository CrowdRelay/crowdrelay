fn policy_evidence<T: Serialize>(
    policy: &AutopilotPolicy,
    domain_config: T,
) -> Result<serde_json::Value, serde_json::Error> {
    Ok(serde_json::json!({
        "version": policy.version,
        "enabled": policy.enabled,
        "autonomy_level": policy.autonomy_level,
        "minimum_confidence_basis_points": policy.minimum_confidence.basis_points(),
        "max_actions_24h": policy.max_actions_24h,
        "config": serde_json::to_value(domain_config)?,
    }))
}

fn ticket_candidate(
    snapshot: TicketYieldSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::TicketYield(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let TicketYieldDecision::Increase {
        from_minor,
        to_minor,
        confidence,
    } = evaluate_ticket_yield(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::TicketType(snapshot.ticket_type_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "increase_ticket_price",
        confidence,
        disposition,
        reason: "paid demand exceeds bounded yield thresholds",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::ChangeTicketPrice {
            ticket_type_id: snapshot.ticket_type_id,
            from_minor,
            to_minor,
        },
        decision_key: format!(
            "decision:ticket:v{}:{}:{}:{}:{}:{}:{}:{from_minor}:{to_minor}",
            policy.version,
            snapshot.ticket_type_id,
            snapshot.current_price_minor,
            snapshot.paid_quantity,
            snapshot.capacity,
            snapshot.paid_last_72h,
            snapshot.days_to_event,
        ),
        action_idempotency_key: format!(
            "action:ticket:{}:{}:{from_minor}:{to_minor}",
            snapshot.ticket_type_id,
            snapshot.last_price_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

fn ticket_allocation_candidate(
    snapshot: TicketYieldSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::TicketYield(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let TicketAllocationDecision::IncreaseCapacity {
        from_capacity,
        to_capacity,
        guardrail_version,
        confidence,
    } = evaluate_ticket_allocation(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::TicketType(snapshot.ticket_type_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "increase_ticket_capacity",
        confidence,
        disposition,
        reason: "paid tier demand is near its operator-bounded allocation ceiling",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::ChangeTicketCapacity {
            ticket_type_id: snapshot.ticket_type_id,
            from_capacity,
            to_capacity,
            guardrail_version,
        },
        decision_key: format!(
            "decision:ticket-capacity:v{}:{}:g{}:{}:{}:{}:{}:{from_capacity}:{to_capacity}",
            policy.version,
            snapshot.ticket_type_id,
            guardrail_version,
            snapshot.paid_quantity,
            snapshot.paid_last_72h,
            snapshot.days_to_event,
            snapshot.sale_capacity,
        ),
        action_idempotency_key: format!(
            "action:ticket-capacity:{}:g{}:{}:{from_capacity}:{to_capacity}",
            snapshot.ticket_type_id,
            guardrail_version,
            snapshot.last_capacity_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            ),
        ),
    }))
}

fn lifecycle_candidate(
    snapshot: FanLifecycleSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::FanLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let (template, confidence) = match evaluate_fan_lifecycle(snapshot, *domain_policy, now) {
        FanLifecycleDecision::RequestMessage {
            template,
            confidence,
        } => (template, confidence),
        // A code before any message that might carry an invite. Costs nothing,
        // reaches nobody, and is the precondition for the one growth loop that
        // compounds without the band doing more work.
        FanLifecycleDecision::IssueReferralCode { confidence } => {
            return Ok(Some(DecisionCandidate {
                context: policy.context,
                subject: ActionSubject::Fan(snapshot.fan_id),
                decision_kind: "issue_referral_code",
                confidence,
                disposition: disposition(
                    policy.autonomy_level,
                    confidence,
                    policy.minimum_confidence,
                ),
                reason: "a consented fan has no referral code, so no invite can be tracked",
                input_snapshot: serde_json::to_value(snapshot)?,
                policy_snapshot: policy_evidence(policy, *domain_policy)?,
                action: AutopilotActionPayload::IssueReferralCode {
                    fan_id: snapshot.fan_id,
                },
                decision_key: format!(
                    "decision:referral-code:v{}:{}",
                    policy.version, snapshot.fan_id
                ),
                // One code per fan, forever. Not windowed like a message: a
                // second code would split a fan's referrals across two
                // identities and make the ledger wrong.
                action_idempotency_key: format!("action:referral-code:{}", snapshot.fan_id),
            }));
        }
        FanLifecycleDecision::Hold(_) => return Ok(None),
    };
    let template_key = match template {
        LifecycleTemplate::Welcome => "crowdrelay.fan.welcome.v1",
        LifecycleTemplate::SynesthesiaFollowUp => "crowdrelay.synesthesia.follow_up.v1",
        LifecycleTemplate::DormantReactivation => "crowdrelay.fan.reactivation.v1",
        LifecycleTemplate::FirstTicketThankYou => "crowdrelay.fan.first_ticket_thanks.v1",
        LifecycleTemplate::ReturningFanThankYou => "crowdrelay.fan.returning_thanks.v1",
        LifecycleTemplate::ReferralThankYou => "crowdrelay.fan.referral_thanks.v1",
        LifecycleTemplate::ReferralInvite => "crowdrelay.fan.referral_invite.v1",
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::Fan(snapshot.fan_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_lifecycle_message",
        confidence,
        disposition,
        reason: "consented fan lifecycle has a deterministic communication step due",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: snapshot.fan_id,
            template_key: template_key.to_owned(),
        },
        decision_key: format!(
            "decision:lifecycle:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.fan_id,
            template_key,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            snapshot
                .last_event_interest_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:lifecycle:{}:{template_key}:{}",
            snapshot.fan_id,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
    }))
}

/// One chase about an editorial pitch nobody has submitted yet.
///
/// A nudge to the band's own operator, so `first_party_reversible`: it reaches
/// nobody outside the workspace and the worst it can do is be ignored. The
/// deadline travels with it, because a reminder that does not say by when is a
/// reminder about nothing.
fn editorial_pitch_escalation(
    snapshot: &ReleasePlanSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &ReleaseAutopilotPolicy,
    due_at: OffsetDateTime,
    confidence: Confidence,
) -> Result<DecisionCandidate, serde_json::Error> {
    Ok(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ReleasePlan(snapshot.release_id),
        decision_kind: "escalate_editorial_pitch",
        confidence,
        disposition: disposition(policy.autonomy_level, confidence, policy.minimum_confidence),
        reason: "the Spotify editorial pitch has no API, so it is somebody's job, and this one is \
                 still not submitted with the deadline in sight",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::EscalateEditorialPitch {
            release_id: snapshot.release_id,
            title: snapshot.title.clone(),
            due_at,
        },
        // Keyed on the last chase, so the next one is a new decision and the
        // one before it is not silently swallowed.
        decision_key: format!(
            "decision:editorial-pitch:v{}:{}:{}",
            policy.version,
            snapshot.release_id,
            snapshot
                .editorial_pitch_escalated_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: format!(
            "action:editorial-pitch:{}:{}",
            snapshot.release_id,
            snapshot
                .editorial_pitch_escalated_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
    })
}

fn release_candidate(
    snapshot: ReleasePlanSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
    collisions: &[ShowWeekCollision],
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Release(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let (milestone, confidence) = match evaluate_release(&snapshot, *domain_policy, now) {
        ReleaseDecision::Request {
            milestone,
            confidence,
        } => (milestone, confidence),
        // Chasing an unfinished pitch is its own candidate: it repeats, and it
        // is keyed on the round rather than on the milestone, so a reminder is
        // not deduplicated against the one before it.
        ReleaseDecision::EscalateEditorialPitch { due_at, confidence } => {
            return editorial_pitch_escalation(&snapshot, policy, domain_policy, due_at, confidence)
                .map(Some);
        }
        ReleaseDecision::Hold(_) => return Ok(None),
    };
    let milestone_key = match milestone {
        ReleaseMilestone::SeedCalendar => "seed_calendar",
        ReleaseMilestone::EditorialPitch => "editorial_pitch",
        ReleaseMilestone::Announcement => "announcement",
        ReleaseMilestone::StartPress => "start_press",
        ReleaseMilestone::FanWarmup => "fan_warmup",
        ReleaseMilestone::Countdown => "countdown",
        ReleaseMilestone::ReleaseDay => "release_day",
        ReleaseMilestone::Sustain => "sustain",
        ReleaseMilestone::Wrap => "wrap",
        ReleaseMilestone::CatalogueRotation => "catalogue_rotation",
    };
    // §4i-2: a week that contains a live show is the show's week. An
    // owned-audience milestone due in it holds rather than spending the same
    // fan attention twice — the send is dropped, never the cap raised. The
    // hold is its own decision row under a week-keyed key, naming the show it
    // protects, so the ledger shows what was held and the ordinary execute
    // key stays untouched: when the week clears, the same milestone offers
    // itself again and goes out. Press and internal milestones reach no fans
    // and are unaffected.
    let action = AutopilotActionPayload::ExecuteReleaseMilestone {
        release_id: snapshot.release_id,
        title: snapshot.title.clone(),
        release_at: snapshot.release_at,
        milestone,
    };
    if matches!(action.action_class(), ActionClass::OwnedAudience)
        && let Some(first_show) = collisions.first()
    {
        // The hold names the collision's own week — the earliest show's local
        // Monday — so the ledger label is the frame the collision was judged
        // in rather than a UTC week that can disagree with it at a boundary.
        let week_start = first_show.week_start;
        return Ok(Some(DecisionCandidate {
            context: policy.context,
            subject: ActionSubject::ReleasePlan(snapshot.release_id),
            decision_kind: "hold_release_milestone_collision",
            confidence,
            disposition: PolicyDisposition::Deny,
            reason: "a live show this week keeps the week's attention; the milestone holds rather than spend \
                     the same fans twice",
            input_snapshot: {
                // Same flat shape as the execute row plus the collision — a
                // reader keying on `release_at` finds it on hold rows too.
                let mut input = serde_json::to_value(&snapshot)?;
                if let Some(fields) = input.as_object_mut() {
                    fields.insert("collision".to_string(), serde_json::json!({
                    "protected_shows": collisions.iter().map(|show| serde_json::json!({
                        "event_id": show.event_id,
                        "title": show.title,
                        "starts_at": crowdrelay_domain::wire_time::Wire(&show.starts_at),
                    })).collect::<Vec<_>>(),
                    "held_milestone": milestone_key,
                }));
                }
                input
            },
            policy_snapshot: policy_evidence(policy, domain_policy)?,
            action,
            decision_key: format!(
                "decision:release:v{}:{}:{}:{}:hold:{}",
                policy.version,
                snapshot.release_id,
                milestone_key,
                snapshot.release_at.unix_timestamp(),
                week_start,
            ),
            action_idempotency_key: format!(
                "action:release:{}:{milestone_key}:hold:{}",
                snapshot.release_id, week_start
            ),
        }));
    }
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // The tier's own outcome ledger answers before the budget does: two or
    // more consecutive releases at this tier that showed no lift turn the
    // next outward rung from an automatic send into a decision a human looks
    // at. Internal rungs keep running — parking the pitch costs no audience
    // attention — and a lesser autonomy than RequireApproval is untouched.
    let tier_missing = snapshot.tier_release_miss_streak >= 2
        && action.action_class() == ActionClass::OwnedAudience;
    let (disposition, reason) = if tier_missing && disposition == PolicyDisposition::AutoExecute {
        (
            PolicyDisposition::RequireApproval,
            "release tier's last R+14 reports showed no lift; a human looks before the next send",
        )
    } else {
        (disposition, "release timeline has a deterministic milestone due")
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ReleasePlan(snapshot.release_id),
        decision_kind: "execute_release_milestone",
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(&snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        decision_key: format!(
            "decision:release:v{}:{}:{}:{}",
            policy.version,
            snapshot.release_id,
            milestone_key,
            snapshot.release_at.unix_timestamp()
        ),
        action_idempotency_key: format!("action:release:{}:{milestone_key}", snapshot.release_id),
    }))
}

/// One move on one live negotiation.
///
/// The score is recomputed here rather than stored on the terms row. A stretch
/// bar checked against the score a show had when the promoter first wrote is a
/// bar checked against a stale fact, and the whole point of that refusal is
/// that it reflects how full the year is *now*.
/// What the autonomy level alone would allow, with the confidence gate set aside.
///
/// Used only for a decision the domain has already routed to a human. Asking
/// `disposition` with the minimum as the confidence is deliberate: it keeps the
/// level the single authority on what dispositions are available, so adding an
/// autonomy level cannot be answered correctly here and wrongly there.
fn disposition_ignoring_confidence(policy: &AutopilotPolicy) -> PolicyDisposition {
    disposition(
        policy.autonomy_level,
        policy.minimum_confidence,
        policy.minimum_confidence,
    )
}

fn live_opportunity_candidate(
    snapshot: LiveOpportunitySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::LiveOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_live_opportunity(snapshot, *domain_policy, now);
    let (score, confidence, forced_approval, decision_kind, reason) = match decision {
        LiveOpportunityDecision::Hold => return Ok(None),
        LiveOpportunityDecision::PrepareForApproval { score, confidence } => (
            score,
            confidence,
            true,
            "apply_live_opportunity",
            "verified live opportunity clears deterministic fit and economics gates",
        ),
        LiveOpportunityDecision::SubmitAutomatically { score, confidence } => (
            score,
            confidence,
            false,
            "apply_live_opportunity",
            "verified live opportunity clears deterministic fit and economics gates",
        ),
        // Never dropped by a budget rule: a full year is a reason to ask, not a
        // reason to throw away the best offer of it. Named separately from the
        // ordinary prepare path so the operator sees *why* this landed in
        // front of them — the calendar, not an ordinary review gate.
        LiveOpportunityDecision::EscalateLandmark { score, confidence } => (
            score,
            confidence,
            true,
            "escalate_landmark_opportunity",
            "a landmark opportunity arrived at or past the annual stretch; the year being full is not a reason to lose it",
        ),
    };
    // The confidence gate decides whether the *machine* may act unattended. It
    // has no say in whether a human gets to look at something the domain gate has
    // already routed to a human, and both directions matter here.
    //
    // Downward, which was already handled: a forced-approval decision must not
    // auto-execute however confident it is.
    //
    // Upward, which was not: `Deny` creates no action row, so the opportunity
    // never enters `awaiting_approval` — the queue `ops/attention` reads and the
    // only place the operator looks. The decision is still written to the ledger,
    // so nothing is destroyed, but finding it means querying for
    // `disposition = 'deny'`, which nobody does.
    //
    // For `EscalateLandmark` that is a straight contradiction with its own
    // reason string, one match arm above: "the year being full is not a reason to
    // lose it". It is escalated precisely so a person sees it, and the confidence
    // gate decided no person would.
    //
    // It is reachable, not theoretical. Confidence here is a linear
    // re-expression of the score — `7_500 + (score - minimum_score) * 100` — so
    // with the default `minimum_score` of 65 and `minimum_confidence` of 8000,
    // every score below 70 denies. A Landmark festival at strategic value 85%,
    // fit 70%, reputation 60%, evidence 70% and a bounded loss scores 67. The
    // configured floor says 65 and the real floor is 70; that gap is worth
    // closing on its own, and it is a policy question rather than this function's.
    // What this function can fix is that the gap silently swallows decisions the
    // domain sent to a human.
    //
    // The autonomy level is still obeyed, and that is why the lift is written the
    // way it is rather than as `Deny => RequireApproval`. `disposition` tests
    // confidence *before* the level, so on an `Observe` workspace a low-confidence
    // decision returns `Deny` too — and rewriting that to `RequireApproval` would
    // put approval requests in front of an operator who asked to observe only.
    // Re-asking with a confidence that clears keeps the level the authority on
    // what the answer may be: Observe still observes, Recommend still recommends.
    //
    // Only ever moves a decision *into* the approval queue, never past it.
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    if forced_approval && matches!(disposition, PolicyDisposition::Deny) {
        disposition = disposition_ignoring_confidence(policy);
    }
    if forced_approval && matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::TeamOpportunity(snapshot.opportunity_id),
        decision_kind,
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::ApplyLiveOpportunity {
            opportunity_id: snapshot.opportunity_id,
            opportunity_kind: snapshot.kind,
            score,
            // Pure evaluation carries no letter — the draft is composed in
            // the persistence transaction, where the opportunity row, the
            // release plan and the sender identity are all readable.
            draft: Default::default(),
        },
        decision_key: format!(
            "decision:live:v{}:{}:{score}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.deadline.map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: format!("action:live:{}:apply", snapshot.opportunity_id),
    }))
}

fn merch_bundle_candidate(
    snapshot: MerchBundleSnapshot,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::MerchBundle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchBundleDecision::Recommend {
        bundle_price_minor,
        affinity_basis_points,
        confidence,
    } = evaluate_merch_bundle(snapshot, *domain_policy)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let (product_a, product_b) = if snapshot.product_a <= snapshot.product_b {
        (snapshot.product_a, snapshot.product_b)
    } else {
        (snapshot.product_b, snapshot.product_a)
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::MerchProduct(product_a),
        decision_kind: "request_merch_bundle",
        confidence,
        disposition,
        reason: "repeat co-purchase evidence supports a bounded margin-safe bundle",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestMerchBundle {
            product_a,
            product_b,
            bundle_price_minor,
            affinity_basis_points,
        },
        decision_key: format!(
            "decision:merch-bundle:v{}:{}:{}:{}:{}:{}",
            policy.version,
            product_a,
            product_b,
            snapshot.joint_orders,
            snapshot.orders_a,
            snapshot.orders_b
        ),
        action_idempotency_key: format!(
            "action:merch-bundle:{}:{}:{}",
            product_a, product_b, bundle_price_minor
        ),
    }))
}

/// One pitch, optionally stamped with the wave it belongs to.
///
/// A wave pitch is the same pitch: same cadence, same relevance bar, same
/// idempotency key. What the wave changes is how it is presented to a human and
/// nothing at all about whether it is allowed. Keying it identically is also
/// what makes the two paths safe to run side by side — the second insert of the
/// same pitch is deduplicated by the database rather than by a rule somebody
/// has to remember.
fn outreach_candidate(
    snapshot: OutreachSnapshot,
    policy: &AutopilotPolicy,
    wave_id: Option<uuid::Uuid>,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Outreach(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let OutreachDecision::Request { phase, confidence } =
        evaluate_outreach(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // A wave is approved as a wave. A pitch inside one that executed on its own
    // would be a batch the operator never saw all of, which is the failure
    // waves exist to prevent.
    if wave_id.is_some() && matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    let template_key = match snapshot.target_kind {
        crowdrelay_domain::outreach::OutreachTargetKind::Playlist => "outreach.playlist.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Radio => "outreach.radio.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Press => "outreach.press.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Creator => "outreach.creator.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::SupportSlot => "outreach.support_slot.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Endorsement => "outreach.endorsement.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::MediaPatronage => {
            "outreach.media_patronage.v1"
        }
        // Representation contacts are approached by the band through the
        // approach path, where consent, allowance and the published listing
        // are the gates. The evaluator proposing one would be an autopilot
        // pitching an agent — exactly the posture §4h-12 forbids.
        crowdrelay_domain::outreach::OutreachTargetKind::Agent
        | crowdrelay_domain::outreach::OutreachTargetKind::Label => return Ok(None),
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::OutreachOpportunity(snapshot.opportunity_id),
        decision_kind: "request_relationship_outreach",
        confidence,
        disposition,
        reason: "verified relationship target matches a fresh high-relevance opportunity",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestOutreach {
            opportunity_id: snapshot.opportunity_id,
            target_id: snapshot.target_id,
            target_version: snapshot.target_version,
            target_name: snapshot.target_id.to_string(),
            phase,
            template_key: template_key.to_owned(),
            wave_id,
            // Composed when the action persists — the evaluator is pure and
            // the sender identity, target name and pitch live in Postgres.
            draft: crowdrelay_domain::outreach_letter::OutreachLetter::default(),
        },
        decision_key: format!(
            "decision:outreach:v{}:{}:{}:tv{}:{:?}:{}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.target_id,
            snapshot.target_version,
            phase,
            snapshot.relevance_basis_points,
            snapshot.observed_at.unix_timestamp()
        ),
        action_idempotency_key: format!(
            "action:outreach:{}:{}:{:?}:{}",
            snapshot.opportunity_id, snapshot.target_id, phase, snapshot.followup_count
        ),
    }))
}

/// One supply snapshot can owe several actions: the artifact the chain is
/// missing, and — for a synced band post — the relays that carry it. The
/// artifact request is unchanged; the relay is the addition (2.11), and it is
/// a *carry*, not a draft: title, caption and link arrive verbatim.
fn content_candidates(
    snapshot: &ContentSupplySnapshot,
    policy: &AutopilotPolicy,
    communities: &[CommunityRelayTarget],
    push_audience: Option<crowdrelay_domain::content_supply::SignalPushAudience>,
    evidence: ContextEvidence,
    now: OffsetDateTime,
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ContentSupply(domain_policy) = &policy.config else {
        return Ok(Vec::new());
    };
    match evaluate_content_supply(snapshot, *domain_policy, now) {
        ContentSupplyDecision::Request {
            artifact,
            confidence,
        } => {
            // Producing an artifact is internal work: the rendered copy
            // lands in the content library and every use of it — a push, a
            // feed post, a newsletter — gates on its own approval action.
            // What used to arrive instead was an "approve this artifact"
            // card describing work the operator could not review before it
            // existed — five of them for one release's signal_push in a
            // single pass, measured in production.
            let disposition = crowdrelay_domain::autonomy::internal_work_disposition(
                disposition(policy.autonomy_level, confidence, policy.minimum_confidence),
            );
            Ok(vec![DecisionCandidate {
                context: policy.context,
                subject: ActionSubject::ContentSource(snapshot.source_id),
                decision_kind: "request_content_artifact",
                confidence,
                disposition,
                reason: "trusted source is missing one required deterministic content artifact",
                input_snapshot: serde_json::to_value(snapshot)?,
                policy_snapshot: policy_evidence(policy, domain_policy)?,
                action: AutopilotActionPayload::RequestContentArtifact {
                    source_id: snapshot.source_id,
                    source_version: snapshot.source_version,
                    artifact,
                    template_key: artifact.template_key().to_owned(),
                },
                decision_key: format!(
                    "decision:content:v{}:{}:sv{}:{:?}",
                    policy.version, snapshot.source_id, snapshot.source_version, artifact
                ),
                action_idempotency_key: format!(
                    "action:content:{}:sv{}:{:?}",
                    snapshot.source_id, snapshot.source_version, artifact
                ),
            }])
        }
        ContentSupplyDecision::Relay { confidence } => Ok(relay_candidates(
            snapshot,
            policy,
            domain_policy,
            communities,
            push_audience,
            confidence,
            evidence,
        )?),
        ContentSupplyDecision::Hold(_) => Ok(Vec::new()),
    }
}

fn experiment_candidate(
    snapshot: &ExperimentSnapshot,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Experimentation(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_experiment(snapshot, *domain_policy);
    let (winner, allocations, complete, confidence) = match decision {
        ExperimentDecision::Reallocate {
            winner,
            allocations,
            confidence,
        } => (winner, allocations, false, confidence),
        ExperimentDecision::Complete { winner, confidence } => {
            let allocations = snapshot
                .variants
                .iter()
                .map(|variant| {
                    (
                        variant.variant_id,
                        if variant.variant_id == winner {
                            10_000
                        } else {
                            0
                        },
                    )
                })
                .collect();
            (winner, allocations, true, confidence)
        }
        ExperimentDecision::Hold(_) => return Ok(None),
    };
    let typed_allocations = allocations
        .into_iter()
        .map(
            |(variant_id, allocation_basis_points)| ExperimentAllocation {
                variant_id,
                allocation_basis_points,
            },
        )
        .collect::<Vec<_>>();
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Experiment(snapshot.experiment_id),
        decision_kind: if complete {
            "complete_experiment"
        } else {
            "reallocate_experiment"
        },
        confidence,
        disposition,
        reason: "aggregate experiment evidence shows a bounded material winner",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::AdjustExperiment {
            experiment_id: snapshot.experiment_id,
            expected_version: snapshot.version,
            winner_variant_id: winner,
            allocations: typed_allocations,
            complete,
        },
        decision_key: format!(
            "decision:experiment:v{}:{}:ev{}:{}:{}",
            policy.version,
            snapshot.experiment_id,
            snapshot.version,
            snapshot
                .variants
                .iter()
                .map(|v| format!(
                    "{}:{}:{}:{}",
                    v.variant_id, v.exposures, v.conversions, v.value_minor
                ))
                .collect::<Vec<_>>()
                .join("|"),
            complete
        ),
        action_idempotency_key: format!(
            "action:experiment:{}:ev{}:{}:{}",
            snapshot.experiment_id,
            snapshot.version,
            winner,
            snapshot.variants.iter().map(|v| v.exposures).sum::<u64>()
        ),
    }))
}

fn show_operations_candidate(
    snapshot: ShowTaskSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ShowOperations(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let show_decision = evaluate_show_task(snapshot, *domain_policy, now);
    let (action, decision_kind, reason, confidence) = match show_decision {
        ShowOperationsDecision::AutoComplete { confidence } => (
            AutopilotActionPayload::CompleteShowTask {
                event_id: snapshot.event_id,
                task: snapshot.task,
            },
            "complete_verified_show_task",
            "first-party evidence proves a non-physical show task is complete",
            confidence,
        ),
        ShowOperationsDecision::EscalateHuman { confidence } => (
            AutopilotActionPayload::EscalateShowTask {
                event_id: snapshot.event_id,
                task: snapshot.task,
            },
            "escalate_show_task",
            "show task is due and requires human or physical confirmation",
            confidence,
        ),
        ShowOperationsDecision::Hold(_) => return Ok(None),
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind,
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        decision_key: format!(
            "decision:show:v{}:{}:{:?}:{}:{}",
            policy.version,
            snapshot.event_id,
            snapshot.task,
            snapshot.verifiable_fact,
            snapshot
                .last_escalated_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: match show_decision {
            ShowOperationsDecision::AutoComplete { .. } => format!(
                "action:show:{}:{:?}:complete",
                snapshot.event_id, snapshot.task
            ),
            _ => format!(
                "action:show:{}:{:?}:escalate:{}",
                snapshot.event_id,
                snapshot.task,
                snapshot
                    .last_escalated_at
                    .map_or(0, OffsetDateTime::unix_timestamp)
            ),
        },
    }))
}

fn promotion_candidate(
    snapshot: PromotionPerformanceSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::PromotionBudget(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let PromotionBudgetDecision::Adjust {
        from_minor,
        to_minor,
        roas_basis_points,
        confidence,
        ..
    } = evaluate_promotion_budget(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::PromotionCampaign(snapshot.campaign_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "adjust_promotion_budget",
        confidence,
        disposition,
        reason: "bounded promotion ROAS is outside configured performance band",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestPromotionBudgetChange {
            campaign_id: snapshot.campaign_id,
            from_minor,
            to_minor,
            roas_basis_points,
        },
        decision_key: format!(
            "decision:promotion:v{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.campaign_id,
            snapshot.current_daily_budget_minor,
            snapshot.spend_last_7d_minor,
            snapshot.attributed_revenue_last_7d_minor,
            snapshot.observed_at.unix_timestamp(),
        ),
        action_idempotency_key: format!(
            "action:promotion:{}:{}:{from_minor}:{to_minor}",
            snapshot.campaign_id,
            snapshot.last_budget_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}
