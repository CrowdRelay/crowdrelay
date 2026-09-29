//! The curator queue against a real schema: admitted handle candidates list
//! with the drafted DM, a marked send writes the candidate-linked
//! interaction, and a replayed mark returns the original send rather than a
//! second row.

use crate::common;
use crowdrelay_application::ports::IdempotencyKey;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::curator_queue::{list_curator_queue, mark_curator_dm_sent};
use sqlx::PgPool;
use uuid::Uuid;

/// A pool when a test database is configured; otherwise the test skips, the
/// same contract every PG test here follows.
async fn pool_or_skip() -> Option<PgPool> {
    common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .await
        .ok()
        .map(|(pool, _)| pool)
}

async fn workspace(pool: &PgPool, name: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("curator-{}", id.simple()))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn video(pool: &PgPool, workspace: Uuid) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO content_sources
           (id, workspace_id, source_kind, source_key, title, occurred_at,
            expires_at, metadata, active)
           VALUES ($1,$2,'video',$3,'Technophobia Live From FLSS 2026',now(),
                   now() + interval '90 days',
                   '{"url":"https://www.youtube.com/watch?v=iijgBMteL9I"}',true)"#,
    )
    .bind(id)
    .bind(workspace)
    .bind(format!("youtube:{}", id.simple()))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn candidate(
    pool: &PgPool,
    workspace: Uuid,
    handle: &str,
    followers: i32,
    status: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outreach_candidates
           (id, workspace_id, target_kind, display_name, source, source_reference,
            evidence, route_kind, route_value, route_is_published,
            fit_basis_points, follower_count, status, screened_at)
           VALUES ($1,$2,'creator',$3,'curator_site','https://t.me/metalworld',
                   'metal videos daily','handle',$4,true,6000,$5,$6,now())"#,
    )
    .bind(id)
    .bind(workspace)
    .bind(handle)
    .bind(format!("@{}", handle.trim_start_matches('@')))
    .bind(followers)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
async fn the_queue_drafts_each_handle_and_a_send_takes_it_off()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool_or_skip().await else {
        return Ok(());
    };
    let ws = workspace(&pool, "Virya").await?;
    let video_id = video(&pool, ws).await?;
    let big = candidate(&pool, ws, "@Riff_Support", 1_600, "admitted").await?;
    let small = candidate(&pool, ws, "@Music_Jesus_Man", 1_200, "admitted").await?;
    // Refused and non-handle candidates never enter the queue.
    candidate(&pool, ws, "@refused_one", 9_000, "refused").await?;

    let items = list_curator_queue(&pool, WorkspaceId::from_uuid(ws), video_id)
        .await?
        .expect("video");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].candidate_id, big, "largest audience first");
    assert!(items[0].sent_at.is_none());
    assert!(items[0].draft_dm.contains("Virya"));
    assert!(
        items[0]
            .draft_dm
            .contains("Technophobia Live From FLSS 2026")
    );
    assert!(items[0].draft_dm.contains("watch?v=iijgBMteL9I"));

    // Marking sent writes the candidate-linked interaction and the queue
    // shows the row as sent for this video.
    let key = IdempotencyKey::parse("curator-sent-1")?;
    let sent = mark_curator_dm_sent(
        &pool,
        WorkspaceId::from_uuid(ws),
        video_id,
        big,
        Some("sent from my own account"),
        &key,
        None,
    )
    .await?;
    assert!(!sent.replayed);

    let interaction = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM outreach_interactions
         WHERE workspace_id=$1 AND candidate_id=$2 AND target_id IS NULL
           AND direction='outbound' AND source_key=$3",
    )
    .bind(ws)
    .bind(big)
    .bind(format!("manual:curator:{video_id}"))
    .fetch_one(&pool)
    .await?;
    assert_eq!(interaction, 1);

    // A replay returns the original send instead of a second row.
    let replay = mark_curator_dm_sent(
        &pool,
        WorkspaceId::from_uuid(ws),
        video_id,
        big,
        Some("sent from my own account"),
        &key,
        None,
    )
    .await?;
    assert!(replay.replayed);
    assert_eq!(replay.sent_at, sent.sent_at);

    let after = list_curator_queue(&pool, WorkspaceId::from_uuid(ws), video_id)
        .await?
        .expect("video");
    assert!(after[0].sent_at.is_some());
    assert!(after[1].sent_at.is_none(), "the unsent handle still waits");
    assert_eq!(after[1].candidate_id, small);
    Ok(())
}

#[tokio::test]
async fn marking_an_inadmissible_or_missing_row_conflicts() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(pool) = pool_or_skip().await else {
        return Ok(());
    };
    let ws = workspace(&pool, "Virya").await?;
    let video_id = video(&pool, ws).await?;
    let refused = candidate(&pool, ws, "@no_route", 500, "refused").await?;
    let key = IdempotencyKey::parse("curator-sent-refused")?;

    let result = mark_curator_dm_sent(
        &pool,
        WorkspaceId::from_uuid(ws),
        video_id,
        refused,
        None,
        &key,
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(crowdrelay_application::RepositoryError::Conflict)
    ));

    // A candidate id that does not exist conflicts the same way.
    let key = IdempotencyKey::parse("curator-sent-missing")?;
    let result = mark_curator_dm_sent(
        &pool,
        WorkspaceId::from_uuid(ws),
        video_id,
        Uuid::now_v7(),
        None,
        &key,
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(crowdrelay_application::RepositoryError::Conflict)
    ));

    // A source that is not a video is a 404, not an empty queue.
    let missing = list_curator_queue(&pool, WorkspaceId::from_uuid(ws), Uuid::now_v7()).await?;
    assert!(missing.is_none());
    Ok(())
}
