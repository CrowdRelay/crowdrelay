//! Thin human-handoff index for work that genuinely needs a band member.
//!
//! Domain actions and show checklist rows remain authoritative. This adapter
//! only assigns an owner, schedules bounded reminders, and queues provider-
//! confirmed email actions through the existing Autopilot execution plane.

use super::team_routing::{load_team_routing, select_member_index_explained};
use super::*;
use crowdrelay_application::autopilot::BriefingLocale;
use crowdrelay_domain::team_operations::{TeamAssignmentNeed, TeamSkill};
use time::Duration as TimeDuration;

#[derive(Debug, FromRow)]
struct UnassignedApprovalRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_id: Uuid,
    approval_expires_at: Option<OffsetDateTime>,
    payload: serde_json::Value,
}

#[derive(Debug, FromRow)]
struct UnassignedShowTaskRow {
    event_id: Uuid,
    event_title: String,
    task_key: String,
    starts_at: OffsetDateTime,
    due_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct ReminderRow {
    assignment_id: Uuid,
    action_id: Option<Uuid>,
    action_kind: Option<String>,
    context: Option<String>,
    source_kind: String,
    source_ref: Option<String>,
    event_title: Option<String>,
    plan_title: Option<String>,
    release_title: Option<String>,
    plan_scheduled_for: Option<time::Date>,
    plan_items: Option<serde_json::Value>,
    display_name: String,
    normalized_email: String,
    due_at: Option<OffsetDateTime>,
    reminder_count: i32,
    payload: Option<serde_json::Value>,
}

impl PostgresAutopilotRepository {
    /// Reconciles approvals and genuinely manual show-checklist work into one
    /// owner index. An assignment is committed only when the `team.email`
    /// executor capability is live, so production cannot silently create work
    /// without a notification path.
    pub async fn reconcile_team_handoffs(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let crew_locale = crew_locale_in_tx(&mut tx, workspace_id).await;

            // Production-day housekeeping runs even where the roster is
            // empty: shows still project into production days and open
            // plans still settle — a memberless workspace loses routing,
            // not the lifecycle. Settle runs before close so a plan that
            // reaches its verdict this sweep takes its assignment down
            // with it instead of one sweep later.
            super::capture_plans::project_shows_to_production_events(&mut tx, workspace_id, now)
                .await?;
            // The door QR is part of the same housekeeping: a published show
            // mints its campaign here so the scan leg exists even when nobody
            // remembered to create one (the 2026-09-11 finding).
            super::capture_plans::mint_door_campaigns(&mut tx, workspace_id, now).await?;
            super::capture_plans::settle_capture_plans(&mut tx, workspace_id, now.date()).await?;
            close_resolved_assignments(&mut tx, workspace_id, now).await?;
            // Checked without erroring, because an operator who has gated
            // team.email off has not broken anything. Erroring here also rolled
            // back `close_resolved_assignments`, which needs no executor at
            // all — a missing capability was undoing housekeeping that had
            // already succeeded.
            let can_email =
                super::executor_capability_available(&mut tx, workspace_id, "team.email").await?;

            // The briefing keeps its own cadence and its own roster — every
            // active member reads it, including members the routing roster
            // filters out (no team profile, at capacity). It runs before the
            // roster-empty early return for that reason, and it is gated on
            // the same email path every other handoff is, because a briefing
            // nobody can receive is a row nobody reads.
            let mut assigned = if can_email {
                super::daily_briefing::issue_daily_briefings(
                    &mut tx,
                    workspace_id,
                    now,
                    crew_locale,
                )
                .await?
            } else {
                0
            };

            // Decline advisories need neither the roster nor the email
            // path — they are a decision row the queue already carries.
            super::decline_advisories::raise_decline_advisories(&mut tx, workspace_id, now).await?;

            let team = load_team_routing(&mut tx, workspace_id, now).await?;
            if team.is_empty() {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(assigned);
            }

            let approvals = sqlx::query_as::<_, UnassignedApprovalRow>(
                r#"
                SELECT action.id, action.context, action.action_kind,
                       action.subject_id, action.approval_expires_at, action.payload
                FROM viryaos_autopilot_actions action
                LEFT JOIN viryaos_team_assignments assignment
                  ON assignment.workspace_id=action.workspace_id
                 AND assignment.action_id=action.id
                WHERE action.workspace_id=$1
                  AND action.status='awaiting_approval'
                  AND assignment.id IS NULL
                  AND (action.approval_expires_at IS NULL OR action.approval_expires_at>$2)
                ORDER BY action.approval_expires_at NULLS LAST, action.created_at, action.id
                FOR UPDATE OF action SKIP LOCKED
                LIMIT 32
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            // Nothing parked means the gated capability is costing nothing, so
            // it stays silent. Work parked behind it is worth exactly one line
            // per cycle, because that is a thing an operator can act on.
            if !can_email {
                if !approvals.is_empty() {
                    tracing::warn!(
                        workspace_id = %workspace_id.into_uuid(),
                        capability = "team.email",
                        parked = approvals.len(),
                        "team handoff reminders are parked: no executor advertises this capability"
                    );
                }
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(assigned);
            }

            let show_tasks = sqlx::query_as::<_, UnassignedShowTaskRow>(
                r#"
                WITH task(item_key) AS (VALUES
                    ('staff_assigned'),('offline_snapshot_ready'),('gate_device_charged'),
                    ('backup_device_ready'),('network_tested'),('guestlist_checked'),
                    ('qr_from_stage'),('post_show_reconciliation')
                )
                SELECT event.id event_id, event.title event_title, task.item_key task_key,
                       event.starts_at,
                       CASE WHEN task.item_key = 'post_show_reconciliation'
                            THEN event.starts_at + INTERVAL '36 hours'
                            ELSE event.starts_at - INTERVAL '2 hours' END due_at
                FROM events event CROSS JOIN task
                LEFT JOIN show_checklist_items checklist
                  ON checklist.workspace_id=event.workspace_id
                 AND checklist.event_id=event.id AND checklist.item_key=task.item_key
                LEFT JOIN viryaos_team_assignments assignment
                  ON assignment.workspace_id=event.workspace_id
                 AND assignment.source_kind='show_task'
                 AND assignment.source_id=event.id
                 AND assignment.source_ref=task.item_key
                WHERE event.workspace_id=$1
                  AND event.status IN ('published','completed')
                  AND COALESCE(checklist.status,'pending') <> 'done'
                  AND assignment.id IS NULL
                  AND event.starts_at BETWEEN $2 - INTERVAL '2 days' AND $2 + INTERVAL '7 days'
                  -- Nobody can announce a QR that was never minted.
                  AND (task.item_key <> 'qr_from_stage'
                       OR EXISTS (
                           SELECT 1 FROM concert_qr_campaigns campaign
                           WHERE campaign.workspace_id=event.workspace_id
                             AND campaign.event_id=event.id
                             AND campaign.active
                             AND campaign.revoked_at IS NULL
                       ))
                  AND CASE
                      WHEN task.item_key = 'post_show_reconciliation'
                          THEN $2 >= event.starts_at + INTERVAL '6 hours'
                      ELSE $2 >= event.starts_at - INTERVAL '72 hours'
                  END
                ORDER BY due_at, event.id, task.item_key
                FOR UPDATE OF event SKIP LOCKED
                LIMIT 32
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            let mut mutable_team = team;
            for action in approvals {
                let need = assignment_need(&action.context, &action.action_kind);
                let member_index = match select_member_index_explained(&mutable_team, need) {
                    Ok(index) => index,
                    Err(refusal) => {
                        // §4i-6: the ask waits — and says so. The action stays
                        // awaiting_approval; if it expires unassigned the
                        // briefing counts it dropped.
                        super::team_release::record_ask_refusal(
                            &mut tx,
                            workspace_id,
                            "autopilot_action",
                            action.subject_id,
                            None,
                            Some(action.id),
                            action.approval_expires_at,
                            need,
                            refusal,
                        )
                        .await?;
                        continue;
                    }
                };
                let member = mutable_team
                    .get_mut(member_index)
                    .ok_or(RepositoryError::Unexpected)?;
                let assignment_id = Uuid::now_v7();
                let inserted = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO viryaos_team_assignments (
                        id, workspace_id, action_id, source_kind, source_id, source_ref,
                        assignee_member_id, required_skill, due_at, next_reminder_at
                    ) VALUES ($1,$2,$3,'autopilot_action',$4,NULL,$5,$6,$7,$8)
                    ON CONFLICT (workspace_id, action_id) DO NOTHING
                    RETURNING id
                    "#,
                )
                .bind(assignment_id)
                .bind(workspace_id.into_uuid())
                .bind(action.id)
                .bind(action.subject_id)
                .bind(member.member_id)
                .bind(need.primary_skill.as_str())
                .bind(action.approval_expires_at)
                .bind(first_reminder_at(now, action.approval_expires_at))
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.is_none() {
                    continue;
                }

                queue_team_email_action(
                    &mut tx,
                    workspace_id,
                    assignment_id,
                    &action.context,
                    &member.normalized_email,
                    &member.display_name,
                    friendly_action_title(&action.action_kind, crew_locale),
                    enriched_task_detail(
                        &action.payload,
                        action.approval_expires_at,
                        None,
                        crew_locale,
                    ),
                    action.approval_expires_at,
                    0,
                    Some(action.id),
                    now,
                )
                .await?;
                member.open_assignments = member.open_assignments.saturating_add(1);
                member.recent_assignments = member.recent_assignments.saturating_add(1);
                member.asks_last_7d = member.asks_last_7d.saturating_add(1);
                assigned = assigned.saturating_add(1);
            }

            assigned = assigned.saturating_add(
                super::team_release::issue_release_making_of_asks(
                    &mut tx,
                    workspace_id,
                    now,
                    &mut mutable_team,
                    crew_locale,
                )
                .await?,
            );

            for task in show_tasks {
                let need = assignment_need("show_operations", &task.task_key);
                let member_index = match select_member_index_explained(&mutable_team, need) {
                    Ok(index) => index,
                    Err(refusal) => {
                        super::team_release::record_ask_refusal(
                            &mut tx,
                            workspace_id,
                            "show_task",
                            task.event_id,
                            Some(&task.task_key),
                            None,
                            Some(task.due_at),
                            need,
                            refusal,
                        )
                        .await?;
                        continue;
                    }
                };
                let member = mutable_team
                    .get_mut(member_index)
                    .ok_or(RepositoryError::Unexpected)?;
                let assignment_id = Uuid::now_v7();
                let inserted = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO viryaos_team_assignments (
                        id, workspace_id, action_id, source_kind, source_id, source_ref,
                        assignee_member_id, required_skill, due_at, next_reminder_at
                    ) VALUES ($1,$2,NULL,'show_task',$3,$4,$5,$6,$7,$8)
                    ON CONFLICT DO NOTHING
                    RETURNING id
                    "#,
                )
                .bind(assignment_id)
                .bind(workspace_id.into_uuid())
                .bind(task.event_id)
                .bind(&task.task_key)
                .bind(member.member_id)
                .bind(need.primary_skill.as_str())
                .bind(task.due_at)
                .bind(first_reminder_at(now, Some(task.due_at)))
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.is_none() {
                    continue;
                }

                queue_team_email_action(
                    &mut tx,
                    workspace_id,
                    assignment_id,
                    "show_operations",
                    &member.normalized_email,
                    &member.display_name,
                    friendly_show_task_title(&task.task_key, crew_locale),
                    show_task_detail(&task, crew_locale),
                    Some(task.due_at),
                    0,
                    None,
                    now,
                )
                .await?;
                member.open_assignments = member.open_assignments.saturating_add(1);
                member.recent_assignments = member.recent_assignments.saturating_add(1);
                member.asks_last_7d = member.asks_last_7d.saturating_add(1);
                assigned = assigned.saturating_add(1);
            }

            assigned = assigned.saturating_add(
                super::capture_plans::issue_capture_plans(
                    &mut tx,
                    workspace_id,
                    now,
                    &mut mutable_team,
                    crew_locale,
                )
                .await?,
            );

            tx.commit().await.map_err(map_sqlx)?;
            Ok(assigned)
        })
        .await
    }

    /// Queues friendly reminders only after their durable schedule becomes due.
    /// The actual email is still an Autopilot action and is only complete after
    /// a provider-confirmed Gmail receipt.
    pub async fn dispatch_team_handoff_reminders(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let crew_locale = crew_locale_in_tx(&mut tx, workspace_id).await;

            // Quiet hours on the tenant's own clock. A reminder due at 03:00
            // is not more urgent for arriving then — nobody reads it, and a
            // mailbox trained to expect overnight mail learns to ignore the
            // morning ones. The rows stay due rather than being rescheduled,
            // so the first sweep after the window ends sends them unchanged.
            let crew_timezone = crew_timezone_in_tx(&mut tx, workspace_id).await?;
            let local_hour: i32 = sqlx::query_scalar(
                "SELECT EXTRACT(HOUR FROM $1::timestamptz AT TIME ZONE $2)::int",
            )
            .bind(now)
            .bind(&crew_timezone)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            // The window wraps midnight, so the check is "not within the
            // waking span" — [08:00, 21:00) local is when crew mail may go.
            if !(CREW_QUIET_END_LOCAL_HOUR..CREW_QUIET_START_LOCAL_HOUR).contains(&local_hour) {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            // Same reasoning as the handoff sweep: a gated capability is an
            // operator's decision, not a fault, and reporting it as a failed
            // cycle every sixty seconds trains everyone to ignore the log.
            let can_email =
                super::executor_capability_available(&mut tx, workspace_id, "team.email").await?;
            let rows = sqlx::query_as::<_, ReminderRow>(
                r#"
                SELECT assignment.id assignment_id,
                       action.id action_id,
                       action.action_kind, action.context, assignment.source_kind,
                       assignment.source_ref, event.title event_title,
                       release.title release_title,
                       day.title plan_title, day.scheduled_for plan_scheduled_for,
                       plan.items plan_items,
                       member.display_name, member.normalized_email,
                       assignment.due_at, assignment.reminder_count,
                       action.payload
                FROM viryaos_team_assignments assignment
                JOIN workspace_members member
                  ON member.workspace_id=assignment.workspace_id
                 AND member.id=assignment.assignee_member_id
                LEFT JOIN viryaos_autopilot_actions action
                  ON action.workspace_id=assignment.workspace_id
                 AND action.id=assignment.action_id
                LEFT JOIN events event
                  ON assignment.source_kind='show_task'
                 AND event.workspace_id=assignment.workspace_id
                 AND event.id=assignment.source_id
                LEFT JOIN viryaos_capture_plans plan
                  ON assignment.source_kind='capture_plan'
                 AND plan.workspace_id=assignment.workspace_id
                 AND plan.id=assignment.source_id
                LEFT JOIN viryaos_release_plans release
                  ON assignment.source_kind='release_making_of'
                 AND release.workspace_id=assignment.workspace_id
                 AND release.id=assignment.source_id
                LEFT JOIN viryaos_production_events day
                  ON day.workspace_id=assignment.workspace_id
                 AND day.id=plan.production_event_id
                WHERE assignment.workspace_id=$1
                  AND assignment.status='open'
                  AND assignment.next_reminder_at IS NOT NULL
                  AND assignment.next_reminder_at <= $2
                  AND (assignment.due_at IS NULL OR assignment.due_at>$2)
                  AND (assignment.action_id IS NULL OR action.status='awaiting_approval')
                ORDER BY assignment.next_reminder_at, assignment.id
                FOR UPDATE OF assignment SKIP LOCKED
                LIMIT 24
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            if !can_email {
                if !rows.is_empty() {
                    tracing::warn!(
                        workspace_id = %workspace_id.into_uuid(),
                        capability = "team.email",
                        due = rows.len(),
                        "team reminders are due but no executor advertises this capability"
                    );
                }
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            let mut queued = 0_u32;
            // One email per person per sweep, not one per assignment.
            //
            // The sweep reads up to 24 due assignments at a time and used to
            // queue an email for every one of them, all stamped the same
            // minute. With four content approvals open, the crew member got
            // four emails with identical subjects — the subject is composed
            // from the action kind, so tasks of one kind are indistinguishable
            // — and no way to tell four tasks from one task sent four times.
            //
            // Grouping by recipient keeps every fact and removes three
            // interruptions: the most urgent assignment supplies the subject
            // and body, and the rest are named underneath it. Each of them
            // still advances its own reminder bookkeeping, because each of them
            // was in fact reminded about.
            let mut by_recipient: Vec<(String, Vec<ReminderRow>)> = Vec::new();
            for row in rows {
                match by_recipient
                    .iter_mut()
                    .find(|(email, _)| *email == row.normalized_email)
                {
                    Some((_, group)) => group.push(row),
                    None => by_recipient.push((row.normalized_email.clone(), vec![row])),
                }
            }

            for (_, group) in by_recipient {
                let Some(primary) = group.first() else {
                    continue;
                };
                let reminder_number = primary.reminder_count.saturating_add(1);
                let title = reminder_title(primary, crew_locale);
                let others: Vec<String> = group
                    .iter()
                    .skip(1)
                    .map(|row| reminder_title(row, crew_locale))
                    .collect();
                let detail = format!(
                    "{}{}",
                    reminder_detail(primary, crew_locale),
                    digest_tail(&others, crew_locale)
                );
                queue_team_email_action(
                    &mut tx,
                    workspace_id,
                    primary.assignment_id,
                    primary.context.as_deref().unwrap_or("show_operations"),
                    &primary.normalized_email,
                    &primary.display_name,
                    title,
                    detail,
                    primary.due_at,
                    // Clamped to the ladder's length so the frame can tell the
                    // recipient which reminder is the last one. It was clamped
                    // at 12 when the cadence had no ceiling.
                    u8::try_from(reminder_number.clamp(1, MAX_REMINDERS_PER_ASSIGNMENT))
                        .unwrap_or(MAX_REMINDERS_PER_ASSIGNMENT as u8),
                    primary.action_id,
                    now,
                )
                .await?;
                // Every assignment in the digest, not only the one that gave
                // the email its subject: each was named in the body, so each
                // has been reminded about and each owes its own next rung.
                for row in &group {
                    let row_next = next_reminder_at(now, row.due_at, row.reminder_count);
                    sqlx::query(
                        r#"UPDATE viryaos_team_assignments
                           SET last_reminded_at=$3, next_reminder_at=$4,
                               reminder_count=reminder_count+1,
                               first_overdue_reminder_at = COALESCE(first_overdue_reminder_at, CASE WHEN due_at IS NOT NULL AND $3 > due_at THEN $3 END)
                           WHERE workspace_id=$1 AND id=$2 AND status='open'"#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(row.assignment_id)
                    .bind(now)
                    .bind(row_next)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_sqlx)?;
                }
                queued = queued.saturating_add(1);
            }
            tx.commit().await.map_err(map_sqlx)?;
            Ok(queued)
        })
        .await
    }
}

