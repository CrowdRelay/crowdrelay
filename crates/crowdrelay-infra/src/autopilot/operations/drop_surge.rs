//! Drop-surge support: the per-lane tracked link and the synthetic task row
//! a brain-drafted channel post needs.
//!
//! A fresh video fans out to every owned lane at once (see
//! `candidates_drop_surge.rs`). Two execution details live here because both
//! are SQL and both are shared by three action arms: each lane's `/l/` slug
//! must exist before the channel executor binds it at materialize time, and
//! a deterministic draft still needs an `agent_service_tasks` row for the
//! executor's template join.

use super::*;

/// Mints (or refreshes) the tracked link a surge lane points at, bound to a
/// per-source campaign so clicks and signups attribute to the drop.
///
/// The slug is derived in the domain (`drop_surge_link_slug`) and the
/// candidate already composed `cta_url` from it — execution only makes the
/// row real. `None` when the source row or its URL is gone or un-linkable;
/// callers then send with a bare destination rather than blocking the lane.
pub(in crate::autopilot) async fn ensure_drop_surge_link(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    source_id: ContentSourceId,
    lane: &str,
) -> Result<Option<String>, RepositoryError> {
    let source = sqlx::query_as::<_, (String, String, Option<String>)>(
        r#"
        SELECT source_key, title, metadata->>'url'
        FROM content_sources
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let Some((source_key, title, url)) = source else {
        return Ok(None);
    };
    let Some(destination) = url.filter(|url| is_http_url(url)) else {
        return Ok(None);
    };
    let Some(slug) = crowdrelay_domain::content_supply::drop_surge_link_slug(&source_key, lane)
    else {
        return Ok(None);
    };

    // One campaign per source keeps every lane's clicks under the drop's own
    // name — `{title} · drop`. Find-or-create by name: campaigns has no
    // source column, and two sources sharing a title sharing a campaign is a
    // display grouping, not a misattribution — the links still carry
    // per-source slugs.
    let campaign_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH existing AS (
            SELECT id FROM campaigns
            WHERE workspace_id = $1 AND name = $3 AND active
            ORDER BY created_at
            LIMIT 1
        ), inserted AS (
            INSERT INTO campaigns (workspace_id, name, active)
            SELECT $1, $3, true
            WHERE NOT EXISTS (SELECT 1 FROM existing)
            RETURNING id
        )
        SELECT id FROM inserted UNION ALL SELECT id FROM existing LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id.into_uuid())
    .bind(format!("{} · drop", title.trim()))
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO smart_links
            (workspace_id, slug, destination_url, campaign_id, active,
             channel_source, channel_community, channel_creative)
        VALUES ($1, $2, $3, $4, true, 'drop_surge', $5, $6)
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            campaign_id = EXCLUDED.campaign_id,
            channel_source = EXCLUDED.channel_source,
            channel_community = EXCLUDED.channel_community,
            channel_creative = EXCLUDED.channel_creative,
            active = true
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(destination)
    .bind(campaign_id)
    .bind(lane)
    .bind(source_key)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(Some(slug))
}

/// What a deterministic channel draft needs before it may succeed: the
/// synthetic task row the channel executors join on, then the lane's
/// tracked `/l/` link. Both keyed from the draft's own `drop_surge` marker
/// — the marker is what tells an `agent.content.request` action apart from
/// a model-drafted one, which already has its task row.
///
/// Returns `false` when the draft is not a surge draft or its marker is
/// malformed — callers then run the ordinary model-draft path unchanged.
pub(in crate::autopilot) async fn materialize_surge_draft(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    task_id: Uuid,
    template_id: Option<&str>,
    draft: &serde_json::Value,
) -> Result<bool, RepositoryError> {
    let Some(surge) = draft.get("drop_surge") else {
        return Ok(false);
    };
    let lane = surge
        .get("lane")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Some(source) = surge
        .get("source_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
    else {
        return Ok(false);
    };
    let Some(template) = template_id else {
        return Ok(false);
    };
    materialize_surge_task(tx, workspace_id, action_id, task_id, template, source, lane).await?;
    ensure_drop_surge_link(tx, workspace_id, ContentSourceId::from_uuid(source), lane).await?;
    Ok(true)
}

/// The `agent_service_tasks` row a deterministic channel draft joins on.
///
/// Telegram, Discord and the social executor all find work by joining the
/// action's `task_id` to this table for the template's name. A drop-surge
/// draft is composed by the brain, not a model, so nothing upstream created
/// the row — this writes it as already `completed` (the scheduler only
/// claims `queued`), with `model_id = 'deterministic'` so the audit trail
/// says plainly who wrote the copy. `ON CONFLICT DO NOTHING` keeps a real
/// model task — or an earlier lane's row — untouched.
pub(in crate::autopilot) async fn materialize_surge_task(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    task_id: Uuid,
    template_id: &str,
    source_id: Uuid,
    lane: &str,
) -> Result<(), RepositoryError> {
    // Same probe `execute_agent_run` runs: without the agent service's table
    // there is nothing to materialize into, and failing loudly beats a draft
    // that reports success but can never be claimed by an executor.
    if !sqlx::query_scalar::<_, bool>("SELECT to_regclass('agent_service_tasks') IS NOT NULL")
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?
    {
        return Err(RepositoryError::ConflictBecause(
            crowdrelay_application::autopilot::AutopilotMeasurementKind::NO_AGENT_SERVICE,
        ));
    }
    let trace_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT trace_id FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .flatten();
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata, completed_at)
        VALUES ($1, $2, $3, 'deterministic', $4, 'completed', 'basic', $5, now())
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(task_id)
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(
        "drop surge — copy composed deterministically from the source's own title and description",
    )
    .bind(serde_json::json!({
        "source": "drop_surge",
        "action_id": action_id.into_uuid(),
        "trace_id": trace_id,
        "source_id": source_id,
        "lane": lane,
    }))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
