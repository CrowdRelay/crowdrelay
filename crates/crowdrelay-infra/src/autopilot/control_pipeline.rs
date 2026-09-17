//! The content page's pipeline read, split out of `control.rs`.
//!
//! Same repository, same bounded reads: pending drafts, live executor
//! capabilities, the live material count, and the titles those drafts
//! cite — the narrow fan-out the page's first paint actually needs.
//! The cockpit-wide overview stays in `control.rs`.

use super::*;
use crowdrelay_application::autopilot::{ContentPipeline, RevisionTrend, RevisionTrendWeek};

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

    // §4d-3.2 — the voice signal alongside the queue: how far the band's
    // edits moved the machine's words, bucketed per week so the panel can
    // draw the direction without a second fetch.
    let revision_rows = sqlx::query_as::<_, (time::Date, i64, i64)>(
        r#"
        SELECT date_trunc('week', created_at)::date AS week_start,
               count(*) AS revised_fields,
               avg(distance_chars)::bigint AS avg_distance_chars
        FROM viryaos_draft_revisions
        WHERE workspace_id = $1
          AND created_at > now() - interval '30 days'
        GROUP BY 1
        ORDER BY 1
        "#,
    )
    .bind(workspace_uuid)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    let revision_trend = if revision_rows.is_empty() {
        // No revised approvals yet — `None`, because 0 would report that the
        // machine writes perfectly when the truth is nobody has edited yet.
        None
    } else {
        let revised_fields_30d: i64 = revision_rows.iter().map(|row| row.1).sum();
        let field_total: i64 = revision_rows
            .iter()
            .map(|row| row.1.saturating_mul(row.2))
            .sum();
        let revised_actions_30d = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(DISTINCT action_id)
            FROM viryaos_draft_revisions
            WHERE workspace_id = $1
              AND created_at > now() - interval '30 days'
            "#,
        )
        .bind(workspace_uuid)
        .fetch_one(&repo.pool)
        .await
        .map_err(map_sqlx)?;
        Some(RevisionTrend {
            revised_fields_30d,
            revised_actions_30d,
            avg_distance_chars_30d: if revised_fields_30d == 0 {
                0
            } else {
                field_total / revised_fields_30d
            },
            weekly: revision_rows
                .into_iter()
                .map(
                    |(week_start, revised_fields, avg_distance_chars)| RevisionTrendWeek {
                        week_start: week_start.to_string(),
                        revised_fields,
                        avg_distance_chars,
                    },
                )
                .collect(),
        })
    };

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
        revision_trend,
    })
}
