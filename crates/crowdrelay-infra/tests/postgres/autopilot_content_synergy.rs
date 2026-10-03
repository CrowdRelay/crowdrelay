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

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
            status, posted_at, platform_post_id, platform_post_url)
           VALUES ($1,$2,'instagram','{}'::jsonb,'/l/post-link',$3,'posted',$4,
                   'ig-post-link','https://instagram.com/p/post-link')"#,
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

/// One fan row plus the canonical conversion row the ledger writes at
/// signup — `record_community_conversion`'s exact shape, seeded directly so
/// the test exercises the read path the production reader shares.
async fn insert_credited_fan(
    f: &Fixture,
    action_id: uuid::Uuid,
    slug: &str,
    email: &str,
    status: &str,
    signed_up_at: OffsetDateTime,
) {
    let fan_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status, created_at)
         VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(email)
    .bind(status)
    .bind(signed_up_at)
    .fetch_one(&f.pool)
    .await
    .expect("fan");
    sqlx::query(
        "INSERT INTO fan_provenance_events
         (workspace_id, fan_id, event_kind, channel, source_target, action_id,
          attribution_method, attribution_confidence, occurred_at)
         VALUES ($1,$2,'conversion','instagram',$3,$4,'last_tracked_click',1.0,$5)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(slug)
    .bind(action_id)
    .bind(signed_up_at)
    .execute(&f.pool)
    .await
    .expect("conversion");
}

/// The canonical rule: a person who clicked post A and then post B before
/// signing up converted once, and the ledger credits B — the last tracked
/// click. The measurement must agree with the ledger exactly: A reads zero
/// (a real measured zero — the post was live and tracked), B reads one.
/// Repeat clicks, suppressed fans and other-workspace traffic cannot
/// inflate either.
#[tokio::test]
#[ignore = "postgres"]
async fn content_fan_acquisition_credits_only_the_last_clicked_post() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(14);
    let posted = f.now - time::Duration::days(13);

    // Post A and post B, each owned by its own action.
    let mut action_ids = Vec::new();
    for slug in ["post-a", "post-b"] {
        let action_id = insert_action(
            &f,
            "agent.content.request",
            serde_json::json!({
                "kind": "request_agent_content",
                "task_id": uuid::Uuid::now_v7(),
                "draft": {"platform": "instagram", "text": slug, "cta_url": "https://virya.test/join"},
            }),
        )
        .await;
        let link_id = insert_smart_link(&f, slug).await;
        sqlx::query(
            r#"INSERT INTO social_posts
               (workspace_id, action_id, platform, content, smart_link, smart_link_id,
                status, posted_at, platform_post_id, platform_post_url)
               VALUES ($1,$2,'instagram','{}'::jsonb,$3,$4,'posted',$5,$6,$7)"#,
        )
        .bind(workspace)
        .bind(action_id)
        .bind(format!("/l/{slug}"))
        .bind(link_id)
        .bind(posted)
        .bind(format!("provider-{slug}"))
        .bind(format!("https://instagram.com/p/{slug}"))
        .execute(&f.pool)
        .await
        .expect("social post with provider receipt");
        action_ids.push(action_id);
    }
    let action_a = action_ids[0];
    let action_b = action_ids[1];

    // The visitor clicked A's link and then B's before signing up — the
    // ledger credits B. A keeps the click as journey evidence, not a
    // conversion.
    let visitor = uuid::Uuid::now_v7();
    for (link_slug, day) in [("post-a", 1), ("post-b", 2)] {
        let link_id: uuid::Uuid =
            sqlx::query_scalar("SELECT id FROM smart_links WHERE workspace_id = $1 AND slug = $2")
                .bind(workspace)
                .bind(link_slug)
                .fetch_one(&f.pool)
                .await
                .expect("link");
        sqlx::query(
            "INSERT INTO click_events
             (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(workspace)
        .bind(link_id)
        .bind(visitor)
        .bind(anchor + time::Duration::days(day))
        .execute(&f.pool)
        .await
        .expect("click");
    }
    insert_credited_fan(
        &f,
        action_b,
        "post-b",
        "winner@example.test",
        "active",
        anchor + time::Duration::days(3),
    )
    .await;
    // A second fan credited to B — suppressed since: consent withdrawal
    // removes them from the count entirely.
    insert_credited_fan(
        &f,
        action_b,
        "post-b",
        "gone@example.test",
        "suppressed",
        anchor + time::Duration::days(4),
    )
    .await;

    let observed_b = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                action_b,
                AutopilotMeasurementKind::ContentFanAcquisition7d,
                action_b,
            ),
            f.now,
        )
        .await
        .expect("tracked acquisition observes cleanly");
    assert_eq!(observed_b, 1.0, "only the last-clicked post is credited");

    let observed_a = f
        .repository
        .observe_measurement(
            f.workspace_id,
            &measurement(
                &f,
                action_a,
                AutopilotMeasurementKind::ContentFanAcquisition7d,
                action_a,
            ),
            f.now,
        )
        .await
        .expect("a tracked post's zero observes cleanly");
    assert_eq!(observed_a, 0.0, "the earlier click is journey, not credit");
}

