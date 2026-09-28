//! Thin human-handoff index for work that genuinely needs a band member.
//!
//! Domain actions and show checklist rows remain authoritative. This adapter
//! only assigns an owner and queues provider-confirmed email actions through
//! the existing Autopilot execution plane.

use super::team_routing::{load_team_routing, select_member_index_explained};
use super::*;
use crowdrelay_application::autopilot::BriefingLocale;
use crowdrelay_domain::team_operations::{TeamAssignmentNeed, TeamSkill};

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
    timezone: String,
    due_at: OffsetDateTime,
}

/// A first-notice held for per-member batching.
///
/// First notices used to go one-per-assignment, so a batch of approvals
/// landing in a single cycle put a dozen same-minute mails in one inbox —
/// twenty-one to two people on 2026-09-20. Holding them until every producer
/// has run lets one e-mail carry the most urgent task and name the rest.
/// Batching changes interruptions, not the work index: every approval still
/// owns its own assignment row.
pub(super) struct PendingInitialNotice {
    pub assignment_id: Uuid,
    pub context: String,
    pub recipient_email: String,
    pub recipient_name: String,
    pub title: String,
    pub detail: String,
    pub due_at: Option<OffsetDateTime>,
    pub source_action_id: Option<Uuid>,
    /// The `team_assignments.source_kind` this notice came from — the flush
    /// holds first notices for everything except show tasks unless the work
    /// is due inside a day, letting the next morning's briefing carry them.
    pub source_kind: &'static str,
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
            let crew_zone = crew_timezone_in_tx(&mut tx, workspace_id).await?;

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
                    &self.pool,
                    workspace_id,
                    now,
                    crew_locale,
                    self.approval_links.as_ref(),
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
                FROM autopilot_actions action
                LEFT JOIN team_assignments assignment
                  ON assignment.workspace_id=action.workspace_id
                 AND assignment.action_id=action.id
                WHERE action.workspace_id=$1
                  AND action.status='awaiting_approval'
                  AND assignment.id IS NULL
                  AND (action.approval_expires_at IS NULL OR action.approval_expires_at>$2)
                  -- A delivery inside a community relay batch asks through the
                  -- batch card — one ask for the spread, not one email per community.
                  AND NOT (
                      action.action_kind = 'community.engage.request'
                      AND action.payload ->> 'source_id' IS NOT NULL
                  )
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
                        "team handoff notices are parked: no executor advertises this capability"
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
                       event.starts_at, event.timezone,
                       CASE WHEN task.item_key = 'post_show_reconciliation'
                            THEN event.starts_at + INTERVAL '36 hours'
                            ELSE event.starts_at - INTERVAL '2 hours' END due_at
                FROM events event CROSS JOIN task
                LEFT JOIN show_checklist_items checklist
                  ON checklist.workspace_id=event.workspace_id
                 AND checklist.event_id=event.id AND checklist.item_key=task.item_key
                LEFT JOIN team_assignments assignment
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
            // First notices collect here rather than mailing per assignment —
            // `flush_initial_notices` sends one e-mail per member once every
            // producer has offered its asks.
            let mut pending_notices: Vec<PendingInitialNotice> = Vec::new();
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
                    INSERT INTO team_assignments (
                        id, workspace_id, action_id, source_kind, source_id, source_ref,
                        assignee_member_id, required_skill, due_at
                    ) VALUES ($1,$2,$3,'autopilot_action',$4,NULL,$5,$6,$7)
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
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.is_none() {
                    continue;
                }

                pending_notices.push(PendingInitialNotice {
                    assignment_id,
                    context: action.context.clone(),
                    recipient_email: member.normalized_email.clone(),
                    recipient_name: member.display_name.clone(),
                    title: friendly_action_title(&action.action_kind, crew_locale),
                    detail: enriched_task_detail(
                        &action.payload,
                        action.approval_expires_at,
                        None,
                        crew_locale,
                        &crew_zone,
                    ),
                    due_at: action.approval_expires_at,
                    source_action_id: Some(action.id),
                    source_kind: "autopilot_action",
                });
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
                    &mut pending_notices,
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
                    INSERT INTO team_assignments (
                        id, workspace_id, action_id, source_kind, source_id, source_ref,
                        assignee_member_id, required_skill, due_at
                    ) VALUES ($1,$2,NULL,'show_task',$3,$4,$5,$6,$7)
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
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.is_none() {
                    continue;
                }

