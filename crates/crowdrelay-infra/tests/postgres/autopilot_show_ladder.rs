//! The show-ladder approval against a real Postgres (P.4).
//!
//! What only real rows can prove: the approval flips the rung parked on the
//! event and marks it `operator:show_ladder` so a later revoke cancels exactly
//! that — never a rung a person approved on its own, and never a rung in
//! another workspace. The action-ledger trigger sits underneath every one of
//! those transitions, so the state machine itself is the test's first
//! assertion.

use std::time::Duration;

use crate::common;
use crowdrelay_application::{
    IdempotencyKey,
    autopilot::{AutopilotControlRepository, AutopilotDecisionRepository},
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: Uuid,
    other_workspace: WorkspaceId,
    other_event: Uuid,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let now = OffsetDateTime::now_utc();

    let workspace_id = WorkspaceId::new();
    let other_workspace = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    for (ws, slug) in [
        (workspace_id.into_uuid(), "ladder"),
        (other_workspace.into_uuid(), "foreign"),
    ] {
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
            .bind(ws)
            .bind(format!("{label}-{slug}-{suffix}"))
            .bind(slug)
            .execute(&pool)
            .await?;
    }
    let city_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .bind(city_id)
    .bind(format!("ladder-city-{suffix}"))
    .bind("Ladder City")
    .execute(&pool)
    .await?;

    let event_id = Uuid::now_v7();
    let other_event = Uuid::now_v7();
    for (ws, event, slug) in [
        (workspace_id.into_uuid(), event_id, "ladder-night"),
        (other_workspace.into_uuid(), other_event, "foreign-night"),
    ] {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, city_id, slug, title, starts_at, status, published_at) \
             VALUES ($1, $2, $3, $4, 'Ladder Night', $5, 'published', $6)",
        )
        .bind(event)
        .bind(ws)
        .bind(city_id)
        .bind(format!("{slug}-{suffix}"))
        .bind(now + time::Duration::days(30))
        .bind(now)
        .execute(&pool)
        .await?;
    }

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
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        event_id,
        other_workspace,
        other_event,
    })
}

/// Seeds a decision + action pair in the given status on the given event.
/// `action_kind` varies so the inflight-subject unique index — one inflight
/// rung per (workspace, context, kind, event) — never masks the test.
async fn seed_rung(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    event_id: Uuid,
    lever: &str,
    action_kind: &str,
    status: &str,
    approved_by: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'show_growth','event',$4,
                  'activate_show_growth_lever',9000,'require_approval','seed rung',
                  '{}','{}','{}',$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(event_id)
    .bind(now)
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, approved_at, approved_by, available_at
        ) VALUES ($1,$2,$3,'show_growth',$4,'event',$5,$6,$7,$8,$9,$10,$11)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(action_kind)
    .bind(event_id)
    .bind(format!("rung-{action_id}"))
    .bind(serde_json::json!({"lever": lever}))
    .bind(status)
    .bind(approved_by.map(|_| now))
    .bind(approved_by)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(action_id)
}

