//! §6-C: the mailed one-click approval links, end to end.
//!
//! The link is the whole credential — a crew member approves or skips from
//! the e-mail without a panel session. What the suite proves: GET never
//! decides, POST decides through the same transition the admin endpoints run
//! (`email-link` recorded as the door), a second click is a no-op, a skip is
//! a declined ask (`skipped_by_email`) rather than a cancelled one, and the
//! failure mapping holds — expired is 410, forged is 404.
//!
//! Runs under `just test-postgres`, which provisions and migrates the
//! disposable database this file points at via `CROWDRELAY_TEST_DATABASE_URL`.

use crate::attestation_anchor::{SIGNING_SECRET, app_state, seed_workspace};
use crate::common;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CONTENT_TYPE},
};
use crowdrelay_api::HttpConfig;
use crowdrelay_domain::{
    WorkspaceId,
    team_approval_token::{self, ApprovalTokenClaims},
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

fn approval_key() -> team_approval_token::TeamApprovalKey {
    team_approval_token::TeamApprovalKey::derive_from_secret(SIGNING_SECRET)
}

fn link_for(action_id: Uuid, expires_at: OffsetDateTime) -> String {
    team_approval_token::encode(
        &ApprovalTokenClaims {
            action_id,
            assignment_id: None,
            expires_at,
        },
        &approval_key(),
    )
}

/// One unanswered ask — the raw material a mailed link points at.
async fn seed_ask(
    pool: &PgPool,
    workspace_id: Uuid,
    title: &str,
    expires_at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                 'request_community_engagement',9000,'require_approval','seeded approval',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("approval-page-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, approval_expires_at)
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request','target_community',
                 $4,$5,$6,'awaiting_approval',$7) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("approval-page-action-{}", Uuid::now_v7()))
    .bind(serde_json::json!({
        "kind": "request_community_engagement",
        "target_id": Uuid::now_v7(),
        "platform": "reddit",
        "subreddit": "r/test",
        "title": title,
        "body": "seeded",
        "smart_link": null,
    }))
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn action_row(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
) -> Result<(String, Option<String>, Option<String>), Box<dyn std::error::Error>> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT status, approved_by, last_error_kind
         FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn get_text(
    app: &axum::Router,
    uri: &str,
) -> Result<(StatusCode, String), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

async fn post_verdict(
    app: &axum::Router,
    token: &str,
    verdict: &str,
) -> Result<(StatusCode, String), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/public/approvals/{token}"))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("verdict={verdict}")))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_mailed_link_renders_then_decides_the_ask() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_uuid = seed_workspace(&pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace_uuid);
    let app = crowdrelay_api::router(
        app_state(&pool, workspace_id)?,
        HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );

    let now = OffsetDateTime::now_utc();
    let approve_id = seed_ask(
        &pool,
        workspace_uuid,
        "Post to r/test",
        now + time::Duration::days(2),
    )
    .await?;
    let token = link_for(approve_id, now + time::Duration::days(2));

    // GET renders the ask — and decides nothing.
    let (status, body) = get_text(&app, &format!("/v1/public/approvals/{token}")).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Post to r/test"),
        "the ask never rendered: {body}"
    );
    assert!(
        body.contains("name=\"verdict\" value=\"approve\""),
        "no approve form: {body}"
    );
    assert!(
        body.contains("name=\"verdict\" value=\"skip\""),
        "no skip form: {body}"
    );
    let (state, _, _) = action_row(&pool, workspace_uuid, approve_id).await?;
    assert_eq!(state, "awaiting_approval", "a GET decided the ask");

    // POST approve → the same transition the admin endpoint runs.
    let (status, body) = post_verdict(&app, &token, "approve").await?;
    assert_eq!(status, StatusCode::OK, "approve failed: {body}");
    let (state, approved_by, _) = action_row(&pool, workspace_uuid, approve_id).await?;
    assert_eq!(state, "queued", "approve did not queue the action");
    assert_eq!(
        approved_by.as_deref(),
        Some("email-link"),
        "the ledger did not record the door"
    );

    // A second click on the same link is a stated no-op, not an error.
    let (status, body) = post_verdict(&app, &token, "approve").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("already decided"),
        "a replayed click did not say so: {body}"
    );

    // Skip is a declined ask — cancelled, marked `skipped_by_email`.
    let skip_id = seed_ask(
        &pool,
        workspace_uuid,
        "Skip me",
        now + time::Duration::days(2),
    )
    .await?;
    let skip_token = link_for(skip_id, now + time::Duration::days(2));
    let (status, body) = post_verdict(&app, &skip_token, "skip").await?;
    assert_eq!(status, StatusCode::OK, "skip failed: {body}");
    let (state, _, error_kind) = action_row(&pool, workspace_uuid, skip_id).await?;
    assert_eq!(state, "cancelled", "skip did not cancel the action");
    assert_eq!(
        error_kind.as_deref(),
        Some("skipped_by_email"),
        "a declined ask is not a cancelled one"
    );

    // A verdict the form never offers is a 400, and decides nothing.
    let open_id = seed_ask(
        &pool,
        workspace_uuid,
        "Still open",
        now + time::Duration::days(2),
    )
    .await?;
    let open_token = link_for(open_id, now + time::Duration::days(2));
    let (status, _) = post_verdict(&app, &open_token, "maybe").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (state, _, _) = action_row(&pool, workspace_uuid, open_id).await?;
    assert_eq!(state, "awaiting_approval", "a bad verdict moved the ask");

    // A link whose window closed answers 410; a forged one answers 404.
    let stale_id = seed_ask(
        &pool,
        workspace_uuid,
        "Too late",
        now - time::Duration::hours(1),
    )
    .await?;
    let stale_token = link_for(stale_id, now - time::Duration::hours(1));
    let (status, _) = get_text(&app, &format!("/v1/public/approvals/{stale_token}")).await?;
    assert_eq!(status, StatusCode::GONE, "an expired link must be 410");
    let (status, _) = post_verdict(&app, &stale_token, "approve").await?;
    assert_eq!(status, StatusCode::GONE, "an expired link must not decide");
    let (state, _, _) = action_row(&pool, workspace_uuid, stale_id).await?;
    assert_eq!(state, "awaiting_approval", "an expired link decided anyway");

    let (payload, _) = token.split_once('.').expect("two parts");
    let forged = format!("{payload}.{}", "A".repeat(43));
    let (status, _) = get_text(&app, &format!("/v1/public/approvals/{forged}")).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "a forged link must be 404");
    Ok(())
}
