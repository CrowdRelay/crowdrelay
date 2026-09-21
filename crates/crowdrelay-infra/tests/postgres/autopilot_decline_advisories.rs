use std::time::Duration;

use crate::common;
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotControlRepository};
use crowdrelay_domain::{AutopilotActionId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
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
        .bind(format!("decline-{}", workspace_id.into_uuid().simple()))
        .bind("Decline Advisory Test")
        .execute(pool)
        .await?;
    Ok(())
}

/// An outreach target the engager may spend on, optionally backed by a
/// discovery place it can be parked through.
async fn seed_target(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
    with_place: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let place_id = if with_place {
        let place_id: Uuid = sqlx::query_scalar(
            "INSERT INTO discovery_places
                 (id, workspace_id, place_kind, platform, name, url, membership_state)
             VALUES ($1,$2,'subreddit','reddit',$3,$4,'joined') RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("{subreddit} place"))
        .bind(format!("https://www.reddit.com/{subreddit}"))
        .fetch_one(pool)
        .await?;
        Some(place_id)
    } else {
        None
    };
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_outreach_targets
             (id, workspace_id, target_kind, display_name, status, subreddit, place_id)
         VALUES ($1,$2,'community',$3,'promoted',$4,$5) RETURNING id",
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

/// A posted community post with a metrics row — the "engages" evidence.
/// `action_id` is a real FK target, so a dummy autopilot action carries it.
async fn seed_post(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
    score: i32,
    posted_days_ago: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                 'seed.post',9000,'auto_execute','seeded engagement post',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("seed-post-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    let action_id: Uuid = sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request','target_community',
                 $4,$5,'{}','succeeded', now()) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("seed-post-{}", Uuid::now_v7()))
    .fetch_one(pool)
    .await?;
    let post_id: Uuid = sqlx::query_scalar(
        "INSERT INTO community_posts
             (id, workspace_id, action_id, subreddit, title, body, status, posted_at)
         VALUES ($1,$2,$3,$4,$5,$6,'posted', now() - make_interval(days => $7))
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(format!("post in {subreddit}"))
    .bind("body")
    .bind(posted_days_ago)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_post_metrics
             (id, workspace_id, community_post_id, reddit_post_id, measured_at,
              score, upvotes, num_comments)
         VALUES ($1,$2,$3,$4, now(), $5, $5, 2)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(post_id)
    .bind(format!("t3_{}", Uuid::now_v7().simple()))
    .bind(score)
    .execute(pool)
    .await?;
    Ok(())
}

/// A fan conversion attributed to a community — the "produces fans" side.
async fn seed_conversion(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    community: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("fan-{}@test.invalid", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events
             (id, workspace_id, fan_id, event_kind, channel, community,
              attribution_method, attribution_confidence, occurred_at)
         VALUES ($1,$2,$3,'conversion','smart_link',$4,'last_community_click',1.0,now())",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .bind(community)
    .execute(pool)
    .await?;
    Ok(())
}

async fn advisory_actions(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(Uuid, String, serde_json::Value)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT id, status, payload FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'community.decline.advisory'
         ORDER BY created_at",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

/// A room that gets real engagement but converts nobody is flagged — with
/// the numbers and the alternative named — while the converting room is
/// never flagged at all.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_engaged_room_with_zero_fans_gets_the_uncomfortable_advice()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    let dead_target = seed_target(&pool, workspace_id, "r/deadroom", true).await?;
    seed_target(&pool, workspace_id, "r/liveroom", false).await?;
    for _ in 0..4 {
        seed_post(&pool, workspace_id, "r/deadroom", 10, 5).await?;
    }
    for _ in 0..2 {
        seed_post(&pool, workspace_id, "r/liveroom", 8, 5).await?;
    }
    seed_conversion(&pool, workspace_id, "r/liveroom").await?;
    seed_conversion(&pool, workspace_id, "r/liveroom").await?;

    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;

    let actions = advisory_actions(&pool, workspace_id).await?;
    assert_eq!(actions.len(), 1, "exactly one advisory: {actions:?}");
    let (action_id, status, payload) = &actions[0];
    assert_eq!(status, "awaiting_approval");
    assert_eq!(payload["kind"], "raise_decline_advisory");
    assert_eq!(payload["subreddit"], "r/deadroom");
    assert_eq!(payload["posts_considered"], 4);
    assert_eq!(
        payload["alternative_label"], "r/liveroom",
        "the alternative is the room that converts"
    );

    // Disagreement recorded: the band keeps the room — and the same
    // subject stays out of the queue for the cooldown rather than being
    // re-asked on the next sweep.
    repo.cancel_action(
        workspace_id,
        AutopilotActionId::from_uuid(*action_id),
        &IdempotencyKey::parse(format!("cancel-{}", action_id.simple())).expect("key"),
        None,
    )
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let after = advisory_actions(&pool, workspace_id).await?;
    assert_eq!(
        after.len(),
        1,
        "a recorded disagreement is not re-raised: {after:?}"
    );

    // The converting room was never itself flagged.
    let subject_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT subject_id FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'community.decline.advisory'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(subject_ids, vec![dead_target]);
    Ok(())
}

/// Approving parks the place — the room leaves the spend pool on the next
/// cycle, and a target with no place row still carries its evidence.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approving_the_advisory_parks_the_community() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    seed_target(&pool, workspace_id, "r/deadroom", true).await?;
    seed_target(&pool, workspace_id, "r/liveroom", false).await?;
    for _ in 0..4 {
        seed_post(&pool, workspace_id, "r/deadroom", 10, 5).await?;
    }
    seed_conversion(&pool, workspace_id, "r/liveroom").await?;

    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let actions = advisory_actions(&pool, workspace_id).await?;
    let (action_id, _, _) = &actions[0];

    repo.approve_action(
        workspace_id,
        AutopilotActionId::from_uuid(*action_id),
        &IdempotencyKey::parse(format!("approve-{}", action_id.simple())).expect("key"),
        None,
        None,
    )
    .await?;

    // The production path: claim the queued action, execute it, and the
    // park lands on the place.
    let claimed = repo
        .claim_due_autonomous_actions(workspace_id, 8, OffsetDateTime::now_utc())
        .await?;
    let action = claimed
        .into_iter()
        .find(|claimed_action| claimed_action.id.into_uuid() == *action_id)
        .expect("the approved advisory must be claimable");
    repo.execute_action(workspace_id, &action, OffsetDateTime::now_utc())
        .await?;

    let membership: String = sqlx::query_scalar(
        "SELECT place.membership_state FROM discovery_places place
         JOIN agent_outreach_targets t ON t.place_id = place.id
         WHERE t.workspace_id = $1 AND t.subreddit = 'r/deadroom'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(membership, "not_a_fit");
    let changed_by: String = sqlx::query_scalar(
        "SELECT place.membership_changed_by FROM discovery_places place
         JOIN agent_outreach_targets t ON t.place_id = place.id
         WHERE t.workspace_id = $1 AND t.subreddit = 'r/deadroom'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(changed_by, "autopilot:decline-advisory");
    Ok(())
}

/// Evidence or silence: an unmeasured workspace raises nothing, and a
/// flagged room with no honest alternative raises nothing either — two
/// engaged rooms that both convert nobody cannot be each other's way out.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn no_evidence_or_no_alternative_means_silence() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;

    // Nothing measured at all — silence.
    let empty = WorkspaceId::new();
    seed_workspace(&pool, empty).await?;
    repo.reconcile_team_handoffs(empty, OffsetDateTime::now_utc())
        .await?;
    assert!(advisory_actions(&pool, empty).await?.is_empty());

    // Two engaged rooms, zero conversions anywhere: each one's only
    // "alternative" is the other flagged room — silence for both.
    let paired = WorkspaceId::new();
    seed_workspace(&pool, paired).await?;
    seed_target(&pool, paired, "r/rooma", false).await?;
    seed_target(&pool, paired, "r/roomb", false).await?;
    for _ in 0..4 {
        seed_post(&pool, paired, "r/rooma", 10, 5).await?;
        seed_post(&pool, paired, "r/roomb", 9, 5).await?;
    }
    repo.reconcile_team_handoffs(paired, OffsetDateTime::now_utc())
        .await?;
    assert!(
        advisory_actions(&pool, paired).await?.is_empty(),
        "two flagged rooms are not each other's alternative"
    );

    // A quiet room — posts exist but engagement is under the floor — is
    // a quiet room, not a decline.
    let quiet = WorkspaceId::new();
    seed_workspace(&pool, quiet).await?;
    seed_target(&pool, quiet, "r/coldroom", false).await?;
    seed_target(&pool, quiet, "r/liveroom", false).await?;
    for _ in 0..4 {
        seed_post(&pool, quiet, "r/coldroom", 1, 5).await?;
    }
    seed_conversion(&pool, quiet, "r/liveroom").await?;
    repo.reconcile_team_handoffs(quiet, OffsetDateTime::now_utc())
        .await?;
    assert!(
        advisory_actions(&pool, quiet).await?.is_empty(),
        "under-floor engagement is cold, not a decline"
    );
    Ok(())
}
