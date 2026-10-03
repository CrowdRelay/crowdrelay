//! The YouTube capture comment, prepared for a person when nothing may post it.
//!
//! The automatic path is behind `social_auto_post`, which a band that runs its
//! own socials keeps off, so the lane never ran and every video visitor left
//! for YouTube with no way to join. The draft path mints the same tracked link
//! and writes the exact comment, and never touches YouTube.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_worker::youtube_replies::fan_capture_draft::{
    OPEN_DRAFTS_MAX, prepare_fan_capture_draft,
};
use sqlx::PgPool;
use uuid::Uuid;

const ROOT: &str = "https://band.example";

async fn workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("yt-capture-{}", id.simple()))
        .bind("YT Capture")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

async fn video(pool: &PgPool, ws: Uuid, id11: &str, days_old: i32, body: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1,$2,'video',$3,$4, now() - make_interval(days => $5), now() + interval '60 days',
                 jsonb_build_object('body', $6::text, 'origin', 'youtube_feed', 'youtube_format', 'long_form'))",
    )
    .bind(id)
    .bind(ws)
    .bind(format!("youtube:{id11}"))
    .bind(format!("Video {id11}"))
    .bind(days_old)
    .bind(body)
    .execute(pool)
    .await
    .context("insert video")?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_fresh_video_gets_a_tracked_link_and_a_comment_to_paste() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let fresh = video(&pool, ws, "aaaaaaaaaaa", 2, "Live at FLSS").await?;
    // None of these is owed a comment: old, already carrying the CTA, not YouTube-shaped.
    video(&pool, ws, "bbbbbbbbbbb", 90, "old").await?;
    video(
        &pool,
        ws,
        "ccccccccccc",
        1,
        "Join: https://band.example/signal",
    )
    .await?;

    let prepared = prepare_fan_capture_draft(&pool, ws, "Want the next show first?", ROOT).await?;
    ensure!(prepared, "the fresh video is drafted");

    let (text, slug): (String, String) = sqlx::query_as(
        "SELECT metadata->>'fan_capture_draft_text', metadata->>'fan_capture_link_slug'
         FROM content_sources WHERE id = $1",
    )
    .bind(fresh)
    .fetch_one(&pool)
    .await?;
    ensure!(
        text == format!("Want the next show first?\n\n{ROOT}/l/{slug}"),
        "the comment is the tenant's own words plus the tracked link: {text}"
    );
    let (dest, channel, creative, active): (String, String, String, bool) = sqlx::query_as(
        "SELECT destination_url, channel_source, channel_creative, active
         FROM smart_links WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(ws)
    .bind(&slug)
    .fetch_one(&pool)
    .await?;
    ensure!(
        dest.starts_with(&format!("{ROOT}/signal?utm_source=youtube")),
        "{dest}"
    );
    ensure!(
        (channel.as_str(), creative.as_str(), active) == ("youtube", "fan_capture_comment", true)
    );

    // Nothing was posted: no posted marker, so the automatic path stays free
    // of it only through the draft marker it checks.
    let posted: bool = sqlx::query_scalar(
        "SELECT metadata ? 'fan_capture_comment_posted_unix' FROM content_sources WHERE id = $1",
    )
    .bind(fresh)
    .fetch_one(&pool)
    .await?;
    ensure!(!posted, "preparing never claims the comment is up");

    // Idempotent: the same video is not drafted twice, and the ones that are
    // not owed one stay undrafted.
    ensure!(
        !prepare_fan_capture_draft(&pool, ws, "Want the next show first?", ROOT).await?,
        "nothing else is owed a comment"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_queue_of_things_to_paste_stays_short() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let letters = ["d", "e", "f", "g", "h"];
    for (i, l) in letters.iter().enumerate() {
        video(&pool, ws, &l.repeat(11), i32::try_from(i)?, "v").await?;
    }
    let mut drafted = 0;
    for _ in 0..letters.len() {
        if prepare_fan_capture_draft(&pool, ws, "Join us", ROOT).await? {
            drafted += 1;
        }
    }
    ensure!(
        drafted == OPEN_DRAFTS_MAX,
        "at most {OPEN_DRAFTS_MAX} comments wait on a person at once: {drafted}"
    );
    Ok(())
}
