/// Result of the shared decision + action persistence primitive.
/// Both `persist_candidate_impl` and `persist_treatment_with_assignment_impl`
/// call `persist_decision_and_action_tx` and match on this outcome.
/// How many cards the *same* proposal may mint while every one dies
/// unanswered: the initial ask plus two re-raises, three chances over
/// roughly a week. Persistence — and a fourth card for the same ask is
/// nagging. The lapse sweep re-keys each expired ask (`:lapsed:{id}`), so
/// this count is the family's dead rows; the decision still records the
/// proposal, only the action stops being minted.
const MAX_APPROVAL_ASKS: i64 = 3;

enum DecisionActionOutcome {
    /// Quota check throttled — no decision or action created.
    Throttled,
    /// Decision row present, but disposition doesn't produce an action.
    /// `decision_created` is false when the INSERT deduped against a row a
    /// prior cycle already wrote — the report must not recount it.
    NoAction { decision_created: bool },
    /// Action INSERT conflicted on an uncovered unique index (e.g. the
    /// inflight-subject partial index) and the existing row could not be
    /// located. The decision was created but no action is safe to reference.
    /// Callers skip downstream writes and continue the cycle — this is a
    /// single-candidate skip, not an abort.
    ActionConflict,
    /// Decision + action created (or action already existed). The caller
    /// may now add treatment-specific writes (assignment, prediction,
    /// evidence) in the same transaction.
    ActionReady {
        decision_created: bool,
        action_id: Uuid,
        inserted: bool,
    },
}

