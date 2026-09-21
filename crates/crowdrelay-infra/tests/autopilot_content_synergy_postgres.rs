//! Content-synergy measurements against a real Postgres.
//!
//! A social post answers for the clicks on the tracked link it carried —
//! joined through `social_posts.smart_link_id`, never the workspace's whole
//! click ledger. A produced artifact answers for whether anything it became
//! reached an audience — posts filed against its content source inside the
//! week after production. What fails here and nowhere else: a click count
//! that credits another post's traffic, a measurement that reads a
//! fabricated zero for a post that carried no link to click, or an artifact
//! credited with a post that cited a different source.

use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("content-synergy-{suffix}"))
        .bind("Content Synergy Tests")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

/// A succeeded action with its decision row — the shape a dispatch leaves.
async fn insert_action(f: &Fixture, action_kind: &str, payload: serde_json::Value) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("key-{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("decision");
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'growth_metrics',$4,'content_source',
                   $5,$6,$7,'succeeded','third_party',$8)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(action_kind)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(payload)
    .bind(f.now - time::Duration::days(14))
    .execute(&f.pool)
    .await
    .expect("action");
    action_id
}

fn measurement(
    f: &Fixture,
    action_id: uuid::Uuid,
    kind: AutopilotMeasurementKind,
    subject_id: uuid::Uuid,
) -> ClaimedAutopilotMeasurement {
    ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
        action_id: AutopilotActionId::from(action_id),
        kind,
        subject_id,
        baseline_value: 0.0,
        action_finished_at: f.now - time::Duration::days(14),
        due_at: f.now,
        attempt_number: 1,
    }
}

async fn insert_smart_link(f: &Fixture, slug: &str) -> uuid::Uuid {
    let link_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO smart_links (id, workspace_id, slug, destination_url)
           VALUES ($1,$2,$3,'https://virya.test/join')"#,
    )
    .bind(link_id)
    .bind(f.workspace_id.into_uuid())
    .bind(slug)
    .execute(&f.pool)
    .await
    .expect("smart link");
    link_id
}

/// The post carried a link, and only clicks through that link — in its own
/// window — belong to it. Another post's traffic and a click that landed
/// after the week closed are somebody else's evidence.
#[tokio::test]
#[ignore = "postgres"]
async fn content_link_clicks_counts_only_the_posts_own_traffic() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let action_id = insert_action(
        &f,
        "agent.content.request",
        serde_json::json!({
            "kind": "request_agent_content",
            "task_id": uuid::Uuid::now_v7(),
            "draft": {"platform": "instagram", "text": "new single out", "cta_url": "https://virya.test/join"},
        }),
    )
    .await;
    let link_id = insert_smart_link(&f, "post-link").await;
    let other_link = insert_smart_link(&f, "other-link").await;
    sqlx::query(
        r#"INSERT INTO social_posts
           (workspace_id, action_id, platform, content, smart_link, smart_link_id,
            status, posted_at)
           VALUES ($1,$2,'instagram','{}'::jsonb,'/l/post-link',$3,'posted',$4)"#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(link_id)
    .bind(f.now - time::Duration::days(13))
    .execute(&f.pool)
    .await
    .expect("social post");

    // Three clicks inside the measurement's own week, one after it closed,
    // and one on a link this post never carried.
    let anchor = f.now - time::Duration::days(14);
    for (link, days) in [
        (link_id, 1),
        (link_id, 3),
        (link_id, 6),
        (link_id, 9),
        (other_link, 2),
    ] {
        sqlx::query(
            "INSERT INTO click_events (workspace_id, smart_link_id, occurred_at) VALUES ($1,$2,$3)",
        )
        .bind(workspace)
        .bind(link)
        .bind(anchor + time::Duration::days(days))
        .execute(&f.pool)
        .await
        .expect("click");
    }

    let observed = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                action_id,
                AutopilotMeasurementKind::ContentLinkClicks7d,
                action_id,
            ),
            f.now,
        )
        .await
        .expect("a tracked post's clicks observe cleanly");
    assert_eq!(observed, 3.0);
}

