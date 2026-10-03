//! The join-ask snapshot, assembled once for every surface that reads it.
//!
//! Two callers need the same facts. The autopilot cycle decides this week's
//! asks from them; the attention board reports what the loop is waiting on.
//! They have to agree — a board that says "connect Instagram" while the cycle
//! is actually held on a missing site URL sends somebody to fix the wrong
//! thing, and is worse than no board at all. So the reads live here once
//! rather than being restated per surface, the same rule
//! `load_blocked_communities` follows for the community queue.

use crowdrelay_domain::join_ask::{
    JoinAskConfig, JoinAskPostRow, JoinAskSnapshot, grounded_starter_variant,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Finds one bounded, current tenant-owned fact that can seed Day-0 copy.
///
/// Explicit `join_ask_variants` always win. This fallback reads only active,
/// unexpired first-party content sources from the last 30 days. Synced owned
/// social captions are preferred over their generated title; release/video/event
/// sources prefer their title. The domain helper preserves that line verbatim
/// and adds only the neutral signup CTA.
pub async fn load_grounded_starter_variant(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        r#"
        SELECT source_kind, title, metadata->>'body'
        FROM content_sources
        WHERE workspace_id = $1
          AND active
          AND source_kind IN ('video', 'release', 'event', 'social_post')
          AND expires_at > now()
          AND occurred_at > now() - interval '30 days'
        ORDER BY occurred_at DESC, id DESC
        LIMIT 12
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    for (source_kind, title, body) in rows {
        let candidates = if source_kind == "social_post" {
            [body.as_deref(), Some(title.as_str())]
        } else {
            [Some(title.as_str()), body.as_deref()]
        };
        for fact in candidates.into_iter().flatten() {
            if let Some(variant) = grounded_starter_variant(fact) {
                return Ok(Some(variant));
            }
        }
    }
    Ok(None)
}

/// Assembles one workspace's join-ask snapshot from scoped first-party reads.
///
/// A tenant that never wrote variants is **not** absent here. It resolves to
/// [`JoinAskConfig::unconfigured`] and then receives one source-derived
/// starter when fresh tenant-owned truth exists. A truly empty tenant still
/// reaches `NoVariants`, so cold start cannot silently disappear or invent
/// copy merely to look healthy.
pub async fn load_join_ask_snapshot(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<JoinAskSnapshot, sqlx::Error> {
    let settings = crate::tenant_settings::TenantSettingsRepository::new(pool.clone());
    let mut config = settings
        .join_ask_config(workspace_id)
        .await?
        .unwrap_or_else(JoinAskConfig::unconfigured);
    if config.variants.is_empty()
        && let Some(starter) = load_grounded_starter_variant(pool, workspace_id).await?
    {
        config.variants.push(starter);
    }

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

    // Only posts this feature filed count toward cadence and learning — a
    // `social_posts` row from an agent draft is a different ledger, and
    // folding it in would let unrelated content steer the tenant's own ask.
    // The action also preserves the exact tenant-authored text, so outcomes
    // survive settings reorder and reset honestly when the wording is edited.
    //
    // `fans_7d` reads the canonical ledger's own assignment: the
    // `last_tracked_click` conversion rows `record_conversion` writes, keyed
    // to this post's action. Any-click joins would credit a fan to every
    // post they clicked before signing up; the ledger picks exactly one
    // (the latest click). The shared organic cohort further requires a real
    // publication/click trace, active marketing consent and no staff/manual/
    // test/seed exclusion. A pending signup is not a fan reward. NULL means the
    // post has no tracked link — unmeasurable, which the selector keeps
    // separate from a measured zero.
    let posts = sqlx::query_as::<
        _,
        (
            String,
            String,
            OffsetDateTime,
            String,
            Option<OffsetDateTime>,
            Option<i64>,
            Option<serde_json::Value>,
        ),
    >(
        r#"
        SELECT post.platform,
               post.status,
               post.created_at,
               COALESCE(action.payload->>'text', '') AS text,
               post.posted_at,
               CASE WHEN post.smart_link_id IS NOT NULL
                    THEN (
                        SELECT COUNT(*)::bigint
                        FROM organic_fan_cohort(post.workspace_id,post.posted_at,
                            post.posted_at + INTERVAL '7 days',now()) AS cohort
                        WHERE cohort.action_id=post.action_id
                          AND cohort.link_id=post.smart_link_id
                          AND cohort.verified AND cohort.contactable AND NOT cohort.excluded
                    )
               END AS fans_7d, decision.input_snapshot->'capture_context' AS capture_context
        FROM social_posts AS post
        JOIN autopilot_actions AS action
          ON action.workspace_id = post.workspace_id
         AND action.id = post.action_id
        LEFT JOIN autopilot_decisions decision ON decision.workspace_id=action.workspace_id AND decision.id=action.decision_id
        WHERE post.workspace_id = $1
          AND action.action_kind = 'social.join_ask.publish'
        ORDER BY post.created_at
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(
        |(platform, status, created_at, text, posted_at, fans_7d, context)| JoinAskPostRow {
            capture_context: context.and_then(|v| serde_json::from_value(v).ok()),
            platform,
            status,
            created_at,
            text,
            posted_at,
            fans_7d: fans_7d.map(|count| u32::try_from(count).unwrap_or(u32::MAX)),
        },
    )
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
        capture_context: config.capture_context,
        variants: config.variants,
        cadence_days: config.cadence_days,
        platforms: config.platforms,
        image_url: config.image_url,
        member_site_base_url,
        social_auto_post: brand.social_auto_post,
        social_autopost_platforms: brand.social_autopost_platforms.clone(),
        connected_platforms,
        posts,
        instagram_photo_count: u32::try_from(instagram_photo_count).unwrap_or(u32::MAX),
    })
}
