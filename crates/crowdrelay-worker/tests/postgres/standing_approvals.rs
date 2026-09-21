//! Standing approvals end to end: the operator's "stop asking me about this
//! one", from the grant row to the action status the worker writes.
//!
//! Four facts, in the order they matter.
//!
//! 1. A community post is governed by the `outreach` context and the
//!    `third_party` ceiling, not by the money context. While it answered to
//!    `promotion_budget`, `GrowthPosture` pinned that to `require_approval` in
//!    every posture, so no setting an operator could choose ever published one.
//! 2. Without a grant it waits for a person. That is the seeded posture and
//!    nothing here weakens it.
//! 3. With a live grant it goes. That is the whole point: approvals expire at
//!    72 hours and the queue refilled faster than a person emptied it.
//! 4. After a revocation it waits again, and a grant for one community never
//!    licenses another. Those two are what make giving a grant safe.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::action_class::ActionClass;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const ACTION_KIND: &str = "community.engage.request";

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("standing-{}", id.simple()))
        .bind("Standing Approvals Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

fn worker(pool: &PgPool, workspace_id: WorkspaceId) -> AgentOutcomeWorker {
    AgentOutcomeWorker::new(
        pool.clone(),
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
        "https://virya.music".to_owned(),
        // Every channel flag off. A standing grant is the only thing that can
        // move an action here, which is what these tests are measuring.
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

/// An admitted community and a live video source: the two gates a community
/// post passes before authority is consulted at all.
async fn admitted_community(pool: &PgPool, workspace_id: WorkspaceId) -> Result<(Uuid, Uuid)> {
    let target_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_outreach_targets \
         (id, workspace_id, target_kind, display_name, subreddit, status, screening_verdict) \
         VALUES ($1,$2,'community',$3,$4,'promoted','admitted')",
    )
    .bind(target_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("r/deathcore{}", target_id.simple()))
    .bind(format!("deathcore{}", target_id.simple()))
    .execute(pool)
    .await
    .context("insert admitted community")?;

    let source_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_content_sources \
         (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata) \
         VALUES ($1,$2,'video',$3,'a video', now(), now() + interval '30 days', \
                 jsonb_build_object('media_url','https://cdn.example/v.jpg', \
                                    'url','https://youtube.com/watch?v=x'))",
    )
    .bind(source_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("video:{}", source_id.simple()))
    .execute(pool)
    .await
    .context("insert content source")?;
    Ok((target_id, source_id))
}

/// Writes the outcome row the community engager's agent run would produce.
async fn insert_community_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    target_id: Uuid,
    source_id: Uuid,
) -> Result<()> {
    let id = Uuid::now_v7();
    let payload = json!({
        "item": {
            "platform": "reddit",
            "target_id": target_id.to_string(),
            "source_id": source_id.to_string(),
            "subreddit": "deathcore",
            "title": "new track",
            "body": "we put out a new one",
        },
        "rationale": "community engager draft",
        "provenance": {
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": {
                "basis_points": 8000,
                "source": "model_self_report",
                "is_evidence_confidence": false
            },
            "model": { "actual": "test-model", "provider": "test" }
        }
    });
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes
          (id, workspace_id, task_id, result_id, kind, schema_version, payload,
           confidence_basis_points, idempotency_key, status)
        VALUES ($1,$2,$3,$4,'social_post',1,$5,8000,$6,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(&payload)
    .bind(format!("post-{}", id.simple()))
    .execute(pool)
    .await
    .context("insert community post outcome")?;
    Ok(())
}

async fn grant_standing(pool: &PgPool, workspace_id: WorkspaceId, target_id: Uuid) -> Result<()> {
    crowdrelay_infra::standing_approvals::grant(
        pool,
        workspace_id.into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: ACTION_KIND,
            target_key: &target_id.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 90,
            note: Some("read three drafts from this one, they are fine"),
        },
        OffsetDateTime::now_utc(),
    )
    .await
    .context("grant standing approval")?;
    Ok(())
}

