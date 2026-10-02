//! The demand scout's read of the community sweep, against a real schema.
//!
//! What only the database proves: the sweep re-reads a room every pass, so one
//! thread is many `fan_observations` rows and must come back once, newest
//! reading; a room the band ruled out or was refused by contributes nothing;
//! another tenant's rooms are not visible; and the window is on the thread's
//! own date (a `date` column, not an instant).

use crate::common;

use crowdrelay_infra::fan_demand::recent_room_threads;
use time::OffsetDateTime;
use uuid::Uuid;

async fn room(
    pool: &sqlx::PgPool,
    ws: Uuid,
    name: &str,
    membership: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO discovery_places
             (id, workspace_id, place_kind, platform, name, url, status, membership_state)
         VALUES ($1,$2,'subreddit','reddit',$3,$4,'active',$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(name)
    .bind(format!("https://www.reddit.com/r/{name}"))
    .bind(membership)
    .fetch_one(pool)
    .await?)
}

async fn thread(
    pool: &sqlx::PgPool,
    ws: Uuid,
    place: Uuid,
    url: &str,
    title: &str,
    days_ago: i32,
    comments: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO fan_observations
             (workspace_id, place_id, observed_at, platform, kind, fact, url, metrics)
         VALUES ($1,$2, current_date - $3::int, 'reddit', 'post', $4, $5,
                 jsonb_build_object('comments', $6::int, 'flair', 'Discussion'))",
    )
    .bind(ws)
    .bind(place)
    .bind(days_ago)
    .bind(title)
    .bind(url)
    .bind(comments)
    .execute(pool)
    .await?;
    Ok(())
}

async fn workspace(pool: &sqlx::PgPool, label: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(format!("{label}-{}", ws.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(ws)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_thread_one_row_in_rooms_the_band_still_reads() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "demand").await?;
    let other = workspace(&pool, "demand-other").await?;
    let joined = room(&pool, ws, "metalcore", "joined").await?;
    let refused = room(&pool, ws, "closedroom", "rejected").await?;
    let not_a_fit = room(&pool, ws, "offtopic", "not_a_fit").await?;
    let foreign = room(&pool, other, "metalcore", "joined").await?;

    // The same thread seen under two titles (an edit): the newest reading wins.
    thread(&pool, ws, joined, "https://r/a", "Bands like Gojira", 0, 1).await?;
    thread(&pool, ws, joined, "https://r/a", "Bands like Gojira?", 0, 9).await?;
    thread(&pool, ws, joined, "https://r/b", "Old ask", 5, 2).await?;
    thread(&pool, ws, refused, "https://r/c", "Bands like X?", 0, 2).await?;
    thread(&pool, ws, not_a_fit, "https://r/d", "Bands like Y?", 0, 2).await?;
    thread(&pool, other, foreign, "https://r/e", "Bands like Z?", 0, 2).await?;

    let since = OffsetDateTime::now_utc().date() - time::Duration::days(3);
    let rows = recent_room_threads(&pool, ws, since, 100).await?;
    let urls: Vec<&str> = rows.iter().map(|row| row.url.as_str()).collect();
    assert_eq!(
        urls,
        ["https://r/a"],
        "refused, off-topic, foreign and out-of-window threads are not read"
    );
    assert_eq!(rows[0].room, "metalcore");
    assert_eq!(rows[0].title, "Bands like Gojira?");
    assert_eq!(rows[0].flair.as_deref(), Some("Discussion"));
    assert_eq!(rows[0].comments, Some(9), "the newest reading, not a sum");

    // The limit applies after de-duplication, newest first.
    thread(
        &pool,
        ws,
        joined,
        "https://r/f",
        "Recommend me metal albums",
        1,
        0,
    )
    .await?;
    let limited = recent_room_threads(&pool, ws, since, 1).await?;
    assert_eq!(limited.len(), 1);
    assert_eq!(
        limited[0].url, "https://r/a",
        "today's thread outranks yesterday's"
    );
    Ok(())
}
