//! R0c — outcome-created actions must carry the same learning envelope as
//! evaluator-created ones.
//!
//! The worker outcome path (`agent_outcomes.rs`) inserts a bare
//! `viryaos_autopilot_actions` row: no decision persist ran for it, so no
//! `viryaos_dispatch_predictions` or `viryaos_growth_evidence` row exists.
//! Production showed the result — every `community.engage.request` action had
//! its measurements scheduled but zero evidence and zero prediction rows, so
//! each measurement resolved into nothing and the causal model learned
//! nothing from the work.
//!
//! `ensure_dispatch_envelope` now writes the cold-prior envelope at the
//! dispatch seam, conflict-safely. What is asserted here:
//!
//! - A `community.engage.request` action leaves execution with a prediction
//!   row, an unresolved evidence row, and its measurement rows.
//! - The prediction carries the honest cold prior, not an invented number.
//! - The evidence names the community channel and keeps outcome fields NULL —
//!   the forum is measured as a fan *source*, and nothing pretends a post
//!   already converted anyone.
//! - An executor-required action gets no envelope at dispatch: its evidence
//!   is committed when the provider-confirmed receipt arrives, so a queued
//!   webhook can never masquerade as completed work.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
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
        .bind(format!("envelope-{suffix}"))
        .bind("Dispatch Envelope Tests")
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
    })
}

/// The bare action insert the worker outcome path performs: a decision row
/// for lineage, then a queued action with no prediction or evidence.
async fn seed_outcome_action(
    f: &Fixture,
    action_kind: &str,
    payload: Value,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'growth_intelligence','agent_outcome',$4,$5,9000,
                   'auto_execute','outcome-created action','{}','{}','{}',$6,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(action_kind)
    .bind(now)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'growth_intelligence',$4,'agent_outcome',$5,$6,$7,
                   'queued',$9,$8,'policy:bounded_auto',$8)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(action_kind)
    .bind(Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(&payload)
    .bind(now)
    .bind(
        // The row's class is the payload's own classification — never a
        // fixture literal, so the evidence gate reads the truth.
        serde_json::from_value::<crowdrelay_application::autopilot::AutopilotActionPayload>(
            payload.clone(),
        )
        .map(|parsed| parsed.action_class().as_str())
        .unwrap_or("first_party_reversible"),
    )
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_community_engagement_action_leaves_execution_with_a_learning_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let action_id = seed_outcome_action(
        &f,
        "community.engage.request",
        json!({
            "kind": "request_community_engagement",
            "target_id": Uuid::now_v7(),
            "platform": "reddit",
            "subreddit": "r/envelope_test",
            "title": "test title",
            "body": "test body",
            "smart_link": null,
        }),
        now,
    )
    .await?;

    // The production path: the autonomous claim picks the queued action up
    // and `execute_action` runs it.
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued community action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    // The prediction the model would have returned for an unseen template —
    // the cold prior, not an invented success.
    let prediction = sqlx::query_as::<_, (String, f64, f64)>(
        "SELECT template_id, expected_new_fans, expected_signal_installs \
         FROM viryaos_dispatch_predictions WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(prediction.0, "community-engager");
    assert!(
        (prediction.1 - crowdrelay_brain::DEFAULT_EXPECTED_FANS).abs() < f64::EPSILON,
        "an outcome-created action predicts the cold prior, got {}",
        prediction.1
    );
    assert!(
        (prediction.2 - crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL).abs() < f64::EPSILON,
        "the signal prior is the cold value, got {}",
        prediction.2
    );

    // The evidence row exists, unresolved, on the community channel — with
    // every outcome field NULL. Nothing pretends the post converted anyone.
    let evidence = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<f64>,
            Option<f64>,
            Option<OffsetDateTime>,
        ),
    >(
        "SELECT channel, treatment, observed_fans, observed_incremental_fans, resolved_at \
         FROM viryaos_growth_evidence WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(evidence.0, "reddit_post");
    assert_eq!(evidence.1, "treatment");
    assert!(
        evidence.2.is_none() && evidence.3.is_none() && evidence.4.is_none(),
        "the dispatch envelope is an expectation, not an outcome: {evidence:?}"
    );

    // The measurements the engagement schedule owns: reception plus the
    // incremental fan-growth counterfactuals.
    let kinds = sqlx::query_scalar::<_, String>(
        "SELECT measurement_kind FROM viryaos_autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    for expected in [
        "agent_run_community_engagement_7d",
        "incremental_fan_growth_14d",
        "incremental_fan_growth_3d",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "measurement {expected} missing; scheduled: {kinds:?}"
        );
    }

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(status, "succeeded");

    // Exactly once: a second claim finds nothing to redo, and the envelope
    // rows are single.
    let reclaims = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now + time::Duration::minutes(1))
        .await?;
    assert!(
        !reclaims.iter().any(|a| a.id.into_uuid() == action_id),
        "a succeeded action must not be re-claimed"
    );
    let (prediction_rows, evidence_rows) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT \
           (SELECT COUNT(*) FROM viryaos_dispatch_predictions WHERE action_id = $1)::bigint, \
           (SELECT COUNT(*) FROM viryaos_growth_evidence \
            WHERE workspace_id = $2 AND action_id = $1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (prediction_rows, evidence_rows),
        (1, 1),
        "the envelope is written exactly once under replay"
    );
    Ok(())
}