async fn rung_state(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, approved_by FROM autopilot_actions \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn ladder_approval_releases_parked_rungs_and_revoke_cancels_only_its_own()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("ladder").await?;
    // One owned rung parked on the event, one relationship-sensitive partner
    // rung parked beside it, one foreign rung, and one already queued by a
    // person's own approval — the ladder must tell all four apart.
    let parked = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        fixture.event_id,
        "fan_ambassadors",
        "show.growth.request",
        "awaiting_approval",
        None,
    )
    .await?;
    let partner = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        fixture.event_id,
        "partner_cross_promo",
        "show.growth.request.partner",
        "awaiting_approval",
        None,
    )
    .await?;
    let foreign = seed_rung(
        &fixture.pool,
        fixture.other_workspace,
        fixture.other_event,
        "partner_cross_promo",
        "show.growth.request",
        "awaiting_approval",
        None,
    )
    .await?;
    let individual = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        fixture.event_id,
        "post_show_recap",
        "content.artifact.request",
        "queued",
        Some("operator:admin_api_key"),
    )
    .await?;

    let mutation = fixture
        .repository
        .approve_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-approve-1")?,
            None,
        )
        .await?;
    assert_eq!(
        mutation.status, "approved:1",
        "only the owned rung released"
    );

    // The live approval row is what future snapshots read.
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM show_ladder_approvals \
         WHERE workspace_id=$1 AND event_id=$2 AND revoked_at IS NULL)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert!(live);

    let (status, approved_by) = rung_state(&fixture.pool, fixture.workspace_id, parked).await?;
    assert_eq!(status, "queued");
    assert_eq!(approved_by.as_deref(), Some("operator:show_ladder"));
    let (partner_status, partner_approved_by) =
        rung_state(&fixture.pool, fixture.workspace_id, partner).await?;
    assert_eq!(
        partner_status, "awaiting_approval",
        "partner outreach stays with the booker"
    );
    assert!(partner_approved_by.is_none());
    let (foreign_status, _) = rung_state(&fixture.pool, fixture.other_workspace, foreign).await?;
    assert_eq!(
        foreign_status, "awaiting_approval",
        "another workspace's rung stays parked"
    );

    // Revoke: the ladder's own release cancels; the individual approval and
    // the still-human-owned partner rung stand.
    let mutation = fixture
        .repository
        .revoke_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-revoke-1")?,
            None,
        )
        .await?;
    assert_eq!(mutation.status, "revoked:1");

    let (status, _) = rung_state(&fixture.pool, fixture.workspace_id, parked).await?;
    assert_eq!(
        status, "cancelled",
        "a ladder-released rung stops with the ladder"
    );
    let (status, approved_by) = rung_state(&fixture.pool, fixture.workspace_id, individual).await?;
    assert_eq!(status, "queued");
    assert_eq!(approved_by.as_deref(), Some("operator:admin_api_key"));
    let (partner_status, _) = rung_state(&fixture.pool, fixture.workspace_id, partner).await?;
    assert_eq!(partner_status, "awaiting_approval");

    // The same revoke key replays instead of double-cancelling.
    let replay = fixture
        .repository
        .revoke_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-revoke-1")?,
            None,
        )
        .await?;
    assert!(replay.replayed);

    // Revoking a ladder that is not live is a conflict, not a silent no-op.
    let result = fixture
        .repository
        .revoke_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-revoke-2")?,
            None,
        )
        .await;
    assert!(result.is_err());

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn quiet_tenant_show_ladder_still_keeps_partner_human_owned()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("ladder-quiet").await?;
    let partner = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        fixture.event_id,
        "partner_cross_promo",
        "show.growth.request.partner",
        "awaiting_approval",
        None,
    )
    .await?;

    let mutation = fixture
        .repository
        .approve_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-approve-quiet")?,
            None,
        )
        .await?;
    assert_eq!(
        mutation.status, "approved:0",
        "a broad ladder releases no relationship-sensitive rung"
    );

    let (status, approved_by) = rung_state(&fixture.pool, fixture.workspace_id, partner).await?;
    assert_eq!(
        status, "awaiting_approval",
        "booking silence is not permission to spend the relationship"
    );
    assert!(approved_by.is_none());
    Ok(())
}

