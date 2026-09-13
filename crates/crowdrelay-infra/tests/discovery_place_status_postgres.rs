//! A decision to stop targeting a community has to survive the next scrape.
//!
//! `discovery_places.status` is active/archived/blocked, and both upserts in
//! `audience_graph` used to set it to `'active'` in their conflict clause. Those
//! two statements are the only writers of the column anywhere in the codebase,
//! so neither of the other two values could persist: discovery re-reads the same
//! sources on a schedule, and each run silently un-blocked every community
//! somebody had ruled out.
//!
//! That is not a cosmetic reset. `agent_outcomes` computes
//! `refused_by_us_or_them` from `place.status == "blocked"` and
//! `target_discovery::screen_community_candidate` refuses on it, so a revived
//! status makes a ruled-out community targetable again — and the join executor,
//! which filters on `status = 'active'`, would act on it.
//!
//! Drafts exist in production for `r/whatisthisthing`, `r/metalgearsolid` and
//! `r/MetalMemes`: discovery matched the substring "metal". Blocking those is a
//! judgement nobody can make in code, but the mechanism that records the
//! judgement must not quietly discard it.

use std::time::Duration;

use crowdrelay_domain::audience_graph::PlaceKind;
use crowdrelay_infra::{
    audience_graph::{PostgresAudienceGraphRepository, UpsertPlaceInput},
    config::DatabaseConfig,
    database,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_TEST_DATABASE_URL";

async fn test_pool() -> Result<PgPool, Box<dyn std::error::Error>> {
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|error| format!("set {TEST_DATABASE_URL_KEY}: {error}"))?;
    let config = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let pool = database::connect(&config).await?;
    database::migrate(&pool).await?;
    Ok(pool)
}

async fn seed_workspace(pool: &PgPool) -> Result<Uuid, sqlx::Error> {
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Place status test')")
        .bind(workspace_id)
        .bind(format!("placestatus-{}", workspace_id.simple()))
        .execute(pool)
        .await?;
    Ok(workspace_id)
}

async fn status_of(pool: &PgPool, place_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query("SELECT status FROM discovery_places WHERE id = $1")
        .bind(place_id)
        .fetch_one(pool)
        .await?
        .try_get("status")
}

async fn set_status(pool: &PgPool, place_id: Uuid, status: &str) -> Result<(), sqlx::Error> {
    // No repository writes this column, so the operator's own correction is
    // modelled the way it is actually made today: by hand.
    sqlx::query("UPDATE discovery_places SET status = $2 WHERE id = $1")
        .bind(place_id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// A second sighting of the same community, with the facts a scrape refreshes.
fn rediscovered<'a>(
    workspace_id: Uuid,
    url: &'a str,
    genres: &'a [String],
) -> UpsertPlaceInput<'a> {
    UpsertPlaceInput {
        workspace_id,
        place_kind: PlaceKind::Subreddit,
        platform: "reddit",
        name: "Metal Gear Solid",
        url,
        country_code: None,
        language: None,
        genres,
        member_count: Some(900_000),
        activity_bp: Some(4_200),
        notes: None,
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn rediscovering_a_blocked_community_does_not_un_block_it()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool).await?;
    let repository = PostgresAudienceGraphRepository::new(pool.clone());
    let genres: Vec<String> = vec!["metal".to_owned()];
    let url = "https://www.reddit.com/r/metalgearsolid";

    let place_id = repository
        .upsert_place(&rediscovered(workspace_id, url, &genres))
        .await?;
    assert_eq!(status_of(&pool, place_id).await?, "active");

    set_status(&pool, place_id, "blocked").await?;

    // The scraper runs again. It has no idea a person ruled this place out.
    let same_place = repository
        .upsert_place(&rediscovered(workspace_id, url, &genres))
        .await?;
    assert_eq!(same_place, place_id, "the upsert found the existing row");
    assert_eq!(
        status_of(&pool, place_id).await?,
        "blocked",
        "the block survived re-discovery",
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn rediscovering_an_archived_community_does_not_revive_it()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool).await?;
    let repository = PostgresAudienceGraphRepository::new(pool.clone());
    let genres: Vec<String> = Vec::new();
    let url = "https://www.reddit.com/r/whatisthisthing";

    let place_id = repository
        .upsert_place(&rediscovered(workspace_id, url, &genres))
        .await?;
    // `upsert_place`'s own documentation prescribes archiving as the way to
    // correct a mis-kinded row, so reviving a tombstone undoes a correction
    // rather than merely re-opening a question.
    set_status(&pool, place_id, "archived").await?;

    repository
        .upsert_place(&rediscovered(workspace_id, url, &genres))
        .await?;
    assert_eq!(
        status_of(&pool, place_id).await?,
        "archived",
        "the archive survived re-discovery",
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_upsert_still_refreshes_the_facts_a_scrape_is_for()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = seed_workspace(&pool).await?;
    let repository = PostgresAudienceGraphRepository::new(pool.clone());
    let url = "https://www.reddit.com/r/metalmemes";

    let first: Vec<String> = vec!["metal".to_owned()];
    let place_id = repository
        .upsert_place(&rediscovered(workspace_id, url, &first))
        .await?;
    set_status(&pool, place_id, "blocked").await?;

    // Not touching `status` must not turn the upsert into a no-op. Membership
    // and observation are separate dimensions on purpose (migration 0217), and
    // a blocked place is still one we watch: its member count is evidence about
    // the platform even when we will never post there.
    let second: Vec<String> = vec!["metal".to_owned(), "memes".to_owned()];
    let mut refreshed = rediscovered(workspace_id, url, &second);
    refreshed.member_count = Some(1_234_567);
    repository.upsert_place(&refreshed).await?;

    let row =
        sqlx::query("SELECT status, member_count, genres FROM discovery_places WHERE id = $1")
            .bind(place_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(row.try_get::<String, _>("status")?, "blocked");
    assert_eq!(row.try_get::<i32, _>("member_count")?, 1_234_567);
    assert_eq!(
        row.try_get::<Vec<String>, _>("genres")?,
        vec!["metal".to_owned(), "memes".to_owned()],
    );

    Ok(())
}
