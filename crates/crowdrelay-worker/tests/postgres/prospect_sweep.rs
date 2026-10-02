//! The prospect sweep, driven through the real worker against a migrated
//! database: people who commented under a post on a surface the band controls
//! become prospects with their words and the post they were under; running it
//! again changes nothing; a person who said no is not collected against; and a
//! prospect nobody has spoken to in the retention window is deleted.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::prospect_sweep::{ProspectSweep, SweepReport};
use sqlx::PgPool;
use std::time::Duration;
use time::{Duration as Span, OffsetDateTime};
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn video(pool: &PgPool, ws: WorkspaceId) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1,$2,'video',$3,'Technophobia', now(), now() + interval '90 days',
                 '{\"url\": \"https://youtu.be/abc123\"}'::jsonb)",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(format!("youtube:{}", id.simple()))
    .execute(pool)
    .await
    .context("insert content source")?;
    Ok(id)
}

async fn comment(
    pool: &PgPool,
    ws: WorkspaceId,
    source: Uuid,
    platform: &str,
    author: &str,
    body: &str,
    days_ago: i64,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO community_comments
             (id, workspace_id, platform_comment_id, parent_id, author, body, status, platform,
              content_source_id, created_at)
         VALUES ($1,$2,$3,'18088912784228243',$4,$5,'skipped',$6,$7, now() - make_interval(days => $8::int))",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind((id.as_u128() % 100_000_000_000_000_000).to_string())
    .bind(author)
    .bind(body)
    .bind(platform)
    .bind(source)
    .bind(i32::try_from(days_ago)?)
    .execute(pool)
    .await
    .context("insert comment")?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn commenters_become_prospects_once_and_a_no_stays_a_no() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = video(&pool, ws).await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "kuba_metal",
        "Kiedy gracie Wrocław?",
        1,
    )
    .await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "Kuba_Metal",
        "Dajcie znać jak będzie bilet",
        0,
    )
    .await?;
    comment(
        &pool,
        ws,
        source,
        "youtube",
        "Ania Rock",
        "same words as a name",
        0,
    )
    .await?;
    comment(&pool, ws, source, "youtube", "@zine_pl", "Świetny numer", 2).await?;
    // Older than the lookback: history, not a signal.
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "old_timer",
        "kiedyś tu byłem",
        45,
    )
    .await?;

    let sweep = ProspectSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let now = OffsetDateTime::now_utc();
    let report = sweep.run_once(now).await?;
    ensure!(
        report
            == SweepReport {
                created: 2,
                appended: 1,
                already_known: 0,
                not_collected: 0,
                not_an_identity: 1,
                expired: 0,
            },
        "first pass: {report:?}"
    );
    let again = sweep.run_once(now).await?;
    ensure!(
        again
            == SweepReport {
                created: 0,
                appended: 0,
                already_known: 3,
                not_collected: 0,
                not_an_identity: 1,
                expired: 0,
            },
        "second pass changes nothing: {again:?}"
    );

    let (asked, confidence): (String, i16) = sqlx::query_as(
        "SELECT observation_kind, confidence_basis_points FROM fan_prospect_observations
         WHERE workspace_id = $1 AND evidence = 'Kiedy gracie Wrocław?'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        (asked.as_str(), confidence) == ("asked_about_show", 8_000),
        "a public question about a show is the strongest evidence this source gives: {asked} {confidence}"
    );
    let (evidence, url, kind): (String, Option<String>, String) = sqlx::query_as(
        "SELECT o.evidence, o.source_url, o.observation_kind
         FROM fan_prospect_observations o
         JOIN fan_prospects p ON p.workspace_id = o.workspace_id AND p.id = o.prospect_id
         WHERE p.workspace_id = $1 AND lower(p.external_identity) = 'zine_pl'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        (evidence.as_str(), url.as_deref(), kind.as_str())
            == (
                "Świetny numer",
                Some("https://youtu.be/abc123"),
                "active_under_our_post"
            ),
        "verbatim words, the post they were under, and what it was"
    );
    let prospects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_prospects WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(
        prospects == 2,
        "kuba_metal (one person, two spellings) and zine_pl: {prospects}"
    );

    // A person who said no is not collected against, even when they comment again.
    sqlx::query(
        "UPDATE fan_prospects SET status = 'suppressed', status_reason = 'asked to stop'
         WHERE workspace_id = $1 AND lower(external_identity) = 'zine_pl'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    comment(&pool, ws, source, "youtube", "zine_pl", "jeszcze jedno", 0).await?;
    let after_no = sweep.run_once(now).await?;
    ensure!(
        after_no.not_collected == 2,
        "both of zine_pl's comments, old and new: {after_no:?}"
    );

    // Retention: nobody has spoken for longer than the window, so they go.
    let far = now + Span::days(120);
    let expired = sweep.run_once(far).await?;
    ensure!(
        expired.expired == 1,
        "kuba_metal lapses, the suppression is kept: {expired:?}"
    );
    let left: Vec<(String,)> = sqlx::query_as(
        "SELECT lower(external_identity) FROM fan_prospects WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    ensure!(left == [("zine_pl".to_owned(),)], "{left:?}");
    Ok(())
}
