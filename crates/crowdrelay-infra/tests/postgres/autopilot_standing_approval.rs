//! Standing approvals against a real Postgres (WS4).
//!
//! What only real rows can prove: a live grant on the exact
//! (action_kind, target_key) turns a `require_approval` outreach candidate
//! into a queued action marked `operator:standing_grant` — while the
//! decision row keeps the disposition the policy actually returned — and
//! a grant that is revoked, expired, on another target, on another kind or
//! in another workspace does not. The re-raise bound needs the same rows:
//! a proposal whose ask family already died `MAX_APPROVAL_ASKS` times must
//! stop minting cards while the decision still records it.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    ActionSubject, AutopilotActionPayload, AutopilotContext, AutopilotDecisionRepository,
    DecisionCandidate,
};
use crowdrelay_domain::{
    OutreachTargetId, TraceContext, WorkspaceId,
    action_class::ActionClass,
    autonomy::{Confidence, PolicyDisposition},
    outreach::OutreachPhase,
};
use crowdrelay_infra::{
    autopilot::PostgresAutopilotRepository,
    config::DatabaseConfig,
    standing_approvals::{GrantRequest, grant, revoke},
};
use time::OffsetDateTime;
use uuid::Uuid;

#[path = "autopilot_standing_approval/episode_tests.rs"]
mod episode_tests;
#[path = "autopilot_standing_approval/install_tests.rs"]
mod install_tests;
#[path = "autopilot_standing_approval/snapshot_tests.rs"]
mod snapshot_tests;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    other_workspace: WorkspaceId,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let other_workspace = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    for (ws, slug) in [
        (workspace_id.into_uuid(), "grant"),
        (other_workspace.into_uuid(), "foreign"),
    ] {
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
            .bind(ws)
            .bind(format!("{label}-{slug}-{suffix}"))
            .bind(slug)
            .execute(&pool)
            .await?;
    }
    Ok(Fixture {
        pool: pool.clone(),
        repository: PostgresAutopilotRepository::new(
            pool,
            &DatabaseConfig {
                url: database_url,
                max_connections: 4,
                connect_timeout: Duration::from_secs(3),
                ping_timeout: Duration::from_secs(2),
                operation_timeout: Duration::from_secs(5),
                lock_timeout: Duration::from_secs(1),
            },
        ),
        workspace_id,
        other_workspace,
    })
}

/// A radio/press-style target row — `enrich_outreach_draft` refuses a
/// `RequestOutreach` naming a target that does not exist.
async fn insert_target(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
) -> Result<OutreachTargetId, Box<dyn std::error::Error>> {
    let target_id = OutreachTargetId::new();
    sqlx::query(
        "INSERT INTO outreach_targets (
             id, workspace_id, target_kind, display_name, contact_email,
             active, verified, accepts_outreach
         ) VALUES ($1,$2,'radio',$3,$4,true,true,true)",
    )
    .bind(target_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "Radio {}",
        &target_id.into_uuid().simple().to_string()[..6]
    ))
    .bind(format!("{}@radio.example", target_id.into_uuid().simple()))
    .execute(&fixture.pool)
    .await?;
    Ok(target_id)
}

/// The candidate exactly as the evaluator emits it for a verified outreach
/// target under a require-approval policy.
fn outreach_candidate(target_id: OutreachTargetId, nonce: Uuid) -> DecisionCandidate {
    let opportunity_id = Uuid::now_v7();
    DecisionCandidate {
        context: AutopilotContext::Outreach,
        subject: ActionSubject::OutreachOpportunity(
            crowdrelay_domain::OutreachOpportunityId::from_uuid(opportunity_id),
        ),
        decision_kind: "request_relationship_outreach",
        confidence: Confidence::from_basis_points(8_000).expect("valid basis points"),
        disposition: PolicyDisposition::RequireApproval,
        reason: "verified relationship target matches a fresh high-relevance opportunity",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({}),
        action: AutopilotActionPayload::RequestOutreach {
            opportunity_id: crowdrelay_domain::OutreachOpportunityId::from_uuid(opportunity_id),
            target_id,
            target_version: 1,
            target_name: target_id.to_string(),
            phase: OutreachPhase::Initial,
            template_key: "outreach.radio.v1".to_owned(),
            wave_id: None,
            draft: crowdrelay_domain::outreach_letter::OutreachLetter::default(),
        },
        decision_key: format!("decision:test-grant:{nonce}"),
        action_idempotency_key: format!("action:test-grant:{nonce}"),
    }
}