/// Shared transactional persistence primitive: quota check + decision
/// INSERT + action INSERT + outbox event. Both the non-experiment and
/// treatment paths call this, then add their specific writes.
///
/// One implementation of the transaction semantics — no divergence
/// between the two paths.
async fn persist_decision_and_action_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    candidate: &DecisionCandidate,
    trace: &TraceContext,
) -> Result<DecisionActionOutcome, RepositoryError> {
    // ── Executor check ──
    // Work no live executor can perform is recorded as a recommendation, not
    // asked for and not queued — asking cost a person attention and queuing
    // cost the week's outward budget, and both bought an action the
    // dispatcher parks and the stale sweep cancels. On 2026-09-25 seven
    // approved `beacon.outreach` actions sat parked (no executor has ever
    // advertised it) and held seven of the ten weekly third-party touches,
    // so no outreach wave could open for a show three weeks out.
    //
    // The decision row still records the finding: a later cycle re-evaluates
    // the same key, so the action is minted the first cycle after an
    // executor advertises the capability. Fail-open on an empty registry —
    // nothing registered is not "everything is blocked".
    let withheld;
    let candidate = match executor_capability_for_payload(&candidate.action) {
        Some(capability)
            if matches!(
                candidate.disposition,
                PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute
            ) && executor_registry_is_active(transaction, workspace_id).await?
                && !executor_capability_available(transaction, workspace_id, capability)
                    .await? =>
        {
            // Debug, not warn: this is the steady state for a capability no
            // executor has ever advertised and it fires every cycle — 308 of
            // the worker's 388 warnings in three hours on 2026-09-27, all
            // `beacon.outreach`/`beacon.discovery`. The decision row keeps
            // the finding (`held_by: no_executor:<capability>`); that row,
            // not a repeated log line, is where the gap is read.
            tracing::debug!(
                action_kind = candidate.action.action_kind(),
                capability,
                decision_key = %candidate.decision_key,
                "recorded as a recommendation: no live executor advertises this \
                 capability, so asking or queuing would spend budget on work \
                 nothing can perform"
            );
            let mut policy_snapshot = candidate.policy_snapshot.clone();
            if let Some(object) = policy_snapshot.as_object_mut() {
                let mut held_by = object
                    .get("held_by")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                held_by.push(serde_json::json!(format!("no_executor:{capability}")));
                object.insert("held_by".to_owned(), serde_json::Value::Array(held_by));
            }
            withheld = DecisionCandidate {
                disposition: PolicyDisposition::RecommendOnly,
                policy_snapshot,
                ..candidate.clone()
            };
            &withheld
        }
        _ => candidate,
    };
    // ── Quota check ──
    if matches!(
        candidate.disposition,
        PolicyDisposition::RequireApproval | PolicyDisposition::AutoExecute
    ) {
        let max_actions_24h = sqlx::query_scalar::<_, i32>(
            r#"
            SELECT max_actions_24h
            FROM autopilot_policies
            WHERE workspace_id = $1 AND context = $2 AND enabled
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(candidate.context.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;
        // Counted per action class, not across the whole context. One
        // context can hold very different work: `content_supply` renders
        // artifacts (first-party, reversible) and relays the band's own posts
        // to fans who opted in (owned audience). Counted together, a render
        // backlog spent the whole day's quota — on 2026-09-26 thirty renders
        // and agent runs against a quota of thirty — and every relay of a
        // post the band had just published waited behind it. Outward classes
        // are bounded again by the growth envelope and the bootstrap
        // allowance, so this lets neither kind of work starve the other
        // without widening what reaches anybody.
        //
        // A row with no recorded class (written before classes existed, or
        // by a path that does not stamp one) counts against every class:
        // an unknown row must never read as headroom.
        let actions_24h = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM autopilot_actions
            WHERE workspace_id = $1
              AND context = $2
              AND COALESCE(action_class, $3) = $3
              AND created_at >= now() - INTERVAL '24 hours'
              AND status <> 'cancelled'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(candidate.context.as_str())
        .bind(candidate.action.action_class().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if actions_24h >= i64::from(max_actions_24h) {
            return Ok(DecisionActionOutcome::Throttled);
        }
    }
    // ── Decision INSERT ──
    let decision_id = Uuid::now_v7();
    // Letters are composed here, inside the same transaction that writes
    // the action, so the payload carries the words the approver will read.
    // A refusal leaves the draft empty — dispatch fails closed on it, and
    // the briefing says "not composed" instead of showing invented text.
    let mut action = candidate.action.clone();
    match &mut action {
        AutopilotActionPayload::RequestBookingOutreach { .. } => {
            enrich_booking_draft(transaction, workspace_id, &mut action).await?;
        }
        AutopilotActionPayload::RequestOutreach { .. } => {
            enrich_outreach_draft(transaction, workspace_id, &mut action).await?;
        }
        AutopilotActionPayload::RequestOutreachReply { .. } => {
            enrich_reply_draft(transaction, workspace_id, &mut action).await?;
        }
        AutopilotActionPayload::ApplyLiveOpportunity { .. } => {
            enrich_application_draft(transaction, workspace_id, &mut action).await?;
        }
        _ => {}
    }
    let action_json =
        serde_json::to_value(&action).map_err(|_| RepositoryError::Unexpected)?;
    let inserted_decision = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
        ON CONFLICT (workspace_id, decision_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(&candidate.decision_key)
    .bind(candidate.context.as_str())
    .bind(candidate.subject.kind())
    .bind(candidate.subject.uuid())
    .bind(candidate.decision_kind)
    .bind(i32::from(candidate.confidence.basis_points()))
    .bind(disposition_str(candidate.disposition))
    .bind(candidate.reason)
    .bind(&candidate.input_snapshot)
    .bind(&candidate.policy_snapshot)
    .bind(&action_json)
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let decision_id = match inserted_decision {
        Some(id) => id,
        None => {
            // Decision already exists from a prior cycle. This happens when
            // the decision was created but the action was withheld (e.g.,
            // treatment assignment was not selected by the portfolio, or
            // the experiment design conflicted). In that case, the action
            // may not exist yet — we should proceed to create it.
            //
            // Look up the existing decision_id so the action INSERT can
            // reference it via FK.
            sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM autopilot_decisions
                WHERE workspace_id = $1 AND decision_key = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&candidate.decision_key)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?
        }
    };
    // ── Action INSERT ──
    // P.4: a live show-ladder approval is the human gate, already answered.
    // The decision row keeps the disposition the policy and class ceiling
    // computed — `require_approval` there means "this would have asked" — but
    // the action is written already approved, provenance `operator:show_ladder`
    // so a revoke can cancel exactly the rungs the ladder released.
    // Scoped to the show-growth context on purpose: the flag is a contract
    // between `evaluate/show_growth` and this write, and the revoke path
    // reaches only `context='show_growth'` rows — anywhere else the flag
    // would queue a rung no revoke could cancel.
    let relationship_sensitive_show_growth = matches!(
        &candidate.action,
        AutopilotActionPayload::RequestShowGrowth { lever, .. }
            if lever.is_relationship_sensitive()
    );
    let mut ladder_authorized = candidate.context == AutopilotContext::ShowGrowth
        && !relationship_sensitive_show_growth
        && candidate.policy_snapshot.get("ladder_authorized") == Some(&json!(true));
    if ladder_authorized {
        // The flag was read when the snapshot loaded; a revoke may have
        // landed since. Re-ask inside this transaction — a rung queued under
        // a dead ladder would be uncancellable (revoke requires a live row),
        // so the honest fallback is the parked state the disposition asked
        // for.
        ladder_authorized = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM show_ladder_approvals \
             WHERE workspace_id=$1 AND event_id=$2 AND revoked_at IS NULL)",
        )
        .bind(workspace_id.into_uuid())
        .bind(candidate.subject.uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;

    }
    // The standing-grant half of the same gate: an operator who already
    // answered "this target is fine, stop asking" is not asked again. Read
    // inside this transaction for the same reason the ladder re-asks — a
    // revoke that landed since evaluation must win. The grant row carries
    // its own class; a grant written when the kind was reversible cannot
    // license it after reclassification, so the class must match what the
    // action carries *now*.
    let mut standing_authorized = false;
    if !ladder_authorized
        && candidate.disposition == PolicyDisposition::RequireApproval
        && let Some(target_key) = candidate.action.standing_approval_target()
    {
        standing_authorized = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM standing_approvals \
             WHERE workspace_id=$1 AND action_kind=$2 AND target_key=$3 \
               AND action_class=$4 AND revoked_at IS NULL AND expires_at > now())",
        )
        .bind(workspace_id.into_uuid())
        .bind(candidate.action.action_kind())
        .bind(&target_key)
        .bind(candidate.action.action_class().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    // Re-raise bound: the sweep re-keys a lapsed ask so the same proposal
    // can come back — but only a bounded number of times. Count the dead
    // asks of this exact key family (`base` plus every `:lapsed:{id}`
    // re-key the sweep wrote) before agreeing to mint another card.
    let mut re_raise_exhausted = false;
    if candidate.disposition == PolicyDisposition::RequireApproval
        && !ladder_authorized
        && !standing_authorized
    {
        let lapsed = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM autopilot_actions
            WHERE workspace_id = $1
              AND last_error_kind = 'approval_expired'
              AND (idempotency_key = $2
                   OR starts_with(idempotency_key, $2 || ':lapsed:'))
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&candidate.action_idempotency_key)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        re_raise_exhausted = lapsed >= MAX_APPROVAL_ASKS;
    }
    let status = match candidate.disposition {
        _ if re_raise_exhausted => None,
        PolicyDisposition::RequireApproval if ladder_authorized || standing_authorized => {
            Some("queued")
        }
        PolicyDisposition::RequireApproval => Some("awaiting_approval"),
        PolicyDisposition::AutoExecute => Some("queued"),
        PolicyDisposition::ObserveOnly
        | PolicyDisposition::RecommendOnly
        | PolicyDisposition::Deny => None,
    };
    let Some(status) = status else {
        return Ok(DecisionActionOutcome::NoAction {
            decision_created: inserted_decision.is_some(),
        });
    };
    let action_id = Uuid::now_v7();
    let action_trace = TraceContext::for_action(
        workspace_id,
        trace.trace_id(),
        action_id,
        Some(decision_id),
    );
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind,
            subject_kind, subject_id, idempotency_key, payload, status,
            action_class,
            approved_at, approved_by, approval_expires_at,
            trace_id, causation_id, available_at
        )
        VALUES (
            $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
            CASE WHEN $10 = 'queued' THEN now() ELSE NULL END,
            CASE WHEN $10 = 'queued' THEN $14 ELSE NULL END,
            -- A pitch inside a wave is approved with the wave, so its clock
            -- is the wave's close — the 72h lapse window would kill it long
            -- before a monthly season ever reached approval.
            CASE WHEN $10 = 'awaiting_approval'
                 THEN COALESCE(
                     (SELECT wave.anchor_at
                      FROM outreach_waves AS wave
                      WHERE wave.workspace_id = $2
                        AND wave.id = ($9->>'wave_id')::uuid),
                     now() + INTERVAL '72 hours')
                 ELSE NULL END,
            $12, $13,
            -- The ladder is the approval, not a shortcut past the hold that
            -- makes revoking meaningful: a ladder-queued outward rung waits
            -- its class's window exactly like an individually approved one.
            CASE WHEN $10 = 'queued'
                 THEN now() + make_interval(secs => $15::double precision)
                 ELSE now() END
        )
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(candidate.context.as_str())
    .bind(candidate.action.action_kind())
    .bind(candidate.subject.kind())
    .bind(candidate.subject.uuid())
    .bind(&candidate.action_idempotency_key)
    .bind(action_json)
    .bind(status)
    // Recorded now rather than derived at read time: this is the class
    // the action was authorised under, which is what an audit needs.
    .bind(candidate.action.action_class().as_str())
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|c| c.into_uuid()))
    // Provenance distinguishes who answered the human gate: the ladder row the
    // operator signed, a standing grant on this target, or the workspace's
    // own bounded-auto policy.
    .bind(
        if candidate.disposition != PolicyDisposition::RequireApproval {
            "policy:bounded_auto"
        } else if ladder_authorized {
            "operator:show_ladder"
        } else if standing_authorized {
            "operator:standing_grant"
        } else {
            "policy:bounded_auto"
        },
    )
    // The hold comes from the domain, per class — a first-party rung queues
    // immediately either way, and a plain bounded-auto row keeps the same
    // available_at it always had. A grant-queued third-party send holds the
    // class window exactly like a ladder-queued or hand-approved one: the
    // grant is an answer to "may this go out", not a way to send it faster,
    // and the hold is the window a revoke can still act inside.
    .bind(
        if (ladder_authorized || standing_authorized)
            && candidate.disposition == PolicyDisposition::RequireApproval
        {
            candidate.action.action_class().hold_seconds() as f64
        } else {
            0.0
        },
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    // When ON CONFLICT DO NOTHING fires, the generated action_id was NOT
    // inserted. Using it for downstream writes (prediction, evidence,
    // assignment) causes a FK violation because that UUID does not exist in
    // autopilot_actions. Fetch the real id of the existing action so
    // downstream writes reference the correct row.
    //
    // ON CONFLICT DO NOTHING (without a conflict target) catches ALL unique
    // constraint violations on the table. There are two that can fire here:
    //   1. UNIQUE (workspace_id, idempotency_key) — same action re-evaluated
    //   2. autopilot_actions_inflight_subject_uidx — a partial unique
    //      index on (workspace_id, context, action_kind, subject_id) WHERE
    //      status IN ('awaiting_approval', 'queued', 'processing'). This fires
    //      when a different action for the same subject is already inflight.
    //
    // We search by idempotency_key first, then by the inflight-subject columns.
    // If neither finds the existing row, the conflict is unrecoverable and we
    // must NOT fall back to the non-inserted UUID — that causes an FK violation
    // on the prediction INSERT.
    let (real_action_id, action_inserted) = match inserted {
        Some(id) => (id, true),
        None => {
            // Try 1: idempotency_key conflict
            let existing_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id FROM autopilot_actions
                WHERE workspace_id = $1 AND idempotency_key = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&candidate.action_idempotency_key)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?;

            // Try 2: inflight-subject partial unique index conflict
            let existing_id = match existing_id {
                Some(id) => Some(id),
                None => {
                    sqlx::query_scalar::<_, Uuid>(
                        r#"
                        SELECT id FROM autopilot_actions
                        WHERE workspace_id = $1
                          AND context = $2
                          AND action_kind = $3
                          AND subject_id = $4
                          AND status IN ('awaiting_approval', 'queued', 'processing')
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(candidate.context.as_str())
                    .bind(candidate.action.action_kind())
                    .bind(candidate.subject.uuid())
                    .fetch_optional(&mut **transaction)
                    .await
                    .map_err(map_sqlx)?
                }
            };

            match existing_id {
                Some(id) => (id, false),
                // Neither constraint found the existing row — the conflict
                // is unrecoverable. Return ActionConflict so the caller
                // skips this candidate without aborting the entire cycle.
                None => return Ok(DecisionActionOutcome::ActionConflict),
            }
        }
    };
    // ── Outbox event (approval requested) ──
    if action_inserted && status == "awaiting_approval" {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts, trace_id, causation_id, action_id)
            VALUES (
                $1, 'crowdrelay.autopilot.approval_requested', 1,
                jsonb_build_object(
                    'action_id', $2::uuid,
                    'context', $3::text,
                    'action_kind', $4::text,
                    'subject_kind', $5::text,
                    'subject_id', $6::uuid,
                    'reason', $7::text,
                    'confidence_basis_points', $8::integer,
                    'approval_expires_at', now() + INTERVAL '72 hours',
                    'trace_id', $9::uuid
                ),
                12,
                $9,
                $10,
                $2
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(candidate.context.as_str())
        .bind(candidate.action.action_kind())
        .bind(candidate.subject.kind())
        .bind(candidate.subject.uuid())
        .bind(candidate.reason)
        .bind(i32::from(candidate.confidence.basis_points()))
        .bind(trace.trace_id().into_uuid())
        .bind(action_trace.causation_id().map(|c| c.into_uuid()))
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(DecisionActionOutcome::ActionReady {
        // The decision INSERT dedupes on a prior cycle's row — `inserted`
        // reporting must say so or the ledger overcounts new decisions.
        decision_created: inserted_decision.is_some(),
        action_id: real_action_id,
        inserted: action_inserted,
    })
}

/// Builds and records the dispatch-time growth evidence + prediction
/// in the same transaction as the action. This is the initial immutable
/// evidence envelope — outcome fields are NULL and filled in when
/// measurements arrive.
///
/// Idempotent: `ON CONFLICT (action_id) DO NOTHING` on the prediction
/// INSERT, and `ON CONFLICT` on the evidence INSERT.
///
/// # Prediction consistency invariant
///
/// The evidence is built from the SAME `prediction` that was used for
/// the decision. This guarantees:
/// `prediction_at_decision == prediction_persisted_in_initial_evidence`.
/// The invariant is enforced structurally — there is no code path that
/// records a prediction without also recording the matching evidence
/// in the same transaction.
///
/// Returns the `GrowthEvidence` so the caller can record the best-effort
/// audit trail (event log + episode upsert) after the transaction commits.
async fn record_prediction_and_evidence_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    prediction: &crowdrelay_brain::DispatchPrediction,
    strategy: Option<&str>,
    holdout_probability: f64,
    is_interference_controllable: bool,
) -> Result<crowdrelay_brain::GrowthEvidence, RepositoryError> {
    // ── Dispatch prediction ──
    let pred_context_json = serde_json::to_value(&prediction.context)
        .unwrap_or(serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO dispatch_predictions
            (workspace_id, action_id, template_id,
             expected_new_fans, expected_signal_installs, context,
             expected_metrics)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(&prediction.template_id)
    .bind(prediction.expected_new_fans)
    .bind(prediction.expected_signal_installs)
    .bind(&pred_context_json)
    .bind(serde_json::to_value(&prediction.expected_metrics)
        .unwrap_or_else(|_| serde_json::json!({})))
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    // ── Growth evidence ──
    // Build the evidence from the SAME prediction that was used for
    // the decision. This is the prediction consistency invariant:
    // prediction_at_decision == prediction_persisted_in_initial_evidence.
    let target = prediction
        .context
        .subreddit_type
        .clone()
        .unwrap_or_else(|| format!("action:{}", action_id));
    let opportunity_id = crowdrelay_brain::OpportunityId::new(
        &prediction.template_id,
        &target,
        crowdrelay_brain::OpportunityAction::Post,
        &prediction.context,
    );
    let recipient_id = target.clone();
    // The channel is a fact about the template's delivery surface.
    // `subreddit_type` names the community's genre (metal, indie), not a
    // handle — reading it as a channel mislabeled every community post.
    let channel = crowdrelay_brain::channel_for_template(&prediction.template_id);
    // A holdout running through interference the design cannot control is a
    // matched quasi-experiment, not a randomized one — the same call
    // `ExperimentDesign::evidence_quality` and `measured_evidence_quality`
    // make, so the dispatch-time stamp agrees with what measurement later
    // writes rather than overstating it until then.
    let (treatment_propensity, evidence_quality) = if holdout_probability > 0.0 {
        (
            1.0 - holdout_probability,
            if is_interference_controllable {
                crowdrelay_brain::EvidenceQuality::RandomizedHoldout
            } else {
                crowdrelay_brain::EvidenceQuality::MatchedQuasiExperiment
            },
        )
    } else {
        (1.0, crowdrelay_brain::EvidenceQuality::Observational)
    };
    let evidence = crowdrelay_brain::GrowthEvidence::at_dispatch(
        workspace_id.into_uuid(),
        Some(action_id),
        Some(opportunity_id.to_string()),
        recipient_id,
        channel,
        1,
        crowdrelay_brain::TreatmentAssignment::Treatment,
        treatment_propensity,
        prediction.expected_new_fans,
        prediction.expected_signal_installs,
        prediction.context.clone(),
        prediction.target_key.clone(),
        prediction.creative_family,
        strategy.map(|s| s.to_owned()),
        evidence_quality,
    );
    super::operations::evidence::record_growth_evidence_in_tx(
        transaction,
        workspace_id,
        &evidence,
    )
    .await?;
    Ok(evidence)
}


macro_rules! decision_persist {
    () => {
    async fn persist_candidate_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let result = match outcome {
                DecisionActionOutcome::Throttled => CandidatePersistence {
                    decision_created: false,
                    action_created: false,
                    quota_throttled: true,
                    action_id: None,
                },
                DecisionActionOutcome::ActionConflict => CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::NoAction { decision_created } => CandidatePersistence {
                    decision_created,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => CandidatePersistence {
                    decision_created,
                    action_created: inserted,
                    quota_throttled: false,
                    action_id: Some(action_id),
                },
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(result)
        })
        .await
    }

    /// Atomically persists a treatment action AND its experiment assignment
    /// in a single transaction.
    ///
    /// P0-2: ACTION EXISTS, ASSIGNMENT EXISTS, EXECUTION INTENT EXISTS,
    /// PREDICTION EXISTS, INITIAL EVIDENCE EXISTS.
    /// The decision, action, idempotency, outbox, experiment assignment,
    /// dispatch prediction, and initial growth evidence commit as one
    /// state transition. If any INSERT fails, the entire transaction
    /// rolls back.
    ///
    /// The assignment is constructed by the caller with `action_id: None`.
    /// This method fills in the real `action_id` from the inserted action
    /// before recording the assignment, so the linkage is durable.
    #[allow(clippy::too_many_arguments)]
    async fn persist_treatment_with_assignment_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        assignment: &crowdrelay_brain::ExperimentAssignment,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        _holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // ── Shared primitive: quota + decision + action + outbox ──
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let (decision_created, real_action_id, inserted) = match outcome {
                DecisionActionOutcome::Throttled => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: false,
                        action_created: false,
                        quota_throttled: true,
                        action_id: None,
                    });
                }
                DecisionActionOutcome::ActionConflict => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created: true,
                        action_created: false,
                        quota_throttled: false,
                        action_id: None,
                    });
                }
                DecisionActionOutcome::NoAction { decision_created } => {
                    transaction.commit().await.map_err(map_sqlx)?;
                    return Ok(CandidatePersistence {
                        decision_created,
                        action_created: false,
                        quota_throttled: false,
                        action_id: None,
                    });
                }
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => (decision_created, action_id, inserted),
            };
            // When the action already existed (inserted=false), the
            // assignment, prediction, and evidence were recorded in a
            // prior cycle. Skip them: the assignment INSERT would hit
            // idx_experiment_assignments_action_id_unique (a partial
            // unique index on (workspace_id, action_id) WHERE action_id
            // IS NOT NULL) because the existing action already has an
            // assignment. That constraint is NOT the one named in the
            // ON CONFLICT clause below, so the conflict is not caught
            // and surfaces as a silent RepositoryError::Conflict.
            if !inserted {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CandidatePersistence {
                    decision_created,
                    action_created: false,
                    quota_throttled: false,
                    action_id: Some(real_action_id),
                });
            }
            // ── Experiment assignment INSERT (atomic with action) ──
            // P0-2: The assignment is recorded with the real action_id,
            // inside the same transaction. If this fails, the action is
            // rolled back too — no action without assignment.
            let context_json = serde_json::to_value(&assignment.context)
                .unwrap_or(serde_json::json!({}));
            let prediction_json = serde_json::to_value(prediction)
                .unwrap_or(serde_json::json!({}));
            let kind = assignment.kind();
            // The assignment was constructed with action_id=None (withheld),
            // but this path creates a real action — so execution_status
            // must be Dispatched (durable intent committed), not Withheld.
            // It transitions to Executed only when the external intervention
            // is confirmed by the executor.
            let execution_status = if assignment.arm
                == crowdrelay_brain::TreatmentAssignment::Treatment
            {
                crowdrelay_brain::ExecutionStatus::Dispatched
            } else {
                assignment.execution_status
            };
            sqlx::query(
                r#"
                INSERT INTO experiment_assignments
                    (id, workspace_id, unit_id, unit_kind, arm, assigned_at,
                     propensity, intended_holdout_probability, intended_template_id,
                     context, prediction, action_id, strategy, experiment_kind,
                     contamination_estimate, is_interference_controllable,
                     experiment_uuid, assignment_round,
                     eligibility_criteria, selection_context,
                     interference_policy, assignment_time_contamination,
                     experiment_status, execution_status)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                        $17, $18, $19, $20, $21, $22, $23, $24)
                ON CONFLICT (workspace_id, experiment_uuid, assignment_round, unit_id)
                DO NOTHING
                "#,
            )
            .bind(&assignment.assignment_id)
            .bind(workspace_id.into_uuid())
            .bind(&assignment.unit_id)
            .bind(assignment.unit_kind.as_str())
            .bind(assignment.arm.as_str())
            .bind(assignment.assigned_at)
            .bind(assignment.propensity)
            .bind(assignment.intended_holdout_probability)
            .bind(&assignment.intended_template_id)
            .bind(&context_json)
            .bind(&prediction_json)
            .bind(Some(real_action_id))
            .bind(strategy)
            .bind(kind.as_str())
            .bind(assignment.interference_score)
            .bind(assignment.is_interference_controllable)
            .bind(assignment.experiment_uuid)
            .bind(assignment.assignment_round as i32)
            .bind(&assignment.eligibility_criteria)
            .bind(&assignment.selection_context)
            .bind(assignment.interference_policy.as_str())
            .bind(assignment.interference_score)
            .bind(assignment.experiment_status.as_str())
            .bind(execution_status.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // ── Dispatch prediction + initial growth evidence (same tx) ──
            // The prediction and initial evidence are recorded atomically
            // with the action. The evidence row is the source of truth for
            // the learning loop — outcome fields are NULL and filled in
            // when measurements arrive. Event/episode materialization is
            // best-effort post-commit (audit trail, not source of truth).
            let evidence = record_prediction_and_evidence_tx(
                &mut transaction,
                workspace_id,
                real_action_id,
                prediction,
                strategy,
                _holdout_probability,
                assignment.is_interference_controllable,
            )
            .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            // Best-effort audit trail (event log + episode upsert).
            // These are NOT source of truth — the evidence row is.
            super::operations::evidence::record_evidence_audit_trail(
                self,
                workspace_id,
                &evidence,
            )
            .await;
            Ok(CandidatePersistence {
                decision_created,
                action_created: inserted,
                quota_throttled: false,
                action_id: Some(real_action_id),
            })
        })
        .await
    }

    /// Persists a candidate with dispatch prediction and initial growth
    /// evidence in the same transaction. Used by the non-experiment path
    /// (scanner, strategist) where there is no experiment assignment but
    /// the prediction and evidence still need to be atomic with the action.
    async fn persist_candidate_with_evidence_impl(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // ── Shared primitive: quota + decision + action + outbox ──
            let outcome =
                persist_decision_and_action_tx(&mut transaction, workspace_id, candidate, trace)
                    .await?;
            let result = match outcome {
                DecisionActionOutcome::Throttled => CandidatePersistence {
                    decision_created: false,
                    action_created: false,
                    quota_throttled: true,
                    action_id: None,
                },
                DecisionActionOutcome::ActionConflict => CandidatePersistence {
                    decision_created: true,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::NoAction { decision_created } => CandidatePersistence {
                    decision_created,
                    action_created: false,
                    quota_throttled: false,
                    action_id: None,
                },
                DecisionActionOutcome::ActionReady {
                    decision_created,
                    action_id,
                    inserted,
                } => {
                    // When the action already existed, the prediction and
                    // evidence were recorded in a prior cycle. Skip them:
                    // the ON CONFLICT INSERTs are no-ops but the post-commit
                    // audit trail would append duplicate events.
                    if !inserted {
                        let persistence = CandidatePersistence {
                            decision_created,
                            action_created: false,
                            quota_throttled: false,
                            action_id: Some(action_id),
                        };
                        transaction.commit().await.map_err(map_sqlx)?;
                        return Ok(persistence);
                    }
                    // ── Prediction + evidence (same tx) ──
                    let evidence = record_prediction_and_evidence_tx(
                        &mut transaction,
                        workspace_id,
                        action_id,
                        prediction,
                        strategy,
                        holdout_probability,
                        // No assignment exists on this path — nothing here
                        // guarantees interference control, so a nonzero
                        // holdout could at most claim matched quality.
                        false,
                    )
                    .await?;
                    let persistence = CandidatePersistence {
                        decision_created,
                        action_created: inserted,
                        quota_throttled: false,
                        action_id: Some(action_id),
                    };
                    // Commit first, then best-effort audit trail.
                    transaction.commit().await.map_err(map_sqlx)?;
                    super::operations::evidence::record_evidence_audit_trail(
                        self,
                        workspace_id,
                        &evidence,
                    )
                    .await;
                    return Ok(persistence);
                }
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(result)
        })
        .await
    }
    };
}