async fn close_resolved_assignments(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status = CASE WHEN action.status IN ('queued','processing','succeeded') THEN 'done' ELSE 'cancelled' END,
               completed_at = CASE WHEN action.status IN ('queued','processing','succeeded') THEN $2 ELSE NULL END,
               next_reminder_at = NULL
           FROM viryaos_autopilot_actions action
           WHERE assignment.workspace_id=$1 AND assignment.workspace_id=action.workspace_id
             AND assignment.action_id=action.id AND assignment.status='open'
             AND action.status <> 'awaiting_approval'"#,
    )
    .bind(workspace_id.into_uuid()).bind(now)
    .execute(&mut **tx).await.map_err(map_sqlx)?;

    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status='done', completed_at=$2, next_reminder_at=NULL
           FROM show_checklist_items checklist
           WHERE assignment.workspace_id=$1 AND assignment.status='open'
             AND assignment.source_kind='show_task'
             AND checklist.workspace_id=assignment.workspace_id
             AND checklist.event_id=assignment.source_id
             AND checklist.item_key=assignment.source_ref
             AND checklist.status='done'"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status='cancelled', completed_at=NULL, next_reminder_at=NULL
           FROM events event
           WHERE assignment.workspace_id=$1 AND assignment.status='open'
             AND assignment.source_kind='show_task'
             AND event.workspace_id=assignment.workspace_id
             AND event.id=assignment.source_id
             AND event.status NOT IN ('published','completed')"#,
    )
    .bind(workspace_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // A settled capture plan settles its assignment the same way a done
    // checklist item does: `done` is work finished, `abandoned` is a day
    // that passed — chasing either is noise.
    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status = CASE WHEN plan.status='done' THEN 'done' ELSE 'cancelled' END,
               completed_at = CASE WHEN plan.status='done' THEN $2 ELSE NULL END,
               next_reminder_at = NULL
           FROM viryaos_capture_plans plan
           WHERE assignment.workspace_id=$1 AND assignment.status='open'
             AND assignment.source_kind='capture_plan'
             AND plan.workspace_id=assignment.workspace_id
             AND plan.id=assignment.source_id
             AND plan.status IN ('done','abandoned')"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    super::team_release::close_release_making_of_assignments(tx, workspace_id, now).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn queue_team_email_action(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    assignment_id: Uuid,
    context: &str,
    recipient_email: &str,
    recipient_name: &str,
    task_title: String,
    task_detail: String,
    due_at: Option<OffsetDateTime>,
    reminder_number: u8,
    source_action_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let suffix = if reminder_number == 0 {
        "initial".to_owned()
    } else {
        format!("reminder-{reminder_number}")
    };
    let decision_key = format!("team-email-decision:{assignment_id}:{suffix}");
    let idempotency_key = format!("team-email:{assignment_id}:{suffix}");
    // System-initiated action: start a root trace so the team email lifecycle
    // is observable in the trace timeline even though no evaluator decision
    // preceded it.
    let trace = TraceContext::root(workspace_id);
    let trace_id = trace.trace_id().into_uuid();
    let decision_id =
        if let Some(id) = sqlx::query_scalar::<_, Uuid>(
            r#"INSERT INTO viryaos_autopilot_decisions (
               id, workspace_id, decision_key, context, subject_kind, subject_id,
               decision_kind, confidence_basis_points, disposition, reason,
               input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
           ) VALUES ($1,$2,$3,$4,'team_assignment',$5,'team.email.route',10000,
                     'auto_execute','Durable human handoff notification',
                     $6,$7,$8,$9,$10)
           ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id"#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(&decision_key)
        .bind(context)
        .bind(assignment_id)
        .bind(json!({"assignment_id":assignment_id,"reminder_number":reminder_number}))
        .bind(json!({"provider_completion_required":true,"capability":"team.email"}))
        .bind(json!({"send_friendly_email":true}))
        .bind(now)
        .bind(trace_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?
        {
            id
        } else {
            sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_autopilot_decisions WHERE workspace_id=$1 AND decision_key=$2",
        ).bind(workspace_id.into_uuid()).bind(&decision_key)
        .fetch_one(&mut **tx).await.map_err(map_sqlx)?
        };

    // Crew mail keeps the tenant's night quiet — `available_at` holds the
    // notice to the window's end instead of a 03:00 inbox. The row exists,
    // audits and says exactly what was composed; it is simply not claimable
    // until morning. The dispatch sweep's own gate covers the reminder half;
    // this covers every first notice, whichever producer queued it.
    let crew_timezone = crew_timezone_in_tx(tx, workspace_id).await?;
    let available_at: OffsetDateTime = sqlx::query_scalar(
        r#"
        SELECT (
            CASE
                WHEN EXTRACT(HOUR FROM $1::timestamptz AT TIME ZONE $2)::int >= $3
                    THEN date_trunc('day', $1::timestamptz AT TIME ZONE $2)
                         + make_interval(days => 1, hours => $4)
                WHEN EXTRACT(HOUR FROM $1::timestamptz AT TIME ZONE $2)::int < $4
                    THEN date_trunc('day', $1::timestamptz AT TIME ZONE $2)
                         + make_interval(hours => $4)
                ELSE $1::timestamptz AT TIME ZONE $2
            END
        ) AT TIME ZONE $2
        "#,
    )
    .bind(now)
    .bind(&crew_timezone)
    .bind(CREW_QUIET_START_LOCAL_HOUR)
    .bind(CREW_QUIET_END_LOCAL_HOUR)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let payload = serde_json::to_value(AutopilotActionPayload::SendTeamAssignmentEmail {
        assignment_id,
        recipient_email: recipient_email.to_owned(),
        recipient_name: recipient_name.to_owned(),
        task_title,
        task_detail,
        due_at,
        action_url_path: match source_action_id {
            Some(id) => format!("/staff/?tab=overview#needs-you&action={id}"),
            None => "/staff/?tab=overview#needs-you".to_owned(),
        },
        reminder_number,
    })
    .map_err(|_| RepositoryError::Unexpected)?;

    let action_id = Uuid::now_v7();
    let action_trace =
        TraceContext::for_action(workspace_id, trace.trace_id(), action_id, Some(decision_id));
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions (
               id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
               idempotency_key, payload, status, approved_at, approved_by, available_at,
               trace_id, causation_id
           ) VALUES ($1,$2,$3,$4,'team.assignment.email','team_assignment',$5,$6,$7,
                     'queued',$8,'system:team-router',$8,$9,$10)
           ON CONFLICT (workspace_id, idempotency_key) DO NOTHING"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(context)
    .bind(assignment_id)
    .bind(idempotency_key)
    .bind(payload)
    .bind(available_at)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|c| c.into_uuid()))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(super) fn assignment_need(context: &str, action_kind: &str) -> TeamAssignmentNeed {
    let action = action_kind.to_ascii_lowercase();
    if context == "content_supply" || action.contains("content") || action.contains("social") {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: Some(TeamSkill::Visual),
            allow_generalist: true,
        }
    } else if context == "funding" || action.contains("funding") {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::PolishCopy,
            secondary_skill: Some(TeamSkill::Operations),
            allow_generalist: true,
        }
    } else if matches!(
        context,
        "live_opportunity" | "booking_opportunity" | "beacon"
    ) || action.contains("booking")
        || action.contains("opportunity")
        || action.contains("beacon")
    {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Booking,
            secondary_skill: Some(TeamSkill::People),
            allow_generalist: true,
        }
    } else if context == "show_growth" {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Operations,
            secondary_skill: Some(TeamSkill::Social),
            allow_generalist: true,
        }
    } else if context == "outreach" || action.contains("outreach") {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::EnglishCopy,
            secondary_skill: Some(TeamSkill::People),
            allow_generalist: true,
        }
    } else if action.contains("capture") {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Video,
            secondary_skill: Some(TeamSkill::Photography),
            allow_generalist: true,
        }
    } else if context == "show_operations" || action.contains("show") {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Operations,
            secondary_skill: Some(TeamSkill::People),
            allow_generalist: true,
        }
    } else {
        TeamAssignmentNeed {
            primary_skill: TeamSkill::Approval,
            secondary_skill: Some(TeamSkill::Operations),
            allow_generalist: true,
        }
    }
}