async fn persist(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    candidate: &DecisionCandidate,
) -> Result<bool, Box<dyn std::error::Error>> {
    let persisted = fixture
        .repository
        .persist_candidate(workspace_id, candidate, &TraceContext::root(workspace_id))
        .await?;
    Ok(persisted.action_created)
}

async fn action_state(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    idempotency_key: &str,
) -> Result<Option<(String, Option<String>, OffsetDateTime)>, Box<dyn std::error::Error>> {
    Ok(
        sqlx::query_as::<_, (String, Option<String>, OffsetDateTime)>(
            "SELECT status, approved_by, available_at FROM autopilot_actions \
         WHERE workspace_id=$1 AND idempotency_key=$2",
        )
        .bind(workspace_id.into_uuid())
        .bind(idempotency_key)
        .fetch_optional(&fixture.pool)
        .await?,
    )
}

/// One dead ask of this proposal's key family, as the lapse sweep leaves
/// them: cancelled, `approval_expired`, re-keyed off the live key.
async fn seed_lapsed_ask(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    idempotency_key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,
                  'request_relationship_outreach',8000,'require_approval','seed lapsed',
                  '{}','{}','{}',$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-lapsed-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(now)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, approved_by, available_at,
            finished_at, last_error_kind
        ) VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',$4,$5,$6,'cancelled',
                  NULL,$7,$7,'approval_expired')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(idempotency_key)
    .bind(serde_json::json!({"kind": "request_outreach"}))
    .bind(now)
    .execute(&fixture.pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_live_grant_queues_the_ask_without_losing_the_audit_answer()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("grant-queue").await?;
    let target = insert_target(&fixture, fixture.workspace_id).await?;

    grant(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "outreach.request",
            target_key: &target.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        OffsetDateTime::now_utc(),
    )
    .await?;

    let candidate = outreach_candidate(target, Uuid::now_v7());
    assert!(persist(&fixture, fixture.workspace_id, &candidate).await?);

    let (status, approved_by, available_at) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "queued");
    assert_eq!(approved_by.as_deref(), Some("operator:standing_grant"));
    // Third-party sends hold the class window even under a grant — the hold
    // is the window a revoke can still act inside.
    assert!(
        available_at > OffsetDateTime::now_utc(),
        "the third-party hold still applies under a grant"
    );

    // The decision keeps the policy's own answer: require_approval, not a
    // pretend auto-execute. The grant answered the gate; it did not change
    // what the policy computed.
    let disposition: String = sqlx::query_scalar(
        "SELECT disposition FROM autopilot_decisions \
         WHERE workspace_id=$1 AND decision_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(&candidate.decision_key)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(disposition, "require_approval");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_grant_is_never_a_wildcard() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("grant-scope").await?;
    let covered = insert_target(&fixture, fixture.workspace_id).await?;
    let other_target = insert_target(&fixture, fixture.workspace_id).await?;
    let now = OffsetDateTime::now_utc();

    // Grant on the other target — same kind, different target_key.
    grant(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "outreach.request",
            target_key: &other_target.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    let candidate = outreach_candidate(covered, Uuid::now_v7());
    persist(&fixture, fixture.workspace_id, &candidate).await?;
    let (status, approved_by, _) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "awaiting_approval");
    assert_eq!(approved_by, None, "no provenance on an unanswered ask");

    // Grant on the same target but a different kind — a beacon grant does
    // not license a press pitch.
    grant(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "beacon.outreach.request",
            target_key: &covered.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    let candidate = outreach_candidate(covered, Uuid::now_v7());
    persist(&fixture, fixture.workspace_id, &candidate).await?;
    let (status, _, _) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "awaiting_approval");

    // A revoked grant stops answering immediately — the read happens inside
    // the write transaction for exactly this reason.
    grant(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "outreach.request",
            target_key: &covered.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    revoke(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        "outreach.request",
        &covered.to_string(),
        "operator:test",
        now,
    )
    .await?;
    let candidate = outreach_candidate(covered, Uuid::now_v7());
    persist(&fixture, fixture.workspace_id, &candidate).await?;
    let (status, approved_by, _) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "awaiting_approval");
    assert_eq!(approved_by, None);

    // An expired grant is dead too — force the expiry rather than waiting.
    grant(
        &fixture.pool,
        fixture.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "outreach.request",
            target_key: &covered.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    // `expires_at > granted_at` is a CHECK, so an expired grant is aged by
    // moving the whole grant into the past, not by violating the relation.
    sqlx::query(
        "UPDATE standing_approvals \
         SET granted_at = now() - INTERVAL '40 days', \
             expires_at = now() - INTERVAL '1 hour' \
         WHERE workspace_id=$1 AND action_kind='outreach.request' AND target_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(covered.to_string())
    .execute(&fixture.pool)
    .await?;
    let candidate = outreach_candidate(covered, Uuid::now_v7());
    persist(&fixture, fixture.workspace_id, &candidate).await?;
    let (status, _, _) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "awaiting_approval");

    // A grant in another workspace never reaches this one.
    let foreign_target = insert_target(&fixture, fixture.other_workspace).await?;
    grant(
        &fixture.pool,
        fixture.other_workspace.into_uuid(),
        GrantRequest {
            action_kind: "outreach.request",
            target_key: &foreign_target.to_string(),
            class: ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    // Same target_key text as the foreign grant — a raw key match without
    // the workspace scope would leak the grant across tenants.
    let candidate = outreach_candidate(covered, Uuid::now_v7());
    persist(&fixture, fixture.workspace_id, &candidate).await?;
    let (status, _, _) = action_state(
        &fixture,
        fixture.workspace_id,
        &candidate.action_idempotency_key,
    )
    .await?
    .expect("the action was minted");
    assert_eq!(status, "awaiting_approval");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_proposal_stops_re_raising_after_the_third_dead_ask()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("grant-reraise").await?;
    let target = insert_target(&fixture, fixture.workspace_id).await?;
    let base_key = format!("action:outreach:{}:Initial:0", target);

    // Two dead asks still leave one re-raise: initial + two = three cards.
    seed_lapsed_ask(
        &fixture,
        fixture.workspace_id,
        &format!("{base_key}:lapsed:{}", Uuid::now_v7()),
    )
    .await?;
    seed_lapsed_ask(
        &fixture,
        fixture.workspace_id,
        &format!("{base_key}:lapsed:{}", Uuid::now_v7()),
    )
    .await?;
    let mut candidate = outreach_candidate(target, Uuid::now_v7());
    candidate.action_idempotency_key = base_key.clone();
    candidate.decision_key = format!("decision:test-reraise:{}", Uuid::now_v7());
    assert!(
        persist(&fixture, fixture.workspace_id, &candidate).await?,
        "two dead asks must still let the third card mint"
    );

    // The third lapse lands the family at the bound — the next identical
    // proposal mints no action.
    seed_lapsed_ask(
        &fixture,
        fixture.workspace_id,
        &format!("{base_key}:lapsed:{}", Uuid::now_v7()),
    )
    .await?;
    let mut candidate = outreach_candidate(target, Uuid::now_v7());
    candidate.action_idempotency_key = base_key.clone();
    candidate.decision_key = format!("decision:test-reraise:{}", Uuid::now_v7());
    assert!(
        !persist(&fixture, fixture.workspace_id, &candidate).await?,
        "the fourth card for the same ask must not be minted"
    );
    // The decision still recorded the proposal — persistence refused the
    // card, not the ledger.
    let decisions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM autopilot_decisions \
         WHERE workspace_id=$1 AND decision_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(&candidate.decision_key)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(decisions, 1);
    Ok(())
}
