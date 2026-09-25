//! "Take my name off the leaderboard", against a real schema.
//!
//! The unpublish used to filter on the first tenant's album campaign slug, so
//! a fan's name published under any other campaign stayed up while the call
//! reported success. What is asserted: every campaign the fan appears in is
//! unlisted, the runs themselves stay (history is not erased, only the public
//! name), another fan's publication is untouched, the audit row names each
//! campaign once, and a second call changes nothing.

use crate::common;

use crowdrelay_infra::fan_privacy::PostgresFanPrivacyRepository;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Unlisted')")
        .bind(id)
        .bind(common::unique_slug("unlisted", id))
        .execute(pool)
        .await?;
    Ok(id)
}

/// An active fan with a live session; returns the fan and the session token.
async fn fan_with_session(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
) -> Result<(Uuid, String), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    let token = format!("session-{fan_id}");
    sqlx::query(
        "INSERT INTO fan_sessions (workspace_id, fan_id, session_token_hash, expires_at)
         VALUES ($1, $2, digest($3, 'sha256'), now() + interval '1 day')",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(&token)
    .execute(pool)
    .await?;
    Ok((fan_id, token))
}

/// A completed, linked run published under `name` in `campaign`.
async fn published_run(
    pool: &PgPool,
    workspace_id: Uuid,
    fan_id: Uuid,
    campaign: &str,
    name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO synesthesia_runs (
             id, workspace_id, campaign_slug, install_hash, run_token_hash, app_version,
             fan_id, linked_at, completed_at, client_total_elapsed_ms,
             leaderboard_name, leaderboard_published_at
         )
         VALUES ($1, $2, $3, digest($1::text, 'sha256'), digest($1::text || 'run', 'sha256'),
                 '1.0.0', $4, now(), now(), 90000, $5, now())",
    )
    .bind(run_id)
    .bind(workspace_id)
    .bind(campaign)
    .bind(fan_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(run_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unlisting_covers_every_campaign_and_nobody_else() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = workspace(&pool).await?;
    let (fan_id, token) = fan_with_session(&pool, workspace_id, "unlist@example.test").await?;
    let (other_fan, _) = fan_with_session(&pool, workspace_id, "stays@example.test").await?;
    let album = published_run(
        &pool,
        workspace_id,
        fan_id,
        "virya-synesthesia-album-v1",
        "Nika",
    )
    .await?;
    let tour = published_run(&pool, workspace_id, fan_id, "tour-2027", "Nika").await?;
    let tour_again = published_run(&pool, workspace_id, fan_id, "tour-2027", "Nika K").await?;
    let theirs = published_run(&pool, workspace_id, other_fan, "tour-2027", "Olek").await?;

    let repository = PostgresFanPrivacyRepository::new(pool.clone());
    let receipt = repository
        .unpublish_synesthesia_leaderboard(workspace_id, &token, Some("req-unlist"))
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert_eq!(receipt.fan_id, fan_id);
    assert!(receipt.changed);

    for run in [album, tour, tour_again] {
        let (name, published, linked): (Option<String>, bool, bool) = sqlx::query_as(
            "SELECT leaderboard_name, leaderboard_published_at IS NOT NULL, fan_id IS NOT NULL
             FROM synesthesia_runs WHERE id = $1",
        )
        .bind(run)
        .fetch_one(&pool)
        .await?;
        assert_eq!(name, None, "run {run} still carries a public name");
        assert!(!published);
        assert!(linked, "unlisting must not unlink the run's history");
    }
    let untouched: Option<String> =
        sqlx::query_scalar("SELECT leaderboard_name FROM synesthesia_runs WHERE id = $1")
            .bind(theirs)
            .fetch_one(&pool)
            .await?;
    assert_eq!(untouched.as_deref(), Some("Olek"));

    let slugs: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata->'campaign_slugs' FROM audit_events
         WHERE workspace_id = $1 AND action = 'synesthesia.leaderboard_unpublished'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        slugs,
        serde_json::json!(["tour-2027", "virya-synesthesia-album-v1"])
    );

    let again = repository
        .unpublish_synesthesia_leaderboard(workspace_id, &token, None)
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(!again.changed, "a second call has nothing left to unlist");
    Ok(())
}
