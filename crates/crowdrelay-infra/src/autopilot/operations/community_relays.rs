//! The community-relay batch read model.
//!
//! One row per `community_relay_batches`: a piece of content's whole community
//! spread read as a single campaign. The card this feeds answers the only
//! question the batch asks — "does this post go to these communities, one an
//! hour?" — with the content, the image, the target list and the cadence in
//! one view; after approval the same row reports what the spread did.
//!
//! Nothing here is denormalized: deliveries are read live off the action rows
//! and their `community_posts` ledger entries, so the card cannot disagree
//! with what the executor is actually doing.

use std::collections::HashMap;

use crowdrelay_application::autopilot::{CommunityRelayBatchView, CommunityRelayDelivery};

use super::*;

/// How many batches the card feed returns. A workspace that has relayed thirty
/// pieces of content is reading history, not deciding — the oldest rows are
/// the least interesting ones to page through anyway.
const MAX_COMMUNITY_RELAYS: i64 = 30;

#[derive(Debug, FromRow)]
struct BatchRow {
    source_id: Uuid,
    status: String,
    interval_seconds: i32,
    created_at: OffsetDateTime,
    approved_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
    observe_until: Option<OffsetDateTime>,
    source_title: Option<String>,
    source_url: Option<String>,
    image_url: Option<String>,
}

/// One delivery under a batch: the action's own status plus what the post
/// ledger recorded for it once it ran.
#[derive(Debug, FromRow)]
struct DeliveryRow {
    source_id: Uuid,
    subreddit: Option<String>,
    language: Option<String>,
    title: Option<String>,
    body: Option<String>,
    action_status: String,
    post_status: Option<String>,
    posted_at: Option<OffsetDateTime>,
    post_url: Option<String>,
}

/// Per-batch progress off the post ledger.
#[derive(Debug, FromRow)]
struct ProgressRow {
    source_id: Uuid,
    posted: i64,
    pending: i64,
    failed: i64,
}