pub(super) fn friendly_action_title(action_kind: &str, locale: BriefingLocale) -> String {
    let title = match (action_kind, locale) {
        ("opportunity.live.apply", BriefingLocale::Pl) => {
            "Sprawdź i zatwierdź zgłoszenie koncertowe"
        }
        ("opportunity.live.apply", BriefingLocale::En) => "Check and approve the show application",
        ("funding.application.submit", BriefingLocale::Pl) => {
            "Sprawdź i zatwierdź wysłanie wniosku"
        }
        ("funding.application.submit", BriefingLocale::En) => {
            "Check and approve the funding application"
        }
        ("promotion.budget_change.request", BriefingLocale::Pl) => {
            "Sprawdź zmianę budżetu promocji"
        }
        ("promotion.budget_change.request", BriefingLocale::En) => {
            "Check the promotion budget change"
        }
        ("agent.content.request", BriefingLocale::Pl) => "Zatwierdź treść od agenta",
        ("agent.content.request", BriefingLocale::En) => "Approve the agent's content",
        ("agent.run.request", BriefingLocale::Pl) => "Zatwierdź uruchomienie agenta",
        ("agent.run.request", BriefingLocale::En) => "Approve the agent run",
        ("audience.campaign.request", BriefingLocale::Pl) => "Zatwierdź kampanię do fanów",
        ("audience.campaign.request", BriefingLocale::En) => "Approve the fan campaign",
        ("beacon.discovery.request", BriefingLocale::Pl) => "Zatwierdź wyszukiwanie Beaconów",
        ("beacon.discovery.request", BriefingLocale::En) => "Approve the Beacon search",
        ("beacon.outreach.request", BriefingLocale::Pl) => "Zatwierdź outreach do Beacona",
        ("beacon.outreach.request", BriefingLocale::En) => "Approve the Beacon outreach",
        ("community.engage.request", BriefingLocale::Pl) => "Zatwierdź publikację w społeczności",
        ("community.engage.request", BriefingLocale::En) => "Approve the community post",
        ("community.decline.advisory", BriefingLocale::Pl) => {
            "Przeczytaj dowody: społeczność angażuje, ale nie daje fanów"
        }
        ("community.decline.advisory", BriefingLocale::En) => {
            "Read the evidence: the community engages but produces zero fans"
        }
        ("content.arc.raise", BriefingLocale::Pl) => "Zatwierdź łuk treści",
        ("content.arc.raise", BriefingLocale::En) => "Approve the content arc",
        ("content.artifact.request", BriefingLocale::Pl) => "Zatwierdź artefakt treści",
        ("content.artifact.request", BriefingLocale::En) => "Approve the content artifact",
        ("content.suggestion.raise", BriefingLocale::Pl) => "Zatwierdź sugestię treści",
        ("content.suggestion.raise", BriefingLocale::En) => "Approve the content suggestion",
        ("fan.lifecycle.message.request", BriefingLocale::Pl) => "Zatwierdź wiadomość do fana",
        ("fan.lifecycle.message.request", BriefingLocale::En) => "Approve the fan message",
        ("growth.opportunity.raise", BriefingLocale::Pl) => "Zatwierdź szansę wzrostu",
        ("growth.opportunity.raise", BriefingLocale::En) => "Approve the growth opportunity",
        ("outreach.discovery.request", BriefingLocale::Pl) => {
            "Zatwierdź wyszukiwanie celów outreach"
        }
        ("outreach.discovery.request", BriefingLocale::En) => "Approve the outreach search",
        ("outreach.target.request" | "outreach.request", BriefingLocale::Pl) => {
            "Zatwierdź cel outreach"
        }
        ("outreach.target.request" | "outreach.request", BriefingLocale::En) => {
            "Approve the outreach target"
        }
        ("show.growth.request", BriefingLocale::Pl) => "Zatwierdź działanie wzrostu koncertu",
        ("show.growth.request", BriefingLocale::En) => "Approve the show growth action",
        ("show.task.complete", BriefingLocale::Pl) => "Domknij zadanie koncertowe",
        ("show.task.complete", BriefingLocale::En) => "Close the show task",
        ("show.task.escalate", BriefingLocale::Pl) => "Eskaluj zadanie koncertowe",
        ("show.task.escalate", BriefingLocale::En) => "Escalate the show task",
        ("signal.push.request", BriefingLocale::Pl) => "Zatwierdź push Signal",
        ("signal.push.request", BriefingLocale::En) => "Approve the Signal push",
        ("latarnik.invite.request", BriefingLocale::Pl) => "Zatwierdź zaproszenie do Latarnika",
        ("latarnik.invite.request", BriefingLocale::En) => "Approve the Latarnik invitation",
        (other, _) => return format!("VIRYA OS — {}", other.replace(['.', '_'], " ")),
    };
    title.to_owned()
}

