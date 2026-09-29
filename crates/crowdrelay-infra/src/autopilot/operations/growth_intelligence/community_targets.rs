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
#[allow(clippy::type_complexity)]
type CommunityTargetRow = (
    uuid::Uuid,
    String,
    String,
    String,
    Option<String>,
    Option<i32>,
    Option<i32>,
    Vec<String>,
    Option<i16>,
    Option<i16>,
    Option<i32>,
    Option<String>,
    i64,
    i64,
    i64,
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
               -- The platform the target is, not the channel the registry
               -- began on: older Reddit rows carry an empty platform.
               COALESCE(NULLIF(t.platform, ''), 'reddit') AS platform,
               -- The community address the ledger and the drafter know it
               -- by: the subreddit slug on Reddit, the community's display
               -- name anywhere else.
               COALESCE(t.subreddit, t.display_name) AS community,
               t.community_url,
               place.member_count,
               place.activity_bp,
               COALESCE(place.genres, ARRAY[]::text[]) AS genres,
               rules.self_promo_ratio_percent,
               rules.cooldown_days,
               last_post.days_since,
               place.membership_state,
               provenance.converted_fans,
               provenance.interactions,
               durable.durable_fans
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
              AND cp.platform = COALESCE(NULLIF(t.platform, ''), 'reddit')
              AND normalize_subreddit(cp.subreddit) =
                  normalize_subreddit(COALESCE(t.subreddit, t.display_name))
              AND cp.posted_at IS NOT NULL
        ) AS last_post ON true
        -- The community's own conversion record: fans this community's
        -- tracked links produced, and the distinct visitors clicking them.
        -- This is the edge member_count never had — a community that made a
        -- fan is evidence about where this band's fans come from, and it
        -- outranks a bigger community that only ever produced reach.
        LEFT JOIN LATERAL (
            SELECT COUNT(DISTINCT pe.fan_id)
                       FILTER (WHERE pe.event_kind = 'conversion')::bigint
                       AS converted_fans,
                   COUNT(DISTINCT COALESCE(pe.fan_id::text,
                                           pe.anonymous_visitor_id::text))
                       FILTER (WHERE pe.event_kind = 'interaction')::bigint
                       AS interactions
            FROM fan_provenance_events pe
            WHERE pe.workspace_id = t.workspace_id
              -- Each channel's evidence names its communities its own way —
              -- a subreddit slug, a chat name. A chat called 'deathcore'
              -- must not credit r/deathcore with conversions the chat made,
              -- so the match is platform-scoped, not just name-scoped.
              AND pe.channel = COALESCE(NULLIF(t.platform, ''), 'reddit')
              AND normalize_subreddit(pe.community) =
                  normalize_subreddit(COALESCE(t.subreddit, t.display_name))
              AND pe.occurred_at >= now() - interval '90 days'
        ) AS provenance ON true
        -- Of those fans, the ones who stayed: still active, still consented
        -- to hear from the band, and did something meaningful in the last
        -- 30 days — the acquisition-channel read's "activated" definition.
        -- A community whose conversions all went quiet produced signups,
        -- not fans, and a signup farm must not outrank a slower community
        -- whose people are still here.
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS durable_fans
            FROM fans AS fan
            WHERE fan.workspace_id = t.workspace_id
              AND fan.status = 'active'
              AND fan.id IN (
                  SELECT pe.fan_id
                  FROM fan_provenance_events pe
                  WHERE pe.workspace_id = t.workspace_id
                    AND pe.event_kind = 'conversion'
                    AND pe.channel = COALESCE(NULLIF(t.platform, ''), 'reddit')
                    AND pe.fan_id IS NOT NULL
                    AND normalize_subreddit(pe.community) =
                        normalize_subreddit(COALESCE(t.subreddit, t.display_name))
                    AND pe.occurred_at >= now() - interval '90 days'
              )
              AND EXISTS (
                  SELECT 1 FROM fan_consents AS consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
                    AND consent.recorded_at = (
                        SELECT max(latest.recorded_at) FROM fan_consents AS latest
                        WHERE latest.workspace_id = fan.workspace_id
                          AND latest.fan_id = fan.id
                          AND latest.purpose = 'marketing'
                    )
              )
              AND fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                  >= now() - interval '30 days'
        ) AS durable ON true
        WHERE t.workspace_id = $1
          AND t.status = 'promoted'
          AND t.target_kind = 'community'
          -- A community needs an address to post at: the subreddit slug, or
          -- for every other platform the community's name/URL to publish
          -- against. Neither is evidence — a name we cannot route to is not
          -- a candidate.
          AND (t.subreddit IS NOT NULL
               OR (COALESCE(NULLIF(t.platform, ''), 'reddit') <> 'reddit'
                   AND (t.display_name IS NOT NULL OR t.community_url IS NOT NULL)))
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
        -- Evidence before audience: a community whose fans stayed leads,
        -- then one whose links produced a fan or a clicker, then biggest measured audience as the
        -- tiebreak. An unmeasured community sorts last rather than first: it
        -- may be excellent, but the cap has to fall on the least evidenced
        -- candidates, not the most recent ones.
        ORDER BY durable.durable_fans DESC,
                 provenance.converted_fans DESC,
                 provenance.interactions DESC,
                 place.member_count DESC NULLS LAST,
                 t.created_at DESC,
                 -- Full tie-break: two targets admitted in the same second
                 -- with identical evidence still need a stable order or the
                 -- LIMIT cut lands on a coin flip between cycles.
                 t.id
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
                platform,
                community,
                community_url,
                member_count,
                activity_bp,
                genres,
                self_promo_ratio_percent,
                cooldown_days,
                days_since_last_engagement,
                membership_state,
                converted_fans,
                interactions,
                durable_fans,
            )| UnengagedTarget {
                target_id,
                display_name,
                platform,
                subreddit: community,
                community_url,
                member_count: member_count.and_then(|v| u32::try_from(v).ok()),
                activity_basis_points: activity_bp.and_then(|v| u16::try_from(v).ok()),
                genres,
                self_promo_ratio_percent: self_promo_ratio_percent
                    .and_then(|v| u8::try_from(v).ok()),
                cooldown_days: cooldown_days.and_then(|v| u16::try_from(v).ok()),
                days_since_last_engagement: days_since_last_engagement
                    .and_then(|v| u32::try_from(v).ok()),
                // `None` when there is no place row at all — an older target
                // that predates membership tracking. Unknown is not the same
                // as not joined, and the candidate gate treats the two
                // differently on purpose.
                joined: membership_state.map(|state| state == "joined"),
                converted_fans_90d: u32::try_from(converted_fans.max(0)).unwrap_or(u32::MAX),
                interactions_90d: u32::try_from(interactions.max(0)).unwrap_or(u32::MAX),
                durable_fans_90d: u32::try_from(durable_fans.max(0)).unwrap_or(u32::MAX),
            },
        )
        .collect())
}
