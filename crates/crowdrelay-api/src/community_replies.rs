//! Operator surface for the reply lane: the band's drafted answers to the
//! people who commented on its Reddit posts. Protocol mapping only — the
//! statements live in `crowdrelay-infra::fanbase::community_replies`, the
//! rules in `crowdrelay-domain::community_reply`.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::fanbase::{
    CommunityReplyError, approve_community_reply, list_community_replies, skip_community_reply,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn workspace(state: &crate::AppState) -> Uuid {
    state.ticketing.workspace_id().into_uuid()
}

/// A uniform draw in `[0, 1)` for the send delay. Timing, not security; the
/// midpoint when the OS has nothing to give.
fn unit_draw() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / (f64::from(u32::MAX) + 1.0)
}

fn error_response(error: CommunityReplyError, request_id_value: Option<String>) -> Response {
    match error {
        CommunityReplyError::NotFound => Problem::not_found(request_id_value),
        CommunityReplyError::NotAwaiting(_) => Problem::conflict_because(
            "This reply is not waiting for an answer — it was sent, skipped, or \
             already approved.",
            request_id_value,
        ),
        CommunityReplyError::InvalidDraft => Problem::bad_request(request_id_value),
        CommunityReplyError::Database(error) => {
            tracing::warn!(%error, "community reply write failed");
            Problem::service_unavailable(request_id_value)
        }
    }
    .into_response()
}

/// `GET /v1/control-plane/community-replies` — waiting drafts first.
pub async fn list(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match list_community_replies(&state.database, workspace(&state)).await {
        Ok(replies) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "replies": replies })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "community replies read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveReplyRequest {
    /// The reply as the operator wants it sent. Omitted: the draft as is.
    #[serde(default)]
    text: Option<String>,
}

/// `POST /v1/control-plane/community-replies/{id}/approve` — send it (as
/// edited). It still leaves only after a human-looking delay and under the
/// account's standing and the reply ceiling.
pub async fn approve(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(reply_id): Path<Uuid>,
    raw: axum::body::Bytes,
) -> Response {
    let request_id_value = request_id(&headers);
    // An empty body approves the draft as it stands; a body must be the
    // strict shape — an unknown field is a mistake, not an approval.
    let body = if raw.iter().all(u8::is_ascii_whitespace) {
        ApproveReplyRequest::default()
    } else {
        match serde_json::from_slice::<ApproveReplyRequest>(&raw) {
            Ok(body) => body,
            Err(_) => return Problem::bad_request(request_id_value).into_response(),
        }
    };
    let not_before = crowdrelay_domain::community_reply::reply_not_before(
        OffsetDateTime::now_utc(),
        unit_draw(),
    );
    match approve_community_reply(
        &state.database,
        workspace(&state),
        reply_id,
        body.text.as_deref(),
        "operator",
        not_before,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

/// `POST /v1/control-plane/community-replies/{id}/skip` — don't answer.
pub async fn skip(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(reply_id): Path<Uuid>,
) -> Response {
    match skip_community_reply(&state.database, workspace(&state), reply_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error, request_id(&headers)),
    }
}

// Executes the reply queue's writes against a real schema: a post, a waiting
// draft, approve-as-edited, and the conflict answers for rows no longer
// waiting. The `_tests` scope exempts the fixture inserts from the
// decision-trace gate (test scaffolding, not a production write path).
#[cfg(test)]
mod community_replies_pg_tests {
    use super::*;
    use crowdrelay_domain::WorkspaceId;

    #[tokio::test]
    async fn a_waiting_reply_is_approved_as_edited_once_and_skips_are_final() {
        let Ok(database_url) = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect");
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .expect("migrate");

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
               (workspace_id, community_post_id, reddit_comment_id, parent_id, author, body,
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

        let listed = list_community_replies(&pool, ws).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "awaiting_approval");

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
        let (status, draft): (String, String) =
            sqlx::query_as("SELECT status, draft FROM community_comments WHERE id = $1")
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
}
