//! Loading the communities the brain may engage, with what is known about them.
//!
//! This used to be three columns off `agent_outreach_targets`, ordered by
//! `created_at DESC LIMIT 20`. That had three separate problems and they
//! compounded:
//!
//! - The pool was capped by recency, so the twenty-first best community could
//!   never become a candidate however good it was.
//! - "Unengaged" filtered on `status = 'promoted'` and nothing else, so a
//!   community posted to yesterday was still offered as untouched.
//! - The audience graph already held member counts, activity, genres and each
//!   community's own promotion rules, and none of it reached the brain — so
//!   every candidate in a genre bucket carried an identical predicted value
//!   and the portfolio optimizer was choosing among them at random.
//!
//! The loader now reads the graph alongside the target, excludes what
//! screening refused, reports how long it has been since each community was
//! last posted to, and orders by measured size so the cap bites on the
//! weakest candidates rather than the oldest ones.

use crowdrelay_brain::UnengagedTarget;
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;

use super::{RepositoryError, map_sqlx};

/// How many communities the brain considers per cycle.
///
/// This is a working-set bound, not a policy: the portfolio optimizer applies
/// the real budget. It is larger than the old twenty because a randomized
/// holdout needs a pool it can split, and because ordering by value means the
/// tail this drops is the part worth dropping.
const MAX_COMMUNITY_CANDIDATES: i64 = 50;

/// Row shape of the community candidate query.
type CommunityTargetRow = (
    uuid::Uuid,
    String,
    String,
    Option<i32>,
    Option<i32>,
    Vec<String>,
    Option<i16>,
    Option<i16>,
    Option<i32>,
);

/// Loads the communities the growth loop may engage this cycle.
///
/// Refused targets are excluded outright — a refusal is a durable judgement,
/// not a ranking penalty the optimizer could outbid. Targets the audience
/// graph has not matched yet are still included: an unmeasured community is a
/// real candidate with weak evidence, and the causal model is what decides
/// what it is worth.
pub(super) async fn load_community_targets(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<UnengagedTarget>, RepositoryError> {
    let rows: Vec<CommunityTargetRow> = sqlx::query_as(
        r#"
        SELECT t.id,
               t.display_name,
               t.subreddit,
               place.member_count,
               place.activity_bp,
               COALESCE(place.genres, ARRAY[]::text[]) AS genres,
               rules.self_promo_ratio_percent,
               rules.cooldown_days,
               last_post.days_since
        FROM agent_outreach_targets AS t
        LEFT JOIN discovery_places AS place
               ON place.id = t.place_id
              AND place.workspace_id = t.workspace_id
        LEFT JOIN discovery_place_rules AS rules
               ON rules.place_id = place.id
        LEFT JOIN LATERAL (
            SELECT (EXTRACT(EPOCH FROM (now() - MAX(cp.posted_at))) / 86400)::int AS days_since
            FROM community_posts cp
            WHERE cp.workspace_id = t.workspace_id
              AND lower(cp.subreddit) = lower(t.subreddit)
              AND cp.posted_at IS NOT NULL
        ) AS last_post ON true
        WHERE t.workspace_id = $1
          AND t.status = 'promoted'
          AND t.target_kind = 'community'
          AND t.subreddit IS NOT NULL
          -- A refusal recorded at ingest keeps the community out of the pool
          -- permanently. NULL means the row predates screening: unscreened is
          -- not the same as refused, so it still competes.
          AND t.screening_verdict IS DISTINCT FROM 'refused'
          -- Our own recorded judgement about the place overrides the target
          -- row, which is what makes blocking a community in the console take
          -- effect on the next cycle rather than the next scan.
          AND (place.id IS NULL
               OR (place.status = 'active'
                   AND place.membership_state NOT IN ('rejected', 'not_a_fit')))
        -- Biggest measured audience first. An unmeasured community sorts last
        -- rather than first: it may be excellent, but the cap has to fall on
        -- the least evidenced candidates, not the most recent ones.
        ORDER BY place.member_count DESC NULLS LAST, t.created_at DESC
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_COMMUNITY_CANDIDATES)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    Ok(rows
        .into_iter()
        .map(
            |(
                target_id,
                display_name,
                subreddit,
                member_count,
                activity_bp,
                genres,
                self_promo_ratio_percent,
                cooldown_days,
                days_since_last_engagement,
            )| UnengagedTarget {
                target_id,
                display_name,
                subreddit,
                member_count: member_count.and_then(|v| u32::try_from(v).ok()),
                activity_basis_points: activity_bp.and_then(|v| u16::try_from(v).ok()),
                genres,
                self_promo_ratio_percent: self_promo_ratio_percent
                    .and_then(|v| u8::try_from(v).ok()),
                cooldown_days: cooldown_days.and_then(|v| u16::try_from(v).ok()),
                days_since_last_engagement: days_since_last_engagement
                    .and_then(|v| u32::try_from(v).ok()),
            },
        )
        .collect())
}
