//! The content-supply snapshot: each active source with the artifacts it has,
//! has in flight, and has failed to get.

use super::*;
use crowdrelay_domain::content_supply::{DropSurgeLaneFailure, FailedArtifact, PostResonance};

#[derive(Debug, FromRow)]
struct ContentRow {
    source_id: Uuid,
    source_kind: String,
    source_version: i64,
    occurred_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    title: String,
    source_key: String,
    source_url: Option<String>,
    source_body: Option<String>,
    source_thumbnail_url: Option<String>,
    member_site_base_url: Option<String>,
    post_url: Option<String>,
    post_platform: Option<String>,
    post_body: Option<String>,
    post_media_url: Option<String>,
    post_media_id: Option<String>,
    post_media_type: Option<String>,
    post_thumbnail_url: Option<String>,
    post_engagement: Option<i64>,
    peer_median: Option<i64>,
    peer_count: i64,
    post_rate: Option<i64>,
    post_watch_ms: Option<i64>,
    peer_rate_median: Option<i64>,
    rate_peer_count: i64,
    peer_watch_median: Option<i64>,
    communication_enabled: Option<bool>,
    press_enabled: Option<bool>,
    release_tier: Option<String>,
    completed_artifacts: Vec<String>,
    inflight_artifacts: Vec<String>,
    failed_artifact_kinds: Vec<String>,
    failed_artifact_counts: Vec<i64>,
    failed_artifact_last: Vec<OffsetDateTime>,
    surge_lane_names: Vec<String>,
    surge_lane_counts: Vec<i64>,
    surge_lane_last: Vec<OffsetDateTime>,
    surge_requested_at: Option<OffsetDateTime>,
}