/// A community post carries no `smart_link_id` — only the `/l/{slug}` path
/// in `smart_link`. The observation resolves the slug back to the
/// `smart_links` row, so its clicks count the same as a joined id's.
#[tokio::test]
#[ignore = "postgres"]
async fn content_link_clicks_reads_a_community_posts_slug_link() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let action_id = insert_action(
        &f,
        "community.engage.request",
        serde_json::json!({
            "kind": "request_community_engagement",
            "target_id": uuid::Uuid::now_v7(),
            "platform": "reddit",
            "title": "new single",
            "body": "link inside",
        }),
    )
    .await;
    let link_id = insert_smart_link(&f, "community-link").await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, smart_link,
            status, posted_at, reddit_post_id, reddit_post_url)
           VALUES ($1,$2,'Metal','new single','link inside','/l/community-link',
                   'posted',$3,'community-proof',
                   'https://www.reddit.com/r/Metal/comments/communityproof/post/')"#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(f.now - time::Duration::days(14))
    .execute(&f.pool)
    .await
    .expect("community post");

    // Three clicks inside the measurement's week, one outside it.
    let anchor = f.now - time::Duration::days(14);
    for days in [1, 3, 6, 9] {
        sqlx::query(
            "INSERT INTO click_events (workspace_id, smart_link_id, occurred_at) VALUES ($1,$2,$3)",
        )
        .bind(workspace)
        .bind(link_id)
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
        .expect("a community post's slug link observes cleanly");
    assert_eq!(observed, 3.0);
}

/// Telegram and Discord posts carry `smart_link_id` outright — same join as
/// social, different table. A telegram post's clicks must reach its action's
/// measurement; a discord post's too.
#[tokio::test]
#[ignore = "postgres"]
async fn content_link_clicks_reads_telegram_and_discord_link_ids() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(14);

    for (table, extra) in [
        ("telegram_posts", "channel"),
        ("discord_posts", "channel_id"),
    ] {
        let action_id = insert_action(
            &f,
            "community.engage.request",
            serde_json::json!({
                "kind": "request_community_engagement",
                "target_id": uuid::Uuid::now_v7(),
                "platform": "reddit",
                "title": "chat post",
                "body": "link inside",
            }),
        )
        .await;
        let link_id = insert_smart_link(&f, &format!("{table}-link")).await;
        let receipt_sql = if table == "telegram_posts" {
            ", message_id"
        } else {
            ", message_id"
        };
        let receipt_value = if table == "telegram_posts" { "42" } else { "'discord-proof'" };
        sqlx::query(&format!(
            "INSERT INTO {table}
             (workspace_id, action_id, {extra}, smart_link, smart_link_id,
              status, posted_at{receipt_sql})
             VALUES ($1,$2,'metal','/l/x',$3,'posted',$4,{receipt_value})"
        ))
        .bind(workspace)
        .bind(action_id)
        .bind(link_id)
        .bind(anchor)
        .execute(&f.pool)
        .await
        .unwrap_or_else(|e| panic!("{table} post: {e}"));

        for days in [1, 4] {
            sqlx::query(
                "INSERT INTO click_events (workspace_id, smart_link_id, occurred_at) VALUES ($1,$2,$3)",
            )
            .bind(workspace)
            .bind(link_id)
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
            .unwrap_or_else(|e| panic!("{table} observation: {e}"));
        assert_eq!(observed, 2.0, "{table}");
    }
}