fn show_task_detail(task: &UnassignedShowTaskRow, locale: BriefingLocale) -> String {
    let starts_at = format!(
        "{} {:02}:{:02} UTC",
        task.starts_at.date(),
        task.starts_at.hour(),
        task.starts_at.minute()
    );
    match locale {
        BriefingLocale::Pl => format!(
            "Koncert: {}. Termin koncertu: {}.",
            task.event_title, starts_at
        ),
        BriefingLocale::En => format!("Show: {}. Showtime: {}.", task.event_title, starts_at),
    }
}

fn friendly_show_task_title(task_key: &str, locale: BriefingLocale) -> String {
    let title = match (task_key, locale) {
        ("staff_assigned", BriefingLocale::Pl) => "Potwierdź obsadę koncertu",
        ("staff_assigned", BriefingLocale::En) => "Confirm the show staffing",
        ("offline_snapshot_ready", BriefingLocale::Pl) => "Przygotuj offline snapshot na koncert",
        ("offline_snapshot_ready", BriefingLocale::En) => {
            "Prepare the offline snapshot for the show"
        }
        ("gate_device_charged", BriefingLocale::Pl) => "Naładuj urządzenie wejściowe",
        ("gate_device_charged", BriefingLocale::En) => "Charge the gate device",
        ("backup_device_ready", BriefingLocale::Pl) => "Przygotuj urządzenie zapasowe",
        ("backup_device_ready", BriefingLocale::En) => "Prepare the backup device",
        ("network_tested", BriefingLocale::Pl) => "Przetestuj internet na wejściu",
        ("network_tested", BriefingLocale::En) => "Test the gate internet",
        ("guestlist_checked", BriefingLocale::Pl) => "Sprawdź guestlistę",
        ("guestlist_checked", BriefingLocale::En) => "Check the guest list",
        ("qr_from_stage", BriefingLocale::Pl) => "Zapowiedz kod QR ze sceny",
        ("qr_from_stage", BriefingLocale::En) => "Announce the QR code from stage",
        ("post_show_reconciliation", BriefingLocale::Pl) => "Zrób rozliczenie po koncercie",
        ("post_show_reconciliation", BriefingLocale::En) => "Do the post-show reconciliation",
        ("post_show_report", BriefingLocale::Pl) => "Domknij raport po koncercie",
        ("post_show_report", BriefingLocale::En) => "Close out the post-show report",
        (other, BriefingLocale::Pl) => {
            return format!("Domknij zadanie koncertowe: {}", other.replace('_', " "));
        }
        (other, BriefingLocale::En) => {
            return format!("Close the show task: {}", other.replace('_', " "));
        }
    };
    title.to_owned()
}