/// The single action this workspace produced: status, context and class.
async fn only_action(pool: &PgPool, workspace_id: WorkspaceId) -> Result<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT status, context, action_class FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?;
    ensure!(rows.len() == 1, "expected exactly one action, got {rows:?}");
    Ok(rows.into_iter().next().expect("length checked"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_community_post_is_outreach_and_waits_for_a_person_without_a_grant() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let (target_id, source_id) = admitted_community(&database, ws).await?;
    insert_community_post(&database, ws, target_id, source_id).await?;

    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let (status, context, class) = only_action(&database, ws).await?;
    ensure!(
        context == "outreach",
        "a forum post is free third-party contact, not promotion spend, got {context}"
    );
    ensure!(
        class == "third_party",
        "a forum post spends a relationship, not money, got {class}"
    );
    ensure!(
        status == "awaiting_approval",
        "the seeded posture drafts outward contact, got {status}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_live_standing_grant_sends_the_post_without_asking_again() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let (target_id, source_id) = admitted_community(&database, ws).await?;
    grant_standing(&database, ws, target_id).await?;
    insert_community_post(&database, ws, target_id, source_id).await?;

    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let (status, _, _) = only_action(&database, ws).await?;
    ensure!(
        status == "queued",
        "a target the operator already judged must not be asked about again, got {status}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_grant_sends_the_post_back_to_a_person() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let (target_id, source_id) = admitted_community(&database, ws).await?;
    grant_standing(&database, ws, target_id).await?;
    crowdrelay_infra::standing_approvals::revoke(
        &database,
        ws.into_uuid(),
        ACTION_KIND,
        &target_id.to_string(),
        "operator:test",
        OffsetDateTime::now_utc(),
    )
    .await
    .context("revoke standing approval")?;
    insert_community_post(&database, ws, target_id, source_id).await?;

    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let (status, _, _) = only_action(&database, ws).await?;
    ensure!(
        status == "awaiting_approval",
        "revoking must put the person back in the loop, got {status}"
    );
    Ok(())
}

/// A grant covers the target it names and nothing else. Without this the
/// mechanism would be a global switch wearing a target's name.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_grant_does_not_cover_a_community_it_does_not_name() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let (granted_target, _) = admitted_community(&database, ws).await?;
    grant_standing(&database, ws, granted_target).await?;
    let (other_target, source_id) = admitted_community(&database, ws).await?;
    insert_community_post(&database, ws, other_target, source_id).await?;

    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let (status, _, _) = only_action(&database, ws).await?;
    ensure!(
        status == "awaiting_approval",
        "a grant for one community must not license another, got {status}"
    );
    Ok(())
}

/// Money may never carry a grant. The migration refuses the row, so the
/// mechanism cannot become the first thing that lets spend run unattended.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn money_cannot_be_granted_a_standing_approval() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let refusal = crowdrelay_infra::standing_approvals::grant(
        &database,
        ws.into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: "promotion.spend",
            target_key: "any-target",
            class: ActionClass::Paid,
            granted_by: "operator:test",
            days: 90,
            note: None,
        },
        OffsetDateTime::now_utc(),
    )
    .await;
    ensure!(
        refusal.is_err(),
        "a standing approval on money must be refused"
    );
    Ok(())
}

/// "Approve this and stop asking me about it" reads its target from the
/// action's own payload.
///
/// The piece that could silently do nothing. The action row's `subject_id` is
/// the agent outcome, not the community, so a grant keyed on the subject would
/// cover one draft and never the next — and would look like it worked.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn remembering_an_approval_keys_the_grant_on_the_community() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&database).await?;
    let (target_id, source_id) = admitted_community(&database, ws).await?;
    insert_community_post(&database, ws, target_id, source_id).await?;
    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let action_id: Uuid =
        sqlx::query_scalar("SELECT id FROM viryaos_autopilot_actions WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&database)
            .await?;
    let subject_id: Uuid =
        sqlx::query_scalar("SELECT subject_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&database)
            .await?;
    ensure!(
        subject_id != target_id,
        "this test is only meaningful while the subject is not the community"
    );

    let target = crowdrelay_infra::standing_approvals::grant_target_for_action(
        &database,
        ws.into_uuid(),
        action_id,
    )
    .await?
    .expect("a community post has a target a grant can cover");
    ensure!(
        target.0 == ACTION_KIND,
        "the grant is recorded against the action kind, got {}",
        target.0
    );
    ensure!(
        target.1 == target_id.to_string(),
        "the grant must name the community, not the outcome: got {}",
        target.1
    );
    ensure!(
        target.2 == ActionClass::ThirdParty,
        "a forum post spends a relationship, got {:?}",
        target.2
    );

    // And the grant it produces is the one the worker then honours: the next
    // post to the same community goes without asking.
    crowdrelay_infra::standing_approvals::grant(
        &database,
        ws.into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: &target.0,
            target_key: &target.1,
            class: target.2,
            granted_by: "operator:test",
            days: 90,
            note: None,
        },
        OffsetDateTime::now_utc(),
    )
    .await?;
    insert_community_post(&database, ws, target_id, source_id).await?;
    ensure!(worker(&database, ws).run_once().await? == 1, "one outcome");

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND status = 'queued'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&database)
    .await?;
    ensure!(
        queued == 1,
        "the grant the approval wrote must license the next post, got {queued} queued"
    );
    Ok(())
}

/// An action with no recurring target cannot be remembered. The operator asked
/// for two things and is told they got one, rather than being handed a grant
/// over an action kind.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_action_with_no_target_has_nothing_to_remember() -> Result<()> {
    use crowdrelay_application::autopilot::AutopilotActionPayload;

    let push = AutopilotActionPayload::RequestSignalPush {
        task_id: Uuid::now_v7(),
        title: "new track".to_owned(),
        body: "out now".to_owned(),
        target_path: None,
        event_id: None,
        segment: None,
        audience_size: None,
        audience_basis: String::new(),
    };
    ensure!(
        push.standing_approval_target().is_none(),
        "a push to the whole audience has no target a standing grant could cover"
    );
    Ok(())
}
