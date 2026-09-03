//! Thin human-handoff index for work that genuinely needs a band member.
//!
//! Domain actions and show checklist rows remain authoritative. This adapter
//! only assigns an owner, schedules bounded reminders, and queues provider-
//! confirmed email actions through the existing Autopilot execution plane.

use super::*;
use crowdrelay_domain::{
    WorkspaceMemberId,
    team_operations::{
        TeamAssignmentNeed, TeamMemberRoutingSnapshot, TeamSkill, select_team_assignee,
    },
};
use time::Duration as TimeDuration;

#[derive(Debug, FromRow)]
pub(in crate::autopilot) struct TeamRoutingRow {
    pub member_id: Uuid,
    pub member_key: String,
    pub display_name: String,
    pub normalized_email: String,
    pub active: bool,
    pub skills: Vec<String>,
    pub capacity_basis_points: i32,
    pub open_assignments: i64,
    pub recent_assignments: i64,
    pub follow_through_basis_points: i32,
    /// Skills this member has settled history for, paired 1:1 with
    /// `skill_follow_through`. Two arrays rather than a map because SQLx
    /// decodes `text[]` and `int[]` directly.
    pub skill_follow_through_skills: Vec<String>,
    pub skill_follow_through: Vec<i32>,
}

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
    action_kind: Option<String>,
    context: Option<String>,
    source_kind: String,
    source_ref: Option<String>,
    event_title: Option<String>,
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

            close_resolved_assignments(&mut tx, workspace_id, now).await?;
            let team = load_team_routing(&mut tx, workspace_id, now).await?;
            if team.is_empty() {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }
            // Checked without erroring, because an operator who has gated
            // team.email off has not broken anything. Erroring here also rolled
            // back `close_resolved_assignments`, which needs no executor at
            // all — a missing capability was undoing housekeeping that had
            // already succeeded.
            let can_email =
                super::executor_capability_available(&mut tx, workspace_id, "team.email").await?;

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
                return Ok(0);
            }

            let show_tasks = sqlx::query_as::<_, UnassignedShowTaskRow>(
                r#"
                WITH task(item_key) AS (VALUES
                    ('staff_assigned'),('offline_snapshot_ready'),('gate_device_charged'),
                    ('backup_device_ready'),('network_tested'),('guestlist_checked'),
                    ('post_show_reconciliation'),('post_show_report')
                )
                SELECT event.id event_id, event.title event_title, task.item_key task_key,
                       event.starts_at,
                       CASE WHEN task.item_key IN ('post_show_reconciliation','post_show_report')
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
                  AND CASE
                      WHEN task.item_key IN ('post_show_reconciliation','post_show_report')
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
            let mut assigned = 0_u32;
            for action in approvals {
                let need = assignment_need(&action.context, &action.action_kind);
                let Some(member_index) = select_member_index(&mutable_team, need) else {
                    continue;
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
                    friendly_action_title(&action.action_kind),
                    enriched_task_detail(&action.payload, action.approval_expires_at, None),
                    action.approval_expires_at,
                    0,
                    now,
                )
                .await?;
                member.open_assignments = member.open_assignments.saturating_add(1);
                member.recent_assignments = member.recent_assignments.saturating_add(1);
                assigned = assigned.saturating_add(1);
            }

            for task in show_tasks {
                let need = assignment_need("show_operations", &task.task_key);
                let Some(member_index) = select_member_index(&mutable_team, need) else {
                    continue;
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
                    friendly_show_task_title(&task.task_key),
                    format!(
                        "Koncert: {}. Termin koncertu: {}.",
                        task.event_title, task.starts_at
                    ),
                    Some(task.due_at),
                    0,
                    now,
                )
                .await?;
                member.open_assignments = member.open_assignments.saturating_add(1);
                member.recent_assignments = member.recent_assignments.saturating_add(1);
                assigned = assigned.saturating_add(1);
            }

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
            // Same reasoning as the handoff sweep: a gated capability is an
            // operator's decision, not a fault, and reporting it as a failed
            // cycle every sixty seconds trains everyone to ignore the log.
            let can_email =
                super::executor_capability_available(&mut tx, workspace_id, "team.email").await?;
            let rows = sqlx::query_as::<_, ReminderRow>(
                r#"
                SELECT assignment.id assignment_id,
                       action.action_kind, action.context, assignment.source_kind,
                       assignment.source_ref, event.title event_title,
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
            for row in rows {
                let reminder_number = row.reminder_count.saturating_add(1);
                let next = next_reminder_at(now, row.due_at, row.reminder_count);
                let title = if row.source_kind == "show_task" {
                    friendly_show_task_title(row.source_ref.as_deref().unwrap_or("show_task"))
                } else {
                    friendly_action_title(row.action_kind.as_deref().unwrap_or("approval"))
                };
                let detail = if row.source_kind == "show_task" {
                    if let Some(event_title) = row.event_title.as_deref() {
                        format!(
                            "To zadanie dotyczące koncertu {event_title} nadal czeka na domknięcie."
                        )
                    } else {
                        "To zadanie nadal czeka na Twoje domknięcie.".to_owned()
                    }
                } else if let Some(payload_json) = row.payload.as_ref() {
                    enriched_task_detail(payload_json, row.due_at, row.due_at)
                } else {
                    "To zadanie nadal czeka na Twoją decyzję lub wykonanie.".to_owned()
                };
                queue_team_email_action(
                    &mut tx,
                    workspace_id,
                    row.assignment_id,
                    row.context.as_deref().unwrap_or("show_operations"),
                    &row.normalized_email,
                    &row.display_name,
                    title,
                    detail,
                    row.due_at,
                    u8::try_from(reminder_number.clamp(1, 12)).unwrap_or(12),
                    now,
                )
                .await?;
                sqlx::query(
                    r#"UPDATE viryaos_team_assignments
                       SET last_reminded_at=$3, next_reminder_at=$4,
                           reminder_count=reminder_count+1
                       WHERE workspace_id=$1 AND id=$2 AND status='open'"#,
                )
                .bind(workspace_id.into_uuid())
                .bind(row.assignment_id)
                .bind(now)
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
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

    let payload = serde_json::to_value(AutopilotActionPayload::SendTeamAssignmentEmail {
        assignment_id,
        recipient_email: recipient_email.to_owned(),
        recipient_name: recipient_name.to_owned(),
        task_title,
        task_detail,
        due_at,
        action_url_path: "/staff/?tab=overview#needs-you".to_owned(),
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
    .bind(now)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|c| c.into_uuid()))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn load_team_routing(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<TeamRoutingRow>, RepositoryError> {
    sqlx::query_as::<_, TeamRoutingRow>(
        r#"SELECT profile.member_id, profile.member_key, member.display_name,
                  member.normalized_email, profile.active, profile.skills,
                  profile.capacity_basis_points,
                  COUNT(assignment.id) FILTER (WHERE assignment.status='open') open_assignments,
                  COUNT(assignment.id) FILTER (WHERE assignment.assigned_at >= $2 - INTERVAL '30 days') recent_assignments,
                  -- Follow-through: of the work this member was given and that
                  -- has had time to be done, how much did they actually finish,
                  -- and how much chasing did it take?
                  --
                  -- Each completion is worth 10000 minus 2500 per reminder, so
                  -- a task done unprompted counts fully and one that needed
                  -- three reminders counts for little. Anything settled and not
                  -- completed counts zero. Members with no settled history get
                  -- the neutral score instead of a zero they did not earn.
                  --
                  -- Only assignments older than a day are considered, so work
                  -- handed out this morning is not scored as ignored.
                  COALESCE((
                      SELECT AVG(
                          CASE WHEN history.completed_at IS NOT NULL
                               THEN GREATEST(0, 10000 - 2500 * LEAST(4, COALESCE(history.reminder_count, 0)))
                               ELSE 0
                          END
                      )::integer
                      FROM viryaos_team_assignments history
                      WHERE history.workspace_id = profile.workspace_id
                        AND history.assignee_member_id = profile.member_id
                        AND history.status <> 'open'
                        AND history.assigned_at < $2 - INTERVAL '1 day'
                  ), 5000) AS follow_through_basis_points,
                  -- The same measure, split by the skill the work needed.
                  -- Whole-member reliability answers "does this person finish
                  -- things"; routing needs "does this person finish *this*".
                  -- Someone who never gets round to press mail may be the
                  -- first to edit a video, and averaging the two hides both.
                  COALESCE(per_skill.skills, ARRAY[]::text[]) AS skill_follow_through_skills,
                  COALESCE(per_skill.scores, ARRAY[]::integer[]) AS skill_follow_through
           FROM viryaos_team_profiles profile
           JOIN workspace_members member
             ON member.workspace_id=profile.workspace_id AND member.id=profile.member_id
           LEFT JOIN viryaos_team_assignments assignment
             ON assignment.workspace_id=profile.workspace_id AND assignment.assignee_member_id=profile.member_id
           LEFT JOIN LATERAL (
               SELECT array_agg(skill.required_skill ORDER BY skill.required_skill) AS skills,
                      array_agg(skill.score ORDER BY skill.required_skill) AS scores
               FROM (
                   SELECT history.required_skill,
                          AVG(
                              CASE WHEN history.completed_at IS NOT NULL
                                   THEN GREATEST(0, 10000 - 2500 * LEAST(4, COALESCE(history.reminder_count, 0)))
                                   ELSE 0
                              END
                          )::integer AS score
                   FROM viryaos_team_assignments history
                   WHERE history.workspace_id = profile.workspace_id
                     AND history.assignee_member_id = profile.member_id
                     AND history.status <> 'open'
                     AND history.assigned_at < $2 - INTERVAL '1 day'
                   GROUP BY history.required_skill
               ) skill
           ) per_skill ON true
           WHERE profile.workspace_id=$1 AND profile.active AND member.status='active'
           -- `profile.workspace_id` is grouped because the follow-through
           -- subquery correlates on it. Postgres only infers functional
           -- dependency from a grouped primary key, and this table's key is
           -- (workspace_id, member_id) — grouping half of it left the other
           -- half ungrouped, and the scalar subquery in the SELECT list is
           -- evaluated after grouping, so the planner refused the whole
           -- statement with "subquery uses ungrouped column
           -- profile.workspace_id from outer query". Every autopilot cycle
           -- then reported a failed phase and no human handoff was ever
           -- assigned. The WHERE clause already pins the column to one value,
           -- so grouping by it changes no result.
           GROUP BY profile.workspace_id, profile.member_id, profile.member_key, member.display_name,
                    member.normalized_email, profile.active, profile.skills, profile.capacity_basis_points,
                    per_skill.skills, per_skill.scores
           ORDER BY profile.member_key"#,
    )
    .bind(workspace_id.into_uuid()).bind(now)
    .fetch_all(&mut **tx).await.map_err(map_sqlx)
}

/// This member's follow-through on the skill actually being routed.
///
/// Falls back to their overall record when they have no settled history for
/// this skill, and to neutral when they have none at all. Whole-member
/// reliability is the weaker signal: the reason to measure at all is that
/// people are not uniformly diligent, and averaging across every kind of work
/// hides exactly the difference routing needs to see. Someone who never gets
/// round to press mail may be first to cut a video.
fn follow_through_for(member: &TeamRoutingRow, need: TeamAssignmentNeed) -> u16 {
    let wanted = need.primary_skill.as_str();
    member
        .skill_follow_through_skills
        .iter()
        .position(|skill| skill == wanted)
        .and_then(|index| member.skill_follow_through.get(index).copied())
        .map_or_else(
            || bounded_u16(i64::from(member.follow_through_basis_points)),
            |score| bounded_u16(i64::from(score)),
        )
}

/// Skill-fit-first, fairness-second selection over the live routing snapshot.
/// Shared by the scheduled routers and the `member_key="auto"` assignment path.
pub(in crate::autopilot) fn select_member_index(
    team: &[TeamRoutingRow],
    need: TeamAssignmentNeed,
) -> Option<usize> {
    let snapshots = team
        .iter()
        .map(|member| TeamMemberRoutingSnapshot {
            member_id: WorkspaceMemberId::from_uuid(member.member_id),
            member_key: member.member_key.clone(),
            active: member.active,
            skills: member
                .skills
                .iter()
                .filter_map(|skill| parse_team_skill(skill))
                .collect(),
            open_assignments: bounded_u16(member.open_assignments),
            recent_assignments: bounded_u16(member.recent_assignments),
            capacity_basis_points: bounded_u16(i64::from(member.capacity_basis_points)),
            follow_through_basis_points: follow_through_for(member, need),
        })
        .collect::<Vec<_>>();
    let decision = select_team_assignee(&snapshots, need)?;
    team.iter()
        .position(|member| member.member_id == decision.member_id.into_uuid())
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

pub(super) fn friendly_action_title(action_kind: &str) -> String {
    match action_kind {
        "opportunity.live.apply" => "Sprawdź i zatwierdź zgłoszenie koncertowe".into(),
        "funding.application.submit" => "Sprawdź i zatwierdź wysłanie wniosku".into(),
        "promotion.budget_change.request" => "Sprawdź zmianę budżetu promocji".into(),
        other => format!("VIRYA OS — {}", other.replace(['.', '_'], " ")),
    }
}

fn friendly_show_task_title(task_key: &str) -> String {
    match task_key {
        "staff_assigned" => "Potwierdź obsadę koncertu".into(),
        "offline_snapshot_ready" => "Przygotuj offline snapshot na koncert".into(),
        "gate_device_charged" => "Naładuj urządzenie wejściowe".into(),
        "backup_device_ready" => "Przygotuj urządzenie zapasowe".into(),
        "network_tested" => "Przetestuj internet na wejściu".into(),
        "guestlist_checked" => "Sprawdź guestlistę".into(),
        "post_show_reconciliation" => "Zrób rozliczenie po koncercie".into(),
        "post_show_report" => "Domknij raport po koncercie".into(),
        other => format!("Domknij zadanie koncertowe: {}", other.replace('_', " ")),
    }
}

/// Builds an enriched `task_detail` string from the action payload's briefing.
/// This is what goes into the team assignment email body — summary, why it
/// matters, steps, and the content being approved. Truncated to 1800 chars
/// to fit the n8n workflow's slice limit.
fn enriched_task_detail(
    payload_json: &serde_json::Value,
    approval_expires_at: Option<OffsetDateTime>,
    assignment_due_at: Option<OffsetDateTime>,
) -> String {
    use crowdrelay_application::autopilot::AutopilotActionPayload;

    let Ok(payload) = serde_json::from_value::<AutopilotActionPayload>(payload_json.clone()) else {
        return "To zadanie nadal czeka na Twoją decyzję lub wykonanie.".to_owned();
    };
    let mut briefing = payload.briefing();
    briefing.deadline_note = format_deadline_note(approval_expires_at, assignment_due_at);

    let mut text = format!(
        "{}\n\nDlaczego to ważne: {}\n\nKroki:",
        briefing.summary, briefing.why_it_matters
    );
    for (i, step) in briefing.steps.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {} — {}",
            i + 1,
            step.what_to_do,
            step.why_it_matters
        ));
    }
    if !briefing.content.is_empty() {
        text.push_str("\n\nTreść:");
        for field in &briefing.content {
            text.push_str(&format!("\n{}: {}", field.label, field.value));
        }
    }
    text.push_str(&format!("\n\n{}", briefing.deadline_note));

    // Truncate to fit the n8n workflow's slice(0, 1800) limit.
    if text.len() > 1800 {
        text.truncate(1799);
        text.push('…');
    }
    text
}

fn parse_team_skill(value: &str) -> Option<TeamSkill> {
    match value {
        "general" => Some(TeamSkill::General),
        "operations" => Some(TeamSkill::Operations),
        "booking" => Some(TeamSkill::Booking),
        "approval" => Some(TeamSkill::Approval),
        "technical" => Some(TeamSkill::Technical),
        "visual" => Some(TeamSkill::Visual),
        "video" => Some(TeamSkill::Video),
        "photography" => Some(TeamSkill::Photography),
        "social" => Some(TeamSkill::Social),
        "english_copy" => Some(TeamSkill::EnglishCopy),
        "polish_copy" => Some(TeamSkill::PolishCopy),
        "people" => Some(TeamSkill::People),
        _ => None,
    }
}

fn bounded_u16(value: i64) -> u16 {
    u16::try_from(value.clamp(0, i64::from(u16::MAX))).unwrap_or(u16::MAX)
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

fn next_reminder_at(
    now: OffsetDateTime,
    due: Option<OffsetDateTime>,
    reminder_count: i32,
) -> Option<OffsetDateTime> {
    let hours = match reminder_count {
        i if i <= 0 => 24,
        1 => 12,
        _ => 6,
    };
    let candidate = now + TimeDuration::hours(hours);
    due.and_then(|due_at| (candidate < due_at).then_some(candidate))
}