/// Builds an enriched `task_detail` string from the action payload's briefing.
/// This is what goes into the team assignment email body — summary, why it
/// matters, steps, and the content being approved. Truncated to 1800 chars
/// to fit the n8n workflow's slice limit.
/// The crew's language for this workspace, read inside the caller's
/// transaction so the briefing and the frame cannot disagree mid-sweep.
///
/// A missing or unreadable row is the source language rather than an error: a
/// task email that arrives in English is usable, one that fails to send is not.
pub(super) async fn crew_locale_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
) -> BriefingLocale {
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_locale'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .ok()
    .flatten();
    stored.map_or(BriefingLocale::default(), |tag| {
        BriefingLocale::from_tag(&tag)
    })
}

/// The tenant's own clock (`crew_timezone`), read inside the caller's
/// transaction so the briefing's day boundary and a reminder's quiet window
/// cannot disagree mid-sweep.
///
/// Same rules as `crew_locale`: trimmed, IANA-validated, and UTC when the row
/// is absent or unreadable — the shipped default `tenant_settings` documents.
/// The error propagates like the briefing's own read: a database that cannot
/// answer this cannot answer the next query either.
pub(super) async fn crew_timezone_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
) -> Result<String, RepositoryError> {
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_timezone'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(stored
        .map(|value| value.trim().to_owned())
        .filter(|value| crate::regional::is_known_iana_timezone(value))
        .unwrap_or_else(|| "UTC".to_owned()))
}

