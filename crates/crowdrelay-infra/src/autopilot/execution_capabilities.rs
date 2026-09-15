fn executor_capability_for_event(event_type: &str) -> &'static str {
    match event_type {
        "crowdrelay.fan_lifecycle.message_requested" => "fan.lifecycle.message",
        "crowdrelay.merch.reorder_requested" => "merch.reorder",
        "crowdrelay.booking.outreach_requested" => "booking.outreach",
        "crowdrelay.merch.bundle_requested" => "merch.bundle",
        "crowdrelay.outreach.requested" => "outreach.send",
        "crowdrelay.beacon.discovery_requested" => "beacon.discovery",
        "crowdrelay.outreach.discovery_requested" => "outreach.discovery",
        "crowdrelay.booking.target_discovery_requested" => "booking.discovery",
        "crowdrelay.beacon.outreach_requested" => "beacon.outreach",
        "crowdrelay.beacon.invite_batch_requested" => "beacon.invite_batch",
        "crowdrelay.beacon.release_delivery_confirmation_requested" => "beacon.release.mail",
        "crowdrelay.beacon.network_discovery_requested" => "beacon.network.discovery",
        "crowdrelay.beacon.invite_delivery_requested" => "beacon.network.invite",
        "crowdrelay.show_growth.requested" => "show.growth",
        "crowdrelay.content.artifact_requested" => "content.artifact",
        "crowdrelay.show.task_attention_required" => "show.escalation",
        // The T+7 report is the same delivery class as a task escalation —
        // an email to the humans around the show — so it rides the executor
        // that already handles show notifications rather than parking behind
        // a capability nobody advertises yet.
        "crowdrelay.show.post_show_report_due" => "show.escalation",
        // The R+3 release report is the same delivery class — an email to the
        // band — so it rides the same executor. An unmapped kind resolves to
        // "unknown" and fails closed wherever executors are registered, which
        // would wedge the whole sustain arm on every retry.
        "crowdrelay.release.r3_report_due" => "show.escalation",
        "crowdrelay.ops.status_changed" => "ops.alert",
        "crowdrelay.promotion.budget_change_requested" => "promotion.budget",
        "crowdrelay.opportunity.application_requested" => "opportunity.application",
        // One capability for both moves. An executor that can write to a
        // promoter can write either message, and splitting them would let a
        // workspace advertise the ability to accept without the ability to
        // counter — which is the wrong half to have.
        "crowdrelay.playlist.placement_check_requested" => "playlist.verify",
        "crowdrelay.opportunity.terms_countered" => "opportunity.terms",
        "crowdrelay.opportunity.terms_accepted" => "opportunity.terms",
        "crowdrelay.funding.package_requested" => "funding.package",
        "crowdrelay.funding.submission_requested" => "funding.submit",
        "crowdrelay.calendar.upsert_requested" => "calendar.upsert",
        "crowdrelay.play.step_requested" => "play.step",
        "crowdrelay.team.assignment_email_requested" => "team.email",
        "crowdrelay.agent.content_requested" => "agent.content",
        "crowdrelay.community.engagement_requested" => "community.engage",
        _ => "unknown",
    }
}

/// The drafted-content template whose artifact is an email to a named
/// journalist. No executor claims it: the social, telegram and discord
/// executors claim `social-post`, `telegram-poster` and `discord-poster`
/// respectively, by direct SQL on the agent task's `template_id`.
pub(in crate::autopilot) const PRESS_PITCH_TEMPLATE: &str = "press-pitch";

/// The capability a press pitch needs, deliberately separate from
/// `agent.content`.
///
/// The worker advertises `agent.content` unconditionally, because the three
/// channel executors are always constructed and share that action kind. A
/// press pitch is not work any of them can do, so sharing one capability let
/// every pitch pass the gate, be dispatched, be marked `succeeded` with no
/// artifact anywhere, and be emitted to a consumer that answers HTTP 422.
/// Measured in production on 2026-09-13 and reported by both
/// `publishing.orphaned_draft` and `delivery.growth_event_refused`.
///
/// With its own capability the pitch parks with `awaiting_executor` instead,
/// the pending-approval list reports `executor_ready: false` before an
/// operator spends an approval on it, and the stale sweep cancels it by name
/// after the grace window. An n8n handler that advertises this capability
/// unparks the queue with no change here.
pub(in crate::autopilot) const PRESS_PITCH_CAPABILITY: &str = "agent.content.press_pitch";

