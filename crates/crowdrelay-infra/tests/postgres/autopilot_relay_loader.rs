//! The relay community loader against a real Postgres.
//!
//! A synced band post used to fan out to every admitted community — fifty-five
//! in production, one approval card each — including subs the audience graph
//! had already parked (rejected, not_a_fit) or lost (inactive). The loader now
//! applies the same place-fitness predicate the growth-intelligence loader
//! does, caps the spread at three communities per post, and rotates by
//! least-recently-drafted so the same three names are not always the ones
//! picked. These tests pin the contract: who is eligible, who is not, and in
//! which order.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use uuid::Uuid;

async fn repository()
-> Result<(PostgresAutopilotRepository, sqlx::PgPool), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let database = DatabaseConfig {
        url: database_url.clone(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    Ok((
        PostgresAutopilotRepository::new(pool.clone(), &database),
        pool,
    ))
}

async fn seed_workspace(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("relay-{}", workspace_id.into_uuid().simple()))
        .bind("Relay Loader Test")
        .execute(pool)
        .await?;
    Ok(())
}

/// An admitted, promoted community target — optionally backed by a discovery
/// place in the given state. `place` is `(status, membership_state)`; `None`
/// means the target predates the graph link.
async fn seed_community(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
    place: Option<(&str, &str)>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let place_id = if let Some((status, membership)) = place {
        let place_id: Uuid = sqlx::query_scalar(
            "INSERT INTO discovery_places
                 (id, workspace_id, place_kind, platform, name, url, status, membership_state)
             VALUES ($1,$2,'subreddit','reddit',$3,$4,$5,$6) RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(subreddit)
        .bind(format!("https://www.reddit.com/r/{subreddit}"))
        .bind(status)
        .bind(membership)
        .fetch_one(pool)
        .await?;
        Some(place_id)
    } else {
        None
    };
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_outreach_targets
             (id, workspace_id, target_kind, display_name, status, subreddit,
              place_id, screening_verdict)
         VALUES ($1,$2,'community',$3,'promoted',$4,$5,'admitted') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(subreddit)
    .bind(subreddit)
    .bind(place_id)
    .fetch_one(pool)
    .await?;
    Ok(target_id)
}

/// A draft/post row pinned to a target — `status` decides whether it consumed
/// the community's turn in the rotation. `action_id` is a real FK target, so a
/// dummy autopilot action carries it.
async fn seed_post(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    target_id: Uuid,
    subreddit: &str,
    status: &str,
    days_ago: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'content_supply','target_community',$4,
                 'seed.post',9000,'auto_execute','seeded relay post',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("seed-relay-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    let action_id: Uuid = sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'content_supply','community.engage.request','target_community',
                 $4,$5,'{}','succeeded', now()) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("seed-relay-{}", Uuid::now_v7()))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_posts
             (id, workspace_id, action_id, target_id, subreddit, title, body, status, created_at, posted_at)
         VALUES ($1,$2,$3,$4,$5,$6,'body',$7, now() - make_interval(days => $8),
                 CASE WHEN $7 = 'posted'
                      THEN now() - make_interval(days => $8) END)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(target_id)
    .bind(subreddit)
    .bind(format!("post in {subreddit}"))
    .bind(status)
    .bind(days_ago)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_relay_pool_excludes_unfit_places_and_non_admitted_targets()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws).await?;

    // Eligible: admitted + promoted, no place row (unknown is not refused),
    // or a place that is active and not parked.
    let ok_no_place = seed_community(&pool, ws, "oknoplace", None).await?;
    let ok_active = seed_community(&pool, ws, "okactive", Some(("active", "joined"))).await?;

    // Excluded by the place-fitness predicate — the graph's own judgement
    // overrides the target row's admission.
    seed_community(&pool, ws, "archivedplace", Some(("archived", "joined"))).await?;
    seed_community(&pool, ws, "blockedplace", Some(("blocked", "joined"))).await?;
    seed_community(&pool, ws, "rejectedplace", Some(("active", "rejected"))).await?;
    seed_community(&pool, ws, "notafitplace", Some(("active", "not_a_fit"))).await?;

    // Excluded by the target's own admission state.
    sqlx::query(
        "INSERT INTO agent_outreach_targets
             (id, workspace_id, target_kind, display_name, status, subreddit, screening_verdict)
         VALUES ($1,$2,'community','refusedone','proposed','refusedone','refused')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_outreach_targets
             (id, workspace_id, target_kind, display_name, status, subreddit, screening_verdict)
         VALUES ($1,$2,'community','nosubreddit','promoted',NULL,'admitted')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;

    let targets = repo.load_relay_community_targets(ws).await?;
    let mut found = targets
        .iter()
        .map(|t| t.subreddit.as_str())
        .collect::<Vec<_>>();
    found.sort_unstable();
    assert_eq!(
        found,
        ["okactive", "oknoplace"],
        "only admitted, promoted, place-fit communities may be relayed into — got {found:?}"
    );
    assert!(
        targets
            .iter()
            .any(|t| t.target_id.into_uuid() == ok_no_place)
    );
    assert!(targets.iter().any(|t| t.target_id.into_uuid() == ok_active));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_relay_caps_at_three_and_rotates_least_recently_drafted()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws).await?;

    // Five eligible communities; two have already consumed a turn — one has a
    // live draft awaiting the operator, one was posted to last week. A failed
    // draft must NOT consume the turn.
    let fresh = seed_community(&pool, ws, "freshsub", None).await?;
    let failed_only = seed_community(&pool, ws, "failedonly", None).await?;
    let also_fresh = seed_community(&pool, ws, "alsofresh", None).await?;
    let queued = seed_community(&pool, ws, "queuedsub", None).await?;
    let posted = seed_community(&pool, ws, "postedsub", None).await?;

    seed_post(&pool, ws, failed_only, "failedonly", "failed", 1).await?;
    seed_post(&pool, ws, queued, "queuedsub", "awaiting_manual_post", 1).await?;
    seed_post(&pool, ws, posted, "postedsub", "posted", 5).await?;

    let targets = repo.load_relay_community_targets(ws).await?;
    assert_eq!(
        targets.len(),
        3,
        "the relay fans out to at most three communities per post, got {}",
        targets.len()
    );
    let picked: Vec<Uuid> = targets.iter().map(|t| t.target_id.into_uuid()).collect();
    assert!(
        picked.contains(&fresh) && picked.contains(&failed_only) && picked.contains(&also_fresh),
        "never-drafted and failed-only communities lead the rotation — live drafts and \
         recent posts sort last: {picked:?}"
    );
    assert!(
        !picked.contains(&queued) && !picked.contains(&posted),
        "a community with a live draft or a recent post consumed its turn: {picked:?}"
    );
    Ok(())
}