pub(super) fn first_reminder_at(
    now: OffsetDateTime,
    due: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    let normal = now + TimeDuration::hours(24);
    due.map_or(Some(normal), |due_at| {
        let urgent = due_at - TimeDuration::hours(6);
        (normal < urgent)
            .then_some(normal)
            .or_else(|| (urgent > now).then_some(urgent))
    })
}

/// Reminders one assignment may ever produce, after the first email.
///
/// Three, and then the machine stops asking. A fourth reminder has never once
/// been the thing that made somebody do a task: if three did not move it, the
/// task is mis-assigned, mis-scoped or not actually wanted, and that is a
/// staffing question for the operator rather than another line in a crew
/// member's inbox. Silence after three is information too — the follow-through
/// score already records how many reminders each completion needed.
const MAX_REMINDERS_PER_ASSIGNMENT: i32 = 3;

/// Crew mail's quiet window, in the tenant's local hour — 21:00 to the
/// briefing's own 08:00, so the first mail of the day lands with the morning
/// briefing rather than ahead of it. Overnight is not an emergency channel:
/// a task whose 6-hour rung falls at 03:00 waits three hours and loses
/// nothing, because nobody should have been reading mail at 03:00.
const CREW_QUIET_START_LOCAL_HOUR: i32 = 21;
const CREW_QUIET_END_LOCAL_HOUR: i32 = 8;

