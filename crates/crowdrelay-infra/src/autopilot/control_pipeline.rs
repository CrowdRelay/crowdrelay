//! The content page's pipeline read, split out of `control.rs`.
//!
//! Same repository, same bounded reads: pending drafts, live executor
//! capabilities, the live material count, and the titles those drafts
//! cite — the narrow fan-out the page's first paint actually needs.
//! The cockpit-wide overview stays in `control.rs`.

use super::*;
use crowdrelay_application::autopilot::ContentPipeline;

pub(super) async fn load_content_pipeline(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<ContentPipeline, RepositoryError> {
    let workspace_uuid = workspace_id.into_uuid();

    // Three independent reads — the queue, who can run it, and how
    // much material is live — run concurrently. Source titles follow,
    // since they depend on which sources the pending payloads cite.
    let (pending_rows, live_capabilities, live_sources) = tokio::try_join!(
        async {
            sqlx::query_as::<_, PendingActionRow>(
                r#"
                SELECT action.id, action.context, action.action_kind, action.subject_kind,
                       action.subject_id, action.payload, action.created_at,
                       action.approval_expires_at,
                       assignment.assignee_member_id,
                       profile.member_key AS assignee_member_key,
                       member.display_name AS assignee_display_name,
                       assignment.due_at AS assignment_due_at
                FROM viryaos_autopilot_actions action
                LEFT JOIN viryaos_team_assignments assignment
                  ON assignment.workspace_id=action.workspace_id
                 AND assignment.action_id=action.id
                 AND assignment.status='open'
                LEFT JOIN viryaos_team_profiles profile
                  ON profile.workspace_id=assignment.workspace_id
                 AND profile.member_id=assignment.assignee_member_id
                LEFT JOIN workspace_members member
                  ON member.workspace_id=assignment.workspace_id
                 AND member.id=assignment.assignee_member_id
                WHERE action.workspace_id = $1
                  AND action.status = 'awaiting_approval'
                  AND (action.approval_expires_at IS NULL OR action.approval_expires_at > now())
                  AND (action.context = 'content_supply'
                       OR action.action_kind LIKE 'content.%'
                       OR action.action_kind LIKE 'agent.content.%')
                ORDER BY action.created_at, action.id
                LIMIT 50
                "#,
            )
            .bind(workspace_uuid)
            .fetch_all(&repo.pool)
            .await
            .map_err(map_sqlx)
        },
        async {
            sqlx::query_scalar::<_, String>(
                r#"
                SELECT DISTINCT capability_row.capability
                FROM viryaos_executor_capabilities capability_row
                JOIN viryaos_executor_instances executor
                  ON executor.workspace_id=capability_row.workspace_id
                 AND executor.executor_id=capability_row.executor_id
                LEFT JOIN viryaos_executor_circuit_breakers breaker
                  ON breaker.workspace_id=executor.workspace_id
                 AND breaker.executor_id=executor.executor_id
                WHERE capability_row.workspace_id=$1
                  AND capability_row.expires_at>now()
                  AND executor.expires_at>now()
                  AND (breaker.guarded_until IS NULL OR breaker.guarded_until<=now())
                "#,
            )
            .bind(workspace_uuid)
            .fetch_all(&repo.pool)
            .await
            .map_err(map_sqlx)
        },
        async {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT count(*)
                FROM viryaos_content_sources
                WHERE workspace_id = $1
                  AND active
                  AND expires_at > now()
                "#,
            )
            .bind(workspace_uuid)
            .fetch_one(&repo.pool)
            .await
            .map_err(map_sqlx)
        },
    )?;

    // Same briefing language the overview resolves — one setting for
    // the whole queue rather than a read per row.
    let crew_locale = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_locale'",
    )
    .bind(workspace_uuid)
    .fetch_optional(&repo.pool)
    .await
    .ok()
    .flatten()
    .map_or(
        crowdrelay_application::autopilot::BriefingLocale::default(),
        |tag| crowdrelay_application::autopilot::BriefingLocale::from_tag(&tag),
    );

    let mut pending = Vec::with_capacity(pending_rows.len());
    let mut source_ids = std::collections::BTreeSet::new();
    for row in pending_rows {
        if let Some(raw) = row.payload.get("source_id").and_then(Value::as_str)
            && let Ok(source_id) = Uuid::parse_str(raw)
        {
            source_ids.insert(source_id);
        }
        pending.push(pending_action(row, &live_capabilities, crew_locale)?);
    }

    let source_titles = if source_ids.is_empty() {
        std::collections::BTreeMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT id, title
            FROM viryaos_content_sources
            WHERE workspace_id = $1 AND id = ANY($2)
            "#,
        )
        .bind(workspace_uuid)
        .bind(source_ids.into_iter().collect::<Vec<_>>())
        .fetch_all(&repo.pool)
        .await
        .map_err(map_sqlx)?
        .into_iter()
        .map(|(id, title)| (id.to_string(), title))
        .collect()
    };

    Ok(ContentPipeline {
        live_sources,
        pending,
        source_titles,
    })
}