/// A published community post whose row carries no link still abandons —
/// the union finding nothing tracked is the same verdict as a social post's
/// missing `smart_link_id`.
#[tokio::test]
#[ignore = "postgres"]
async fn content_link_clicks_abandons_a_community_post_with_no_link() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let action_id = insert_action(
        &f,
        "community.engage.request",
        serde_json::json!({
            "kind": "request_community_engagement",
            "target_id": uuid::Uuid::now_v7(),
            "platform": "reddit",
            "title": "no link",
            "body": "plain text",
        }),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, posted_at,
            reddit_post_id, reddit_post_url)
           VALUES ($1,$2,'Metal','no link','plain text','posted',$3,'nolink-proof',
                   'https://www.reddit.com/r/Metal/comments/nolinkproof/post/')"#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(f.now - time::Duration::days(10))
    .execute(&f.pool)
    .await
    .expect("community post");

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
        other => panic!("an untracked community post must abandon, not observe: {other:?}"),
    }
}

/// The posted transition schedules the two content-funnel measurements
/// (clicks and acquired fans), both anchored at `posted_at` — and a replayed
/// transition inserts nothing a second time. A post with no tracked link
/// schedules neither.
#[tokio::test]
#[ignore = "postgres"]
async fn posted_transition_schedules_one_click_measurement() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();

    for with_link in [true, false] {
        let action_id = insert_action(
            &f,
            "community.engage.request",
            serde_json::json!({
                "kind": "request_community_engagement",
                "target_id": uuid::Uuid::now_v7(),
                "platform": "reddit",
                "title": "post",
                "body": "text",
            }),
        )
        .await;
        let post_id = uuid::Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO community_posts
               (id, workspace_id, action_id, subreddit, title, body, smart_link,
                status, posted_at, reddit_post_id, reddit_post_url)
               VALUES ($1,$2,$3,'Metal','post','text',$4,'posted',$5,$6,$7)"#,
        )
        .bind(post_id)
        .bind(workspace)
        .bind(action_id)
        .bind(if with_link { "/l/linked" } else { "" })
        .bind(f.now)
        .bind(format!("scheduled-{}", post_id.simple()))
        .bind(format!(
            "https://www.reddit.com/r/Metal/comments/{}/scheduled/",
            post_id.simple()
        ))
        .execute(&f.pool)
        .await
        .expect("community post with provider receipt");

        let mut transaction = f.pool.begin().await.expect("tx");
        crowdrelay_infra::fanbase::schedule_link_click_measurement(
            &mut transaction,
            workspace,
            "community_posts",
            post_id,
        )
        .await
        .expect("first schedule");
        // A replayed posted transition must not schedule a second time.
        crowdrelay_infra::fanbase::schedule_link_click_measurement(
            &mut transaction,
            workspace,
            "community_posts",
            post_id,
        )
        .await
        .expect("replayed schedule");
        transaction.commit().await.expect("commit");

        let (rows, due_offset_secs): (i64, Option<i64>) = sqlx::query_as(
            r#"SELECT COUNT(*)::bigint,
                      max(EXTRACT(EPOCH FROM (due_at - action_finished_at)))::bigint
               FROM autopilot_measurements
               WHERE workspace_id = $1 AND action_id = $2
                 AND measurement_kind IN (
                     'content_link_clicks_7d',
                     'content_fan_acquisition_7d'
                 )"#,
        )
        .bind(workspace)
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("count");
        if with_link {
            assert_eq!(rows, 2, "two funnel measurements even after a replay");
            assert_eq!(due_offset_secs, Some(7 * 24 * 60 * 60));
        } else {
            assert_eq!(rows, 0, "an untracked post schedules nothing");
        }
    }
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
           (workspace_id, action_id, platform, content, status, posted_at,
            platform_post_id, platform_post_url)
           VALUES ($1,$2,'facebook','{}'::jsonb,'posted',$3,'fb-nolink',
                   'https://www.facebook.com/fb-nolink')"#,
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
           (workspace_id, action_id, subreddit, title, body, status, posted_at,
            reddit_post_id, reddit_post_url)
           VALUES ($1,$2,'Metal','playthrough','we filmed one','posted',$3,'artifact-community',
                   'https://www.reddit.com/r/Metal/comments/artifactcommunity/post/')"#,
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
           (workspace_id, action_id, platform, content, status, posted_at,
            platform_post_id, platform_post_url)
           VALUES ($1,$2,'instagram','{}'::jsonb,'posted',$3,'artifact-social',
                   'https://instagram.com/p/artifact-social')"#,
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
           (workspace_id, action_id, subreddit, title, body, status, posted_at,
            reddit_post_id, reddit_post_url)
           VALUES ($1,$2,'Metal','other','not this artifact','posted',$3,'other-artifact',
                   'https://www.reddit.com/r/Metal/comments/otherartifact/post/')"#,
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
