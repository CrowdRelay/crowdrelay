//! The brain's starting belief, read from what this tenant's dispatches did.
//!
//! The causal model learns only from resolved evidence, and until some
//! resolves every prediction it makes is its prior. The compiled-in prior is
//! two fans per dispatch. On 2026-10-03 production had thirteen published,
//! tracked dispatches and twenty active fans, none traceable to any of them —
//! the band's own and direct invites — so the brain was ranking, dispatching
//! and reporting confidence against a number the tenant's own record already
//! contradicted. This reads that record so the model starts from it.
//!
//! First-party rows only, and the same rule the North Star uses: a fan counts
//! only when a conversion row ties them to an action, and only while they are
//! still an active fan. A suppressed or deleted conversion is not a fan the
//! tenant has.

use super::*;
use crowdrelay_brain::CausalModel;

/// A model whose fan prior is this tenant's realized yield, or the default
/// while there are too few deliveries to say anything.
///
/// A read failure keeps the default rather than failing the cycle: the prior
/// is a starting belief and the evidence replay that follows still corrects it.
pub(super) async fn seeded_model(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> CausalModel {
    match realized_record(repo, workspace_id).await {
        Ok((delivered, attributed)) => {
            let model = CausalModel::with_realized_yield(delivered, attributed);
            tracing::info!(
                delivered,
                attributed_fans = attributed,
                prior_mean_fans = model.fans.global.mean(),
                "causal model prior seeded from the tenant's realized yield"
            );
            model
        }
        Err(error) => {
            tracing::warn!(%error, "realized yield unreadable; compiled-in prior kept");
            CausalModel::default()
        }
    }
}

/// `(published tracked dispatches, active fans attributed to an action)`.
async fn realized_record(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<(u32, u32), RepositoryError> {
    let (delivered, attributed): (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM (
                SELECT 1 FROM community_posts
                 WHERE workspace_id = $1 AND status = 'posted' AND smart_link IS NOT NULL
                UNION ALL
                SELECT 1 FROM social_posts
                 WHERE workspace_id = $1 AND status = 'posted' AND smart_link IS NOT NULL
                UNION ALL
                SELECT 1 FROM telegram_posts
                 WHERE workspace_id = $1 AND status = 'posted'
                UNION ALL
                SELECT 1 FROM discord_posts
                 WHERE workspace_id = $1 AND status = 'posted'
            ) AS delivered),
            (SELECT count(DISTINCT fan.id)
               FROM fan_provenance_events event
               JOIN fans fan
                 ON fan.workspace_id = event.workspace_id AND fan.id = event.fan_id
              WHERE event.workspace_id = $1
                AND event.event_kind = 'conversion'
                AND event.action_id IS NOT NULL
                AND fan.status = 'active'
                AND fan.deleted_at IS NULL
                AND fan.merged_into_fan_id IS NULL)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    Ok((
        u32::try_from(delivered).unwrap_or(u32::MAX),
        u32::try_from(attributed).unwrap_or(u32::MAX),
    ))
}