pub(in crate::autopilot) async fn load_content_supply_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<ContentSupplySnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, ContentRow>(
        r#"
        SELECT
            source.id AS source_id,
            source.source_kind,
            source.version AS source_version,
            source.occurred_at,
            source.expires_at,
            source.title,
            source.source_key,
            -- The drop surge reads the source's own destination and words:
            -- a video's YouTube URL and description. Only meaningful on
            -- `video`/`release` sources — a synced post's URL is its relay
            -- permalink, read through `post_url` instead.
            CASE WHEN source.source_kind IN ('video', 'release')
                 THEN source.metadata->>'url' END AS source_url,
            CASE WHEN source.source_kind IN ('video', 'release')
                 THEN source.metadata->>'body' END AS source_body,
            -- The video's thumbnail for post drafts: stored when the sync
            -- wrote one, else derived from the YouTube video id the feed
            -- always carries. Other origins leave it NULL — a draft posts
            -- without art rather than with a guess.
            CASE WHEN source.source_kind = 'video'
                 THEN COALESCE(
                     source.metadata->>'thumbnail_url',
                     CASE WHEN source.metadata->>'video_id' ~ '^[A-Za-z0-9_-]{6,}$'
                          THEN 'https://i.ytimg.com/vi/'
                               || (source.metadata->>'video_id')
                               || '/hqdefault.jpg' END
                 ) END AS source_thumbnail_url,
            -- The tenant's public site origin, needed to compose the
            -- absolute tracked URL the email lane's copy carries. One row
            -- serves every source; the scalar subquery is cheaper than a
            -- second round-trip.
            (SELECT setting.value
             FROM tenant_settings AS setting
             WHERE setting.workspace_id = source.workspace_id
               AND setting.key = 'member_site_base_url') AS member_site_base_url,
            -- The synced post's own fields — only meaningful on a
            -- `social_post` source, where the relay reads them verbatim.
            -- On every other kind they ride along unused.
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'url' END AS post_url,
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'platform' END AS post_platform,
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'body' END AS post_body,
            -- The post's media, same provenance as the caption: the CDN URL
            -- the sync stored plus the Graph id that re-mints it fresh at
            -- post time (signed URLs expire).
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'media_url' END AS post_media_url,
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'media_id' END AS post_media_id,
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'media_type' END AS post_media_type,
            CASE WHEN source.source_kind = 'social_post'
                 THEN source.metadata->>'thumbnail_url' END AS post_thumbnail_url,
            -- How the post landed at home (weighted engagement the sync
            -- stores), against the median of the same account's earlier posts.
            CASE WHEN source.source_kind = 'social_post'
                  AND jsonb_typeof(source.metadata->'engagement') = 'number'
                 THEN (source.metadata->>'engagement')::bigint END AS post_engagement,
            peers.peer_median,
            COALESCE(peers.peer_count, 0) AS peer_count,
            -- Engagement per thousand reached, and average watch time, when
            -- the platform's insights reported them (Instagram).
            CASE WHEN source.source_kind = 'social_post'
                  AND jsonb_typeof(source.metadata->'engagement') = 'number'
                  AND jsonb_typeof(source.metadata->'reach') = 'number'
                  AND (source.metadata->>'reach')::bigint > 0
                 THEN (source.metadata->>'engagement')::bigint * 1000
                      / (source.metadata->>'reach')::bigint END AS post_rate,
            CASE WHEN source.source_kind = 'social_post'
                  AND jsonb_typeof(source.metadata->'avg_watch_ms') = 'number'
                 THEN (source.metadata->>'avg_watch_ms')::bigint END AS post_watch_ms,
            peers.peer_rate_median,
            COALESCE(peers.rate_peer_count, 0) AS rate_peer_count,
            peers.peer_watch_median,
            -- The three switches are release-plan vocabulary, so the read is
            -- scoped to release rows: a video or event whose own metadata
            -- happens to carry a `tier` key must not inherit release gating.
            -- The typeof guard keeps a non-boolean value (the metadata column
            -- is operator-writable) from failing the whole snapshot load.
            CASE WHEN source.source_kind = 'release'
                  AND jsonb_typeof(source.metadata->'communication_enabled') = 'boolean'
                 THEN (source.metadata->>'communication_enabled')::boolean
            END AS communication_enabled,
            CASE WHEN source.source_kind = 'release'
                  AND jsonb_typeof(source.metadata->'press_enabled') = 'boolean'
                 THEN (source.metadata->>'press_enabled')::boolean
            END AS press_enabled,
            CASE WHEN source.source_kind = 'release'
                 THEN source.metadata->>'tier'
            END AS release_tier,
            COALESCE(ARRAY(
                SELECT DISTINCT action.payload->>'artifact'
                FROM autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.subject_id = source.id
                  -- A shared subject row can carry an action that is not an
                  -- artifact build (the assignment emails the approval queue
                  -- sends are content_supply actions on the same source).
                  -- Without the guard their NULL element fails the Vec<String>
                  -- decode and takes the whole snapshot load with it.
                  AND action.payload ? 'artifact'
                  AND action.status = 'succeeded'
                  -- "Completed" means the artifact landed somewhere, not
                  -- that a notification webhook accepted the request. The
                  -- executor's terminal report must carry
                  -- `metadata.artifact_delivery` with a non-blank
                  -- url/surface/reference — the 2026-09-28 drop measured
                  -- what counting a Discord-notify 200 as delivery costs:
                  -- fifty-nine requests "completed" and zero artifacts
                  -- ever reached a fan-facing surface.
                  AND EXISTS (
                      SELECT 1
                      FROM autopilot_execution_reports AS report
                      WHERE report.workspace_id = action.workspace_id
                        AND report.action_id = action.id
                        AND report.status = 'succeeded'
                        AND COALESCE(
                            NULLIF(btrim(report.metadata->'artifact_delivery'->>'url'), ''),
                            NULLIF(btrim(report.metadata->'artifact_delivery'->>'surface'), ''),
                            NULLIF(btrim(report.metadata->'artifact_delivery'->>'reference'), '')
                        ) IS NOT NULL
                  )
            ), ARRAY[]::text[]) AS completed_artifacts,
            COALESCE(ARRAY(
                SELECT DISTINCT action.payload->>'artifact'
                FROM autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.subject_id = source.id
                  AND action.payload ? 'artifact'
                  AND (
                      action.status IN ('awaiting_approval','queued','processing')
                      OR (
                          action.status = 'succeeded'
                          AND EXISTS (
                              SELECT 1
                              FROM autopilot_action_emissions AS emission
                              WHERE emission.workspace_id = action.workspace_id
                                AND emission.action_id = action.id
                          )
                          AND NOT EXISTS (
                              SELECT 1
                              FROM autopilot_execution_reports AS report
                              WHERE report.workspace_id = action.workspace_id
                                AND report.action_id = action.id
                                AND report.status IN ('succeeded','failed')
                          )
                      )
                  )
            ), ARRAY[]::text[]) AS inflight_artifacts,
            COALESCE(failed.kinds, ARRAY[]::text[]) AS failed_artifact_kinds,
            COALESCE(failed.counts, ARRAY[]::bigint[]) AS failed_artifact_counts,
            COALESCE(failed.last_failed, ARRAY[]::timestamptz[]) AS failed_artifact_last,
            COALESCE(surge_failed.lanes, ARRAY[]::text[]) AS surge_lane_names,
            COALESCE(surge_failed.counts, ARRAY[]::bigint[]) AS surge_lane_counts,
            COALESCE(surge_failed.last_failed, ARRAY[]::timestamptz[]) AS surge_lane_last,
            -- The operator's explicit promote stamp (`…/promote` writes it).
            -- A non-timestamp value read back as NULL rather than failing
            -- the whole supply load.
            CASE WHEN source.metadata->>'surge_requested_at'
                      ~ '^\d{4}-\d{2}-\d{2}T'
                 THEN (source.metadata->>'surge_requested_at')::timestamptz
            END AS surge_requested_at
        FROM content_sources AS source
        -- The account's own normal: earlier posts on the same platform in the
        -- last 90 days that the sync has read engagement for. Earlier only —
        -- they have had at least as long to collect it.
        LEFT JOIN LATERAL (
            SELECT
                ceil(percentile_cont(0.5) WITHIN GROUP (
                    ORDER BY (peer.metadata->>'engagement')::double precision
                ))::bigint AS peer_median,
                count(*)::bigint AS peer_count,
                ceil(percentile_cont(0.5) WITHIN GROUP (
                    ORDER BY (peer.metadata->>'engagement')::double precision * 1000
                             / NULLIF((peer.metadata->>'reach')::double precision, 0)
                ))::bigint AS peer_rate_median,
                count(*) FILTER (
                    WHERE jsonb_typeof(peer.metadata->'reach') = 'number'
                      AND (peer.metadata->>'reach')::bigint > 0
                )::bigint AS rate_peer_count,
                ceil(percentile_cont(0.5) WITHIN GROUP (
                    ORDER BY (peer.metadata->>'avg_watch_ms')::double precision
                ))::bigint AS peer_watch_median
            FROM content_sources AS peer
            WHERE peer.workspace_id = source.workspace_id
              AND peer.source_kind = 'social_post'
              AND peer.id <> source.id
              AND peer.metadata->>'platform' = source.metadata->>'platform'
              AND jsonb_typeof(peer.metadata->'engagement') = 'number'
              AND peer.occurred_at < source.occurred_at
              AND peer.occurred_at > source.occurred_at - INTERVAL '90 days'
        ) AS peers ON source.source_kind = 'social_post'
        -- Requests that failed for this source version. Without them the
        -- evaluator asked again under the failed action's own key, which
        -- dedupes and writes nothing, and the chain stopped for good. A new
        -- version starts clean: its keys differ anyway.
        LEFT JOIN LATERAL (
            SELECT
                array_agg(per_artifact.artifact ORDER BY per_artifact.artifact) AS kinds,
                array_agg(per_artifact.failures ORDER BY per_artifact.artifact) AS counts,
                array_agg(per_artifact.last_failed_at ORDER BY per_artifact.artifact) AS last_failed
            FROM (
                SELECT
                    action.payload->>'artifact' AS artifact,
                    count(*)::bigint AS failures,
                    max(COALESCE(action.finished_at, action.updated_at)) AS last_failed_at
                FROM autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.subject_id = source.id
                  AND action.payload ? 'artifact'
                  AND action.payload->>'source_version' = source.version::text
                  AND (
                      action.status = 'failed'
                      -- A "succeeded" request whose executor reports ended
                      -- without artifact evidence did not deliver the
                      -- artifact: the request notified, the artifact never
                      -- landed. Counting it as failed is the honest state,
                      -- and it rides the same bounded retry a transport
                      -- failure does rather than silently closing the
                      -- artifact's file.
                      OR (
                          action.status = 'succeeded'
                          AND EXISTS (
                              SELECT 1
                              FROM autopilot_execution_reports AS report
                              WHERE report.workspace_id = action.workspace_id
                                AND report.action_id = action.id
                                AND report.status IN ('succeeded','failed')
                          )
                          AND NOT EXISTS (
                              SELECT 1
                              FROM autopilot_execution_reports AS evidence
                              WHERE evidence.workspace_id = action.workspace_id
                                AND evidence.action_id = action.id
                                AND evidence.status = 'succeeded'
                                AND COALESCE(
                                    NULLIF(btrim(evidence.metadata->'artifact_delivery'->>'url'), ''),
                                    NULLIF(btrim(evidence.metadata->'artifact_delivery'->>'surface'), ''),
                                    NULLIF(btrim(evidence.metadata->'artifact_delivery'->>'reference'), '')
                                ) IS NOT NULL
                          )
                      )
                  )
                GROUP BY action.payload->>'artifact'
            ) AS per_artifact
        ) AS failed ON true
        -- Surge lanes that already failed for this source. The lane name
        -- lives in the idempotency key (`action:drop_surge:{source}:{lane}`
        -- with an `:attempt{n}` suffix on retries), so the failure read keys
        -- on the key's prefix and suffix rather than a payload column —
        -- the same place the dedupe actually happens.
        LEFT JOIN LATERAL (
            SELECT
                array_agg(per_lane.lane ORDER BY per_lane.lane) AS lanes,
                array_agg(per_lane.failures ORDER BY per_lane.lane) AS counts,
                array_agg(per_lane.last_failed_at ORDER BY per_lane.lane) AS last_failed
            FROM (
                SELECT
                    -- Element 4 of 'action:drop_surge:{uuid}:{lane}…' — the
                    -- uuid is element 3, the lane is always the fourth
                    -- colon-separated segment because the retry suffix
                    -- comes after it.
                    split_part(action.idempotency_key, ':', 4) AS lane,
                    count(*)::bigint AS failures,
                    max(COALESCE(action.finished_at, action.updated_at)) AS last_failed_at
                FROM autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.idempotency_key
                      LIKE 'action:drop_surge:' || source.id::text || ':%'
                  AND action.status = 'failed'
                GROUP BY split_part(action.idempotency_key, ':', 4)
            ) AS per_lane
        ) AS surge_failed ON true
        WHERE source.workspace_id = $1
          AND source.active
          -- Every artifact an `event` source owes is pre-show promotion: a
          -- listing, a press hook, a push, feed and story posts, a
          -- newsletter block. Once the night is over none of it is true any
          -- more — the harvest chain runs from the `show_completed` source.
          -- The source itself outlives the show (the projection trigger
          -- keeps it to `starts_at + 14 days`, and stamps `occurred_at` with
          -- the import time), so on 2026-09-24, when past shows were
          -- imported, the brain asked for listings and newsletter blocks for
          -- nights in February, May, June and July. A day of grace keeps the
          -- day-of posts for a date-only show stored at midnight UTC.
          AND NOT (
              source.source_kind = 'event'
              AND EXISTS (
                  SELECT 1 FROM events AS event
                  WHERE event.workspace_id = source.workspace_id
                    AND 'event:' || event.id::text = source.source_key
                    AND event.starts_at + INTERVAL '1 day' <= $3
              )
          )
        ORDER BY source.occurred_at DESC, source.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .bind(now)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            let source_kind = parse_content_source_kind(&row.source_kind)?;
            Ok(ContentSupplySnapshot {
                source_id: ContentSourceId::from_uuid(row.source_id),
                source_kind,
                source_version: row.source_version,
                source_key: row.source_key.clone(),
                title: row.title.clone(),
                source_url: row.source_url.clone(),
                source_body: row.source_body.clone(),
                source_thumbnail_url: row.source_thumbnail_url.clone(),
                site_origin: row
                    .member_site_base_url
                    .as_deref()
                    .map(|origin| origin.trim().trim_end_matches('/').to_owned())
                    .filter(|origin| !origin.is_empty()),
                occurred_at: row.occurred_at,
                expires_at: row.expires_at,
                communication_enabled: row.communication_enabled,
                press_enabled: row.press_enabled,
                release_tier: row.release_tier.as_deref().and_then(ReleaseTier::parse),
                completed_artifacts: row
                    .completed_artifacts
                    .iter()
                    .map(|value| parse_artifact(value))
                    .collect::<Result<_, _>>()?,
                in_flight_artifacts: row
                    .inflight_artifacts
                    .iter()
                    .map(|value| parse_artifact(value))
                    .collect::<Result<_, _>>()?,
                failed_artifacts: row
                    .failed_artifact_kinds
                    .iter()
                    .zip(&row.failed_artifact_counts)
                    .zip(&row.failed_artifact_last)
                    .map(|((kind, failures), last_failed_at)| {
                        Ok(FailedArtifact {
                            artifact: parse_artifact(kind)?,
                            failures: u32::try_from(*failures).unwrap_or(u32::MAX),
                            last_failed_at: *last_failed_at,
                        })
                    })
                    .collect::<Result<_, RepositoryError>>()?,
                drop_surge_failures: row
                    .surge_lane_names
                    .iter()
                    .zip(&row.surge_lane_counts)
                    .zip(&row.surge_lane_last)
                    .map(|((lane, failures), last_failed_at)| DropSurgeLaneFailure {
                        lane: lane.clone(),
                        failures: u32::try_from(*failures).unwrap_or(u32::MAX),
                        last_failed_at: *last_failed_at,
                    })
                    .collect(),
                surge_requested_at: row.surge_requested_at,
                social_post: if source_kind == ContentSourceKind::SocialPost {
                    Some(SocialPostFact {
                        title: row.title.clone(),
                        url: row.post_url.clone(),
                        platform: row
                            .post_platform
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        body: row.post_body.clone(),
                        media_url: row.post_media_url.clone(),
                        media_id: row.post_media_id.clone(),
                        media_type: row.post_media_type.clone(),
                        thumbnail_url: row.post_thumbnail_url.clone(),
                        resonance: row.post_engagement.map(|engagement| PostResonance {
                            engagement,
                            peer_median: row.peer_median,
                            peers: u32::try_from(row.peer_count).unwrap_or(0),
                            rate_per_mille: row.post_rate,
                            peer_rate_median: row.peer_rate_median,
                            rate_peers: u32::try_from(row.rate_peer_count).unwrap_or(0),
                            watch_ms: row.post_watch_ms,
                            peer_watch_median: row.peer_watch_median,
                        }),
                    })
                } else {
                    None
                },
            })
        })
        .collect()
}