/// The second half of the ladder: a rung decided *after* the approval lands
/// `queued` directly — never parked — and carries the same
/// `operator:show_ladder` provenance a release does, so revoke still reaches
/// it. The decision row keeps `require_approval`: the ledger records what the
/// policy asked, and the ladder row records who answered.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn ladder_authorized_candidates_queue_with_operator_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("ladder-persist").await?;

    fixture
        .repository
        .approve_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-approve-persist")?,
            None,
        )
        .await?;

    // A candidate exactly as the evaluator emits it under a live ladder:
    // RequireApproval disposition plus the provenance flag.
    let candidate = crowdrelay_application::autopilot::DecisionCandidate {
        context: crowdrelay_application::autopilot::AutopilotContext::ShowGrowth,
        subject: crowdrelay_application::autopilot::ActionSubject::Event(
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
        ),
        decision_kind: "activate_show_growth_lever",
        confidence: crowdrelay_domain::autonomy::Confidence::from_basis_points(9_000)?,
        disposition: crowdrelay_domain::autonomy::PolicyDisposition::RequireApproval,
        reason: "a bounded attendance-growth lever is due from first-party show evidence",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({"ladder_authorized": true}),
        action: crowdrelay_application::autopilot::AutopilotActionPayload::RequestShowGrowth {
            event_id: crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            lever: crowdrelay_domain::show_growth::ShowGrowthLever::FanAmbassadors,
            template_key: "fan_ambassadors".to_owned(),
            send_at: None,
        },
        decision_key: format!("decision:test-ladder:{}", Uuid::now_v7()),
        action_idempotency_key: format!("action:test-ladder:{}", Uuid::now_v7()),
    };
    let persisted = fixture
        .repository
        .persist_candidate(
            fixture.workspace_id,
            &candidate,
            &crowdrelay_domain::TraceContext::root(fixture.workspace_id),
        )
        .await?;
    assert!(persisted.action_created);

    let (status, approved_by, available_at): (String, Option<String>, OffsetDateTime) =
        sqlx::query_as(
            "SELECT status, approved_by, available_at FROM autopilot_actions \
             WHERE workspace_id=$1 AND subject_id=$2 AND context='show_growth'",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(fixture.event_id)
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(status, "queued", "the ladder pre-authorized the human gate");
    assert_eq!(approved_by.as_deref(), Some("operator:show_ladder"));
    // FanAmbassadors is an outward class — the hold window is what makes the
    // revoke below meaningful, so the rung must not be claimable yet.
    assert!(
        available_at > OffsetDateTime::now_utc(),
        "the outward hold still applies under the ladder"
    );

    // A revoked ladder reaches a queued rung it pre-authorized, not just the
    // ones it released by hand.
    fixture
        .repository
        .revoke_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-revoke-persist")?,
            None,
        )
        .await?;
    let (status, _): (String, Option<String>) = sqlx::query_as(
        "SELECT status, approved_by FROM autopilot_actions \
         WHERE workspace_id=$1 AND subject_id=$2 AND context='show_growth'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(status, "cancelled");

    // A mistyped or foreign event id is a not-found, not a conflict that
    // would imply a ladder existed.
    let result = fixture
        .repository
        .approve_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(Uuid::now_v7()),
            &IdempotencyKey::parse("ladder-approve-missing")?,
            None,
        )
        .await;
    assert!(
        result.is_err(),
        "approve on a missing event must not orphan"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn ladder_flag_cannot_pre_authorize_relationship_sensitive_partner_action()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("ladder-partner-persist").await?;

    fixture
        .repository
        .approve_show_ladder(
            fixture.workspace_id,
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            &IdempotencyKey::parse("ladder-approve-partner-persist")?,
            None,
        )
        .await?;

    // Deliberately forge the stale/legacy shape: even if some upstream caller
    // still carries ladder_authorized=true, persistence owns the final safety
    // boundary for relationship-sensitive promotion.
    let candidate = crowdrelay_application::autopilot::DecisionCandidate {
        context: crowdrelay_application::autopilot::AutopilotContext::ShowGrowth,
        subject: crowdrelay_application::autopilot::ActionSubject::Event(
            crowdrelay_domain::EventId::from_uuid(fixture.event_id),
        ),
        decision_kind: "activate_show_growth_lever",
        confidence: crowdrelay_domain::autonomy::Confidence::from_basis_points(9_000)?,
        disposition: crowdrelay_domain::autonomy::PolicyDisposition::RequireApproval,
        reason: "relationship-sensitive show promotion",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({"ladder_authorized": true}),
        action: crowdrelay_application::autopilot::AutopilotActionPayload::RequestShowGrowth {
            event_id: crowdrelay_domain::EventId::from_uuid(fixture.event_id),
            lever: crowdrelay_domain::show_growth::ShowGrowthLever::PartnerCrossPromo,
            template_key: "partner_cross_promo".to_owned(),
            send_at: None,
        },
        decision_key: format!("decision:test-ladder-partner:{}", Uuid::now_v7()),
        action_idempotency_key: format!("action:test-ladder-partner:{}", Uuid::now_v7()),
    };

    let persisted = fixture
        .repository
        .persist_candidate(
            fixture.workspace_id,
            &candidate,
            &crowdrelay_domain::TraceContext::root(fixture.workspace_id),
        )
        .await?;
    assert!(persisted.action_created);

    let (status, approved_by): (String, Option<String>) = sqlx::query_as(
        "SELECT status, approved_by FROM autopilot_actions \
         WHERE workspace_id=$1 AND subject_id=$2 \
           AND idempotency_key=$3",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id)
    .bind(&candidate.action_idempotency_key)
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(status, "awaiting_approval");
    assert!(
        approved_by.is_none(),
        "the booker must still approve this ask"
    );

    Ok(())
}
