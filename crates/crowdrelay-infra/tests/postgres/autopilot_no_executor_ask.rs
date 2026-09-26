//! A decision whose work no live executor can perform is a recommendation,
//! against a real Postgres.
//!
//! What only real rows can prove: with an executor registry in place, a
//! `require_approval` or `auto_execute` candidate whose capability nobody
//! advertises writes its decision as `recommend_only` and no action — so no
//! approval is asked and no outward budget is held by work the dispatcher
//! would park. The same candidate becomes an ask the first time it is seen
//! after the capability is advertised. A workspace with no registry at all
//! still asks, which is the fail-open rule every executor gate shares.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    ActionSubject, AutopilotActionPayload, AutopilotContext, AutopilotDecisionRepository,
    DecisionCandidate,
};
use crowdrelay_domain::{
    OutreachTargetId, TraceContext, WorkspaceId,
    autonomy::{Confidence, PolicyDisposition},
    outreach::OutreachPhase,
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId, Box<dyn std::error::Error>> {
    let id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'No executor')")
        .bind(id.into_uuid())
        .bind(format!("no-exec-{}", id.into_uuid().simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn target(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<OutreachTargetId, Box<dyn std::error::Error>> {
    let id = OutreachTargetId::new();
    sqlx::query(
        "INSERT INTO outreach_targets (
             id, workspace_id, target_kind, display_name, contact_email,
             active, verified, accepts_outreach
         ) VALUES ($1,$2,'radio','Radio',$3,true,true,true)",
    )
    .bind(id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("{}@radio.example", id.into_uuid().simple()))
    .execute(pool)
    .await?;
    Ok(id)
}

/// A live executor that advertises only `capabilities`.
async fn register_executor(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    capabilities: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO executor_instances (
             workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
         ) VALUES ($1,'n8n-no-exec-test','test','test-manifest',$2,$3)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    for capability in capabilities {
        sqlx::query(
            "INSERT INTO executor_capabilities (
                 workspace_id, executor_id, capability, capability_version, observed_at, expires_at
             ) VALUES ($1,'n8n-no-exec-test',$2,'1',$3,$4)",
        )
        .bind(workspace_id.into_uuid())
        .bind(*capability)
        .bind(now)
        .bind(now + time::Duration::minutes(30))
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn outreach_candidate(
    target_id: OutreachTargetId,
    disposition: PolicyDisposition,
) -> DecisionCandidate {
    let opportunity_id = crowdrelay_domain::OutreachOpportunityId::from_uuid(Uuid::now_v7());
    let nonce = Uuid::now_v7();
    DecisionCandidate {
        context: AutopilotContext::Outreach,
        subject: ActionSubject::OutreachOpportunity(opportunity_id),
        decision_kind: "request_relationship_outreach",
        confidence: Confidence::from_basis_points(8_000).expect("valid basis points"),
        disposition,
        reason: "verified relationship target matches a fresh high-relevance opportunity",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({}),
        action: AutopilotActionPayload::RequestOutreach {
            opportunity_id,
            target_id,
            target_version: 1,
            target_name: target_id.to_string(),
            phase: OutreachPhase::Initial,
            template_key: "outreach.radio.v1".to_owned(),
            wave_id: None,
            draft: crowdrelay_domain::outreach_letter::OutreachLetter::default(),
        },
        decision_key: format!("decision:test-no-exec:{nonce}"),
        action_idempotency_key: format!("action:test-no-exec:{nonce}"),
    }
}

async fn decision_disposition(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    decision_key: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT disposition FROM autopilot_decisions WHERE workspace_id=$1 AND decision_key=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(decision_key)
    .fetch_one(pool)
    .await?)
}

async fn action_status(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    idempotency_key: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT status FROM autopilot_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn work_no_executor_can_perform_is_recommended_not_asked()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let persist = |ws: WorkspaceId, candidate: DecisionCandidate| {
        let repository = &repository;
        async move {
            repository
                .persist_candidate(ws, &candidate, &TraceContext::root(ws))
                .await
                .map(|persisted| persisted.action_created)
        }
    };

    // No registry: nothing has ever advertised anything, so nothing is
    // judged missing and the ask goes out as it always has.
    let open = workspace(&pool).await?;
    let open_target = target(&pool, open).await?;
    let asked = outreach_candidate(open_target, PolicyDisposition::RequireApproval);
    assert!(persist(open, asked.clone()).await?);
    assert_eq!(
        action_status(&pool, open, &asked.action_idempotency_key)
            .await?
            .as_deref(),
        Some("awaiting_approval")
    );

    // A live executor that does not send outreach: the finding is kept as a
    // recommendation and nobody is asked.
    let ws = workspace(&pool).await?;
    let ws_target = target(&pool, ws).await?;
    register_executor(&pool, ws, &["fan.lifecycle.message"]).await?;
    let withheld = outreach_candidate(ws_target, PolicyDisposition::RequireApproval);
    assert!(!persist(ws, withheld.clone()).await?);
    assert_eq!(
        decision_disposition(&pool, ws, &withheld.decision_key).await?,
        "recommend_only"
    );
    let held_by: serde_json::Value = sqlx::query_scalar(
        "SELECT policy_snapshot->'held_by' FROM autopilot_decisions
         WHERE workspace_id=$1 AND decision_key=$2",
    )
    .bind(ws.into_uuid())
    .bind(&withheld.decision_key)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        held_by,
        serde_json::json!(["no_executor:outreach.send"]),
        "the decision says what held it"
    );
    assert_eq!(
        action_status(&pool, ws, &withheld.action_idempotency_key).await?,
        None,
        "no approval card for work nothing can perform"
    );

    // Unattended work is held the same way: queued, it would sit parked on
    // the week's outward budget until the stale sweep cancelled it.
    let unattended = outreach_candidate(ws_target, PolicyDisposition::AutoExecute);
    assert!(!persist(ws, unattended.clone()).await?);
    assert_eq!(
        action_status(&pool, ws, &unattended.action_idempotency_key).await?,
        None
    );

    // Once the capability is advertised, the same decision becomes the ask.
    register_executor(&pool, ws, &["outreach.send"]).await?;
    assert!(persist(ws, withheld.clone()).await?);
    assert_eq!(
        action_status(&pool, ws, &withheld.action_idempotency_key)
            .await?
            .as_deref(),
        Some("awaiting_approval")
    );
    Ok(())
}
