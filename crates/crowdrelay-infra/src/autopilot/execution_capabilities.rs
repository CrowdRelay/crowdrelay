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
        "crowdrelay.social.post_requested" => "social.post",
        _ => "unknown",
    }
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
