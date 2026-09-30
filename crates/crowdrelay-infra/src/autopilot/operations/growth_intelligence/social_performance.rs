//! Recent owned-social posts with first-party fan outcomes for fresh drafts.

use super::*;
use crowdrelay_brain::SocialContentPerformance;
use crowdrelay_domain::hook_scorecard::parse_social_count;

/// A recent owned-social post with first-party fan outcomes. Kept distinct
/// from attention metrics so the next draft can learn what created people in
/// the fan graph rather than merely what accumulated engagement.
#[derive(Debug, sqlx::FromRow)]
struct SocialPerformanceRow {
    platform: String,
    media_type: Option<String>,
    body: Option<String>,
    reach: Option<String>,
    fans_acquired: i64,
    fans_activated_within_30d: i64,
}

pub(super) async fn load_social_content_history(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<SocialContentPerformance>, RepositoryError> {
    // Owned-social examples for the next social draft. This mirrors the hook
    // scorecard's first-party attribution: action -> last-tracked-click
    // conversion -> fan. Only posts that actually acquired at least one fan
    // ride the prompt, bounded to five examples from the same 60-day window.
    // Activated fans outrank signup-only examples. Historical activity in the
    // acquisition window survives a later return outside that window.
    let social_rows: Vec<SocialPerformanceRow> = sqlx::query_as(
        r#"
        SELECT source.metadata->>'platform' AS platform,
               source.metadata->>'media_type' AS media_type,
               source.metadata->>'body' AS body,
               source.metadata->>'reach' AS reach,
               count(DISTINCT provenance.fan_id)::bigint AS fans_acquired,
               count(DISTINCT provenance.fan_id) FILTER (
                   WHERE fan.status = 'active'
                     AND consent.granted
                     AND fan_has_meaningful_action_between(
                         fan.workspace_id, fan.id, fan.normalized_email,
                         fan.created_at,
                         LEAST($2, fan.created_at + interval '30 days')
                     )
               )::bigint AS fans_activated_within_30d
        FROM content_sources AS source
        JOIN autopilot_actions AS action
          ON action.workspace_id = source.workspace_id
         AND (
              lower(action.payload->>'source_id') = source.id::text
              OR lower(action.payload->'draft'->>'source_id') = source.id::text
         )
        JOIN fan_provenance_events AS provenance
          ON provenance.workspace_id = action.workspace_id
         AND provenance.action_id = action.id
         AND provenance.event_kind = 'conversion'
         AND provenance.attribution_method = 'last_tracked_click'
        JOIN fans AS fan
          ON fan.workspace_id = provenance.workspace_id
         AND fan.id = provenance.fan_id
        LEFT JOIN LATERAL (
            SELECT latest.granted
            FROM fan_consents AS latest
            WHERE latest.workspace_id = fan.workspace_id
              AND latest.fan_id = fan.id
              AND latest.purpose = 'marketing'
              AND latest.recorded_at <= $2
            ORDER BY latest.recorded_at DESC, latest.id DESC
            LIMIT 1
        ) AS consent ON true
        WHERE source.workspace_id = $1
          AND source.source_kind = 'social_post'
          AND source.occurred_at >= $2 - interval '60 days'
          AND source.occurred_at <= $2
          AND provenance.occurred_at >= $2 - interval '60 days'
          AND provenance.occurred_at <= $2
          AND provenance.occurred_at >= source.occurred_at
          AND provenance.occurred_at < source.occurred_at + interval '14 days'
          AND source.metadata->>'platform' IS NOT NULL
        GROUP BY source.id, source.metadata->>'platform',
                 source.metadata->>'media_type', source.metadata->>'body',
                 source.metadata->>'reach', source.occurred_at
        HAVING count(DISTINCT provenance.fan_id) > 0
        ORDER BY fans_activated_within_30d DESC, fans_acquired DESC,
                 source.occurred_at DESC
        LIMIT 5
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    Ok(social_rows
        .into_iter()
        .map(|row| {
            let reach = parse_social_count(row.reach.as_deref())
                .and_then(|value| u64::try_from(value).ok());
            let fans_acquired = u32::try_from(row.fans_acquired.max(0)).unwrap_or(u32::MAX);
            let fans_activated_within_30d =
                u32::try_from(row.fans_activated_within_30d.max(0)).unwrap_or(u32::MAX);
            let fan_conversion_per_1000_reach = reach.filter(|value| *value > 0).map(|value| {
                u32::try_from((u64::from(fans_acquired) * 1_000 / value).min(u64::from(u32::MAX)))
                    .unwrap_or(u32::MAX)
            });
            let opening = row.body.as_deref().and_then(|body| {
                body.lines()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| line.chars().take(160).collect::<String>())
            });
            SocialContentPerformance {
                platform: row.platform,
                media_type: row.media_type,
                opening,
                reach,
                fans_acquired,
                fans_activated_within_30d,
                fan_conversion_per_1000_reach,
            }
        })
        .collect())
}