/// The capability an emission needs. For drafted content that depends on the
/// template inside the payload, not on the event type alone, so this wraps
/// `executor_capability_for_event` rather than widening it — the public
/// executor manifest is generated from that function's event mapping.
fn executor_capability_for_emission(event_type: &str, payload: &Value) -> &'static str {
    if event_type == "crowdrelay.agent.content_requested"
        && payload.get("template_id").and_then(Value::as_str) == Some(PRESS_PITCH_TEMPLATE)
    {
        return PRESS_PITCH_CAPABILITY;
    }
    executor_capability_for_event(event_type)
}

/// Whether this payload is executed by an external executor that must file
/// a terminal execution receipt. Public for the worker's receipt
/// reconciliation sweep, which flags dispatched actions whose receipts
/// never arrived.
pub const fn payload_requires_executor(payload: &AutopilotActionPayload) -> bool {
    match payload {
        // CanonicalLinkSetup is a pure first-party DB write (smart_links), so
        // it must not be gated behind an executor capability. is_first_party
        // covers both communication campaigns and the canonical-link write.
        AutopilotActionPayload::RequestShowGrowth { lever, .. } => !lever.is_first_party(),
        _ => matches!(
            payload,
            AutopilotActionPayload::RequestFanLifecycleMessage { .. }
                | AutopilotActionPayload::RequestMerchReorder { .. }
                | AutopilotActionPayload::RequestBookingOutreach { .. }
                | AutopilotActionPayload::RequestMerchBundle { .. }
                | AutopilotActionPayload::RequestOutreach { .. }
                | AutopilotActionPayload::RequestBeaconDiscovery { .. }
                | AutopilotActionPayload::RequestOutreachDiscovery { .. }
                | AutopilotActionPayload::RequestBeaconInviteBatch { .. }
                | AutopilotActionPayload::RequestBeaconOutreach { .. }
                | AutopilotActionPayload::RequestContentArtifact { .. }
                | AutopilotActionPayload::EscalateShowTask { .. }
                | AutopilotActionPayload::RequestPromotionBudgetChange { .. }
                | AutopilotActionPayload::ApplyLiveOpportunity { .. }
                | AutopilotActionPayload::VerifyPlaylistPlacement { .. }
                | AutopilotActionPayload::CounterLiveOpportunityTerms { .. }
                | AutopilotActionPayload::AcceptLiveOpportunityTerms { .. }
                | AutopilotActionPayload::PrepareFundingPackage { .. }
                | AutopilotActionPayload::SubmitFundingApplication { .. }
                | AutopilotActionPayload::RunPlayStep { .. }
                | AutopilotActionPayload::SendTeamAssignmentEmail { .. }
                | AutopilotActionPayload::RequestOutreachTarget { .. }
        ),
    }
}

/// The capability an action will need before it is claimed, so work behind a
/// gated executor can be parked instead of claimed, attempted and burned.
///
/// `None` means the action is executed entirely inside CrowdRelay and no
/// executor is involved. The strings here are the same ones
/// `executor_capability_for_event` derives at emission time; a contract test
/// keeps the two from drifting.
pub(in crate::autopilot) fn executor_capability_for_payload(
    payload: &AutopilotActionPayload,
) -> Option<&'static str> {
    // Checked before `payload_requires_executor`, which is false for drafted
    // content: the three channel executors claim those actions by direct SQL
    // and file no receipt, so their evidence is committed at dispatch. A press
    // pitch is the one template none of them claims, so it is gated here even
    // though its siblings are not.
    if let AutopilotActionPayload::RequestAgentContent { template_id, .. } = payload
        && template_id.as_deref() == Some(PRESS_PITCH_TEMPLATE)
    {
        return Some(PRESS_PITCH_CAPABILITY);
    }
    if !payload_requires_executor(payload) {
        return None;
    }
    Some(match payload {
        AutopilotActionPayload::RequestFanLifecycleMessage { .. } => "fan.lifecycle.message",
        AutopilotActionPayload::RequestMerchReorder { .. } => "merch.reorder",
        AutopilotActionPayload::RequestBookingOutreach { .. } => "booking.outreach",
        AutopilotActionPayload::RequestMerchBundle { .. } => "merch.bundle",
        AutopilotActionPayload::RequestOutreach { .. } => "outreach.send",
        AutopilotActionPayload::RequestBeaconDiscovery { .. } => "beacon.discovery",
        AutopilotActionPayload::RequestOutreachDiscovery { .. } => "outreach.discovery",
        AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => "booking.discovery",
        AutopilotActionPayload::RequestBeaconOutreach { .. } => "beacon.outreach",
        AutopilotActionPayload::RequestBeaconInviteBatch { .. } => "beacon.invite_batch",
        AutopilotActionPayload::RequestShowGrowth { .. } => "show.growth",
        AutopilotActionPayload::RequestContentArtifact { .. } => "content.artifact",
        AutopilotActionPayload::EscalateShowTask { .. } => "show.escalation",
        AutopilotActionPayload::RequestPromotionBudgetChange { .. } => "promotion.budget",
        AutopilotActionPayload::ApplyLiveOpportunity { .. } => "opportunity.application",
        AutopilotActionPayload::VerifyPlaylistPlacement { .. } => "playlist.verify",
        AutopilotActionPayload::CounterLiveOpportunityTerms { .. } => "opportunity.terms",
        AutopilotActionPayload::AcceptLiveOpportunityTerms { .. } => "opportunity.terms",
        AutopilotActionPayload::PrepareFundingPackage { .. } => "funding.package",
        AutopilotActionPayload::SubmitFundingApplication { .. } => "funding.submit",
        AutopilotActionPayload::RunPlayStep { .. } => "play.step",
        AutopilotActionPayload::SendTeamAssignmentEmail { .. } => "team.email",
        // `payload_requires_executor` is the authority on which variants reach
        // this point; anything else executes without one.
        _ => return None,
    })
}

