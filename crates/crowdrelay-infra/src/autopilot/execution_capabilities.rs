fn executor_capability_for_event(event_type: &str) -> &'static str {
    match event_type {
        "crowdrelay.fan_lifecycle.message_requested" => "fan.lifecycle.message",
        "crowdrelay.merch.reorder_requested" => "merch.reorder",
        "crowdrelay.booking.outreach_requested" => "booking.outreach",
        // Its own capability rather than riding `booking.outreach`, which is
        // the opposite of the choice made for the report kinds above and for
        // the same reason read the other way: this event carries a recipient
        // set and a different template, so an executor that advertises
        // `booking.outreach` would claim it and then have nothing to do with
        // it. Until one advertises `gig.outreach` these park, which is the
        // honest state — nothing was sent, and the board says so.
        "crowdrelay.gig.outreach_requested" => "gig.outreach",
        "crowdrelay.merch.bundle_requested" => "merch.bundle",
        "crowdrelay.outreach.requested" => "outreach.send",
        "crowdrelay.representation.approach_requested" => "representation.approach",
        // Its own capability rather than riding `representation.approach`:
        // the payload names a registry agent and carries the draw snapshot,
        // and an executor that only knows the listing-letter shape would
        // claim it and have nothing to send. Until one advertises
        // `booking_agent.approach` these park, which is the honest state.
        "crowdrelay.booking_agent.approach_requested" => "booking_agent.approach",
        "crowdrelay.beacon.discovery_requested" => "beacon.discovery",
        "crowdrelay.outreach.discovery_requested" => "outreach.discovery",
        "crowdrelay.booking.target_discovery_requested" => "booking.discovery",
        "crowdrelay.beacon.outreach_requested" => "beacon.outreach",
        // Its own capability rather than riding `beacon.outreach`: the
        // contract bundled them on the premise that a beacon-pitch transport
        // exists to share, and none does — no executor has ever advertised
        // `beacon.outreach` and a pitch branch was never built. Advertising
        // `beacon.outreach` to unblock this send would also unpark pitch
        // emissions onto a route nobody serves, so the letter that is ready
        // to send gets a capability that says exactly that.
        "crowdrelay.latarnik.invite_requested" => "latarnik.invite",
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
        // The R+14 outcome read is the same delivery class — an email to the
        // band with the second wave's receipts.
        "crowdrelay.release.r14_report_due" => "show.escalation",
        // The named likely-listener list is likewise a note to the band, not
        // a task — the pre-save send itself is the campaign, this is its
        // day-one evidence.
        "crowdrelay.release.likely_listeners" => "show.escalation",
        // Same class again: a parked or escalated editorial pitch is a note to
        // the band about work that needs a human, not an executor task.
        "crowdrelay.release.editorial_pitch_parked" => "show.escalation",
        "crowdrelay.release.editorial_pitch_escalated" => "show.escalation",
        "crowdrelay.ops.status_changed" => "ops.alert",
        "crowdrelay.promotion.budget_change_requested" => "promotion.budget",
        "crowdrelay.opportunity.application_requested" => "opportunity.application",
        // The counterparty report is the same delivery class as the post-show
        // report it precedes in the negotiation — a note to the humans around
        // the deal — so it rides the same show.escalation executor.
        "crowdrelay.opportunity.counterparty_report_issued" => "show.escalation",
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
                | AutopilotActionPayload::RequestGigOutreach { .. }
                | AutopilotActionPayload::RequestLatarnikInvite { .. }
                | AutopilotActionPayload::RequestMerchBundle { .. }
                | AutopilotActionPayload::RequestOutreach { .. }
                | AutopilotActionPayload::RequestRepresentationApproach { .. }
                | AutopilotActionPayload::RequestBookingAgentApproach { .. }
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
                // Sends through the same machinery as the checklist's report
                // escalation — same capability, same event.
                | AutopilotActionPayload::IssueCounterpartyReport { .. }
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
        AutopilotActionPayload::RequestGigOutreach { .. } => "gig.outreach",
        AutopilotActionPayload::RequestMerchBundle { .. } => "merch.bundle",
        AutopilotActionPayload::RequestOutreach { .. } => "outreach.send",
        AutopilotActionPayload::RequestRepresentationApproach { .. } => "representation.approach",
        AutopilotActionPayload::RequestBookingAgentApproach { .. } => "booking_agent.approach",
        AutopilotActionPayload::RequestBeaconDiscovery { .. } => "beacon.discovery",
        AutopilotActionPayload::RequestOutreachDiscovery { .. } => "outreach.discovery",
        AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => "booking.discovery",
        AutopilotActionPayload::RequestBeaconOutreach { .. } => "beacon.outreach",
        // Its own capability rather than riding `beacon.outreach`: the shared
        // pitch transport the bundling assumed was never built, so keeping the
        // mapping would let one unbuilt branch hold the other's letters
        // hostage. An executor that can send an approved invitation verbatim
        // is live today; a pitch composer is a different, unbuilt thing.
        AutopilotActionPayload::RequestLatarnikInvite { .. } => "latarnik.invite",
        AutopilotActionPayload::RequestBeaconInviteBatch { .. } => "beacon.invite_batch",
        AutopilotActionPayload::RequestShowGrowth { .. } => "show.growth",
        AutopilotActionPayload::RequestContentArtifact { .. } => "content.artifact",
        AutopilotActionPayload::EscalateShowTask { .. } => "show.escalation",
        AutopilotActionPayload::IssueCounterpartyReport { .. } => "show.escalation",
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

/// Whether an approval taken right now could actually be executed (4G.4).
///
/// # Why an approval needs to ask
///
/// The dispatcher parks an action whose capability nobody advertises, and the
/// stale sweep cancels it `NO_EXECUTOR_GRACE` later with `no_executor`. For
/// work the brain proposed that is the right shape: the decision cost nobody
/// anything. For work a **person just approved** it is a silent loss — the
/// band read the reasons, said "write to these three promoters", got an
/// acknowledgement, and a day later the letter was cancelled by a sweep
/// nothing shows them. Approvals that cannot be executed must be refused at
/// the moment of asking, with the sentence that says why.
///
/// Fail-open on an empty registry, exactly as the dispatcher's own gate does:
/// a workspace where nothing has ever advertised anything is not a workspace
/// where everything is blocked.
///
/// # Errors
///
/// Propagates the database error.
pub async fn capability_is_serviceable(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<bool, RepositoryError> {
    let registry_active = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    if !registry_active {
        return Ok(true);
    }
    capability_is_advertised(pool, workspace_id, capability)
        .await
        .map_err(map_sqlx)
}

/// The advertisement predicate itself: a live capability, on a live executor,
/// whose circuit breaker is not holding it open.
///
/// One definition, two callers — the dispatcher's transaction-scoped probe and
/// the approval-time check above. A second copy would be a gate that drifts,
/// and a gate that drifts admits work the other one refuses.
async fn capability_is_advertised<'e, E>(
    executor: E,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
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
    .fetch_one(executor)
    .await
}

/// Non-failing probe for callers that treat a missing executor as a soft
/// skip (best-effort notifications) instead of refusing the operation.
pub(in crate::autopilot) async fn executor_capability_available(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    capability: &str,
) -> Result<bool, RepositoryError> {
    capability_is_advertised(&mut **transaction, workspace_id, capability)
        .await
        .map_err(map_sqlx)
}

/// The roster's monthly share of one person's attention (§4d-3): how many
/// touches the whole organization may spend on one contact in a trailing
/// thirty days, counted in `viryaos_contact_touches`.
///
/// The governor says *not twice this week*; the budget says *of the four
/// things this roster wants to tell this person this month, these three were
/// worth it* — the pooled form of `fatigue_decay`. It is a rate the label
/// sets, not a per-workspace knob: a workspace with no `organization_id` is a
/// roster of one and the same cap binds it, because three touches a month is
/// what one sender owes one person, whoever holds the pen.
pub(in crate::autopilot) const ORG_MONTHLY_CONTACT_BUDGET: u32 = 3;

/// Reserves the next contact window for one address, across the whole
/// organization when there is one.
///
/// The table is keyed `(workspace_id, normalized_contact)`, and an act is a
/// workspace: a roster of eight acts is eight workspaces under one
/// `organizations` row. Keyed that way alone, the same person takes one message
/// per act per week with every cooldown satisfied, and a `do_not_contact` given
/// to one act does not bind the others. To someone who reads the roster as one
/// sender that is not a cooldown at all.
///
/// So the insert is gated on no sibling workspace in the same organization
/// holding a live block — either `do_not_contact`, or a window that has not
/// expired. It stays one statement: a pre-check followed by an insert would
/// leave a gap two acts could both pass through.
///
/// The monthly budget is the same rule one level up. `viryaos_contact_touches`
/// counts what the organization already sent this person in the trailing
/// thirty days — the workspace's own rows included, so the cap binds a lone
/// tenant too — and the count rides inside the insert's `WHERE` for the same
/// reason the sibling block does: a pre-check plus an insert leaves a gap two
/// acts could both pass through. The `EXISTS` half is the replay exception —
/// an action that already reserved this window is not a new spend, so its own
/// earlier touch row must not count against it.
///
/// A workspace with no `organization_id` sees only its own touches. The
/// sibling half of the count is vacuous for it, so a single-act tenant keeps
/// the cooldown it always had and gains the monthly cap — which is the point:
/// the budget is owed to the person, not to the roster shape.
///
/// # Errors
///
/// `ConflictBecause(ORG_ATTENTION_BUDGET_ERROR_KIND)` when the organization's
/// thirty-day share for this contact is already spent, so the worker can
/// record `org_attention_budget` rather than the generic `state_changed`.
/// `Conflict` for the older refusals — sibling cooldown, `do_not_contact`,
/// and the per-workspace window in the `ON CONFLICT` arm.
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
    // Read the budget before reserving so the refusal can say what refused
    // it — the insert returns no row for a spent budget exactly as it does
    // for a sibling cooldown, and `last_error_kind` is the only channel that
    // survives to the operator. The count is also folded into the insert
    // itself, so this read is the reason and that one is the gate; a touch
    // committed between the two still cannot pass. The join shape is the
    // sibling gate's own: the roster is one sender, so the count is one
    // sender's count — the workspace's own touches plus its siblings'.
    let budget_spent = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT COUNT(*) >= $4 AND NOT EXISTS (
            SELECT 1
            FROM viryaos_contact_touches replay
            WHERE replay.workspace_id = $1
              AND replay.normalized_contact = $2
              AND replay.action_id = $5
        )
        FROM viryaos_contact_touches sibling
        JOIN workspaces sibling_ws ON sibling_ws.id = sibling.workspace_id
        JOIN workspaces self_ws ON self_ws.id = $1
        WHERE sibling.normalized_contact = $2
          AND sibling.touched_at > $3 - INTERVAL '30 days'
          AND (
              sibling.workspace_id = $1
              OR (self_ws.organization_id IS NOT NULL
                  AND sibling_ws.organization_id = self_ws.organization_id)
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&normalized)
    .bind(now)
    .bind(i64::from(ORG_MONTHLY_CONTACT_BUDGET))
    .bind(action_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if budget_spent {
        return Err(RepositoryError::ConflictBecause(
            ORG_ATTENTION_BUDGET_ERROR_KIND,
        ));
    }
    let reserved = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO viryaos_contact_governor (
            workspace_id, normalized_contact, last_context, last_action_id,
            last_outbound_at, next_contact_after
        )
        SELECT $1,$2,$3,$4,$5,$5 + INTERVAL '7 days'
        WHERE NOT EXISTS (
            SELECT 1
            FROM viryaos_contact_governor sibling
            JOIN workspaces sibling_ws ON sibling_ws.id = sibling.workspace_id
            JOIN workspaces self_ws ON self_ws.id = $1
            WHERE sibling.normalized_contact = $2
              AND sibling.workspace_id <> $1
              AND self_ws.organization_id IS NOT NULL
              AND sibling_ws.organization_id = self_ws.organization_id
              AND (sibling.do_not_contact OR sibling.next_contact_after > $5)
        )
        AND (
            EXISTS (
                SELECT 1
                FROM viryaos_contact_touches replay
                WHERE replay.workspace_id = $1
                  AND replay.normalized_contact = $2
                  AND replay.action_id = $4
            )
            OR (
                SELECT COUNT(*)
                FROM viryaos_contact_touches sibling
                JOIN workspaces sibling_ws ON sibling_ws.id = sibling.workspace_id
                JOIN workspaces self_ws ON self_ws.id = $1
                WHERE sibling.normalized_contact = $2
                  AND sibling.touched_at > $5 - INTERVAL '30 days'
                  AND (
                      sibling.workspace_id = $1
                      OR (self_ws.organization_id IS NOT NULL
                          AND sibling_ws.organization_id = self_ws.organization_id)
                  )
            ) < $6
        )
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
    .bind(i64::from(ORG_MONTHLY_CONTACT_BUDGET))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if reserved.is_none() {
        return Err(RepositoryError::Conflict);
    }
    // The ledger write shares the reservation's transaction and its action_id:
    // a replayed action finds its own row already counted and the primary key
    // turns the second insert into a no-op instead of a second spend.
    sqlx::query(
        r#"
        INSERT INTO viryaos_contact_touches (
            workspace_id, normalized_contact, action_id, touched_at
        ) VALUES ($1,$2,$3,$4)
        ON CONFLICT (workspace_id, normalized_contact, action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&normalized)
    .bind(action_id.into_uuid())
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// One executor capability as the workspace stands right now.
///
/// `state` is one of:
/// - `live` — an unexpired executor advertises it and no breaker holds it open.
/// - `blocked` — advertised, but not usable this minute: the advertisement or
///   the executor expired, or the circuit breaker is holding it. Temporary by
///   construction, which is why it is not lumped into `missing`.
/// - `missing` — the workspace needed it (an action is parked behind it) and
///   nobody advertises it.
#[derive(Debug, serde::Serialize)]
pub struct ExecutorCapabilityPosture {
    pub capability: String,
    pub state: &'static str,
    /// The executors that advertise it — names matter when two run and only
    /// one is healthy.
    pub executors: Vec<String>,
    /// Queued actions parked behind this capability right now. The number a
    /// missing row costs, so the screen orders by what is actually waiting.
    pub awaiting: u32,
}

/// The capability posture for one workspace: everything its executors offer
/// and everything its queued actions are parked behind.
///
/// The needed set is measured, not catalogued: a capability enters it when a
/// parked action's payload resolves to it through the same
/// `executor_capability_for_payload` mapping the dispatcher enforces, so the
/// list cannot drift from the gate it describes. A capability the workspace
/// has never needed and nobody advertises does not appear — there is nothing
/// to say about it.
///
/// `executors_registered` separates "no executor has ever heartbeated" from
/// "executors run but this lane is dark" — the dispatcher fails open on the
/// first and fails closed on the second, and the screen must not conflate them.
#[derive(Debug, serde::Serialize)]
pub struct ExecutorCapabilityReport {
    pub executors_registered: bool,
    pub capabilities: Vec<ExecutorCapabilityPosture>,
}

/// # Errors
///
/// Propagates the database error.
pub async fn executor_capability_posture(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<ExecutorCapabilityReport, sqlx::Error> {
    let ws = workspace_id.into_uuid();

    // Advertised half: every capability row on a live or dead executor, with
    // what makes it usable or not — expiry on either row and the breaker.
    let advertised = sqlx::query_as::<_, (String, String, OffsetDateTime, OffsetDateTime, Option<OffsetDateTime>)>(
        r#"
        SELECT capability.capability, capability.executor_id,
               capability.expires_at, executor.expires_at, breaker.guarded_until
        FROM viryaos_executor_capabilities capability
        JOIN viryaos_executor_instances executor
          ON executor.workspace_id = capability.workspace_id
         AND executor.executor_id = capability.executor_id
        LEFT JOIN viryaos_executor_circuit_breakers breaker
          ON breaker.workspace_id = executor.workspace_id
         AND breaker.executor_id = executor.executor_id
        WHERE capability.workspace_id = $1
        "#,
    )
    .bind(ws)
    .fetch_all(pool)
    .await?;

    let executors_registered = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id = $1)",
    )
    .bind(ws)
    .fetch_one(pool)
    .await?;

    // Needed half: queued actions the capability gate parked. The payload
    // carries the answer because the gate reads the payload — counting any
    // other way would be a second definition of "waiting on an executor".
    let parked_payloads = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT payload
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND status = 'queued'
          AND last_error_kind = 'awaiting_executor'
        "#,
    )
    .bind(ws)
    .fetch_all(pool)
    .await?;

    let mut awaiting: HashMap<String, u32> = HashMap::new();
    for payload in &parked_payloads {
        if let Ok(parsed) = serde_json::from_value::<AutopilotActionPayload>(payload.clone())
            && let Some(capability) = executor_capability_for_payload(&parsed)
        {
            *awaiting.entry(capability.to_owned()).or_insert(0) += 1;
        }
    }

    let mut by_capability: HashMap<String, ExecutorCapabilityPosture> = HashMap::new();
    for (capability, executor_id, cap_expires, exec_expires, guarded_until) in advertised {
        let live = cap_expires > now
            && exec_expires > now
            && guarded_until.is_none_or(|until| until <= now);
        let entry = by_capability
            .entry(capability.clone())
            .or_insert_with(|| ExecutorCapabilityPosture {
                capability: capability.clone(),
                state: "blocked",
                executors: Vec::new(),
                awaiting: 0,
            });
        entry.executors.push(executor_id);
        // One live advertisement makes the lane live; the rest staying blocked
        // is detail the executor list already carries.
        if live {
            entry.state = "live";
        }
    }
    for (capability, count) in awaiting {
        by_capability
            .entry(capability.clone())
            .or_insert_with(|| ExecutorCapabilityPosture {
                capability: capability.clone(),
                state: "missing",
                executors: Vec::new(),
                awaiting: 0,
            })
            .awaiting = count;
    }
    // Advertised-and-parked is `blocked`, never `missing` — the lane exists,
    // something is holding it, and the breaker or expiry says what.
    let mut capabilities: Vec<ExecutorCapabilityPosture> = by_capability.into_values().collect();
    // What is waiting, then what is missing, then what is held, then the
    // lanes that work — the order a screen should show them in. The state
    // rank is a number because the string order would put missing last.
    fn state_rank(state: &str) -> u8 {
        match state {
            "missing" => 0,
            "blocked" => 1,
            _ => 2,
        }
    }
    capabilities.sort_by(|left, right| {
        right
            .awaiting
            .cmp(&left.awaiting)
            .then_with(|| state_rank(left.state).cmp(&state_rank(right.state)))
            .then_with(|| left.capability.cmp(&right.capability))
    });

    Ok(ExecutorCapabilityReport {
        executors_registered,
        capabilities,
    })
}
