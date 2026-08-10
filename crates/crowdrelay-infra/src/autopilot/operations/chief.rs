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
        let mut tx=repo.pool.begin().await.map_err(map_sqlx)?; super::configure_transaction(&mut tx,repo.operation_timeout,repo.lock_timeout).await?;
        let operation_id=Uuid::now_v7(); let disposition=booking_reply_str(command.disposition);
        let details=json!({"target_id":command.target_id,"disposition":disposition,"occurred_at":command.occurred_at});
        if let Some(existing)=super::insert_operator_action(&mut tx,workspace_id,operation_id,"record_autopilot_booking_reply","booking_target",command.target_id.into_uuid(),idempotency_key,request_id,&details).await?{
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

pub(in crate::autopilot) async fn load_chief_of_staff(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<crowdrelay_application::autopilot::AutopilotChiefOfStaff, RepositoryError> {
    use crowdrelay_application::autopilot::{
        AutopilotChiefOfStaff, ChiefOfStaffOpportunity, ChiefOfStaffShowTask,
    };

    let stats = sqlx::query_as::<_, ChiefStatsRow>(r#"
        SELECT
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions
              WHERE workspace_id=$1 AND status='succeeded' AND finished_at >= $2 - INTERVAL '24 hours') executed_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions
              WHERE workspace_id=$1 AND status='failed' AND finished_at >= $2 - INTERVAL '24 hours') failed_24h,
            (SELECT count(*)::bigint FROM viryaos_autopilot_actions
              WHERE workspace_id=$1 AND status='awaiting_approval') awaiting_approval,
            (SELECT COALESCE(sum(CASE
                WHEN action_kind IN ('booking.outreach.request','outreach.request') THEN 10
                WHEN action_kind='content.artifact.request' THEN 8
                WHEN action_kind IN ('fan.lifecycle.message.request','audience.campaign.request') THEN 5
                WHEN action_kind IN ('merch.reorder.request','merch.bundle.request') THEN 5
                WHEN action_kind IN ('ticket.price.change','merch.price.change','promotion.budget_change.request') THEN 3
                WHEN action_kind IN ('show.task.complete','show.task.escalate') THEN 5
                WHEN action_kind LIKE 'experiment.%' THEN 3 ELSE 2 END),0)::bigint
             FROM viryaos_autopilot_actions
             WHERE workspace_id=$1 AND status='succeeded' AND finished_at >= $2 - INTERVAL '24 hours') estimated_minutes_saved_24h,
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
          AND decision.context IN ('booking_opportunity','outreach','promotion_budget','merch_bundle','merch_pricing','ticket_yield')
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

    let needs_you = stats
        .awaiting_approval
        .checked_add(i64::try_from(show_tasks.len()).map_err(|_| RepositoryError::Unexpected)?)
        .ok_or(RepositoryError::Unexpected)?;
    Ok(AutopilotChiefOfStaff {
        executed_24h: stats.executed_24h,
        failed_24h: stats.failed_24h,
        needs_you,
        estimated_minutes_saved_24h: stats.estimated_minutes_saved_24h,
        measured_improved_7d: stats.measured_improved_7d,
        measured_neutral_7d: stats.measured_neutral_7d,
        measured_worsened_7d: stats.measured_worsened_7d,
        top_opportunities,
        show_tasks,
    })
}
