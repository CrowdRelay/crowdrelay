//! The join-ask snapshot, assembled once for every surface that reads it.
//!
//! Two callers need the same facts. The autopilot cycle decides this week's
//! asks from them; the attention board reports what the loop is waiting on.
//! They have to agree — a board that says "connect Instagram" while the cycle
//! is actually held on a missing site URL sends somebody to fix the wrong
//! thing, and is worse than no board at all. So the reads live here once
//! rather than being restated per surface, the same rule
//! `load_blocked_communities` follows for the community queue.

use crowdrelay_domain::join_ask::{JoinAskConfig, JoinAskPostRow, JoinAskSnapshot};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Assembles one workspace's join-ask snapshot from six small scoped reads.
///
/// A tenant that never wrote variants is **not** absent here. It resolves to
/// [`JoinAskConfig::unconfigured`] — empty words, default platforms — so the
/// cold-start case reaches the gates and is reported as held. The earlier
/// shape returned `None` for that tenant and the cycle skipped the context
/// without recording anything, which made a brand-new workspace read exactly
/// like an evaluator that never ran. That is the one outcome `JoinAskHold`
/// exists to prevent, and the emptiest tenants were the ones getting it.
pub async fn load_join_ask_snapshot(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<JoinAskSnapshot, sqlx::Error> {
    let settings = crate::tenant_settings::TenantSettingsRepository::new(pool.clone());
    let config = settings
        .join_ask_config(workspace_id)
        .await?
        .unwrap_or_else(JoinAskConfig::unconfigured);

    // The site URL and the standing publish approval ride the brand seam, not
    // a raw row read, so every consumer builds links the same way. A tenant
    // that never set a site URL, or blanked it, maps to `None` — the domain's
    // `NoSiteUrl` hold rather than a link with no destination (or, while the
    // default was the first tenant's site, another band's).
    let brand = settings.brand_settings(workspace_id).await?;
    let member_site_base_url = brand.site_root().map(str::to_owned);

    let connected_platforms = sqlx::query_scalar::<_, String>(
        r#"
        SELECT platform FROM fanbase_connections
        WHERE workspace_id = $1 AND status = 'connected'
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    // Only posts this feature filed count toward cadence and rotation — a
    // `social_posts` row from an agent draft is a different ledger, and
    // folding it in would let an LLM post delay the tenant's own ask.
    let posts = sqlx::query_as::<_, (String, String, OffsetDateTime)>(
        r#"
        SELECT post.platform, post.status, post.created_at
        FROM social_posts AS post
        JOIN autopilot_actions AS action
          ON action.workspace_id = post.workspace_id
         AND action.id = post.action_id
        WHERE post.workspace_id = $1
          AND action.action_kind = 'social.join_ask.publish'
        ORDER BY post.created_at
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(platform, status, created_at)| JoinAskPostRow {
        platform,
        status,
        created_at,
    })
    .collect();

    // Instagram has no text-only post: the executor's selector reads the
    // tenant's own press assets, so the eligibility check counts the same pool
    // it would publish from — photos and logos both satisfy the executor's
    // `asset_kind IN ('photo','logo')`.
    let instagram_photo_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*) FROM beacon_press_assets
        WHERE workspace_id = $1 AND active AND asset_kind IN ('photo', 'logo')
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    Ok(JoinAskSnapshot {
        variants: config.variants,
        cadence_days: config.cadence_days,
        platforms: config.platforms,
        image_url: config.image_url,
        member_site_base_url,
        social_auto_post: brand.social_auto_post,
        connected_platforms,
        posts,
        instagram_photo_count: u32::try_from(instagram_photo_count).unwrap_or(u32::MAX),
    })
}
