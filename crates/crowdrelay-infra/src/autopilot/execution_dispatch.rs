// Dispatch envelope — the prediction and experiment rows an action needs
// before its outcome can teach anything.
//
// Split out of `execution.rs` when that chunk crossed the 1000-line limit the
// modularity contract sets for one `include!` chunk. Pure relocation: an
// included chunk shares its parent's scope, so nothing here changed but the
// file it lives in.

/// Writes the dispatch-prediction and growth-evidence envelope for an action
/// that never passed through the decision persist — outcome-created actions
/// (an approved engager post, an agent content draft, a signal push) are
/// real fan-facing interventions, and without the envelope their scheduled
/// measurements UPDATE rows that do not exist and the causal model learns
/// nothing from the outcome.
///
/// The prediction is the cold prior (`DEFAULT_EXPECTED_*`), the honest value
/// the model would have returned for an unseen template. The outcome model
/// learns from raw observed counts, not the prediction — so the constant
/// narrows calibration fidelity, never corrupts the learner.
///
/// Both inserts are `ON CONFLICT DO NOTHING`: evaluator-created actions
/// already carry their envelope and this is a no-op for them.
pub(super) async fn ensure_dispatch_envelope(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_brain::{
        DispatchContext, GrowthEvidence, OpportunityAction, OpportunityId, TreatmentAssignment,
        channel_for_template,
    };

    let (template_id, recipient_id, target_key, context) = match payload {
        AutopilotActionPayload::RequestCommunityEngagement {
            target_id,
            subreddit,
            ..
        } => {
            let handle = subreddit
                .clone()
                .unwrap_or_else(|| target_id.to_string());
            let context = DispatchContext {
                post_format: Some("link".to_owned()),
                ..DispatchContext::default()
            };
            (
                "community-engager".to_owned(),
                handle.clone(),
                Some(format!("community:{handle}")),
                context,
            )
        }
        AutopilotActionPayload::RequestAgentContent {
            template_id,
            task_id,
            ..
        } => (
            template_id.clone().unwrap_or_else(|| "agent-content".to_owned()),
            task_id.to_string(),
            None,
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestSignalPush { task_id, .. } => (
            "signal-inviter".to_owned(),
            task_id.to_string(),
            None,
            DispatchContext::default(),
        ),
        // A brain-requested artifact is a fan-facing intervention like the
        // kinds above, but its candidates persist through the plain path —
        // no envelope anywhere. Its measurements are scheduled at the
        // executor receipt, so the envelope is filled there; without it the
        // fan-growth trio would UPDATE evidence rows that do not exist.
        //
        // `channel_for_template` reads `Other` for this template id, which is
        // the honest surface: the artifact kind names the piece, not where
        // it was published — a video and a newsletter block share the same
        // request shape. The kind itself travels as `post_format`.
        AutopilotActionPayload::RequestContentArtifact {
            source_id,
            artifact,
            template_key,
            ..
        } => {
            let artifact_key = serde_json::to_value(artifact)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            (
                format!("content-artifact:{template_key}"),
                format!("source:{source_id}"),
                Some(format!("source:{source_id}")),
                DispatchContext {
                    post_format: Some(artifact_key),
                    ..DispatchContext::default()
                },
            )
        }
        // The persist-path kinds below are real interventions whose
        // measurements write `observed_metrics` onto this row at completion.
        // They pass through `persist_candidate`, which writes decision and
        // action but no evidence — without the envelope filled here, those
        // UPDATEs hit zero rows and the measured outcome never reaches a
        // posterior. Each template id groups the action's own subject so the
        // posterior learns the lever, not the workspace's weather.
        AutopilotActionPayload::ChangeTicketPrice { ticket_type_id, .. } => (
            "ticket-price".to_owned(),
            format!("ticket-type:{ticket_type_id}"),
            Some(format!("ticket-type:{ticket_type_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::ChangeMerchPrice { product_id, .. } => (
            "merch-price".to_owned(),
            format!("merch-product:{product_id}"),
            Some(format!("merch-product:{product_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestPromotionBudgetChange { campaign_id, .. } => (
            "promotion-budget".to_owned(),
            format!("promo-campaign:{campaign_id}"),
            Some(format!("promo-campaign:{campaign_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestBookingOutreach { target_id, .. } => (
            "booking-outreach".to_owned(),
            format!("booking-target:{target_id}"),
            Some(format!("booking-target:{target_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestGigOutreach { venue, .. } => (
            "gig-outreach".to_owned(),
            format!("venue:{venue}"),
            None,
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestOutreach { target_id, .. } => (
            "outreach".to_owned(),
            format!("contact:{target_id}"),
            Some(format!("contact:{target_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestRepresentationApproach { target_id, .. } => (
            "representation-approach".to_owned(),
            format!("contact:{target_id}"),
            Some(format!("contact:{target_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestBookingAgentApproach { agent_id, .. } => (
            "booking-agent-approach".to_owned(),
            format!("agent:{agent_id}"),
            Some(format!("agent:{agent_id}")),
            DispatchContext::default(),
        ),
        // A reply rides the contact's own grouping: whether the person wrote
        // back again is the pitch's posterior continuing, not a new lever.
        AutopilotActionPayload::RequestOutreachReply { target_id, .. } => (
            "outreach-reply".to_owned(),
            format!("contact:{target_id}"),
            Some(format!("contact:{target_id}")),
            DispatchContext::default(),
        ),
        // The agent reply rides the agent's own grouping for the same reason.
        AutopilotActionPayload::RequestBookingAgentReply { agent_id, .. } => (
            "booking-agent-reply".to_owned(),
            format!("agent:{agent_id}"),
            Some(format!("agent:{agent_id}")),
            DispatchContext::default(),
        ),
        // One evidence row per wave — the per-agent measurements merge their
        // observed counts onto it, so the batch learns as one intervention.
        AutopilotActionPayload::RequestBookingAgentApproachWave { wave_id, .. } => (
            "booking-agent-approach-wave".to_owned(),
            format!("wave:{wave_id}"),
            None,
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestAudienceCampaign { event_id, .. } => (
            "audience-campaign".to_owned(),
            format!("event:{event_id}"),
            Some(format!("event:{event_id}")),
            DispatchContext::default(),
        ),
        // The drop surge's email leg groups by its source: two videos
        // dropping in one day are two campaigns, each measuring its own.
        AutopilotActionPayload::RequestSourceCampaign { source_id, .. } => (
            "source-campaign".to_owned(),
            format!("source:{source_id}"),
            Some(format!("source:{source_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestShowGrowth { event_id, lever, .. } => (
            format!("show-growth:{}", lever.as_str()),
            format!("event:{event_id}"),
            Some(format!("event:{event_id}")),
            DispatchContext::default(),
        ),
        // Internal rungs schedule no measurements — an envelope for one
        // would sit unresolved forever, exactly the clutter the `_` arm's
        // comment below warns against.
        AutopilotActionPayload::ExecuteReleaseMilestone {
            milestone: ReleaseMilestone::SeedCalendar | ReleaseMilestone::EditorialPitch,
            ..
        } => {
            return Ok(());
        }
        AutopilotActionPayload::ExecuteReleaseMilestone {
            release_id,
            milestone,
            ..
        } => (
            format!("release-milestone:{}", milestone.as_str()),
            format!("release:{release_id}"),
            Some(format!("release:{release_id}")),
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestFanLifecycleMessage { fan_id, .. } => (
            "fan-lifecycle".to_owned(),
            format!("fan:{fan_id}"),
            Some(format!("fan:{fan_id}")),
            DispatchContext::default(),
        ),
        // The weekly join-ask posts through the social executor, which files
        // no executor receipt — the envelope is filled here so the
        // `content_link_clicks_7d` measurement scheduled below has rows to
        // write to. The platform is the posterior's target: a Facebook ask
        // and an Instagram ask are different levers and must not share one
        // belief.
        AutopilotActionPayload::PublishJoinAsk { platform, .. } => (
            format!("join-ask:{platform}"),
            format!("platform:{platform}"),
            Some(format!("platform:{platform}")),
            DispatchContext {
                post_format: Some("join_ask".to_owned()),
                ..DispatchContext::default()
            },
        ),
        // Everything left schedules no measurements — an internal mark, a
        // held escalation, a task completion — so it owes no evidence row,
        // and writing one would leave an unresolved row nothing resolves.
        _ => return Ok(()),
    };

    // The engager's chosen angle rides the engage payload so the published
    // post's measurement teaches the family posterior. Every other kind
    // leaves it `None` — the family is a property of drafted community
    // content, not of a price change or a send.
    let creative_family = match payload {
        AutopilotActionPayload::RequestCommunityEngagement {
            creative_family,
            ..
        } => *creative_family,
        _ => None,
    };

    let context_json =
        serde_json::to_value(&context).unwrap_or_else(|_| serde_json::json!({}));
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
    .bind(action_id.into_uuid())
    .bind(&template_id)
    .bind(crowdrelay_brain::DEFAULT_EXPECTED_FANS)
    .bind(crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL)
    .bind(&context_json)
    .bind(serde_json::json!({}))
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    let target = target_key
        .clone()
        .unwrap_or_else(|| format!("action:{action_id}"));
    let opportunity_id = OpportunityId::new(
        &template_id,
        &target,
        OpportunityAction::Post,
        &context,
    );
    let evidence = GrowthEvidence::at_dispatch(
        workspace_id.into_uuid(),
        Some(action_id.into_uuid()),
        Some(opportunity_id.to_string()),
        recipient_id,
        channel_for_template(&template_id),
        1,
        TreatmentAssignment::Treatment,
        1.0,
        crowdrelay_brain::DEFAULT_EXPECTED_FANS,
        crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL,
        context,
        target_key,
        creative_family,
        None,
        crowdrelay_brain::EvidenceQuality::Observational,
    );
    crate::autopilot::operations::evidence::record_growth_evidence_in_tx(
        transaction,
        workspace_id,
        &evidence,
    )
    .await?;
    Ok(())
}

/// Builds the `send_evidence` value an outward emission must carry (2.1).
///
/// The constructor refuses blank facts; the gate in [`emit_external_action`]
/// refuses the whole send when the key is absent. Building it here and failing
/// is the same refusal one layer earlier, with the call site named.
pub(super) fn send_evidence(
    source_id: impl Into<String>,
    recipient_reason: impl Into<String>,
) -> Result<Value, RepositoryError> {
    crowdrelay_domain::outward_evidence::OutwardEvidence::new(source_id, recipient_reason)
        .map(|evidence| evidence.to_json())
        .map_err(|refusal| RepositoryError::ConflictBecause(refusal.message()))
}

/// Emits an outward send with `send_evidence` built and attached (2.1).
///
/// Call sites hand over the payload without the key plus the two facts the
/// evidence needs; a blank fact refuses the send before it is expressed. This
/// is the shape an outward emission is supposed to take — the gate in
/// [`emit_external_action`] is the backstop for a hand-rolled call, not the
/// intended path.
pub(super) async fn emit_outward_action(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &'static str,
    source_id: impl Into<String>,
    recipient_reason: impl Into<String>,
    mut payload: Value,
) -> Result<(), RepositoryError> {
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "send_evidence".to_owned(),
            send_evidence(source_id, recipient_reason)?,
        );
    }
    emit_external_action(transaction, workspace_id, action_id, event_type, payload).await
}

/// The keyed form of [`emit_outward_action`] — for the one action that
/// legitimately sends more than one letter under a single approval: an
/// approach wave. The caller builds the evidence itself so the function
/// stays inside the argument budget; the suffix becomes part of the
/// emission key, so every approach gets its own outbox row and a dispatch
/// retry re-targets the same rows instead of minting a second letter — or
/// swallowing every approach after the first under the shared default key.
pub(super) async fn emit_outward_action_keyed(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &'static str,
    send_evidence: Value,
    mut payload: Value,
    emission_suffix: &str,
) -> Result<(), RepositoryError> {
    if let Some(map) = payload.as_object_mut() {
        map.insert("send_evidence".to_owned(), send_evidence);
    }
    emit_external_action_keyed(
        transaction,
        workspace_id,
        action_id,
        event_type,
        payload,
        Some(emission_suffix),
    )
    .await
}

/// The evidence gate every outward send must pass (2.1/2.2).
///
/// Bound on the action's durable `action_class` — the same classification the
/// envelope charged at decision time — not on anything the payload claims. An
/// outward send without `send_evidence` is refused, a third-party send whose
/// draft is byte-identical to one that already went out is refused, and the
/// refusal lands inside the caller's transaction so the whole emission rolls
/// back rather than partially sending.
async fn gate_outward_emission(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &str,
    payload: &mut Value,
) -> Result<(), RepositoryError> {
    let gate_row = sqlx::query_as::<_, (Option<String>, Uuid)>(
        r#"
        SELECT action_class, subject_id
        FROM autopilot_actions
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    // The emission hangs off the action row — emitting for an action that does
    // not exist is a wiring bug, not a missing row.
    let Some((class_raw, subject_id)) = gate_row else {
        return Err(RepositoryError::NotFound);
    };
    let Some(class) = class_raw
        .as_deref()
        .and_then(crowdrelay_domain::action_class::ActionClass::parse)
    else {
        return Ok(());
    };
    if !class.is_outward() {
        return Ok(());
    }

    let evidence = crowdrelay_domain::outward_evidence::OutwardEvidence::from_payload(payload)
        .map_err(|refusal| RepositoryError::ConflictBecause(refusal.message()))?;

    // The tracked-link backstop, scoped to stranger outreach — the class this
    // rule exists for. Composers make an untracked URL unrepresentable —
    // `TrackedLink` is the only link a letter can carry — so what reaches
    // this check is a bypass: a draft persisted before the gate, or a body a
    // person edited after approval. Either way the send refuses: the one
    // thing a pitch must never do is ask a stranger to click a link the
    // ledger cannot count. Owned-audience broadcasts are exempt — an event
    // announcement may legitimately carry the ticketing provider's own URL,
    // which is not ours to wrap in a redirect.
    if class == crowdrelay_domain::action_class::ActionClass::ThirdParty
        && let Some(body) = payload
            .get("draft")
            .and_then(|draft| draft.get("body"))
            .and_then(Value::as_str)
    {
        let site_root = sqlx::query_scalar::<_, String>(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
        )
        .bind(workspace_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty());
        let untracked =
            crowdrelay_domain::untracked_links_in(body, site_root.as_deref());
        if !untracked.is_empty() {
            return Err(RepositoryError::ConflictBecause(
                "letter body carries a URL the ledger cannot see — only \
                 {site}/l/{slug} links may reach a reader",
            ));
        }
    }

    // `last_contact_at` is measured, never claimed: the newest outward touch
    // on this subject, read from the durable action rows. A first contact
    // reads `NULL` — the honest answer to "when did we last reach them".
    let last_contact = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        r#"
        SELECT max(created_at)
        FROM autopilot_actions
        WHERE workspace_id = $1
          AND subject_id = $2
          AND action_class IN ('owned_audience', 'third_party')
          AND status <> 'cancelled'
          AND id <> $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(subject_id)
    .bind(action_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    payload["send_evidence"] = evidence.with_last_contact(last_contact).to_json();

    // Identical-draft refusal (2.2). A third-party send whose text already
    // went out — on the same event type, to anyone — is a broadcast wearing a
    // pitch's costume. Owned-audience sends are exempt on purpose: one text to
    // many consented fans is what a broadcast *is*.
    if class == crowdrelay_domain::action_class::ActionClass::ThirdParty {
        let draft = payload.get("draft").filter(|value| value.is_object()).cloned();
        let body = payload.get("body").and_then(Value::as_str).map(str::to_owned);
        let title = payload.get("title").and_then(Value::as_str).map(str::to_owned);
        if draft.is_some() || body.is_some() {
            // Community engagement is scoped per target: a verbatim repeat at
            // the *same* community is the broadcast this gate exists to stop,
            // while the same approved copy at a different community is exactly
            // what a relay batch is. Other third-party kinds (outreach,
            // representation) keep the workspace-wide rule — the same pitch
            // text to many venues remains refused. A NULL bind therefore means
            // "unscoped", not "match only targetless rows".
            let target = if event_type == "crowdrelay.community.engagement_requested" {
                payload.get("target_id").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            };
            let duplicated = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM autopilot_action_emissions emission
                    JOIN outbox_events outbound
                      ON outbound.id = emission.outbox_event_id
                     AND outbound.workspace_id = emission.workspace_id
                    WHERE emission.workspace_id = $1
                      AND outbound.event_type = $2
                      AND emission.action_id <> $3
                      AND ($7::text IS NULL OR outbound.payload->>'target_id' = $7)
                      AND (
                          ($4::jsonb IS NOT NULL AND outbound.payload->'draft' = $4)
                          OR ($5::text IS NOT NULL
                              AND outbound.payload->>'body' = $5
                              AND outbound.payload->>'title' IS NOT DISTINCT FROM $6)
                      )
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(event_type)
            .bind(action_id.into_uuid())
            .bind(draft)
            .bind(body)
            .bind(title)
            .bind(target)
            .fetch_one(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            if duplicated {
                return Err(RepositoryError::ConflictBecause(
                    "third-party send refused: this exact draft already went out — \
                     a broadcast is not a pitch",
                ));
            }
        }
    }
    Ok(())
}

pub(super) async fn emit_external_action(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &'static str,
    payload: Value,
) -> Result<(), RepositoryError> {
    // The default key keeps the one-emission-per-action invariant — a replay
    // lands on the same row. Actions that legitimately emit a second event
    // (a marker beside the payload send) must name it explicitly through
    // [`emit_external_action_keyed`] or the second event is swallowed by the
    // conflict with no trace.
    emit_external_action_keyed(
        transaction,
        workspace_id,
        action_id,
        event_type,
        payload,
        None,
    )
    .await
}

/// Same emission with an explicit key suffix. Use only when one action sends
/// more than one event — the suffix is what keeps the two emissions distinct
/// under `ON CONFLICT (workspace_id, emission_key)`.
pub(super) async fn emit_external_action_keyed(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    event_type: &'static str,
    mut payload: Value,
    emission_suffix: Option<&str>,
) -> Result<(), RepositoryError> {
    gate_outward_emission(transaction, workspace_id, action_id, event_type, &mut payload).await?;
    ensure_executor_capability(
        transaction,
        workspace_id,
        executor_capability_for_emission(event_type, &payload),
    )
    .await?;
    let emission_key = match emission_suffix {
        Some(suffix) => format!("autopilot-action:{}:{}", action_id, suffix),
        None => format!("autopilot-action:{}", action_id),
    };
    let outbox_id = Uuid::now_v7();
    // Propagate trace context (trace_id, causation_id) from the autopilot
    // action onto the outbox event so the trace spine stays continuous from
    // decision → action → outbox delivery. The action_id is already a bind
    // parameter; we join to fetch its trace columns inside the same CTE so
    // the outbox insert and the emission insert commit atomically.
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH action_trace AS (
            SELECT trace_id, causation_id
            FROM autopilot_actions
            WHERE id = $2
        ), emission AS (
            INSERT INTO autopilot_action_emissions (
                workspace_id, action_id, emission_key, outbox_event_id
            ) VALUES ($1,$2,$3,$4)
            ON CONFLICT (workspace_id, emission_key) DO NOTHING
            RETURNING outbox_event_id
        ), outbox AS (
            INSERT INTO outbox_events (
                id, workspace_id, event_type, event_version, payload,
                request_id, max_attempts, trace_id, causation_id, action_id
            )
            SELECT $4,$1,$5,$6,$7,$3,12,at.trace_id,at.causation_id,$2
            FROM emission CROSS JOIN action_trace at
            RETURNING id
        )
        SELECT id FROM outbox
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(&emission_key)
    .bind(outbox_id)
    .bind(event_type)
    .bind(EXTERNAL_ACTION_EVENT_VERSION)
    .bind(payload)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if inserted.is_some() {
        return Ok(());
    }

    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM autopilot_action_emissions
            WHERE workspace_id = $1 AND emission_key = $2 AND action_id = $3
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&emission_key)
    .bind(action_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}
