//! The reply queue's writes against a real schema: a post, a waiting draft,
//! approve-as-edited, and the conflict answers for rows no longer waiting.

use crate::common;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::fanbase::{
    CommunityReplyError, approve_community_reply, list_community_replies, skip_community_reply,
};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_waiting_reply_is_approved_as_edited_once_and_skips_are_final() {
    let (pool, _) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(ws)
        .bind(format!("replies-{}", ws.simple()))
        .bind("Reply Lane Tests")
        .execute(&pool)
        .await
        .expect("workspace");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_intelligence','workspace',$2,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$4)"#,
    )
    .bind(decision_id)
    .bind(ws)
    .bind(format!("key-{decision_id}"))
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await
    .expect("decision");
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, trace_id, finished_at)
           VALUES ($1,$2,$3,'growth_intelligence','community.engage.request','workspace',$2,
                   $4,'{}'::jsonb,'succeeded','third_party',$5,now())"#,
    )
    .bind(action_id)
    .bind(ws)
    .bind(decision_id)
    .bind(format!("idem-{action_id}"))
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await
    .expect("action");
    let post_id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, reddit_post_id, posted_at)
           VALUES ($1,$2,'doommetal','Ashes — new video','','posted','abc123',now())
           RETURNING id"#,
    )
    .bind(ws)
    .bind(action_id)
    .fetch_one(&pool)
    .await
    .expect("post");
    let waiting: Uuid = sqlx::query_scalar(
        r#"INSERT INTO community_comments
           (workspace_id, community_post_id, platform_comment_id, parent_id, author, body,
            status, draft, review_score)
           VALUES ($1,$2,'t1_aaa','t3_abc123','fan1','what tuning is this?',
                   'awaiting_approval','Drop C, the whole record.',8)
           RETURNING id"#,
    )
    .bind(ws)
    .bind(post_id)
    .fetch_one(&pool)
    .await
    .expect("waiting reply");

    // A comment on the band's own Instagram post joins the same queue,
    // attached to the synced post rather than a community post.
    let source_id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO content_sources
           (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
           VALUES ($1,'social_post','instagram:17900000000000001','Ashes — live',
                   now(), now() + interval '45 days',
                   '{"platform":"instagram","url":"https://instagram.com/p/x","body":"Dzięki za wczoraj"}'::jsonb)
           RETURNING id"#,
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("synced instagram post");
    sqlx::query(
        r#"INSERT INTO community_comments
           (workspace_id, platform, content_source_id, platform_comment_id, parent_id,
            author, body, status, draft)
           VALUES ($1,'instagram',$2,'17900000000000002','17900000000000001',
                   'fan2','czy będzie winyl?','awaiting_approval','Będzie, jesienią.')"#,
    )
    .bind(ws)
    .bind(source_id)
    .execute(&pool)
    .await
    .expect("instagram reply row");
    // A row that claims both parents is refused by the schema.
    let both = sqlx::query(
        r#"INSERT INTO community_comments
           (workspace_id, platform, community_post_id, content_source_id,
            platform_comment_id, parent_id, author, body)
           VALUES ($1,'instagram',$2,$3,'17900000000000003','17900000000000001','x','y')"#,
    )
    .bind(ws)
    .bind(post_id)
    .bind(source_id)
    .execute(&pool)
    .await;
    assert!(
        both.is_err(),
        "a comment belongs to one post, on one platform"
    );

    let listed = list_community_replies(&pool, ws).await.expect("list");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|reply| reply.status == "awaiting_approval")
    );
    let instagram = listed
        .iter()
        .find(|reply| reply.platform == "instagram")
        .expect("the instagram reply lists");
    assert_eq!(instagram.post_title, "Ashes — live");
    assert_eq!(
        instagram.post_url.as_deref(),
        Some("https://instagram.com/p/x")
    );

    let later = OffsetDateTime::now_utc() + time::Duration::minutes(20);
    assert!(matches!(
        approve_community_reply(&pool, ws, waiting, Some("   "), "operator", later).await,
        Err(CommunityReplyError::InvalidDraft)
    ));
    approve_community_reply(
        &pool,
        ws,
        waiting,
        Some("Drop C — the whole record."),
        "operator",
        later,
    )
    .await
    .expect("approve as edited");
    let (status, draft): (String, String) = sqlx::query_as(
        "SELECT status, draft FROM community_comments WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(waiting)
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(status, "approved");
    assert_eq!(draft, "Drop C — the whole record.");
    assert!(matches!(
        approve_community_reply(&pool, ws, waiting, None, "operator", later).await,
        Err(CommunityReplyError::NotAwaiting(_))
    ));

    skip_community_reply(&pool, ws, waiting)
        .await
        .expect("an approved reply can still be withdrawn");
    assert!(matches!(
        skip_community_reply(&pool, ws, waiting).await,
        Err(CommunityReplyError::NotAwaiting(_))
    ));
    assert!(matches!(
        skip_community_reply(&pool, ws, Uuid::now_v7()).await,
        Err(CommunityReplyError::NotFound)
    ));
}