pub(in crate::autopilot) async fn ensure_executor_capability(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<(), RepositoryError> {
    let registry_enabled = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if !registry_enabled {
        return Ok(());
    }
    let available = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM viryaos_executor_capabilities capability
            JOIN viryaos_executor_instances executor
              ON executor.workspace_id=capability.workspace_id
             AND executor.executor_id=capability.executor_id
            LEFT JOIN viryaos_executor_circuit_breakers breaker
              ON breaker.workspace_id=executor.workspace_id
             AND breaker.executor_id=executor.executor_id
            WHERE capability.workspace_id=$1
              AND capability.capability=$2
              AND capability.expires_at>now()
              AND executor.expires_at>now()
              AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(capability)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if available {
        Ok(())
    } else {
        Err(RepositoryError::Unavailable)
    }
}

/// Strict capability gate for new external features. Unlike the backwards-
/// compatible gate above, absence of the registry is unavailable: a task must
/// never be committed unless an active executor explicitly advertises it.
/// Whether a capability is currently advertised, with no logging and no error.
///
/// The strict version is right at the moment an action is about to need a
/// capability. It is wrong for a scheduled sweep that merely *might* need one:
/// a capability an operator has deliberately gated off is a steady state, not a
/// fault, and treating it as an error makes a healthy system report a failing
/// cycle every sixty seconds forever.
/// Whether any executor has registered at all. A workspace with no registry is
/// one where nothing has ever advertised anything, and gating there would park
/// every action forever; the same fail-open rule `ensure_executor_capability`
/// applies.
pub(in crate::autopilot) async fn executor_registry_is_active(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

/// Non-failing probe for callers that treat a missing executor as a soft
/// skip (best-effort notifications) instead of refusing the operation.
pub(in crate::autopilot) async fn executor_capability_available(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM viryaos_executor_capabilities capability
            JOIN viryaos_executor_instances executor
              ON executor.workspace_id=capability.workspace_id
             AND executor.executor_id=capability.executor_id
            LEFT JOIN viryaos_executor_circuit_breakers breaker
              ON breaker.workspace_id=executor.workspace_id
             AND breaker.executor_id=executor.executor_id
            WHERE capability.workspace_id=$1
              AND capability.capability=$2
              AND capability.expires_at>now()
              AND executor.expires_at>now()
              AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(capability)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

pub(in crate::autopilot) async fn reserve_contact_window(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    context: &'static str,
    contact: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let normalized = contact.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized.len() > 320 {
        return Err(RepositoryError::Conflict);
    }
    let reserved = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO viryaos_contact_governor (
            workspace_id, normalized_contact, last_context, last_action_id,
            last_outbound_at, next_contact_after
        ) VALUES ($1,$2,$3,$4,$5,$5 + INTERVAL '7 days')
        ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
        SET last_context=EXCLUDED.last_context,
            last_action_id=EXCLUDED.last_action_id,
            last_outbound_at=EXCLUDED.last_outbound_at,
            next_contact_after=EXCLUDED.next_contact_after
        WHERE NOT viryaos_contact_governor.do_not_contact
          AND (
              viryaos_contact_governor.next_contact_after <= EXCLUDED.last_outbound_at
              OR viryaos_contact_governor.last_action_id = EXCLUDED.last_action_id
          )
        RETURNING normalized_contact
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&normalized)
    .bind(context)
    .bind(action_id.into_uuid())
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if reserved.is_some() {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}
