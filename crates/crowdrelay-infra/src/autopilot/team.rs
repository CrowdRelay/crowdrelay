//! Thin human-handoff index for Autopilot approvals.
//!
//! Domain actions remain authoritative. This adapter only assigns an owner and
//! emits bounded notification/reminder intents; it never invents a second task.

use super::*;
use crowdrelay_domain::{
    WorkspaceMemberId,
    team_operations::{
        TeamAssignmentNeed, TeamMemberRoutingSnapshot, TeamSkill, select_team_assignee,
    },
};
use time::Duration as TimeDuration;

#[derive(Debug, FromRow)]
struct TeamRoutingRow {
    member_id: Uuid,
    member_key: String,
    display_name: String,
    normalized_email: String,
    active: bool,
    skills: Vec<String>,
    capacity_basis_points: i32,
    open_assignments: i64,
    recent_assignments: i64,
}

#[derive(Debug, FromRow)]
struct UnassignedApprovalRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    approval_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct ReminderRow {
    assignment_id: Uuid,
    action_id: Option<Uuid>,
    action_kind: Option<String>,
    context: Option<String>,
    display_name: String,
    normalized_email: String,
    due_at: Option<OffsetDateTime>,
    reminder_count: i32,
}

impl PostgresAutopilotRepository {
    /// Assigns new approval handoffs to a suitable, least-loaded team member.
    /// Returns the number of newly assigned items.
    pub async fn reconcile_team_handoffs(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout).await?;
            // The assignment is a handoff index only. As soon as the underlying
            // approval leaves `awaiting_approval`, stop reminders and close it.
            sqlx::query(
                r#"
                UPDATE viryaos_team_assignments assignment
                SET status = CASE
                        WHEN action.status IN ('queued','processing','succeeded') THEN 'done'
                        ELSE 'cancelled'
                    END,
                    completed_at = CASE
                        WHEN action.status IN ('queued','processing','succeeded') THEN $2
                        ELSE NULL
                    END,
                    next_reminder_at = NULL
                FROM viryaos_autopilot_actions action
                WHERE assignment.workspace_id=$1
                  AND assignment.workspace_id=action.workspace_id
                  AND assignment.action_id=action.id
                  AND assignment.status='open'
                  AND action.status <> 'awaiting_approval'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            let team = load_team_routing(&mut tx, workspace_id, now).await?;
            if team.is_empty() {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(0);
            }

            let actions = sqlx::query_as::<_, UnassignedApprovalRow>(
                r#"
                SELECT action.id, action.context, action.action_kind, action.subject_kind,
                       action.subject_id, action.approval_expires_at
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

            let mut mutable_team = team;
            let mut assigned = 0_u32;
            for action in actions {
                let need = assignment_need(&action.context, &action.action_kind);
                let snapshots = mutable_team
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
                    })
                    .collect::<Vec<_>>();
                let Some(decision) = select_team_assignee(&snapshots, need) else {
                    continue;
                };
                let Some(member) = mutable_team
                    .iter_mut()
                    .find(|member| member.member_id == decision.member_id.into_uuid())
                else {
                    continue;
                };
                let assignment_id = Uuid::now_v7();
                let next_reminder_at = first_reminder_at(now, action.approval_expires_at);
                let inserted = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO viryaos_team_assignments (
                        id, workspace_id, action_id, source_kind, source_id,
                        assignee_member_id, required_skill, due_at, next_reminder_at
                    ) VALUES ($1,$2,$3,'autopilot_action',$4,$5,$6,$7,$8)
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
                .bind(next_reminder_at)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.is_none() {
                    continue;
                }

                emit_team_notification(
                    &mut tx,
                    workspace_id,
                    "viryaos.team.assignment_notification_requested",
                    json!({
                        "assignment_id": assignment_id,
                        "action_id": action.id,
                        "context": action.context,
                        "action_kind": action.action_kind,
                        "subject_kind": action.subject_kind,
                        "subject_id": action.subject_id,
                        "assignee": {
                            "member_key": member.member_key,
                            "display_name": member.display_name,
                            "email": member.normalized_email,
                        },
                        "due_at": action.approval_expires_at,
                        "action_url_path": "/staff/control/",
                        "message_contract": {
                            "tone": "friendly_concise_human",
                            "include": ["what_to_do", "why_it_matters", "deadline", "action_link"],
                            "do_not_invent_business_facts": true
                        }
                    }),
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

    /// Emits friendly reminders only after their durable schedule becomes due.
    /// Returns the number of reminders emitted in this bounded cycle.
    pub async fn dispatch_team_handoff_reminders(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            configure_transaction(&mut tx, self.operation_timeout, self.lock_timeout).await?;
            let rows = sqlx::query_as::<_, ReminderRow>(
                r#"
                SELECT assignment.id assignment_id, assignment.action_id,
                       action.action_kind, action.context,
                       member.display_name, member.normalized_email,
                       assignment.due_at, assignment.reminder_count
                FROM viryaos_team_assignments assignment
                JOIN workspace_members member
                  ON member.workspace_id=assignment.workspace_id
                 AND member.id=assignment.assignee_member_id
                LEFT JOIN viryaos_autopilot_actions action
                  ON action.workspace_id=assignment.workspace_id
                 AND action.id=assignment.action_id
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

            let mut emitted = 0_u32;
            for row in rows {
                let next = next_reminder_at(now, row.due_at, row.reminder_count);
                sqlx::query(
                    r#"
                    UPDATE viryaos_team_assignments
                    SET last_reminded_at=$3,
                        next_reminder_at=$4,
                        reminder_count=reminder_count+1
                    WHERE workspace_id=$1 AND id=$2 AND status='open'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(row.assignment_id)
                .bind(now)
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                emit_team_notification(
                    &mut tx,
                    workspace_id,
                    "viryaos.team.assignment_reminder_requested",
                    json!({
                        "assignment_id": row.assignment_id,
                        "action_id": row.action_id,
                        "context": row.context,
                        "action_kind": row.action_kind,
                        "assignee": {
                            "display_name": row.display_name,
                            "email": row.normalized_email,
                        },
                        "due_at": row.due_at,
                        "action_url_path": "/staff/control/",
                        "reminder_number": row.reminder_count.saturating_add(1),
                        "message_contract": {
                            "tone": "friendly_concise_human",
                            "include": ["what_is_still_needed", "deadline", "action_link"],
                            "avoid_guilt_or_pressure": true
                        }
                    }),
                )
                .await?;
                emitted = emitted.saturating_add(1);
            }
            tx.commit().await.map_err(map_sqlx)?;
            Ok(emitted)
        })
        .await
    }
}

async fn load_team_routing(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<TeamRoutingRow>, RepositoryError> {
    sqlx::query_as::<_, TeamRoutingRow>(
        r#"
        SELECT profile.member_id, profile.member_key, member.display_name,
               member.normalized_email, profile.active, profile.skills,
               profile.capacity_basis_points,
               COUNT(assignment.id) FILTER (WHERE assignment.status='open') open_assignments,
               COUNT(assignment.id) FILTER (
                   WHERE assignment.assigned_at >= $2 - INTERVAL '30 days'
               ) recent_assignments
        FROM viryaos_team_profiles profile
        JOIN workspace_members member
          ON member.workspace_id=profile.workspace_id AND member.id=profile.member_id
        LEFT JOIN viryaos_team_assignments assignment
          ON assignment.workspace_id=profile.workspace_id
         AND assignment.assignee_member_id=profile.member_id
        WHERE profile.workspace_id=$1 AND profile.active AND member.status='active'
        GROUP BY profile.member_id, profile.member_key, member.display_name,
                 member.normalized_email, profile.active, profile.skills,
                 profile.capacity_basis_points
        ORDER BY profile.member_key
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)
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

pub(super) async fn emit_team_notification(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &'static str,
    payload: Value,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, max_attempts
        ) VALUES ($1,$2,1,$3,12)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