pub(in crate::autopilot) async fn load_community_relays(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<CommunityRelayBatchView>, RepositoryError> {
    let batches = sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT
            batch.source_id,
            batch.status,
            batch.interval_seconds,
            batch.created_at,
            batch.approved_at,
            batch.revoked_at,
            batch.observe_until,
            source.title AS source_title,
            source.metadata ->> 'url' AS source_url,
            -- The still the posts carry: a VIDEO's thumbnail, everything
            -- else's media_url — the same precedence the drafting mapper
            -- uses, so the card shows the picture the posts will show.
            COALESCE(
                CASE WHEN source.metadata ->> 'media_type' = 'VIDEO'
                     THEN source.metadata ->> 'thumbnail_url'
                END,
                source.metadata ->> 'media_url',
                source.metadata ->> 'thumbnail_url'
            ) AS image_url
        FROM community_relay_batches batch
        LEFT JOIN viryaos_content_sources source
          ON source.workspace_id = batch.workspace_id
         AND source.id = batch.source_id
        WHERE batch.workspace_id = $1
        ORDER BY batch.created_at DESC, batch.id DESC
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_COMMUNITY_RELAYS)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    if batches.is_empty() {
        return Ok(Vec::new());
    }
    let source_ids: Vec<Uuid> = batches.iter().map(|row| row.source_id).collect();

    // Every delivery the batch covers: the engage action is the unit of work
    // (its status is where the delivery stands before it runs) and its
    // community_posts row is the outcome once it did.
    let deliveries = sqlx::query_as::<_, DeliveryRow>(
        r#"
        SELECT
            batch.source_id,
            action.payload ->> 'subreddit' AS subreddit,
            target.language,
            action.payload ->> 'title' AS title,
            action.payload ->> 'body' AS body,
            action.status AS action_status,
            post.status AS post_status,
            post.posted_at,
            post.reddit_post_url AS post_url
        FROM community_relay_batches batch
        JOIN viryaos_autopilot_actions action
          ON action.workspace_id = batch.workspace_id
         AND action.action_kind = 'community.engage.request'
         AND action.payload ->> 'source_id' = batch.source_id::text
        LEFT JOIN community_posts post
          ON post.workspace_id = action.workspace_id
         AND post.action_id = action.id
        LEFT JOIN agent_outreach_targets target
          ON target.workspace_id = action.workspace_id
         AND target.id::text = action.payload ->> 'target_id'
        WHERE batch.workspace_id = $1
          AND batch.source_id = ANY($2)
        ORDER BY action.created_at, action.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&source_ids)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    // Posts made, still waiting, failed — off the ledger, not the actions,
    // because the ledger is what the executor trusts.
    let progress = sqlx::query_as::<_, ProgressRow>(
        r#"
        SELECT
            post.relay_source_id AS source_id,
            count(*) FILTER (WHERE post.status = 'posted') AS posted,
            count(*) FILTER (
                WHERE post.status IN ('pending','posting','rate_limited','awaiting_manual_post')
            ) AS pending,
            count(*) FILTER (WHERE post.status = 'failed') AS failed
        FROM community_posts post
        WHERE post.workspace_id = $1
          AND post.relay_source_id = ANY($2)
        GROUP BY post.relay_source_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&source_ids)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .map(|row| (row.source_id, row))
    .collect::<HashMap<_, _>>();

    // Clicks on the tracked links the batch's posts carried. A post's
    // `smart_link` is the `/l/{slug}` path; the slug joins back to the
    // smart_links row the click events point at.
    let clicks: HashMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT post.relay_source_id AS source_id, count(click.*) AS clicks
        FROM community_posts post
        JOIN smart_links link
          ON link.workspace_id = post.workspace_id
         AND post.smart_link = '/l/' || link.slug
        JOIN click_events click
          ON click.workspace_id = post.workspace_id
         AND click.smart_link_id = link.id
        WHERE post.workspace_id = $1
          AND post.relay_source_id = ANY($2)
        GROUP BY post.relay_source_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&source_ids)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .collect();

    let mut deliveries_by_source: HashMap<Uuid, Vec<DeliveryRow>> = HashMap::new();
    for delivery in deliveries {
        deliveries_by_source
            .entry(delivery.source_id)
            .or_default()
            .push(delivery);
    }

    Ok(batches
        .into_iter()
        .map(|batch| {
            let batch_deliveries = deliveries_by_source
                .remove(&batch.source_id)
                .unwrap_or_default();
            // The card shows one draft as a sample of what the posts look
            // like — the earliest drafted one, since targets each get their
            // own wording and there is no "the" text to show.
            let sample = batch_deliveries.first();
            let counts = progress.get(&batch.source_id);
            CommunityRelayBatchView {
                source_id: batch.source_id,
                status: batch.status,
                interval_seconds: batch.interval_seconds,
                created_at: batch.created_at,
                approved_at: batch.approved_at,
                revoked_at: batch.revoked_at,
                observe_until: batch.observe_until,
                source_title: batch.source_title,
                source_url: batch.source_url,
                image_url: batch.image_url,
                sample_title: sample.and_then(|d| d.title.clone()),
                sample_body: sample.and_then(|d| d.body.clone()),
                targets: batch_deliveries
                    .into_iter()
                    .map(|delivery| CommunityRelayDelivery {
                        subreddit: delivery.subreddit.unwrap_or_default(),
                        language: delivery.language,
                        // The ledger's word wins once a post row exists — the
                        // action can only say "queued"; the post can say
                        // "posted" with a permalink.
                        status: delivery.post_status.unwrap_or(delivery.action_status),
                        posted_at: delivery.posted_at,
                        post_url: delivery.post_url,
                    })
                    .collect(),
                posts_posted: counts.map_or(0, |row| row.posted),
                posts_pending: counts.map_or(0, |row| row.pending),
                posts_failed: counts.map_or(0, |row| row.failed),
                clicks: clicks.get(&batch.source_id).copied().unwrap_or(0),
            }
        })
        .collect())
}