                pending_notices.push(PendingInitialNotice {
                    assignment_id,
                    context: "show_operations".to_owned(),
                    recipient_email: member.normalized_email.clone(),
                    recipient_name: member.display_name.clone(),
                    title: friendly_show_task_title(&task.task_key, crew_locale),
                    detail: show_task_detail(&task, crew_locale),
                    due_at: Some(task.due_at),
                    source_action_id: None,
                    source_kind: "show_task",
                });
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
                    &mut pending_notices,
                )
                .await?,
            );

            flush_initial_notices(
                &mut tx,
                workspace_id,
                pending_notices,
                crew_locale,
                self.approval_links.as_ref(),
                now,
            )
            .await?;

            tx.commit().await.map_err(map_sqlx)?;
            Ok(assigned)
        })
        .await
    }

    /// Retires the per-assignment reminder lane without mailing anyone.
    ///
    /// Reminder e-mails are gone: the daily briefing already names every open
    /// ask each morning, so a "VIRYA — przypomnienie" chase repeated a
    /// notification the briefing had already aggregated — two copies of the
    /// same fact in one inbox, which is what the crew reported. Nothing here
    /// schedules `next_reminder_at` anymore; this sweep exists to drain the
    /// schedules written before the lane retired (and any a missed writer
    /// still stamps), so an old row can never fire a mail that no longer
    /// exists. Returns the rows cleared — the first sweep after deploy is the
    /// backlog, every later one should be zero.
    pub async fn drain_team_reminder_schedule(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u32, RepositoryError> {
        self.bounded(async {
            let cleared = sqlx::query(
                r#"UPDATE team_assignments
                   SET next_reminder_at = NULL
                   WHERE workspace_id = $1 AND next_reminder_at IS NOT NULL"#,
            )
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?
            .rows_affected();
            Ok(u32::try_from(cleared).unwrap_or(u32::MAX))
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
        r#"UPDATE team_assignments assignment
           SET status = CASE WHEN action.status IN ('queued','processing','succeeded') THEN 'done' ELSE 'cancelled' END,
               completed_at = CASE WHEN action.status IN ('queued','processing','succeeded') THEN $2 ELSE NULL END,
               next_reminder_at = NULL
           FROM autopilot_actions action
           WHERE assignment.workspace_id=$1 AND assignment.workspace_id=action.workspace_id
             AND assignment.action_id=action.id AND assignment.status='open'
             AND action.status <> 'awaiting_approval'"#,
    )
    .bind(workspace_id.into_uuid()).bind(now)
    .execute(&mut **tx).await.map_err(map_sqlx)?;

    sqlx::query(
        r#"UPDATE team_assignments assignment
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
        r#"UPDATE team_assignments assignment
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
        r#"UPDATE team_assignments assignment
           SET status = CASE WHEN plan.status='done' THEN 'done' ELSE 'cancelled' END,
               completed_at = CASE WHEN plan.status='done' THEN $2 ELSE NULL END,
               next_reminder_at = NULL
           FROM capture_plans plan
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

/// The one-click links a team email carries. `direct` is the pair for the
/// single pending approval a notice fronts; `pending` is the per-ask list a
/// morning briefing mails. Both default to empty — a mail with no asks mints
/// no links.
#[derive(Default)]
pub(super) struct EmailApprovalLinks {
    pub direct: Option<(String, String)>,
    pub pending: Vec<PendingApprovalLink>,
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
    // Who declares what arrived — the producer, not the plumbing. A briefing
    // passes `true` and its own title is the subject; every task asks for
    // work, so it passes `false` and the subject says "task". Deriving this
    // from `source_action_id` mislabels show-task, capture-plan and
    // making-of mail, which carries no approval link but asks for real work.
    informational: bool,
    links: EmailApprovalLinks,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    // `email_opt_out` marks a member whose address must never receive crew
    // mail: the seeded service seats have to stay 'active' — admission
    // resolves them by their configured email — but no mailbox sits behind
    // the address, so every send bounced. The check lives at this choke
    // point so briefing, routing and reminder producers cannot bypass it;
    // the assignment row itself is still written by the caller.
    let opted_out = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM workspace_members
             WHERE workspace_id = $1 AND normalized_email = $2 AND email_opt_out)",
    )
    .bind(workspace_id.into_uuid())
    .bind(recipient_email)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if opted_out {
        return Ok(());
    }
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
    let decision_id = if let Some(id) = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO autopilot_decisions (
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
            "SELECT id FROM autopilot_decisions WHERE workspace_id=$1 AND decision_key=$2",
        )
        .bind(workspace_id.into_uuid())
        .bind(&decision_key)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?
    };

    // Crew mail keeps the tenant's night quiet — `available_at` holds the
    // notice to the window's end instead of a 03:00 inbox. The row exists,
    // audits and says exactly what was composed; it is simply not claimable
    // until morning. This covers every notice, whichever producer queued it.
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
        informational,
        action_url_path: match source_action_id {
            Some(id) => format!("/staff/?tab=overview#needs-you&action={id}"),
            None => "/staff/?tab=overview#needs-you".to_owned(),
        },
        reminder_number,
        approve_url: links.direct.as_ref().map(|pair| pair.0.clone()),
        skip_url: links.direct.as_ref().map(|pair| pair.1.clone()),
        pending_approvals: links.pending,
    })
    .map_err(|_| RepositoryError::Unexpected)?;

    let action_id = Uuid::now_v7();
    let action_trace =
        TraceContext::for_action(workspace_id, trace.trace_id(), action_id, Some(decision_id));
    sqlx::query(
        r#"INSERT INTO autopilot_actions (
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

/// Sends every held first-notice as one e-mail per member: the earliest-due
/// task supplies the subject and body, and the rest are named underneath it
/// through the digest tail.
///
/// A batch of approvals landing in one cycle used to mail one notice per
/// assignment, all stamped the same minute — the inbox learned to ignore
/// VIRYA mail entirely, which is what the digest tail was built to undo.
/// The e-mail is still one durable action row per member, idempotent on the
/// primary assignment — a notice folded into somebody else's e-mail is never
/// mailed on its own.
async fn flush_initial_notices(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    pending: Vec<PendingInitialNotice>,
    crew_locale: BriefingLocale,
    approval_links: Option<&ApprovalLinkMinter>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    // §6: a first notice mails only when the work is due inside a day or is
    // a genuinely manual show task; everything else stays an open assignment
    // and reaches the member through tomorrow's briefing instead of another
    // same-minute e-mail. Holding drops nothing — the briefing's open-tasks
    // and pending-asks sections list every open assignment regardless.
    let mailable = pending.into_iter().filter(|notice| {
        notice.source_kind == "show_task"
            || notice
                .due_at
                .is_some_and(|due| due < now + time::Duration::hours(24))
    });

    let mut by_recipient: Vec<(String, Vec<PendingInitialNotice>)> = Vec::new();
    for notice in mailable {
        match by_recipient
            .iter_mut()
            .find(|(email, _)| *email == notice.recipient_email)
        {
            Some((_, group)) => group.push(notice),
            None => by_recipient.push((notice.recipient_email.clone(), vec![notice])),
        }
    }

    for (_, mut group) in by_recipient {
        // The most urgent task carries the e-mail — soonest deadline first,
        // undated last — regardless of which producer offered it.
        group.sort_by_key(|notice| (notice.due_at.is_none(), notice.due_at));
        let Some(primary) = group.first() else {
            continue;
        };
        let others: Vec<String> = group
            .iter()
            .skip(1)
            .map(|notice| notice.title.clone())
            .collect();
        let tail = digest_tail(&others, crew_locale);
        // The n8n workflow slices `task_detail` at 1800 chars — a tail
        // appended behind a full-length detail would be cut wholesale, and
        // the named tasks are the point of the digest. The primary's body
        // gives up its room instead: the panel behind the link carries its
        // full record. The ellipsis's three bytes come out of the body's
        // budget too, or a full-length detail ends at 1803 and the workflow
        // cuts the tail anyway.
        let budget = 1800_usize.saturating_sub(tail.len() + 3);
        let mut detail = primary.detail.clone();
        if detail.len() > budget {
            detail.truncate(detail.floor_char_boundary(budget));
            detail.push('…');
        }
        detail.push_str(&tail);
        // The link window is the ask's own expiry — a notice for plain work
        // (`source_action_id` is None) mints nothing.
        let direct = primary.source_action_id.and_then(|action_id| {
            approval_links.and_then(|minter| {
                minter.mint(action_id, Some(primary.assignment_id), primary.due_at)
            })
        });
        queue_team_email_action(
            tx,
            workspace_id,
            primary.assignment_id,
            &primary.context,
            &primary.recipient_email,
            &primary.recipient_name,
            primary.title.clone(),
            detail,
            primary.due_at,
            0,
            primary.source_action_id,
            // Every notice flushed here is work: an approval to answer or a
            // checklist task to do — none of it is read-only.
            false,
            EmailApprovalLinks {
                direct,
                pending: Vec::new(),
            },
            now,
        )
        .await?;
    }
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

/// The crew-facing name for an action kind — the same wording the first
/// notice and the mailed approval page share.
pub fn friendly_action_title(action_kind: &str, locale: BriefingLocale) -> String {
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
        ("booking_agent.approach.request", BriefingLocale::Pl) => {
            "Zatwierdź zgłoszenie do agenta bookingowego"
        }
        ("booking_agent.approach.request", BriefingLocale::En) => {
            "Approve the booking-agent approach"
        }
        ("booking_agent.approach_wave.request", BriefingLocale::Pl) => {
            "Zatwierdź falę zgłoszeń do agentów"
        }
        ("booking_agent.approach_wave.request", BriefingLocale::En) => {
            "Approve the booking-agent approach wave"
        }
        ("booking_agent.reply.request", BriefingLocale::Pl) => {
            "Zatwierdź odpowiedź do agenta bookingowego"
        }
        ("booking_agent.reply.request", BriefingLocale::En) => "Approve the booking-agent reply",
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
        ("social.join_ask.publish", BriefingLocale::Pl) => "Zatwierdź post „dołącz do nas”",
        ("social.join_ask.publish", BriefingLocale::En) => "Approve the join-ask post",
        ("latarnik.invite.request", BriefingLocale::Pl) => "Zatwierdź zaproszenie do Latarnika",
        ("latarnik.invite.request", BriefingLocale::En) => "Approve the Latarnik invitation",
        ("opportunity.counterparty_report.issue", BriefingLocale::Pl) => {
            "Zatwierdź wysłanie raportu po koncercie do kontrahenta"
        }
        ("opportunity.counterparty_report.issue", BriefingLocale::En) => {
            "Approve the post-show report to the counterparty"
        }
        ("opportunity.terms.counter", BriefingLocale::Pl) => "Zatwierdź kontrpropozycję warunków",
        ("opportunity.terms.counter", BriefingLocale::En) => "Approve the terms counter",
        ("opportunity.terms.accept", BriefingLocale::Pl) => "Zatwierdź przyjęcie warunków",
        ("opportunity.terms.accept", BriefingLocale::En) => "Approve accepting the terms",
        (other, _) => return format!("CrowdRelay — {}", other.replace(['.', '_'], " ")),
    };
    title.to_owned()
}

/// The showtime on the room's own clock, named. It used to print UTC —
/// "2026-10-17 17:30 UTC" for a 19:30 Warsaw show — to crew who read a clock
/// time as local. The event's zone is recorded, so this is a conversion, not
/// a guess; an unknown zone falls back to UTC and says so.
fn show_task_detail(task: &UnassignedShowTaskRow, locale: BriefingLocale) -> String {
    let local = crate::regional::at_event_timezone(task.starts_at, &task.timezone);
    let zone = if crate::regional::is_known_iana_timezone(&task.timezone) {
        task.timezone.trim()
    } else {
        "UTC"
    };
    let starts_at = format!(
        "{} {:02}:{:02} ({zone})",
        local.date(),
        local.hour(),
        local.minute()
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
/// transaction so the briefing's day boundary and a notice's quiet window
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

/// The reminder ceiling the e-mail frame still renders for.
///
/// The reminder lane is retired — nothing schedules `next_reminder_at` and the
/// drain sweep clears whatever is left — but the payload contract still
/// carries `reminder_number`, and a row written before the retirement (or a
/// rolled-back build) can still arrive with a nonzero one. The frame keeps
/// answering "which reminder is this" for those rows; three was the ladder's
/// length when it existed.
const MAX_REMINDERS_PER_ASSIGNMENT: i32 = 3;

/// Crew mail's quiet window, in the tenant's local hour — 21:00 to the
/// briefing's own 08:00, so the first mail of the day lands with the morning
/// briefing rather than ahead of it. Overnight is not an emergency channel:
/// a first notice composed at 03:00 waits three hours and loses nothing,
/// because nobody should have been reading mail at 03:00.
const CREW_QUIET_START_LOCAL_HOUR: i32 = 21;
const CREW_QUIET_END_LOCAL_HOUR: i32 = 8;

include!("team/task_copy.rs");
include!("team/reminder_copy.rs");

include!("team/tests.rs");

#[cfg(test)]
mod show_task_detail_tests {
    use super::*;
    use time::macros::datetime;

    fn task(timezone: &str) -> UnassignedShowTaskRow {
        UnassignedShowTaskRow {
            event_id: Uuid::nil(),
            event_title: "Sanity Check Tour".to_owned(),
            task_key: "staff_assigned".to_owned(),
            starts_at: datetime!(2026-10-17 17:30 UTC),
            timezone: timezone.to_owned(),
            due_at: datetime!(2026-10-17 15:30 UTC),
        }
    }

    #[test]
    fn the_showtime_reads_on_the_room_s_clock() {
        assert_eq!(
            show_task_detail(&task("Europe/Warsaw"), BriefingLocale::Pl),
            "Koncert: Sanity Check Tour. Termin koncertu: 2026-10-17 19:30 (Europe/Warsaw)."
        );
    }

    #[test]
    fn an_unknown_zone_stays_utc_and_says_so() {
        assert_eq!(
            show_task_detail(&task("Mars/Olympus"), BriefingLocale::En),
            "Show: Sanity Check Tour. Showtime: 2026-10-17 17:30 (UTC)."
        );
    }
}
