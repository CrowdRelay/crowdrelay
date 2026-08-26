//! Relationship outcomes and deterministic Chief-of-Staff read model.

use super::*;

pub(in crate::autopilot) async fn record_booking_reply(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    command: RecordBookingReply,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
) -> Result<AutopilotControlMutation, RepositoryError> {
    repo.bounded(async{
        if matches!(command.disposition, BookingReplyDisposition::None) { return Err(RepositoryError::Unexpected); }
        let mut tx=repo.pool.begin().await.map_err(map_sqlx)?;
        let operation_id=Uuid::now_v7(); let disposition=booking_reply_str(command.disposition);
        let details=json!({"target_id":command.target_id,"disposition":disposition,"occurred_at":command.occurred_at});
        if let Some(existing)=super::insert_operator_action(&mut tx,workspace_id,operation_id,"record_autopilot_booking_reply","booking_target",command.target_id.into_uuid(),"admin_api_key",idempotency_key,request_id,&details).await?{
            tx.commit().await.map_err(map_sqlx)?; return Ok(AutopilotControlMutation{operation_id:existing,target_id:command.target_id.into_uuid(),status:"reply_recorded".into(),replayed:true});
        }
        let relationship_delta: i32 = match command.disposition {
            BookingReplyDisposition::Received => 0,
            BookingReplyDisposition::Positive => 5,
            BookingReplyDisposition::Booked => 15,
            BookingReplyDisposition::Declined => -5,
            BookingReplyDisposition::DoNotContact => -15,
            BookingReplyDisposition::None => 0,
        };
        let new_version = sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE viryaos_booking_targets
            SET accepts_booking = CASE
                    WHEN $3 = 'do_not_contact' THEN false
                    ELSE accepts_booking
                END,
                relationship_score = GREATEST(0, LEAST(100, relationship_score + $4)),
                version = version + 1
            WHERE workspace_id = $1 AND id = $2
            RETURNING version
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(disposition)
        .bind(relationship_delta)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;

        // Relationship changes are part of the verified target history and the
        // version bump invalidates any action prepared before this reply.
        sqlx::query(
            r#"
            INSERT INTO viryaos_booking_target_history (
                workspace_id, target_id, version, target_kind, display_name, contact_email,
                capacity, priority, relationship_score, active, accepts_booking
            )
            SELECT workspace_id, id, version, target_kind, display_name, contact_email,
                   capacity, priority, relationship_score, active, accepts_booking
            FROM viryaos_booking_targets
            WHERE workspace_id = $1 AND id = $2 AND version = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(new_version)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        if disposition == "do_not_contact" {
            sqlx::query(
                r#"
                INSERT INTO viryaos_contact_governor (
                    workspace_id, normalized_contact, last_context, last_action_id,
                    last_outbound_at, next_contact_after, do_not_contact
                )
                SELECT $1, lower(btrim(contact_email)), 'booking', NULL, $3, $3, true
                FROM viryaos_booking_targets
                WHERE workspace_id=$1 AND id=$2
                ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
                SET do_not_contact=true,
                    last_context=EXCLUDED.last_context,
                    next_contact_after=GREATEST(viryaos_contact_governor.next_contact_after, EXCLUDED.next_contact_after),
                    updated_at=now()
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.target_id.into_uuid())
            .bind(command.occurred_at)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }

        sqlx::query(
            "INSERT INTO viryaos_booking_interactions(workspace_id,target_id,direction,phase,disposition,source_key,occurred_at) VALUES($1,$2,'inbound','reply',$3,$4,$5)",
        )
        .bind(workspace_id.into_uuid())
        .bind(command.target_id.into_uuid())
        .bind(disposition)
        .bind(format!("operator:{}", operation_id))
        .bind(command.occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)?; Ok(AutopilotControlMutation{operation_id,target_id:command.target_id.into_uuid(),status:"reply_recorded".into(),replayed:false})
    }).await
}

#[derive(Debug, FromRow)]
struct ChiefStatsRow {
    executed_24h: i64,
    failed_24h: i64,
    emitted_24h: i64,
    executor_confirmed_24h: i64,
    executor_failed_24h: i64,
    awaiting_approval: i64,
    estimated_minutes_saved_24h: i64,
    measured_improved_7d: i64,
    measured_neutral_7d: i64,
    measured_worsened_7d: i64,
}

#[derive(Debug, FromRow)]
struct ChiefOpportunityRow {
    context: String,
    decision_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    confidence_basis_points: i32,
    reason: String,
    disposition: String,
}

#[derive(Debug, FromRow)]
struct ChiefShowTaskRow {
    event_id: Uuid,
    event_title: String,
    task_key: String,
    status: String,
    starts_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct ChiefAttentionRow {
    kind: String,
    subject_kind: String,
    subject_id: Uuid,
    title: String,
    detail: String,
    due_at: OffsetDateTime,
}

pub(in crate::autopilot) async fn load_chief_of_staff(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<crowdrelay_application::autopilot::AutopilotChiefOfStaff, RepositoryError> {
    use crowdrelay_application::autopilot::{
        AutopilotChiefOfStaff, ChiefOfStaffAttentionItem, ChiefOfStaffOpportunity,
        ChiefOfStaffShowTask,
    };

    let stats = sqlx::query_as::<_, ChiefStatsRow>(r#"
        SELECT
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions action
              WHERE action.workspace_id=$1 AND action.status='succeeded'
                AND action.finished_at >= $2 - INTERVAL '24 hours'
                AND (
                    NOT EXISTS (
                        SELECT 1 FROM viryaos_autopilot_action_emissions emission
                        WHERE emission.workspace_id=action.workspace_id AND emission.action_id=action.id
                    )
                    OR EXISTS (
                        SELECT 1 FROM viryaos_autopilot_execution_reports report
                        WHERE report.workspace_id=action.workspace_id AND report.action_id=action.id
                          AND report.status='succeeded'
                    )
                )) executed_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions
              WHERE workspace_id=$1 AND status='failed' AND finished_at >= $2 - INTERVAL '24 hours') failed_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_action_emissions
              WHERE workspace_id=$1 AND emitted_at >= $2 - INTERVAL '24 hours') emitted_24h,
            (SELECT count(DISTINCT action_id)::bigint FROM viryaos_autopilot_execution_reports
              WHERE workspace_id=$1 AND status='succeeded' AND occurred_at >= $2 - INTERVAL '24 hours') executor_confirmed_24h,
            (SELECT count(DISTINCT report.action_id)::bigint FROM viryaos_autopilot_execution_reports report
              WHERE report.workspace_id=$1 AND report.status='failed'
                AND report.occurred_at >= $2 - INTERVAL '24 hours'
                AND NOT EXISTS (
                    SELECT 1 FROM viryaos_autopilot_execution_reports success
                    WHERE success.workspace_id=report.workspace_id
                      AND success.action_id=report.action_id AND success.status='succeeded'
                )) executor_failed_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions
              WHERE workspace_id=$1 AND status='awaiting_approval') awaiting_approval,
            (SELECT COALESCE(sum(CASE
                WHEN action_kind IN ('booking.outreach.request','outreach.request','beacon.outreach.request','beacon.discovery.request','opportunity.live.apply') THEN 10
                WHEN action_kind='show.growth.request' THEN 9
                WHEN action_kind='content.artifact.request' THEN 8
                WHEN action_kind IN ('fan.lifecycle.message.request','audience.campaign.request','release.milestone.execute') THEN 5
                WHEN action_kind IN ('merch.reorder.request','merch.bundle.request') THEN 5
                WHEN action_kind IN ('ticket.price.change','merch.price.change','promotion.budget_change.request') THEN 3
                WHEN action_kind IN ('show.task.complete','show.task.escalate','funding.package.prepare') THEN 8
                WHEN action_kind='funding.application.submit' THEN 10
                WHEN action_kind LIKE 'experiment.%' THEN 3 ELSE 2 END),0)::bigint
             FROM viryaos_autopilot_actions action
             WHERE action.workspace_id=$1 AND action.status='succeeded'
               AND action.finished_at >= $2 - INTERVAL '24 hours'
               AND (
                   NOT EXISTS (
                       SELECT 1 FROM viryaos_autopilot_action_emissions emission
                       WHERE emission.workspace_id=action.workspace_id AND emission.action_id=action.id
                   )
                   OR EXISTS (
                       SELECT 1 FROM viryaos_autopilot_execution_reports report
                       WHERE report.workspace_id=action.workspace_id AND report.action_id=action.id
                         AND report.status='succeeded'
                   )
               )) estimated_minutes_saved_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_outcomes
              WHERE workspace_id=$1 AND measurement_id IS NOT NULL AND observed_at >= $2 - INTERVAL '7 days' AND effect_assessment='improved') measured_improved_7d,
            (SELECT count(*)::bigint FROM viryaos_autopilot_outcomes
              WHERE workspace_id=$1 AND measurement_id IS NOT NULL AND observed_at >= $2 - INTERVAL '7 days' AND effect_assessment='neutral') measured_neutral_7d,
            (SELECT count(*)::bigint FROM viryaos_autopilot_outcomes
              WHERE workspace_id=$1 AND measurement_id IS NOT NULL AND observed_at >= $2 - INTERVAL '7 days' AND effect_assessment='worsened') measured_worsened_7d
    "#).bind(workspace_id.into_uuid()).bind(now).fetch_one(&repo.pool).await.map_err(map_sqlx)?;

    let opportunity_rows = sqlx::query_as::<_, ChiefOpportunityRow>(r#"
        SELECT decision.context, decision.decision_kind, decision.subject_kind, decision.subject_id,
               decision.confidence_basis_points, decision.reason, decision.disposition
        FROM viryaos_autopilot_decisions decision
        WHERE decision.workspace_id=$1
          AND decision.evaluated_at >= $2 - INTERVAL '48 hours'
          AND decision.disposition IN ('recommend_only','require_approval','auto_execute')
          AND decision.context IN ('booking_opportunity','outreach','promotion_budget','merch_bundle','merch_pricing','ticket_yield','release','live_opportunity','funding','beacon','show_growth','growth_metrics','growth_debt')
          AND NOT EXISTS (
              SELECT 1 FROM viryaos_autopilot_actions action
              WHERE action.workspace_id=decision.workspace_id AND action.decision_id=decision.id
                AND action.status IN ('succeeded','cancelled')
          )
        ORDER BY (decision.disposition='require_approval') DESC,
                 decision.confidence_basis_points DESC, decision.evaluated_at DESC, decision.id DESC
        LIMIT 10
    "#).bind(workspace_id.into_uuid()).bind(now).fetch_all(&repo.pool).await.map_err(map_sqlx)?;
    let top_opportunities = opportunity_rows
        .into_iter()
        .map(|row| {
            Ok(ChiefOfStaffOpportunity {
                context: super::parse_context(&row.context)?,
                decision_kind: row.decision_kind,
                subject_kind: row.subject_kind,
                subject_id: row.subject_id,
                confidence: super::parse_confidence(row.confidence_basis_points)?,
                reason: row.reason,
                needs_approval: row.disposition == "require_approval",
            })
        })
        .collect::<Result<Vec<_>, RepositoryError>>()?;

    // Deadline radar: reuse existing approval expiry and opportunity deadline
    // facts. No duplicate task table, scheduler or polling path is introduced.
    let attention_rows = sqlx::query_as::<_, ChiefAttentionRow>(r#"
        SELECT * FROM (
            SELECT 'approval'::text kind, action.subject_kind, action.subject_id,
                   action.action_kind title, action.context detail,
                   action.approval_expires_at due_at
            FROM viryaos_autopilot_actions action
            WHERE action.workspace_id=$1 AND action.status='awaiting_approval'
              AND action.approval_expires_at IS NOT NULL
              AND action.approval_expires_at BETWEEN $2 - INTERVAL '2 hours' AND $2 + INTERVAL '24 hours'
            UNION ALL
            SELECT CASE WHEN opportunity.opportunity_kind='funding' THEN 'funding_deadline' ELSE 'opportunity_deadline' END kind,
                   'team_opportunity'::text subject_kind, opportunity.id subject_id,
                   opportunity.title, opportunity.organization detail, opportunity.deadline due_at
            FROM viryaos_team_opportunities opportunity
            WHERE opportunity.workspace_id=$1 AND opportunity.eligible AND opportunity.deadline IS NOT NULL
              AND opportunity.status IN ('new','prepared','awaiting_approval','submission_requested')
              AND opportunity.deadline BETWEEN $2 - INTERVAL '2 days' AND $2 + INTERVAL '14 days'
        ) attention
        ORDER BY due_at ASC, kind ASC, subject_id ASC
        LIMIT 12
    "#).bind(workspace_id.into_uuid()).bind(now).fetch_all(&repo.pool).await.map_err(map_sqlx)?;
    let attention_items = attention_rows
        .into_iter()
        .map(|row| {
            let seconds = (row.due_at - now).whole_seconds();
            let urgency = if seconds < 0 {
                "overdue"
            } else if seconds < 6 * 3600 {
                "critical"
            } else if seconds < 24 * 3600 {
                "today"
            } else if seconds < 3 * 24 * 3600 {
                "soon"
            } else {
                "upcoming"
            };
            ChiefOfStaffAttentionItem {
                kind: row.kind,
                subject_kind: row.subject_kind,
                subject_id: row.subject_id,
                title: row.title,
                detail: row.detail,
                due_at: row.due_at,
                urgency: urgency.into(),
            }
        })
        .collect::<Vec<_>>();

    let show_rows = sqlx::query_as::<_, ChiefShowTaskRow>(r#"
        WITH task(item_key) AS (VALUES
            ('announcement_published'),('ticketing_verified'),('staff_assigned'),('offline_snapshot_ready'),
            ('gate_device_charged'),('backup_device_ready'),('network_tested'),('guestlist_checked'),
            ('post_show_reconciliation'),('post_show_report'))
        SELECT event.id event_id, event.title event_title, task.item_key task_key,
               COALESCE(checklist.status,'pending') status, event.starts_at
        FROM events event CROSS JOIN task
        LEFT JOIN show_checklist_items checklist
          ON checklist.workspace_id=event.workspace_id AND checklist.event_id=event.id AND checklist.item_key=task.item_key
        WHERE event.workspace_id=$1 AND event.status IN ('published','completed')
          AND event.starts_at BETWEEN $2 - INTERVAL '2 days' AND $2 + INTERVAL '7 days'
          AND COALESCE(checklist.status,'pending') <> 'done'
          AND CASE
              WHEN task.item_key IN ('post_show_reconciliation','post_show_report') THEN $2 >= event.starts_at + INTERVAL '12 hours'
              ELSE $2 >= event.starts_at - INTERVAL '36 hours'
          END
        ORDER BY event.starts_at, task.item_key
        LIMIT 20
    "#).bind(workspace_id.into_uuid()).bind(now).fetch_all(&repo.pool).await.map_err(map_sqlx)?;
    let show_tasks = show_rows
        .into_iter()
        .map(|row| ChiefOfStaffShowTask {
            event_id: EventId::from_uuid(row.event_id),
            event_title: row.event_title,
            task_key: row.task_key,
            status: row.status,
            starts_at: row.starts_at,
        })
        .collect::<Vec<_>>();

    // Awaiting approvals are already included in `stats.awaiting_approval`.
    // Only non-approval deadline items are added here to avoid double counting.
    let deadline_attention = attention_items
        .iter()
        .filter(|item| item.kind != "approval" && item.urgency != "upcoming")
        .count();
    let needs_you = stats
        .awaiting_approval
        .checked_add(i64::try_from(show_tasks.len()).map_err(|_| RepositoryError::Unexpected)?)
        .and_then(|value| value.checked_add(i64::try_from(deadline_attention).ok()?))
        .ok_or(RepositoryError::Unexpected)?;
    let (acted_alone_24h, about_to_act, parked_for_approval) =
        chief_activity(repo, workspace_id).await?;
    let stopped = chief_stopped(repo, workspace_id).await?;
    let moved = chief_movements(repo, workspace_id).await?;
    let objectives_at_risk = chief_objectives(repo, workspace_id, now).await?;
    Ok(AutopilotChiefOfStaff {
        executed_24h: stats.executed_24h,
        failed_24h: stats.failed_24h,
        emitted_24h: stats.emitted_24h,
        executor_confirmed_24h: stats.executor_confirmed_24h,
        executor_failed_24h: stats.executor_failed_24h,
        needs_you,
        estimated_minutes_saved_24h: stats.estimated_minutes_saved_24h,
        measured_improved_7d: stats.measured_improved_7d,
        measured_neutral_7d: stats.measured_neutral_7d,
        measured_worsened_7d: stats.measured_worsened_7d,
        attention_items,
        top_opportunities,
        show_tasks,
        acted_alone_24h,
        about_to_act,
        parked_for_approval,
        stopped,
        moved,
        objectives_at_risk,
    })
}

#[derive(sqlx::FromRow)]
struct ActivityRow {
    bucket: String,
    action_kind: String,
    action_class: String,
    count: i64,
}

#[derive(sqlx::FromRow)]
struct StoppedRow {
    kind: String,
    reason: String,
    count: i64,
    detail: String,
}

#[derive(sqlx::FromRow)]
struct MovementRow {
    subject: String,
    claim: String,
    assessment: String,
    delta_basis_points: Option<i32>,
}

/// What the agent did alone, what it is about to do, and what is parked.
///
/// One query for all three: they are the same rows partitioned by status and
/// by who authorised them, and three round trips would let the sections
/// disagree about a single action that changed state between reads.
///
/// "Alone" means the action was approved by policy rather than by a person.
/// That is the distinction an operator is checking for, and it is stored on the
/// row rather than inferred.
async fn chief_activity(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<
    (
        Vec<crowdrelay_application::autopilot::ChiefOfStaffActivity>,
        Vec<crowdrelay_application::autopilot::ChiefOfStaffActivity>,
        Vec<crowdrelay_application::autopilot::ChiefOfStaffActivity>,
    ),
    RepositoryError,
> {
    use crowdrelay_application::autopilot::ChiefOfStaffActivity;
    let rows = sqlx::query_as::<_, ActivityRow>(
        r#"
        SELECT
            CASE
                WHEN action.status = 'succeeded' THEN 'acted_alone'
                WHEN action.status = 'awaiting_approval' THEN 'parked'
                ELSE 'about_to_act'
            END AS bucket,
            action.action_kind,
            COALESCE(action.action_class, 'first_party_reversible') AS action_class,
            count(*)::bigint AS count
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = $1
          AND (
                (action.status = 'succeeded'
                 AND action.finished_at >= now() - INTERVAL '24 hours'
                 AND action.approved_by = 'policy:bounded_auto')
             OR action.status IN ('queued', 'processing', 'awaiting_approval')
          )
        GROUP BY 1, 2, 3
        ORDER BY count DESC, action.action_kind
        LIMIT 60
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let mut alone = Vec::new();
    let mut about = Vec::new();
    let mut parked = Vec::new();
    for row in rows {
        let entry = ChiefOfStaffActivity {
            action_kind: row.action_kind,
            action_class: row.action_class,
            count: row.count,
        };
        match row.bucket.as_str() {
            "acted_alone" => alone.push(entry),
            "parked" => parked.push(entry),
            _ => about.push(entry),
        }
    }
    Ok((alone, about, parked))
}

/// What the agent stopped, and why.
///
/// Every reason is the stored one, verbatim. Summarising `window_closed` and
/// `no_eligible_recipients` into "skipped" would merge a queue nobody worked
/// with an audience that did not exist, and those have different fixes.
async fn chief_stopped(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<crowdrelay_application::autopilot::ChiefOfStaffStopped>, RepositoryError> {
    use crowdrelay_application::autopilot::ChiefOfStaffStopped;
    let rows = sqlx::query_as::<_, StoppedRow>(
        r#"
        SELECT 'play_step_skipped' AS kind, step.skip_reason AS reason,
               count(*)::bigint AS count,
               'campaign steps that will never be sent' AS detail
        FROM viryaos_play_steps AS step
        WHERE step.workspace_id = $1
          AND step.skip_reason IS NOT NULL
          AND step.settled_at >= now() - INTERVAL '7 days'
        GROUP BY step.skip_reason
        UNION ALL
        SELECT 'action_failed', COALESCE(action.last_error_kind, 'unknown'),
               count(*)::bigint,
               'actions that reached their attempt limit'
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = $1
          AND action.status = 'failed'
          AND action.finished_at >= now() - INTERVAL '7 days'
        GROUP BY action.last_error_kind
        UNION ALL
        SELECT 'play_retired', learning.retired_reason, count(*)::bigint,
               'play kinds the agent will not propose again'
        FROM viryaos_play_learning AS learning
        WHERE learning.workspace_id = $1 AND learning.retired_reason IS NOT NULL
        GROUP BY learning.retired_reason
        UNION ALL
        SELECT 'outcome_insufficient', outcome.evidence_reason, count(*)::bigint,
               'campaigns whose effect could not be measured'
        FROM viryaos_play_outcomes AS outcome
        WHERE outcome.workspace_id = $1
          AND outcome.evidence = 'insufficient'
          AND outcome.finished_at >= now() - INTERVAL '7 days'
        GROUP BY outcome.evidence_reason
        ORDER BY 3 DESC, 1, 2
        LIMIT 40
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .map(|row| ChiefOfStaffStopped {
            kind: row.kind,
            reason: row.reason,
            count: row.count,
            detail: row.detail,
        })
        .collect())
}

/// What moved, with the strength of the claim on every number.
///
/// Only settled, measured outcomes appear. A claim that could not be made is
/// reported under `stopped` instead, because "we could not tell" belongs with
/// the gaps rather than with the results.
async fn chief_movements(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<crowdrelay_application::autopilot::ChiefOfStaffMovement>, RepositoryError> {
    use crowdrelay_application::autopilot::ChiefOfStaffMovement;
    let rows = sqlx::query_as::<_, MovementRow>(
        r#"
        SELECT
            outcome.success_metric_platform || ' ' || outcome.success_metric_key AS subject,
            outcome.claim,
            outcome.effect_assessment AS assessment,
            outcome.delta_basis_points
        FROM viryaos_play_outcomes AS outcome
        WHERE outcome.workspace_id = $1
          AND outcome.evidence = 'measured'
          AND outcome.effect_assessment IS NOT NULL
          AND outcome.finished_at >= now() - INTERVAL '7 days'
        ORDER BY outcome.finished_at DESC
        LIMIT 20
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .map(|row| ChiefOfStaffMovement {
            subject: row.subject,
            claim: row.claim,
            assessment: row.assessment,
            delta_basis_points: row.delta_basis_points,
        })
        .collect())
}

/// Declared targets that warrant an operator's attention without being asked.
///
/// The state is derived from the series by the same rule the objectives read
/// uses, so the brief and the objectives screen can never disagree about
/// whether something is behind.
async fn chief_objectives(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<crowdrelay_application::autopilot::ChiefOfStaffObjective>, RepositoryError> {
    use crowdrelay_application::autopilot::{AutopilotObjectiveRepository, ChiefOfStaffObjective};
    use crowdrelay_domain::objectives::ObjectiveState;
    Ok(repo
        .load_growth_objectives(workspace_id, now)
        .await?
        .into_iter()
        .filter(|objective| objective.state.warrants_attention())
        .map(|objective| ChiefOfStaffObjective {
            platform: objective.platform,
            metric_key: objective.metric_key,
            scope_kind: objective.scope_kind,
            state: objective.state.as_str().to_owned(),
            progress_basis_points: match objective.state {
                ObjectiveState::Behind {
                    progress_basis_points,
                    ..
                }
                | ObjectiveState::Missed {
                    progress_basis_points,
                    ..
                } => progress_basis_points,
                ObjectiveState::Met { .. }
                | ObjectiveState::OnTrack { .. }
                | ObjectiveState::Unmeasurable { .. } => 0,
            },
            shortfall: match objective.state {
                ObjectiveState::Behind { shortfall, .. }
                | ObjectiveState::Missed { shortfall, .. } => shortfall,
                ObjectiveState::Met { .. }
                | ObjectiveState::OnTrack { .. }
                | ObjectiveState::Unmeasurable { .. } => 0,
            },
            deadline: objective.deadline,
        })
        .collect())
}