/// An action whose work only counts when the executor reports back must not
/// leave dispatch with evidence — the envelope belongs to the receipt.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_executor_required_action_gets_no_premature_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let action_id = seed_outcome_action(
        &f,
        "outreach.discovery.request",
        json!({
            "kind": "request_outreach_discovery",
            "requested_candidates": 5
        }),
        now,
    )
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued discovery action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let (prediction_rows, evidence_rows, measurement_rows) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
               (SELECT COUNT(*) FROM viryaos_dispatch_predictions WHERE action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM viryaos_growth_evidence \
                WHERE workspace_id = $2 AND action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM viryaos_autopilot_measurements \
                WHERE workspace_id = $2 AND action_id = $1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (prediction_rows, evidence_rows, measurement_rows),
        (0, 0, 0),
        "a dispatched intent is not evidence: the executor receipt writes those rows"
    );
    Ok(())
}

/// The growth envelope's `max_recipients_per_step` is the operator's
/// blast-radius dial. Before it was wired into the send path it was a
/// documented promise the SQL ignored — a broadcast push to a workspace of
/// thousands would have delivered to every consented endpoint while the
/// operator panel showed a bound of 250.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_signal_push_respects_the_envelope_recipient_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let suffix = f.workspace_id.into_uuid().simple().to_string();

    // Five eligible fans; the operator's envelope permits two per step.
    sqlx::query(
        "UPDATE viryaos_growth_envelope SET max_recipients_per_step = 2
         WHERE workspace_id = $1",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;
    for index in 0..5 {
        let fan_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
             VALUES ($1, $2, $3, 'Fan', 'active')",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("bounded-{suffix}-{index}@example.test"))
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
             VALUES ($1,$2,'marketing',true,'v1','test')",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_push_endpoints
               (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active)
             VALUES ($1, $2, $3, $4, 'android_fcm', $5, true)",
        )
        .bind(Uuid::now_v7())
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("install-{suffix}-{index}"))
        .bind(format!("token-{suffix}-{index}"))
        .execute(&f.pool)
        .await?;
    }

    let action_id = seed_outcome_action(
        &f,
        "signal.push.request",
        json!({
            "kind": "request_signal_push",
            "task_id": Uuid::now_v7(),
            "title": "bounded push",
            "body": "bounded push body",
            "target_path": null,
            "event_id": null,
            "segment": null,
        }),
        now,
    )
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued push action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let (deliveries, recipients) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT COUNT(*)::bigint, COUNT(DISTINCT fan_id)::bigint \
         FROM fan_push_deliveries WHERE workspace_id = $1 AND source_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (deliveries, recipients),
        (2, 2),
        "the envelope bound clamps the fan set itself, not just the report"
    );
    Ok(())
}