/// Hours before the due time at which each reminder lands.
///
/// Anchored to the deadline rather than to the previous send, which is the
/// whole fix. The old rule spaced reminders 24h, then 12h, then every 6 hours
/// from *now* until the due time, so a task due in a week produced somewhere
/// near thirty emails and every one of them said the same thing. Anchoring to
/// the deadline means each reminder arrives when it changes what the recipient
/// would do: two days out is still plannable, a day out is today's problem,
/// six hours out is now or never.
const REMINDER_LADDER_HOURS: [i64; MAX_REMINDERS_PER_ASSIGNMENT as usize] = [48, 24, 6];

/// When the next reminder for this assignment is due, if there should be one.
///
/// `None` means never again: the ladder is spent, the deadline is too close for
/// another rung to fall before it, or the assignment has no due time at all —
/// in which case the single nudge `first_reminder_at` scheduled is the whole of
/// the chasing, because there is no deadline to count down to.
fn next_reminder_at(
    now: OffsetDateTime,
    due: Option<OffsetDateTime>,
    reminder_count: i32,
) -> Option<OffsetDateTime> {
    let due_at = due?;
    if reminder_count >= MAX_REMINDERS_PER_ASSIGNMENT {
        return None;
    }
    // Start at the rung matching how many reminders this assignment has had,
    // then walk down: a task assigned 30 hours before it is due skips the
    // 48-hour rung, because that moment is already behind us.
    let start = usize::try_from(reminder_count.max(0)).unwrap_or(0);
    REMINDER_LADDER_HOURS
        .get(start..)?
        .iter()
        .map(|hours| due_at - TimeDuration::hours(*hours))
        .find(|candidate| *candidate > now)
}

include!("team/task_copy.rs");
include!("team/reminder_copy.rs");

include!("team/tests.rs");
