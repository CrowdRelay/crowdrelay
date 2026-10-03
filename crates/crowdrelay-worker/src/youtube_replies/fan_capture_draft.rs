//! The capture comment, prepared for a person when nothing may post it.
//!
//! `seed_fan_capture_comment` posts the tracked join link under a fresh owned
//! video, but only under `social_auto_post`. That switch is off by design for a
//! band that runs its own socials, so the lane has never run: on 2026-10-03 a
//! Reddit-driven video had 84 visitors, every one sent straight to YouTube with
//! no way to join. Doing the preparation without the posting keeps what needs no
//! trust (a tracked link that exists, in the tenant's own join-ask words) and
//! leaves only the act of posting to a person.
//!
//! Nothing here writes to YouTube, sends anything or needs a token. It mints the
//! same smart link the automatic path would, and records the exact comment on
//! the video's source row so the attention board can show it ready to paste.
//! The automatic path skips a drafted source, so turning the switch on later
//! cannot put a second comment under a video a person already commented on.

use super::fan_capture::{capture_comment_text, capture_slug, signal_destination};
use sqlx::PgPool;
use uuid::Uuid;

/// Videos newer than this are worth a capture comment (same window as the
/// automatic path).
pub const DRAFT_WINDOW_DAYS: i32 = 30;
/// Drafts waiting on a person at once. A queue of ten paste-this chores is a
/// chore nobody does; the newest few are the ones that still matter.
pub const OPEN_DRAFTS_MAX: i64 = 3;

/// Prepares at most one capture comment draft. Returns whether it did.
///
/// # Errors
///
/// Propagates the database error.
pub async fn prepare_fan_capture_draft(
    pool: &PgPool,
    workspace_id: Uuid,
    variant: &str,
    site_root: &str,
) -> Result<bool, sqlx::Error> {
    let open: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*) FROM content_sources cs
        WHERE cs.workspace_id = $1 AND cs.source_kind = 'video' AND cs.active
          AND cs.metadata ? 'fan_capture_draft_at'
          AND NOT (cs.metadata ? 'fan_capture_comment_posted_unix')
          AND cs.occurred_at > now() - make_interval(days => $2)
          -- A click on the capture link means the comment is already up.
          AND NOT EXISTS (
              SELECT 1 FROM smart_links l
              JOIN click_events c ON c.smart_link_id = l.id
              WHERE l.workspace_id = cs.workspace_id
                AND l.slug = cs.metadata->>'fan_capture_link_slug')
        "#,
    )
    .bind(workspace_id)
    .bind(DRAFT_WINDOW_DAYS)
    .fetch_one(pool)
    .await?;
    if open >= OPEN_DRAFTS_MAX {
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    let candidate: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, source_key
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'video'
          AND source_key LIKE 'youtube:%'
          AND active
          AND occurred_at > now() - make_interval(days => $2)
          AND NOT (metadata ? 'fan_capture_draft_at')
          AND NOT (metadata ? 'fan_capture_comment_posted_unix')
          -- Same rule as the automatic path: a description that already
          -- carries the owned CTA does not need a second one in comments.
          AND COALESCE(metadata->>'body', '') NOT ILIKE '%/signal%'
        ORDER BY occurred_at DESC
        LIMIT 1
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(workspace_id)
    .bind(DRAFT_WINDOW_DAYS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((source_id, source_key)) = candidate else {
        tx.rollback().await?;
        return Ok(false);
    };
    let Some(video_id) = source_key
        .strip_prefix("youtube:")
        .filter(|id| super::is_youtube_id(id))
        .map(str::to_owned)
    else {
        tx.rollback().await?;
        return Ok(false);
    };
    let Some(campaign_id) = crowdrelay_infra::promotion_campaign::ensure_source_campaign(
        &mut tx,
        workspace_id,
        source_id,
    )
    .await?
    else {
        tx.rollback().await?;
        return Ok(false);
    };

    let slug = capture_slug(source_id);
    sqlx::query(
        r#"
        INSERT INTO smart_links
            (workspace_id, slug, destination_url, campaign_id, active,
             channel_source, channel_community, channel_creative)
        VALUES ($1, $2, $3, $4, true, 'youtube', $5, 'fan_capture_comment')
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            campaign_id = EXCLUDED.campaign_id,
            channel_source = EXCLUDED.channel_source,
            channel_community = EXCLUDED.channel_community,
            channel_creative = EXCLUDED.channel_creative,
            active = true
        "#,
    )
    .bind(workspace_id)
    .bind(&slug)
    .bind(signal_destination(site_root, source_id))
    .bind(campaign_id)
    .bind(format!("video:{video_id}"))
    .execute(&mut *tx)
    .await?;

    let public_link = format!("{}/l/{slug}", site_root.trim_end_matches('/'));
    sqlx::query(
        r#"
        UPDATE content_sources
        SET metadata = metadata || jsonb_build_object(
                'fan_capture_draft_at', now(),
                'fan_capture_draft_text', $3::text,
                'fan_capture_link_slug', $4::text),
            updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(source_id)
    .bind(capture_comment_text(variant, &public_link))
    .bind(&slug)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
