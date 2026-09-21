//! Release-window asks — the §4b-3 / 1R.4 making-of handoff — and the
//! shared "ask deferred" record every router writes when nobody eligible
//! exists (§4i-6).

use super::{
    team::PendingInitialNotice,
    team_routing::{TeamRoutingRow, select_member_index_explained},
    *,
};
use crowdrelay_application::autopilot::BriefingLocale;
use crowdrelay_domain::team_operations::{TeamAssignmentNeed, TeamRoutingRefusal, TeamSkill};
use time::Duration as TimeDuration;

/// §4b-3 / 1R.4: the making-of ask. A release inside its R-14 window with
/// no making-of material on file owes the band one ask — file the
/// studio/rehearsal material that already exists — not a production plan.
/// The ask dedupes on the plan (source identity) and closes when a making-of
/// source bound to the release or filed after the ask lands.
pub(super) async fn issue_release_making_of_asks(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    mutable_team: &mut [TeamRoutingRow],
    crew_locale: BriefingLocale,
    pending_notices: &mut Vec<PendingInitialNotice>,
) -> Result<u32, RepositoryError> {
    let mut assigned = 0_u32;
    let making_of_asks = sqlx::query_as::<_, (Uuid, String, OffsetDateTime)>(
        r#"
        SELECT plan.id, plan.title, plan.release_at
        FROM viryaos_release_plans plan
        LEFT JOIN viryaos_team_assignments assignment
          ON assignment.workspace_id=plan.workspace_id
         AND assignment.source_kind='release_making_of'
         AND assignment.source_id=plan.id
         -- The ask is keyed to the release window: a postponed release gets
         -- a fresh ask, and a row filed under a stale window (or plain
         -- 'making_of' from before windowing) cannot block it. UTC is pinned
         -- because the write side derives the date from the OffsetDateTime,
         -- which is always +00:00 — a session TimeZone shift would split the
         -- key and the ask would cancel itself unread.
         AND assignment.source_ref =
             'making_of:' || (plan.release_at AT TIME ZONE 'UTC')::date::text
        WHERE plan.workspace_id=$1
          AND plan.active
          AND plan.tier <> 'filler'
          AND $2 >= plan.release_at - INTERVAL '14 days'
          AND $2 <= plan.release_at + INTERVAL '7 days'
          AND assignment.id IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM viryaos_content_sources source
              WHERE source.workspace_id=plan.workspace_id
                AND source.format_key='making_of'
                AND source.active
                AND (
                    source.metadata->>'release_plan_id' = plan.id::text
                    OR source.created_at >= plan.release_at - INTERVAL '45 days'
                )
          )
        ORDER BY plan.release_at, plan.id
        LIMIT 16
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    for (plan_id, plan_title, release_at) in making_of_asks {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Video,
            secondary_skill: Some(TeamSkill::Visual),
            allow_generalist: true,
        };
        let member_index = match select_member_index_explained(mutable_team, need) {
            Ok(index) => index,
            Err(refusal) => {
                // The ask's window ends a week after release day —
                // past that the making-of framing no longer applies.
                record_ask_refusal(
                    tx,
                    workspace_id,
                    "release_making_of",
                    plan_id,
                    Some(&format!("making_of:{}", release_at.date())),
                    None,
                    Some(release_at + TimeDuration::days(7)),
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
        let source_ref = format!("making_of:{}", release_at.date());
        let assignment_id = Uuid::now_v7();
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_team_assignments (
                id, workspace_id, action_id, source_kind, source_id, source_ref,
                assignee_member_id, required_skill, due_at
            ) VALUES ($1,$2,NULL,'release_making_of',$3,$4,$5,$6,$7)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(assignment_id)
        .bind(workspace_id.into_uuid())
        .bind(plan_id)
        .bind(&source_ref)
        .bind(member.member_id)
        .bind(need.primary_skill.as_str())
        .bind(release_at)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        if inserted.is_none() {
            continue;
        }

        let detail = match crew_locale {
            BriefingLocale::Pl => format!(
                "Premiera „{plan_title}” zbliża się — zarchiwizuj i oznacz materiał z planu/prób (making-of), który już istnieje."
            ),
            BriefingLocale::En => format!(
                "\"{plan_title}\" is inside R-14 — file the making-of material that already exists and mark it for this release."
            ),
        };
        pending_notices.push(PendingInitialNotice {
            assignment_id,
            context: "release".to_owned(),
            recipient_email: member.normalized_email.clone(),
            recipient_name: member.display_name.clone(),
            title: match crew_locale {
                BriefingLocale::Pl => format!("Making-of do wydania: {plan_title}"),
                BriefingLocale::En => format!("Making-of for the release: {plan_title}"),
            },
            detail,
            due_at: Some(release_at),
            source_action_id: None,
        });
        member.open_assignments = member.open_assignments.saturating_add(1);
        member.recent_assignments = member.recent_assignments.saturating_add(1);
        member.asks_last_7d = member.asks_last_7d.saturating_add(1);
        assigned = assigned.saturating_add(1);
    }
    Ok(assigned)
}

/// The making-of ask resolves when the material lands — a `making_of`
/// format source bound to the release by `metadata.release_plan_id`, or
/// filed after the ask went out. A plan that went inactive or passed its
/// window unfilled cancels the ask: post-release the material is ordinary
/// filler stock, not a deadline.
pub(super) async fn close_release_making_of_assignments(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status='done', completed_at=$2, next_reminder_at=NULL
           FROM viryaos_content_sources source
           WHERE assignment.workspace_id=$1 AND assignment.status='open'
             AND assignment.source_kind='release_making_of'
             AND source.workspace_id=assignment.workspace_id
             AND source.format_key='making_of'
             AND source.active
             AND (
                 source.metadata->>'release_plan_id' = assignment.source_id::text
                 OR source.created_at >= assignment.assigned_at
             )"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    sqlx::query(
        r#"UPDATE viryaos_team_assignments assignment
           SET status='cancelled', completed_at=NULL, next_reminder_at=NULL
           FROM viryaos_release_plans plan
           WHERE assignment.workspace_id=$1 AND assignment.status='open'
             AND assignment.source_kind='release_making_of'
             AND plan.workspace_id=assignment.workspace_id
             AND plan.id=assignment.source_id
             AND (NOT plan.active OR plan.release_at < $2 - INTERVAL '7 days'
                  OR assignment.source_ref <>
                     'making_of:' || (plan.release_at AT TIME ZONE 'UTC')::date::text)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// §4i-6: an ask nobody could take is recorded, not dropped on the floor —
/// one audit row per (ask, reason), so a sweep re-refusing the same ask for
/// the same reason does not spam the trail, and the daily briefing can count
/// the asks still waiting on capacity.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_ask_refusal(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    source_kind: &str,
    source_id: Uuid,
    source_ref: Option<&str>,
    action_id: Option<Uuid>,
    deadline_at: Option<OffsetDateTime>,
    need: TeamAssignmentNeed,
    refusal: TeamRoutingRefusal,
) -> Result<(), RepositoryError> {
    // The dedup identity is one row per ask: an autopilot action asks once,
    // but a show event can carry several distinct checklist tasks — folding
    // source_ref in keeps a refused "photos" task from swallowing the same
    // event's refused "setlist" task.
    let target_id = match (action_id, source_ref) {
        (Some(action_id), _) => action_id.to_string(),
        (None, Some(source_ref)) => format!("{source_id}:{source_ref}"),
        (None, None) => source_id.to_string(),
    };
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        )
        SELECT $1, 'service', 'team.ask_deferred', 'team_ask', $2, $3
        WHERE NOT EXISTS (
            SELECT 1 FROM audit_events prior
            WHERE prior.workspace_id = $1
              AND prior.action = 'team.ask_deferred'
              AND prior.target_type = 'team_ask'
              AND prior.target_id = $2
              AND prior.metadata->>'reason' = $3->>'reason'
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&target_id)
    .bind(serde_json::json!({
        "reason": refusal.as_str(),
        "source_kind": source_kind,
        "source_id": source_id,
        "source_ref": source_ref,
        "action_id": action_id,
        "deadline_at": deadline_at,
        "primary_skill": need.primary_skill.as_str(),
        "secondary_skill": need.secondary_skill.map(|skill| skill.as_str()),
    }))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