/// The approval screen quotes `signal_push_audience`: the count must match
/// what `execute_signal_push` would deliver — same eligibility, same segment
/// predicates, same envelope bound. Pinned here so a briefing can never drift
/// from the send it describes.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn signal_push_audience_counts_what_the_send_would_reach()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let suffix = f.workspace_id.into_uuid().simple().to_string();

    // The operator's envelope permits two recipients per step.
    sqlx::query(
        "UPDATE viryaos_growth_envelope SET max_recipients_per_step = 2
         WHERE workspace_id = $1",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;

    // Three fans the send would reach: active, marketing consent granted,
    // one live endpoint each.
    for index in 0..3 {
        let fan_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
             VALUES ($1, $2, $3, 'Fan', 'active')",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("aud-{suffix}-{index}@example.test"))
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
             VALUES ($1,$2,'marketing',true,'v1','test')",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_push_endpoints
               (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active)
             VALUES ($1, $2, $3, $4, 'android_fcm', $5, true)",
        )
        .bind(Uuid::now_v7())
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("install-aud-{suffix}-{index}"))
        .bind(format!("token-aud-{suffix}-{index}"))
        .execute(&f.pool)
        .await?;
    }

    // One fan who consented but whose only endpoint is gone — unreachable.
    let unreachable = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
         VALUES ($1, $2, $3, 'Fan', 'active')",
    )
    .bind(unreachable)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("aud-{suffix}-gone@example.test"))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1,$2,'marketing',true,'v1','test')",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(unreachable)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_push_endpoints
           (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active)
         VALUES ($1, $2, $3, $4, 'android_fcm', $5, false)",
    )
    .bind(Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(unreachable)
    .bind(format!("install-aud-{suffix}-gone"))
    .bind(format!("token-aud-{suffix}-gone"))
    .execute(&f.pool)
    .await?;

    // One fan who never consented — the base guard excludes them regardless
    // of endpoints.
    let unconsented = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
         VALUES ($1, $2, $3, 'Fan', 'active')",
    )
    .bind(unconsented)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("aud-{suffix}-no@example.test"))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_push_endpoints
           (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active)
         VALUES ($1, $2, $3, $4, 'android_fcm', $5, true)",
    )
    .bind(Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(unconsented)
    .bind(format!("install-aud-{suffix}-no"))
    .bind(format!("token-aud-{suffix}-no"))
    .execute(&f.pool)
    .await?;

    let audience =
        crowdrelay_infra::autopilot::signal_push_audience(&f.pool, f.workspace_id, None).await?;
    assert_eq!(
        (audience.eligible, audience.reached),
        (3, 2),
        "eligible counts who could receive; reached is what the send actually delivers"
    );

    // A segment narrows the count with the same predicates the send applies.
    let segment_slug = format!("aud-seg-{suffix}");
    sqlx::query(
        "INSERT INTO audience_segments (workspace_id, slug, name, filter, active)
         VALUES ($1, $2, 'five referrals', '{\"min_qualified_referrals\": 5}', true)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(&segment_slug)
    .execute(&f.pool)
    .await?;
    let segmented = crowdrelay_infra::autopilot::signal_push_audience(
        &f.pool,
        f.workspace_id,
        Some(&segment_slug),
    )
    .await?;
    assert_eq!(
        (segmented.eligible, segmented.reached),
        (0, 0),
        "the segment's predicates count the same set the send would filter to"
    );

    // A segment that does not resolve is a refusal, not a broadcast — the
    // count must fail the same way the send does.
    let missing = crowdrelay_infra::autopilot::signal_push_audience(
        &f.pool,
        f.workspace_id,
        Some("no-such-segment"),
    )
    .await;
    assert!(
        missing.is_err(),
        "an unresolvable segment refuses rather than reporting the broadcast count"
    );
    Ok(())
}
