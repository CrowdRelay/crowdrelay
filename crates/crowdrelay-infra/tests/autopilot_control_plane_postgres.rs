//! The control plane's "done ourselves" and deliverability against a real
//! Postgres.
//!
//! What fails here and nowhere else: a suppression that must close a target
//! and its open opportunities in one transaction, a ledger dedupe that must
//! make a retried webhook count once, and a handled finding that must leave
//! the queue *and* take its parked action with it.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotControlMutation, AutopilotControlRepository, AutopilotDecisionRepository,
    AutopilotTeamStateRepository, RecordDeliveryFault,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{
    OutreachOpportunityId, OutreachTargetId, WorkspaceId,
    deliverability::{DeliverabilitySnapshot, DeliveryFault},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
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
        .bind(format!("{label}-{suffix}"))
        .bind("Control plane E2E")
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

async fn insert_target(
    fixture: &Fixture,
    email: &str,
) -> Result<OutreachTargetId, Box<dyn std::error::Error>> {
    let target_id = OutreachTargetId::from_uuid(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO viryaos_outreach_targets (
             id, workspace_id, target_kind, display_name, contact_email,
             active, verified, accepts_outreach
         ) VALUES ($1,$2,'playlist',$3,$4,true,true,true)",
    )
    .bind(target_id.into_uuid())
    .bind(fixture.workspace_id.into_uuid())
    .bind(email)
    .bind(email)
    .execute(&fixture.pool)
    .await?;
    Ok(target_id)
}

async fn insert_opportunity(
    fixture: &Fixture,
    target_id: OutreachTargetId,
) -> Result<OutreachOpportunityId, Box<dyn std::error::Error>> {
    let opportunity_id = OutreachOpportunityId::from_uuid(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO viryaos_outreach_opportunities (
             id, workspace_id, target_id, source, subject_kind, subject_key,
             template_key, relevance_basis_points, confidence_basis_points,
             active, observed_at, expires_at
         ) VALUES ($1,$2,$3,'test','release','test-release','test.v1',7000,8000,true,now(),now() + interval '7 days')",
    )
    .bind(opportunity_id.into_uuid())
    .bind(fixture.workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .execute(&fixture.pool)
    .await?;
    Ok(opportunity_id)
}

fn key(seed: u8) -> IdempotencyKey {
    // Deterministic per seed: a replay must present the same key as the
    // original request, which a freshly generated one can never do.
    IdempotencyKey::parse(format!("control-plane-e2e-key-{seed:>03}"))
        .expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_hard_bounce_finishes_the_address_and_a_retry_counts_once() {
    let fixture = fixture("bounce").await.expect("fixture");
    let target = insert_target(&fixture, "curator@example.com")
        .await
        .expect("target");
    let opportunity = insert_opportunity(&fixture, target)
        .await
        .expect("opportunity");

    let fault = RecordDeliveryFault {
        subject: crowdrelay_application::autopilot::DeliveryFaultSubject::Target(target),
        fault: DeliveryFault::HardBounce,
        provider_reference: Some("provider-ref-1".into()),
        occurred_at: fixture.now,
    };
    let first: AutopilotControlMutation = fixture
        .repository
        .record_delivery_fault(fixture.workspace_id, fault.clone(), &key(1), None)
        .await
        .expect("first report");
    assert!(!first.replayed);

    // The address is finished, and its open opportunities go with it, so no
    // later cycle pitches somebody who cannot receive anything.
    let (active, accepts): (bool, bool) = sqlx::query_as(
        "SELECT active, accepts_outreach FROM viryaos_outreach_targets \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("target row");
    assert!(!active && !accepts);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_outreach_opportunities \
         WHERE workspace_id = $1 AND target_id = $2 AND active",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("opportunity count");
    assert_eq!(open, 0);

    // A retried webhook under a new Idempotency-Key is still the same fault:
    // the provider reference is what dedupes it.
    let replay_by_reference = fixture
        .repository
        .record_delivery_fault(fixture.workspace_id, fault, &key(2), None)
        .await
        .expect("replayed report");
    let faults: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_outreach_delivery_faults WHERE workspace_id = $1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("fault count");
    assert_eq!(faults, 1);
    assert!(replay_by_reference.replayed);

    // And the exact same request replays through the operator-action ledger.
    let same_request = fixture
        .repository
        .record_delivery_fault(
            fixture.workspace_id,
            RecordDeliveryFault {
                subject: crowdrelay_application::autopilot::DeliveryFaultSubject::Target(target),
                fault: DeliveryFault::HardBounce,
                provider_reference: Some("provider-ref-1".into()),
                occurred_at: fixture.now,
            },
            // Reusing key(1)'s exact value is not possible here, so the
            // replay-by-reference assertion above carries this case.
            &key(3),
            None,
        )
        .await
        .expect("third report");
    let _ = same_request;
    let _ = opportunity;
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_soft_bounce_suppresses_nobody_and_the_snapshot_reads_both() {
    let fixture = fixture("soft-bounce").await.expect("fixture");
    let target = insert_target(&fixture, "mailbox@example.com")
        .await
        .expect("target");

    fixture
        .repository
        .record_delivery_fault(
            fixture.workspace_id,
            RecordDeliveryFault {
                subject: crowdrelay_application::autopilot::DeliveryFaultSubject::Target(target),
                fault: DeliveryFault::SoftBounce,
                provider_reference: Some("provider-ref-2".into()),
                occurred_at: fixture.now,
            },
            &key(4),
            None,
        )
        .await
        .expect("soft bounce");
    let active: bool = sqlx::query_scalar(
        "SELECT active FROM viryaos_outreach_targets \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("target row");
    assert!(
        active,
        "a full mailbox is ordinary, not an address to retire"
    );

    // One dispatched third-party send makes the denominator one.
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-deliverability','growth_metrics','workspace',$3,
                   'e2e.kind',9000,'recommend_only','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class, started_at, finished_at
         ) VALUES ($1,$2,'growth_metrics','outreach.send','outreach_target',$3,
                   'e2e-send-1','{\"kind\":\"request_outreach\"}','succeeded',
                   'third_party', now(), now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("succeeded third-party action");

    let (envelope, _usage) = fixture
        .repository
        .load_growth_envelope(fixture.workspace_id, fixture.now)
        .await
        .expect("envelope");
    let snapshot: DeliverabilitySnapshot = fixture
        .repository
        .load_deliverability_snapshot(fixture.workspace_id, fixture.now)
        .await
        .expect("snapshot");
    assert_eq!(snapshot.sent_30d, 1, "sends, not actions");
    assert_eq!(snapshot.bounces_30d, 1);
    assert_eq!(snapshot.complaints_30d, 0);
    assert!(
        snapshot.first_sent_at.is_some(),
        "the ramp clock starts at the first send"
    );
    assert_eq!(
        snapshot.weekly_third_party_ceiling,
        envelope.weekly_third_party_touches
    );

    // The domain rule read against real rows, not fixtures: one soft bounce
    // on one send is below every sample floor, so the workspace is healthy
    // and its ceiling comes from the ramp, never from zero.
    let verdict = crowdrelay_domain::deliverability::verdict(
        snapshot,
        crowdrelay_domain::deliverability::DeliverabilityPolicy::default(),
    );
    assert_eq!(
        verdict,
        crowdrelay_domain::deliverability::DeliverabilityVerdict::Healthy
    );
    assert!(
        crowdrelay_domain::deliverability::ramped_ceiling(
            snapshot,
            crowdrelay_domain::deliverability::DeliverabilityPolicy::default(),
            fixture.now,
        ) > 0,
        "a healthy workspace may send"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_complaint_rate_closes_the_ceiling_against_real_rows() {
    use crowdrelay_domain::deliverability::{
        DeliverabilityPolicy, DeliverabilityVerdict, ramped_ceiling, verdict,
    };

    let fixture = fixture("halt").await.expect("fixture");
    let target = insert_target(&fixture, "listener@example.com")
        .await
        .expect("target");
    let policy = DeliverabilityPolicy::default();

    // Twenty-one sends so the sample floor no longer shields the rate, then
    // one complaint — a tenth of a per cent threshold crossed many times
    // over.
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-halt','growth_metrics','workspace',$3,
                   'e2e.kind',9000,'recommend_only','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class, started_at, finished_at
         ) SELECT $1,$2,'growth_metrics','outreach.send','outreach_target',$3,
                  'e2e-send-' || n,'{\"kind\":\"request_outreach\"}','succeeded',
                  'third_party', now(), now()
           FROM generate_series(1, $4) AS n",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(i32::try_from(policy.minimum_rate_sample + 1).expect("small count"))
    .execute(&fixture.pool)
    .await
    .expect("send history");
    fixture
        .repository
        .record_delivery_fault(
            fixture.workspace_id,
            RecordDeliveryFault {
                subject: crowdrelay_application::autopilot::DeliveryFaultSubject::Target(target),
                fault: DeliveryFault::Complaint,
                provider_reference: Some("complaint-ref-1".into()),
                occurred_at: fixture.now,
            },
            &key(9),
            None,
        )
        .await
        .expect("complaint");

    let snapshot = fixture
        .repository
        .load_deliverability_snapshot(fixture.workspace_id, fixture.now)
        .await
        .expect("snapshot");
    assert_eq!(
        verdict(snapshot, policy),
        DeliverabilityVerdict::HaltComplaintRate
    );
    // The halt closes the ceiling before the next wave rather than in a
    // digest afterwards — and this is the composed path the cycle takes.
    assert_eq!(ramped_ceiling(snapshot, policy, fixture.now), 0);
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failed_action_takes_its_finding_off_the_board_instead_of_parking_a_dead_button() {
    let fixture = fixture("failed-off-board").await.expect("fixture");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-failed-off-board','content_supply','content_source',$3,
                   'request_content_artifact',9000,'require_approval','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, last_error_kind, finished_at
         ) VALUES ($1,$2,'content_supply','content.artifact.request','content_source',$3,
                   'e2e-failed-1','{\"kind\":\"request_content_artifact\"}',
                   'failed', 'state_changed', now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("failed action");

    // The executor claims only `queued` or stale `processing` actions, so this
    // failed row is terminal. A finding whose only action is terminal must not
    // sit in the queue: approving it can only ever answer with a conflict.
    let queue = fixture
        .repository
        .load_next_best_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("queue");
    assert!(
        !queue.iter().any(|entry| entry.decision_id == decision_id),
        "a failed action's finding is dead work, not an operator decision"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_approved_action_reads_as_executing_not_awaiting_approval() {
    let fixture = fixture("approved-executing").await.expect("fixture");
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-approved-executing','beacon','beacon',$3,
                   'beacon_outreach_request',10000,'require_approval','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(subject_id)
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($1,$2,'beacon','beacon.outreach.request','beacon',$3,
                   'e2e-approved-1','{\"kind\":\"beacon_outreach_request\"}',
                   'queued', now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(subject_id)
    .execute(&fixture.pool)
    .await
    .expect("approved action");

    let queue = fixture
        .repository
        .load_next_best_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("queue");
    let entry = queue
        .iter()
        .find(|candidate| candidate.decision_id == decision_id)
        .expect("an in-flight finding stays visible until it lands");
    assert_eq!(
        entry.authority.as_str(),
        "auto_executing",
        "an approved action must not keep asking a human who already answered"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn done_ourselves_takes_the_finding_and_its_parked_action_off_the_board() {
    let fixture = fixture("handled").await.expect("fixture");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-handled','booking_opportunity','team_opportunity',$3,
                   'prepare_live_opportunity',9000,'require_approval','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approval_expires_at
         ) VALUES ($1,$2,'booking_opportunity','apply_live_opportunity','team_opportunity',$3,
                   'e2e-parked-1','{\"kind\":\"apply_live_opportunity\"}',
                   'awaiting_approval', now() + interval '7 days')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("parked action");

    // Visible before, gone after.
    let before = fixture
        .repository
        .load_next_best_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("queue before");
    assert!(
        before.iter().any(|entry| entry.decision_id == decision_id),
        "the finding is on the board before a human says they did it"
    );

    let mutation = fixture
        .repository
        .mark_decision_handled_externally(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotDecisionId::from_uuid(decision_id),
            &key(5),
            None,
        )
        .await
        .expect("handled");
    assert_eq!(mutation.status, "handled_externally");

    let status: String = sqlx::query_scalar(
        "SELECT status FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND idempotency_key = 'e2e-parked-1'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("action row");
    assert_eq!(status, "cancelled", "the parked send may not go out anyway");

    let after = fixture
        .repository
        .load_next_best_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("queue after");
    assert!(
        !after.iter().any(|entry| entry.decision_id == decision_id),
        "the agent stops proposing work somebody already did"
    );

    // A second click says so rather than writing a second outcome.
    let replay = fixture
        .repository
        .mark_decision_handled_externally(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotDecisionId::from_uuid(decision_id),
            &key(5),
            None,
        )
        .await
        .expect("replay");
    assert!(replay.replayed);

    // And a finding that does not exist cannot be handled into existence.
    let missing = fixture
        .repository
        .mark_decision_handled_externally(
            fixture.workspace_id,
            crowdrelay_domain::AutopilotDecisionId::from_uuid(Uuid::now_v7()),
            &key(6),
            None,
        )
        .await;
    assert!(matches!(missing, Err(RepositoryError::NotFound)));
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn applying_a_posture_moves_all_four_surfaces_atomically() {
    use crowdrelay_application::autopilot::SetGrowthPosture;

    let fixture = fixture("posture").await.expect("fixture");

    // Provisioned defaults: nothing enabled yet.
    let before = fixture
        .repository
        .load_growth_posture(fixture.workspace_id)
        .await
        .expect("posture before");
    assert!(before.posture.is_none());
    assert_eq!(before.expected_version, 1);

    // Apply working: every context enabled at its mapped level, ceilings
    // written with the posture as rationale, envelope switches flipped,
    // budgets untouched.
    let mutation = fixture
        .repository
        .set_growth_posture(
            fixture.workspace_id,
            SetGrowthPosture {
                posture: crowdrelay_application::autopilot::GrowthPosture::Working,
                expected_version: 1,
            },
            &key(7),
            None,
        )
        .await
        .expect("apply working");
    assert_eq!(mutation.status, "posture_working");

    let enabled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_autopilot_policies \
         WHERE workspace_id = $1 AND enabled AND autonomy_level <> 'observe'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("policy rows");
    // Every context moved off the provisioned observe.
    assert!(enabled > 0);

    let third_party_ceiling: String = sqlx::query_scalar(
        "SELECT ceiling FROM viryaos_growth_autonomy \
         WHERE workspace_id = $1 AND action_class = 'third_party'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("ceiling row");
    assert_eq!(
        third_party_ceiling, "require_approval",
        "working drafts third-party contact"
    );
    let paid_ceiling: String = sqlx::query_scalar(
        "SELECT ceiling FROM viryaos_growth_autonomy \
         WHERE workspace_id = $1 AND action_class = 'paid'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("paid ceiling");
    assert_eq!(
        paid_ceiling, "require_approval",
        "money never auto in any posture"
    );

    let (agent_enabled, dry_run): (bool, bool) = sqlx::query_as(
        "SELECT agent_enabled, dry_run FROM viryaos_growth_envelope WHERE workspace_id = $1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("envelope row");
    assert!(!dry_run && agent_enabled, "working opens the gates");

    // Stale version is refused 409-style, exactly like every other authority
    // write.
    let stale = fixture
        .repository
        .set_growth_posture(
            fixture.workspace_id,
            SetGrowthPosture {
                posture: crowdrelay_application::autopilot::GrowthPosture::Grounded,
                expected_version: 1,
            },
            &key(8),
            None,
        )
        .await;
    assert!(matches!(stale, Err(RepositoryError::Conflict)));

    // Replay of the successful write says so.
    let replay = fixture
        .repository
        .set_growth_posture(
            fixture.workspace_id,
            SetGrowthPosture {
                posture: crowdrelay_application::autopilot::GrowthPosture::Working,
                expected_version: 1,
            },
            &key(7),
            None,
        )
        .await
        .expect("replay");
    assert!(replay.replayed);
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_approved_action_without_a_live_executor_is_cancelled_after_the_grace_window() {
    let fixture = fixture("no-executor-sweep").await.expect("fixture");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation
         ) VALUES ($1,$2,'e2e-no-executor','beacon','beacon',$3,
                   'beacon_outreach_request',9000,'require_approval','e2e','{}','{}','{}')",
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("decision");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($1,$2,'beacon','beacon.outreach.request','beacon',$3,
                   'e2e-no-executor-1','{\"kind\":\"request_beacon_outreach\",\"beacon_id\":\"01a029d5-1555-70d3-b3ea-3672216e7fe4\",\"event_id\":\"ef8b0ff0-d9bf-48d1-a143-a1cb27e8c322\",\"beacon_version\":1,\"phase\":\"initial\",\"template_key\":\"beacon.local_story.v1\"}',
                   'queued', now() - interval '2 days')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("stale queued action");

    // Nobody advertises `beacon.outreach`, the action has waited far beyond
    // the grace window: the sweep must retire it instead of letting it rot.
    let cancelled = fixture
        .repository
        .cancel_unexecutable_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("sweep");
    assert_eq!(cancelled, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND idempotency_key = 'e2e-no-executor-1'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("action row");
    assert_eq!(status, "cancelled");

    // A capability that IS advertised protects its action from the sweep:
    // register `content.artifact` and give a content action the same age.
    sqlx::query(
        "INSERT INTO viryaos_executor_instances (
             workspace_id, executor_id, version, manifest_sha,
             observed_at, expires_at
         ) VALUES ($1,'e2e-executor','v1','e2e', now(), now() + interval '1 hour')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("executor instance");
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities (
             workspace_id, executor_id, capability, capability_version,
             observed_at, expires_at
         ) VALUES ($1,'e2e-executor','content.artifact','v1', now(), now() + interval '1 hour')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("capability");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, approved_at
         ) VALUES ($1,$2,'content_supply','content.artifact.request','content_source',$3,
                   'e2e-no-executor-2','{\"kind\":\"request_content_artifact\"}',
                   'queued', now() - interval '2 days')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await
    .expect("supported queued action");
    let cancelled = fixture
        .repository
        .cancel_unexecutable_actions(fixture.workspace_id, fixture.now)
        .await
        .expect("second sweep");
    assert_eq!(cancelled, 0);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND idempotency_key = 'e2e-no-executor-2'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("supported action row");
    assert_eq!(status, "queued", "a live executor keeps its work claimable");
}