/// A published post whose draft named no trackable destination has no click
/// count to report — `no_tracked_link` is the honest answer, not a zero the
/// learner would read as the content failing.
#[tokio::test]
#[ignore = "postgres"]
async fn content_link_clicks_abandons_when_the_post_carried_no_link() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let action_id = insert_action(
        &f,
        "agent.content.request",
        serde_json::json!({
            "kind": "request_agent_content",
            "task_id": uuid::Uuid::now_v7(),
            "draft": {"platform": "facebook", "text": "no link in this one"},
        }),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO social_posts
           (workspace_id, action_id, platform, content, status, posted_at)
           VALUES ($1,$2,'facebook','{}'::jsonb,'posted',$3)"#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(f.now - time::Duration::days(13))
    .execute(&f.pool)
    .await
    .expect("social post");

    let result = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                action_id,
                AutopilotMeasurementKind::ContentLinkClicks7d,
                action_id,
            ),
            f.now,
        )
        .await;
    match result {
        Err(crowdrelay_application::RepositoryError::ConflictBecause(reason)) => {
            assert_eq!(reason, AutopilotMeasurementKind::NO_TRACKED_LINK);
        }
        other => panic!("an untracked post must abandon, not observe: {other:?}"),
    }
}

/// The artifact's week asks whether the thing it became reached an audience:
/// posts filed against its content source — a community post naming the
/// source outright, a social draft carrying it — inside the window. A post
/// citing another source is not this artifact's outcome.
#[tokio::test]
#[ignore = "postgres"]
async fn artifact_outcome_counts_only_posts_citing_its_source() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let source_id = uuid::Uuid::now_v7();
    let artifact_action = insert_action(
        &f,
        "content.artifact.request",
        serde_json::json!({
            "kind": "request_content_artifact",
            "source_id": source_id,
            "source_version": 1,
            "artifact": "video_clip",
            "template_key": "playthrough",
        }),
    )
    .await;

    // A community post whose action payload names the source outright.
    let community_action = insert_action(
        &f,
        "community.engage.request",
        serde_json::json!({
            "kind": "request_community_engagement",
            "target_id": uuid::Uuid::now_v7(),
            "platform": "reddit",
            "title": "playthrough",
            "body": "we filmed one",
            "source_id": source_id.to_string(),
        }),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, posted_at)
           VALUES ($1,$2,'Metal','playthrough','we filmed one','posted',$3)"#,
    )
    .bind(workspace)
    .bind(community_action)
    .bind(f.now - time::Duration::days(10))
    .execute(&f.pool)
    .await
    .expect("community post");

    // A social post whose draft carries the source — the field lives inside
    // `draft` for agent-content actions, not at the payload's top level.
    let social_action = insert_action(
        &f,
        "agent.content.request",
        serde_json::json!({
            "kind": "request_agent_content",
            "task_id": uuid::Uuid::now_v7(),
            "draft": {"platform": "instagram", "text": "clip", "source_id": source_id.to_string()},
        }),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO social_posts
           (workspace_id, action_id, platform, content, status, posted_at)
           VALUES ($1,$2,'instagram','{}'::jsonb,'posted',$3)"#,
    )
    .bind(workspace)
    .bind(social_action)
    .bind(f.now - time::Duration::days(9))
    .execute(&f.pool)
    .await
    .expect("social post");

    // A post citing a different source — same week, somebody else's outcome.
    let other_source = uuid::Uuid::now_v7();
    let other_action = insert_action(
        &f,
        "community.engage.request",
        serde_json::json!({
            "kind": "request_community_engagement",
            "target_id": uuid::Uuid::now_v7(),
            "platform": "reddit",
            "title": "other",
            "body": "not this artifact",
            "source_id": other_source.to_string(),
        }),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, posted_at)
           VALUES ($1,$2,'Metal','other','not this artifact','posted',$3)"#,
    )
    .bind(workspace)
    .bind(other_action)
    .bind(f.now - time::Duration::days(10))
    .execute(&f.pool)
    .await
    .expect("other post");

    let observed = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                artifact_action,
                AutopilotMeasurementKind::ArtifactOutcome7d,
                source_id,
            ),
            f.now,
        )
        .await
        .expect("artifact outcome observes cleanly");
    assert_eq!(observed, 2.0);
}

/// An artifact nothing ever posted reads its real zero — production was
/// confirmed when the measurement scheduled, so an empty week is the
/// produced-and-never-posted verdict, not an unmeasurable one.
#[tokio::test]
#[ignore = "postgres"]
async fn artifact_outcome_zero_is_the_never_posted_verdict() {
    let f = setup().await.expect("fixture");
    let source_id = uuid::Uuid::now_v7();
    let artifact_action = insert_action(
        &f,
        "content.artifact.request",
        serde_json::json!({
            "kind": "request_content_artifact",
            "source_id": source_id,
            "source_version": 1,
            "artifact": "video_clip",
            "template_key": "playthrough",
        }),
    )
    .await;

    let observed = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                artifact_action,
                AutopilotMeasurementKind::ArtifactOutcome7d,
                source_id,
            ),
            f.now,
        )
        .await
        .expect("a produced-and-never-posted artifact observes as zero");
    assert_eq!(observed, 0.0);
}
