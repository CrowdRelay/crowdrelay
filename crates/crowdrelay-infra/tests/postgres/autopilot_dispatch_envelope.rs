//! R0c — outcome-created actions must carry the same learning envelope as
//! evaluator-created ones.
//!
//! The worker outcome path (`agent_outcomes.rs`) inserts a bare
//! `autopilot_actions` row: no decision persist ran for it, so no
//! `dispatch_predictions` or `growth_evidence` row exists.
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

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotRuntimeRepository, ClaimExecution, ExecutorReportStatus,
    RecordExecutionReport,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
        r#"INSERT INTO autopilot_decisions
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
        r#"INSERT INTO autopilot_actions
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
         FROM dispatch_predictions WHERE action_id = $1",
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
         FROM growth_evidence WHERE workspace_id = $1 AND action_id = $2",
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
        "SELECT measurement_kind FROM autopilot_measurements \
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

    let status =
        sqlx::query_scalar::<_, String>("SELECT status FROM autopilot_actions WHERE id = $1")
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
           (SELECT COUNT(*) FROM dispatch_predictions WHERE action_id = $1)::bigint, \
           (SELECT COUNT(*) FROM growth_evidence \
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
               (SELECT COUNT(*) FROM dispatch_predictions WHERE action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM growth_evidence \
                WHERE workspace_id = $2 AND action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM autopilot_measurements \
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

/// A brain-requested artifact is the fourth loop closure: the brain asks for
/// a piece of content, the executor produces it, and the provider-confirmed
/// receipt both writes the learning envelope and schedules the fan-growth
/// trio. Before this path a published video or post left nothing for the
/// model to learn from — the artifact request sat in the do-nothing arm of
/// `schedule_effect_measurement`.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_content_artifact_receipt_writes_the_envelope_and_growth_measurements()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let suffix = f.workspace_id.into_uuid().simple().to_string();

    // The artifact is made from a live content source — the dispatch path
    // locks it before emitting the executor task.
    let source_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources
           (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at)
         VALUES ($1,$2,'event',$3,'Envelope test event',$4,$5)",
    )
    .bind(source_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("event-{suffix}"))
    .bind(now - time::Duration::days(1))
    .bind(now + time::Duration::days(60))
    .execute(&f.pool)
    .await?;

    let action_id = seed_outcome_action(
        &f,
        "content.artifact.request",
        json!({
            "kind": "request_content_artifact",
            "source_id": source_id,
            "source_version": 1,
            "artifact": "social_feed",
            "template_key": "content.social_feed.v1",
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
        .expect("the queued artifact action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    // Executor-required: dispatch is an intent, not a produced artifact. No
    // envelope and no measurements exist until the executor reports back.
    let (prediction_rows, evidence_rows, measurement_rows) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
               (SELECT COUNT(*) FROM dispatch_predictions WHERE action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM growth_evidence \
                WHERE workspace_id = $2 AND action_id = $1)::bigint, \
               (SELECT COUNT(*) FROM autopilot_measurements \
                WHERE workspace_id = $2 AND action_id = $1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (prediction_rows, evidence_rows, measurement_rows),
        (0, 0, 0),
        "a dispatched artifact request is not yet a produced artifact"
    );

    // The executor claim and the provider-confirmed receipt — the moment the
    // artifact actually exists in front of an audience.
    let produced_at = now + time::Duration::hours(2);
    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: now + time::Duration::minutes(5),
            },
        )
        .await
        .expect("claim");
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("artifact-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: claim.claim_token,
                provider_reference: Some("yt:envelope-test".to_owned()),
                error_kind: None,
                metadata: json!({}),
                occurred_at: produced_at,
            },
        )
        .await?;

    // The cold-prior envelope the measurement trio will update on resolution.
    let prediction = sqlx::query_as::<_, (String, f64)>(
        "SELECT template_id, expected_new_fans FROM dispatch_predictions \
         WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(prediction.0, "content-artifact:content.social_feed.v1");
    assert!(
        (prediction.1 - crowdrelay_brain::DEFAULT_EXPECTED_FANS).abs() < f64::EPSILON,
        "an artifact request predicts the cold prior, got {}",
        prediction.1
    );

    let evidence = sqlx::query_as::<_, (String, Option<f64>, Option<OffsetDateTime>)>(
        "SELECT channel, observed_fans, resolved_at FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(evidence.0, "other");
    assert!(
        evidence.1.is_none() && evidence.2.is_none(),
        "the receipt envelope is an expectation, not an outcome: {evidence:?}"
    );

    // The fan-growth trio, anchored at the confirmed production time and
    // pointed at the content source the artifact was made from.
    let measurements = sqlx::query_as::<_, (String, Uuid, OffsetDateTime, OffsetDateTime)>(
        "SELECT measurement_kind, subject_id, action_finished_at, due_at \
         FROM autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    let expected_windows: [(&str, i64); 4] = [
        ("artifact_outcome_7d", 7),
        ("durable_fan_growth_30d", 44),
        ("incremental_fan_growth_14d", 14),
        ("incremental_fan_growth_3d", 3),
    ];
    assert_eq!(
        measurements
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>(),
        expected_windows
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>()
    );
    for (kind, subject, finished, due) in &measurements {
        assert_eq!(
            *subject, source_id,
            "{kind} must measure the artifact's source"
        );
        assert!(
            *finished > now && *finished <= produced_at,
            "{kind} anchors at the receipt, not the request"
        );
        let window = expected_windows
            .iter()
            .find(|(expected, _)| expected == kind)
            .map(|(_, days)| *days)
            .expect("expected measurement kind");
        assert_eq!(
            *due - *finished,
            time::Duration::days(window),
            "{kind} due_at"
        );
    }

    let status =
        sqlx::query_scalar::<_, String>("SELECT status FROM autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(status, "succeeded");
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
        "UPDATE growth_envelope SET max_recipients_per_step = 2
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
        "UPDATE growth_envelope SET max_recipients_per_step = 2
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

/// O.3: the campaign row carries the approved words, and a draftless action
/// is refused rather than sending copy nobody read.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_audience_campaign_stores_the_approved_copy_and_a_draftless_one_refuses()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let suffix = f.workspace_id.into_uuid().simple().to_string();

    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled)
         VALUES ($1, 'communication_campaigns_enabled', true)",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;

    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, venue, timezone, starts_at, status, published_at, ticket_url)
         VALUES ($1, $2, $3, 'Virya live', 'Klub Testowy', 'Europe/Warsaw', $4, 'published', $5, 'https://tickets.test/virya')",
    )
    .bind(event_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("aud-copy-{suffix}"))
    .bind(now + time::Duration::days(30))
    .bind(now - time::Duration::days(2))
    .execute(&f.pool)
    .await?;

    let action_id = seed_outcome_action(
        &f,
        "audience.campaign.request",
        json!({
            "kind": "request_audience_campaign",
            "event_id": event_id,
            "phase": "announcement",
            "template_key": "event.announcement.v1",
            "audience_size": null,
            "audience_basis": "every consented fan in the event's city",
            "draft": {
                "subject": "Virya live — 18 października, Klub Testowy",
                "body": "Cześć,\n\nVirya live — 18 października, Klub Testowy.\n\nBilety: https://tickets.test/virya\n\nDo zobaczenia.\n\n- VIRYA",
            },
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
        .expect("the queued campaign action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let (subject, body) = sqlx::query_as::<_, (String, String)>(
        "SELECT content->>'subject', content->>'body'
         FROM communication_campaigns WHERE workspace_id = $1",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(subject, "Virya live — 18 października, Klub Testowy");
    assert!(body.contains("Bilety: https://tickets.test/virya"));

    let due = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'communication.campaign_due'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(due, 1, "the campaign dispatch is due exactly once");

    // A payload with no copy — a row queued before drafts existed — refuses
    // rather than letting the mailer invent the words.
    let bare_action = seed_outcome_action(
        &f,
        "audience.campaign.request",
        json!({
            "kind": "request_audience_campaign",
            "event_id": event_id,
            "phase": "last_call",
            "template_key": "event.last_call.v1",
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
        .find(|a| a.id.into_uuid() == bare_action)
        .expect("the draftless campaign action must be claimable");
    let refused = f
        .repository
        .execute_action(f.workspace_id, action, now)
        .await;
    assert!(
        refused.is_err(),
        "a campaign without the approved copy refuses instead of shipping template-only words"
    );
    Ok(())
}

/// A release milestone is a `persist_candidate` action — the decision path
/// wrote its decision row but never an envelope, so every measurement its
/// schedule owns resolved into rows that did not exist and the release's
/// outcomes taught the model nothing. The envelope now lands at dispatch like
/// every other measured kind, and the milestone's own counters are due on the
/// release, not on a promoter or an event.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_release_milestone_leaves_dispatch_with_envelope_and_measurements()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::AutopilotActionPayload;
    use crowdrelay_domain::release_autopilot::ReleaseMilestone;

    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    // Micros, not nanos: the plan row stores timestamptz and the executor's
    // staleness check compares the payload's timestamp to it — a nanosecond
    // tail would read as a newer plan.
    let release_at =
        OffsetDateTime::from_unix_timestamp((now + time::Duration::days(10)).unix_timestamp())?;
    let release_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO release_plans
           (id, workspace_id, source_key, title, release_at, tier, listen_url)
           VALUES ($1,$2,$3,'Signal Lost',$4,'single','https://example.test/listen')"#,
    )
    .bind(release_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("release-{release_id}"))
    .bind(release_at)
    .execute(&f.pool)
    .await?;
    // The wave sends through the campaign machinery, which is flag-gated —
    // a workspace without the flag refuses the milestone before any of the
    // rows this test counts exist.
    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled) \
         VALUES ($1, 'communication_campaigns_enabled', true)",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;

    let payload = serde_json::to_value(AutopilotActionPayload::ExecuteReleaseMilestone {
        release_id: crowdrelay_domain::ReleasePlanId::from_uuid(release_id),
        title: "Signal Lost".to_owned(),
        release_at,
        milestone: ReleaseMilestone::ReleaseDay,
    })?;
    let action_id = seed_outcome_action(&f, "release.milestone.execute", payload, now).await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued milestone action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let prediction = sqlx::query_scalar::<_, String>(
        "SELECT template_id FROM dispatch_predictions WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        prediction, "release-milestone:release_day",
        "the envelope names the rung, so the posterior learns per milestone"
    );

    let evidence_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        evidence_rows, 1,
        "one evidence row per dispatched milestone"
    );

    let kinds = sqlx::query_scalar::<_, String>(
        "SELECT measurement_kind FROM autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    for expected in [
        "release_bound_acquisition_14d",
        "release_fan_conversion_14d",
        "release_link_clicks_14d",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "measurement {expected} missing; scheduled: {kinds:?}"
        );
    }
    assert!(
        !kinds.iter().any(|kind| kind == "release_channel_lift_14d"),
        "no lift measurement without a declared series: {kinds:?}"
    );

    // And the milestone itself still executed — the tracked link it needed
    // exists because the executor wrote it.
    let campaign = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM campaigns \
         WHERE workspace_id = $1 AND release_plan_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(release_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(campaign, 1, "the milestone executor still owns its link");
    Ok(())
}

/// Install checkpoints are planned against new installs in a pre-window of
/// their own width, never against the standing install total. With the old
/// total as baseline every dispatch that added none scored −100%, which is
/// how 43 of one week's 87 worsened outcomes were produced.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn install_checkpoints_are_planned_against_a_matched_pre_window()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let suffix = f.workspace_id.into_uuid().simple().to_string();

    // Three installs a month old, one three days old: the 7-day pre-window
    // holds one, the 1-day pre-window holds none, the total is four.
    for (index, age_days) in [40_i64, 35, 30, 3].into_iter().enumerate() {
        let fan_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
             VALUES ($1, $2, $3, 'Fan', 'active')",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("window-{suffix}-{index}@example.test"))
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
               (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active,
                created_at)
             VALUES ($1, $2, $3, $4, 'android_fcm', $5, true, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("window-install-{suffix}-{index}"))
        .bind(format!("window-token-{suffix}-{index}"))
        .bind(now - time::Duration::days(age_days))
        .execute(&f.pool)
        .await?;
    }

    let action_id = seed_outcome_action(
        &f,
        "signal.push.request",
        json!({
            "kind": "request_signal_push",
            "task_id": Uuid::now_v7(),
            "title": "window push",
            "body": "window push body",
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

    let baselines = sqlx::query_as::<_, (String, f64)>(
        "SELECT measurement_kind, baseline_value FROM autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 \
           AND measurement_kind IN ('agent_run_signal_installs_7d', 'signal_installs_1d') \
         ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(
        baselines,
        vec![
            ("agent_run_signal_installs_7d".to_owned(), 1.0),
            ("signal_installs_1d".to_owned(), 0.0),
        ],
        "each install checkpoint counts its own pre-window, not the total of four"
    );
    Ok(())
}
