//! Business invariants for the measurement spine.
//!
//! The chain these tests cover is the one the brain learns from:
//!
//! ```text
//! dispatch -> evidence row -> measurement queue -> outcome -> readiness
//!          -> delta replay -> causal model
//! ```
//!
//! Every defect this suite pins down was green under `cargo test`, green under
//! clippy, and invisible on a dashboard, because each one produced a plausible
//! number rather than an error. They are asserted as business facts — what the
//! row means — rather than as the shape of the SQL that produces it.
//!
//! A: partial outcomes never look complete. Four measurements, one evidence
//!    row, and the row is model-ready only when the last of them has landed.
//! B: delta replay finds evidence that became ready after the checkpoint, and
//!    finds it exactly once.
//! C: an action that did worse than nothing is a result, not an error.
//! D: a community outcome is read from the community's own ledger, and a
//!    community with no published post reports nothing rather than zero.
//! E: the counterfactual is scoped to the same community, window and unit as
//!    the outcome it will be subtracted from.
//! F: a manually published post is measured from publication, not from the
//!    moment its draft was written.
//! G: re-running a resolved measurement changes nothing, and leaves every
//!    other measurement alone.

use crowdrelay_application::autopilot::{
    AutopilotDecisionRepository, AutopilotMeasurementKind, AutopilotMeasurementRepository,
    ClaimedAutopilotMeasurement, assess_measurement_effect,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
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
        .bind(format!("meas-spine-{suffix}"))
        .bind("Measurement Spine Tests")
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

/// A decision, an action and the growth evidence row a dispatch writes.
///
/// `dispatched_at` is deliberately a parameter: several of these invariants
/// only bite when the dispatch is old and its outcome is new, which is the
/// exact shape the delta cursor used to get wrong.
async fn insert_dispatch(
    f: &Fixture,
    opportunity_id: &str,
    dispatched_at: OffsetDateTime,
) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("key-{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("decision");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                   $4,$5,'{}'::jsonb,'succeeded','third_party',$6)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("action");
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality)
           VALUES ($1,$2,$3,$4,'recipient','reddit_post',100,'treatment',0.9,false,
                   2.0,1.0,'{}'::jsonb,'observational')"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(opportunity_id)
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("evidence");
    action_id
}

/// Queues one measurement and hands back the claim shape the worker would see.
async fn queue_measurement(
    f: &Fixture,
    action_id: uuid::Uuid,
    kind: AutopilotMeasurementKind,
    baseline_value: f64,
    action_finished_at: OffsetDateTime,
) -> ClaimedAutopilotMeasurement {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8)"#,
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(kind.as_str())
    .bind(action_id)
    .bind(action_finished_at)
    .bind(baseline_value)
    .bind(action_finished_at + time::Duration::days(7))
    .execute(&f.pool)
    .await
    .expect("measurement");
    ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(id),
        action_id: AutopilotActionId::from(action_id),
        kind,
        subject_id: action_id,
        baseline_value,
        action_finished_at,
        attempt_number: 1,
    }
}

/// Puts a queued measurement into the state `complete_measurement` requires.
async fn mark_processing(f: &Fixture, measurement: &ClaimedAutopilotMeasurement) {
    sqlx::query(
        "UPDATE viryaos_autopilot_measurements SET status='processing', started_at=now() \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(measurement.id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("processing");
}

/// Observes and completes one measurement the way the worker loop does, so the
/// tests exercise the real classification step rather than a hand-made effect.
async fn resolve(f: &Fixture, measurement: &ClaimedAutopilotMeasurement, observed: f64) {
    mark_processing(f, measurement).await;
    let effect = assess_measurement_effect(measurement, observed)
        .expect("a measurement the worker can classify");
    f.repository
        .complete_measurement(f.workspace_id, measurement, observed, effect, f.now)
        .await
        .expect("complete");
}

/// `resolve` with the clock chosen by the caller.
///
/// The control arm resolves only once its own measurement window has elapsed,
/// so reproducing the production ordering — treated units finishing days apart,
/// the control becoming measurable only at the end — needs the completion time
/// to be a parameter rather than the wall clock.
async fn resolve_at(
    f: &Fixture,
    measurement: &ClaimedAutopilotMeasurement,
    observed: f64,
    at: OffsetDateTime,
) {
    mark_processing(f, measurement).await;
    let effect = assess_measurement_effect(measurement, observed)
        .expect("a measurement the worker can classify");
    f.repository
        .complete_measurement(f.workspace_id, measurement, observed, effect, at)
        .await
        .expect("complete");
}

async fn evidence_state(
    f: &Fixture,
    action_id: uuid::Uuid,
) -> (
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<OffsetDateTime>,
) {
    sqlx::query_as::<
        _,
        (
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<OffsetDateTime>,
        ),
    >(
        "SELECT observed_fans, observed_incremental_fans, durable_fans_30d, resolved_at \
         FROM viryaos_growth_evidence WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence state")
}

/// A: one evidence row, four measurements, and readiness only at the end.
///
/// The seven-day signal measurement resolves first every time. It used to
/// stamp `resolved_at`, which said "this row is done" while the two outcomes
/// the learner actually reads were still a week and a month away — and since
/// the delta cursor moves with `resolved_at`, the row entered its one and only
/// replay at the moment it had nothing to teach.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_partial_outcomes_never_look_complete() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "community-engager:a", f.now).await;
    let signal = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::AgentRunSignalInstalls7d,
        0.0,
        f.now,
    )
    .await;
    let raw = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::AgentRunFanGrowth14d,
        0.0,
        f.now,
    )
    .await;
    let incremental = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.5,
        f.now,
    )
    .await;
    let durable = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::DurableFanGrowth30d,
        0.1,
        f.now,
    )
    .await;

    resolve(&f, &signal, 3.0).await;
    let (_, y14, y30, resolved_at) = evidence_state(&f, action_id).await;
    assert!(
        resolved_at.is_none(),
        "seven-day installs must not close a row still waiting on Y14 and Y30"
    );
    assert_eq!(y14, None, "Y14 has not been measured yet");
    assert_eq!(y30, None, "Y30 has not been measured yet");

    resolve(&f, &raw, 6.0).await;
    resolve(&f, &incremental, 4.0).await;
    let (raw_fans, y14, y30, resolved_at) = evidence_state(&f, action_id).await;
    assert_eq!(raw_fans, Some(6.0), "the raw count landed");
    assert_eq!(
        y14,
        Some(4.0),
        "Y14 landed and was not silenced by day seven"
    );
    assert_eq!(y30, None, "Y30 is still pending");
    assert!(
        resolved_at.is_none(),
        "a row waiting on its forty-four-day outcome is not model-ready"
    );

    resolve(&f, &durable, 2.0).await;
    let (_, y14, y30, resolved_at) = evidence_state(&f, action_id).await;
    assert_eq!(y14, Some(4.0), "Y14 survives the later write");
    assert_eq!(y30, Some(2.0), "Y30 landed");
    assert!(
        resolved_at.is_some(),
        "with the queue empty the row is finally model-ready"
    );
}

/// B: the delta cursor tracks readiness, not dispatch.
///
/// The dispatch is backdated and the checkpoint is taken after it, which is
/// the ordinary case in production — checkpoints are written every cycle and
/// outcomes arrive days later. Filtering on the dispatch timestamp meant this
/// row could never appear in any delta again.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn b_delta_replay_finds_evidence_that_became_ready_after_the_checkpoint() {
    let f = setup().await.expect("fixture");
    let dispatched_at = f.now - time::Duration::days(20);
    let action_id = insert_dispatch(&f, "community-engager:b", dispatched_at).await;
    let incremental = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.5,
        dispatched_at,
    )
    .await;
    // The checkpoint is taken after the dispatch and before the outcome.
    let checkpoint = f.now - time::Duration::days(3);
    resolve(&f, &incremental, 4.0).await;

    let delta = f
        .repository
        .load_growth_evidence(f.workspace_id, Some(checkpoint))
        .await
        .expect("delta replay");
    assert_eq!(
        delta.len(),
        1,
        "the outcome landed after the checkpoint, so its evidence belongs in the delta"
    );
    assert_eq!(
        delta.first().and_then(|ev| ev.observed_incremental_fans),
        Some(4.0),
        "and it carries the outcome the model needs"
    );

    // Advancing the checkpoint past the resolution consumes it exactly once.
    let after = f
        .repository
        .load_growth_evidence(f.workspace_id, Some(f.now + time::Duration::minutes(1)))
        .await
        .expect("second delta replay");
    assert!(
        after.is_empty(),
        "a row already consumed must not be applied a second time"
    );
}

/// C: a harmful outcome is a finding.
///
/// `assess_effect` refuses a negative level, which is right for ticket revenue
/// and wrong for a difference-in-differences estimate whose sign is the whole
/// answer. The worker turned that refusal into `RepositoryError::Unexpected`,
/// retried it three times and filed it as `unexpected` — so the one result the
/// brain most needs to see was the one result it could never receive.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn c_a_negative_signed_effect_resolves_as_a_result() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "community-engager:c", f.now).await;
    let incremental = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.5,
        f.now,
    )
    .await;

    let effect = assess_measurement_effect(&incremental, -3.0)
        .expect("a negative incremental outcome must classify, not vanish");
    assert_eq!(
        effect.assessment,
        crowdrelay_domain::performance::EffectAssessment::Worsened,
        "doing worse than nothing is a worsened effect"
    );

    mark_processing(&f, &incremental).await;
    f.repository
        .complete_measurement(f.workspace_id, &incremental, -3.0, effect, f.now)
        .await
        .expect("a negative outcome completes");
    let (_, y14, _, resolved_at) = evidence_state(&f, action_id).await;
    assert_eq!(y14, Some(-3.0), "the negative outcome is what gets learned");
    assert!(resolved_at.is_some(), "and the row is model-ready");

    // The level-based kinds keep their old protection.
    let raw = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::AgentRunFanGrowth14d,
        4.0,
        f.now,
    )
    .await;
    assert!(
        assess_measurement_effect(&raw, -1.0).is_none(),
        "a negative fan *count* is still a malformed reading"
    );
}

/// D and E: the community outcome and its counterfactual come from the same
/// place, and an unpublished post reports nothing rather than zero.
///
/// The unit id on a community assignment is an `agent_outreach_targets` UUID
/// while the ledger is keyed by handle, so the old query matched nothing — and
/// `COUNT` renders nothing as `0`, which is indistinguishable from a community
/// that genuinely converted no one. Subtracting a workspace-wide arrival rate
/// from that zero made every community post look actively harmful.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn d_community_outcome_is_read_from_the_community_ledger() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "community-engager:d", f.now).await;
    let target_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_outreach_targets
           (id, workspace_id, target_kind, display_name, subreddit, status)
           VALUES ($1,$2,'community','r/spinetest','spinetest','promoted')"#,
    )
    .bind(target_id)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("target");
    let experiment_uuid = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
           (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
            unit_kind, holdout_probability, interference_policy, experiment_status)
           VALUES ($1,$2,'community-engager',$3,'target_community',0.1,'none','active')"#,
    )
    .bind(experiment_uuid)
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid.to_string())
    .execute(&f.pool)
    .await
    .expect("design");
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind, arm,
            intended_template_id, propensity, context, prediction,
            contamination_estimate, is_interference_controllable, experiment_status,
            execution_status, action_id, experiment_kind)
           VALUES ($5,$1,$2,$3,'target_community','treatment','community-engager',0.9,
                   '{}'::jsonb,'{}'::jsonb,0.0,false,'active','executed',$4,
                   'randomized_holdout')"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(target_id.to_string())
    .bind(action_id)
    .bind(uuid::Uuid::now_v7().to_string())
    .execute(&f.pool)
    .await
    .expect("assignment");

    let measurement = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        f.now,
    )
    .await;

    // No published post yet: the outcome does not exist, and that is not the
    // same fact as an outcome of zero.
    let unpublished = f
        .repository
        .observe_measurement(f.workspace_id, &measurement, f.now)
        .await;
    assert!(
        unpublished.is_err(),
        "a community whose post is still a draft has no outcome to report"
    );

    // Publish, and give the community two real conversions.
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, target_id, subreddit, title, body, status, posted_at)
           VALUES ($1,$2,$3,'r/spinetest','t','b','posted',$4)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(target_id)
    .bind(f.now)
    .execute(&f.pool)
    .await
    .expect("post");
    for _ in 0..2 {
        let fan_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) \
             VALUES ($1,$2,$3,'active')",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("{fan_id}@example.test"))
        .execute(&f.pool)
        .await
        .expect("fan");
        sqlx::query(
            r#"INSERT INTO fan_provenance_events
               (workspace_id, fan_id, event_kind, channel, community,
                attribution_method, attribution_confidence, occurred_at)
               VALUES ($1,$2,'conversion','reddit','r/spinetest',
                       'last_community_click',1.0,$3)"#,
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(f.now + time::Duration::hours(1))
        .execute(&f.pool)
        .await
        .expect("conversion");
    }

    let observed = f
        .repository
        .observe_measurement(f.workspace_id, &measurement, f.now)
        .await
        .expect("a published community reports its own conversions");
    // Baseline is zero for a first post, so the effect is the outcome itself:
    // two real conversions from this community, not a workspace-wide count.
    assert!(
        (observed - 2.0).abs() < f64::EPSILON,
        "expected the two community conversions, got {observed}"
    );

    // E: the counterfactual is expressed in the same unit and window as the
    // outcome, so a community with no history subtracts nothing.
    assert!(
        (measurement.counterfactual_value() - 0.0).abs() < f64::EPSILON,
        "a community with no prior conversions has no arrivals to subtract"
    );
}

/// F: the window starts when the post reached the community.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn f_manual_publication_moves_the_measurement_window() {
    let f = setup().await.expect("fixture");
    let drafted_at = f.now - time::Duration::days(3);
    let action_id = insert_dispatch(&f, "community-engager:f", drafted_at).await;
    let measurement = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        drafted_at,
    )
    .await;
    let post_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO community_posts
           (id, workspace_id, action_id, target_id, subreddit, title, body, status)
           VALUES ($1,$2,$3,$4,'r/spinetest','t','b','awaiting_manual_post')"#,
    )
    .bind(post_id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("draft");

    crowdrelay_infra::fanbase::register_manual_reddit_post(
        &f.pool,
        f.workspace_id.into_uuid(),
        post_id,
        "https://www.reddit.com/r/spinetest/comments/abc123/title/",
    )
    .await
    .expect("registration");

    let (anchored_at, due_at) = sqlx::query_as::<_, (OffsetDateTime, OffsetDateTime)>(
        "SELECT action_finished_at, due_at FROM viryaos_autopilot_measurements \
             WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(measurement.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("anchored measurement");
    assert!(
        anchored_at > drafted_at,
        "the window must start at publication, not when the draft was written"
    );
    // The offset is preserved: seven days from publication, not seven days
    // minus however long the draft sat waiting for an operator.
    let offset = due_at - anchored_at;
    assert_eq!(
        offset,
        time::Duration::days(7),
        "each measurement keeps its own offset; only the origin moves"
    );
}

/// G: resolution is idempotent and local.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn g_re_resolving_changes_nothing_and_touches_nothing_else() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "community-engager:g", f.now).await;
    let other_action_id = insert_dispatch(&f, "community-engager:g2", f.now).await;
    let incremental = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.5,
        f.now,
    )
    .await;
    let untouched = queue_measurement(
        &f,
        other_action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.5,
        f.now,
    )
    .await;

    resolve(&f, &incremental, 4.0).await;
    let (_, y14, _, resolved_at) = evidence_state(&f, action_id).await;

    // A second attempt on an already-succeeded measurement must not be able to
    // rewrite the outcome: the row is no longer `processing`.
    mark_processing(&f, &incremental).await;
    let effect = assess_measurement_effect(&incremental, 99.0).expect("classifies");
    let _ = f
        .repository
        .complete_measurement(f.workspace_id, &incremental, 99.0, effect, f.now)
        .await;
    let (_, y14_again, _, resolved_again) = evidence_state(&f, action_id).await;
    assert_eq!(y14, y14_again, "the first outcome stands");
    assert_eq!(resolved_at, resolved_again, "readiness does not move");

    let (_, other_y14, _, other_resolved) = evidence_state(&f, other_action_id).await;
    assert_eq!(other_y14, None, "another action's outcome is unaffected");
    assert!(
        other_resolved.is_none(),
        "and another action's readiness is unaffected"
    );
    assert_eq!(
        untouched.action_id.into_uuid(),
        other_action_id,
        "the untouched measurement belongs to the other action"
    );
}

/// H: contamination decides whether a randomised assignment is causal evidence.
///
/// `evidence_quality` is written at dispatch from the design and records that
/// the unit was randomised. Whether that randomisation survived the measurement
/// window is a different fact, and it lives in `final_contamination`. The row
/// used to carry the first and nothing could read the second, so a randomised
/// unit that also received two other campaigns still read as clean causal
/// evidence — as did one whose contamination was never evaluated at all.
#[test]
fn h_causal_status_requires_randomisation_and_established_cleanliness() {
    use crowdrelay_brain::{EvidenceQuality, GrowthEvidence};

    let randomised = |contamination: Option<f64>| GrowthEvidence {
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: contamination,
        ..GrowthEvidence::default()
    };

    assert_eq!(
        randomised(Some(0.0)).effective_evidence_quality(),
        EvidenceQuality::RandomizedHoldout,
        "randomised and established clean is causal evidence"
    );
    assert_eq!(
        randomised(Some(0.5)).effective_evidence_quality(),
        EvidenceQuality::MatchedQuasiExperiment,
        "a unit that also received other campaigns is not a clean experiment"
    );
    assert_eq!(
        randomised(None).effective_evidence_quality(),
        EvidenceQuality::MatchedQuasiExperiment,
        "never evaluated is not the same fact as clean"
    );
    // The threshold is the one `evaluate_contamination` and
    // `mark_causal_credits` both use, so the three cannot drift apart.
    assert_eq!(
        randomised(Some(crowdrelay_brain::CONTAMINATION_CEILING)).effective_evidence_quality(),
        EvidenceQuality::MatchedQuasiExperiment,
        "at the ceiling is not below it"
    );
    // A row that never claimed randomisation is left alone.
    assert_eq!(
        GrowthEvidence {
            evidence_quality: EvidenceQuality::Observational,
            final_contamination: None,
            ..GrowthEvidence::default()
        }
        .effective_evidence_quality(),
        EvidenceQuality::Observational,
        "contamination does not upgrade or downgrade non-randomised evidence"
    );
}

/// I: a control unit gets an outcome, and intent-to-treat uses it.
///
/// Control is withheld on purpose, so no action exists and nothing schedules a
/// measurement for it. Its evidence row sat unresolved forever while treatment
/// rows resolved around it, and the learner averaged the treated units' own
/// pre/post differences and called the result intent-to-treat.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn i_control_units_produce_an_outcome_and_itt_uses_it() {
    let f = setup().await.expect("fixture");
    let assigned_at = f.now - time::Duration::days(50);

    // A community that converts nobody, assigned to control.
    let control_target = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_outreach_targets
           (id, workspace_id, target_kind, display_name, subreddit, status)
           VALUES ($1,$2,'community','r/controlcomm','controlcomm','promoted')"#,
    )
    .bind(control_target)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("control target");

    let experiment_uuid = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
           (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
            unit_kind, holdout_probability, interference_policy, experiment_status)
           VALUES ($1,$2,'community-engager',$3,'target_community',0.1,'none','active')"#,
    )
    .bind(experiment_uuid)
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid.to_string())
    .execute(&f.pool)
    .await
    .expect("design");

    // The control assignment, and the evidence row that names it.
    let control_assignment = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind, arm, assigned_at,
            intended_template_id, propensity, context, prediction,
            contamination_estimate, is_interference_controllable, experiment_status,
            execution_status, experiment_kind)
           VALUES ($1,$2,$3,$4,'target_community','control',$5,'community-engager',0.9,
                   '{}'::jsonb,'{}'::jsonb,0.0,true,'active','control','randomized_holdout')"#,
    )
    .bind(&control_assignment)
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(control_target.to_string())
    .bind(assigned_at)
    .execute(&f.pool)
    .await
    .expect("control assignment");
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality,
            experiment_assignment_id)
           VALUES ($1,NULL,'community-engager:control',$2,'r/controlcomm','reddit_post',
                   1,'control',0.9,false,2.0,1.0,'{}'::jsonb,'randomized_holdout',$3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(assigned_at)
    .bind(&control_assignment)
    .execute(&f.pool)
    .await
    .expect("control evidence");

    // A treated unit in the same experiment, whose measurement is what drives
    // the control sweep.
    let action_id = insert_dispatch(&f, "community-engager:i", assigned_at).await;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind, arm, assigned_at,
            intended_template_id, propensity, context, prediction,
            contamination_estimate, is_interference_controllable, experiment_status,
            execution_status, action_id, experiment_kind)
           VALUES ($1,$2,$3,$4,'target_community','treatment',$5,'community-engager',0.9,
                   '{}'::jsonb,'{}'::jsonb,0.0,true,'active','executed',$6,
                   'randomized_holdout')"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(assigned_at)
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("treatment assignment");
    sqlx::query(
        "UPDATE viryaos_growth_evidence SET experiment_assignment_id = \
         (SELECT id FROM viryaos_experiment_assignments WHERE action_id = $2) \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("link treatment evidence");

    let incremental = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        assigned_at,
    )
    .await;
    resolve(&f, &incremental, 5.0).await;

    // The control row now has an outcome it could never have had before.
    let (control_y14, control_resolved) =
        sqlx::query_as::<_, (Option<f64>, Option<OffsetDateTime>)>(
            "SELECT observed_incremental_fans, resolved_at FROM viryaos_growth_evidence \
             WHERE workspace_id=$1 AND experiment_assignment_id=$2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(&control_assignment)
        .fetch_one(&f.pool)
        .await
        .expect("control evidence state");
    assert!(
        control_resolved.is_some(),
        "the control arm must be measured, not left unresolved forever"
    );
    assert_eq!(
        control_y14,
        Some(0.0),
        "a community that converted nobody is a real control outcome of zero"
    );

    // And contamination was established inside the measurement transaction, so
    // the treated row can be causal evidence at all.
    let final_contamination = sqlx::query_scalar::<_, Option<f64>>(
        "SELECT final_contamination FROM viryaos_experiment_assignments \
         WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("contamination");
    assert!(
        final_contamination.is_some(),
        "contamination must be established with the outcome, not after it"
    );

    // Both arms reach the learner, which is what makes the contrast possible.
    let evidence = f
        .repository
        .load_growth_evidence(f.workspace_id, None)
        .await
        .expect("evidence");
    let arms: Vec<_> = evidence.iter().map(|ev| ev.treatment).collect();
    assert!(
        arms.contains(&crowdrelay_brain::TreatmentAssignment::Control)
            && arms.contains(&crowdrelay_brain::TreatmentAssignment::Treatment),
        "intent-to-treat needs both arms in the batch, got {arms:?}"
    );
    assert!(
        evidence
            .iter()
            .all(|ev| ev.experiment_uuid == Some(experiment_uuid)),
        "both arms must name the experiment they are being compared within"
    );
}

/// J: a treated row waits for its control arm, so the two are replayed together.
///
/// Intent-to-treat compares arms, and the contrast is computed per delta batch.
/// The treated units of one experiment finish their measurements days apart —
/// production has nine spread across 2026-10-13 to 2026-10-17 — while the
/// control arm resolves once, on whichever measurement happens to fire after
/// its own window elapses. Every treated row that resolved before that moment
/// was consumed in an earlier batch, contrasted against nothing, and capped at
/// a quasi-experiment. The delta cursor never returns, so the intent-to-treat
/// estimate those units were randomised to produce could not be recovered.
///
/// Readiness is the right place to hold them: the row is not model-ready until
/// every outcome its model update needs exists, and under intent-to-treat the
/// control arm's outcome is one of them.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn j_treated_rows_wait_for_their_control_arm() {
    let f = setup().await.expect("fixture");
    // Old enough that every measurement window has elapsed.
    let assigned_at = f.now - time::Duration::days(50);
    let experiment_uuid = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
           (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
            unit_kind, holdout_probability, interference_policy, experiment_status)
           VALUES ($1,$2,'community-engager',$3,'target_community',0.1,'none','active')"#,
    )
    .bind(experiment_uuid)
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid.to_string())
    .execute(&f.pool)
    .await
    .expect("design");

    // One control unit, its assignment and the evidence row that names it.
    let control_target = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_outreach_targets
           (id, workspace_id, target_kind, display_name, subreddit, status)
           VALUES ($1,$2,'community','r/waitctrl','waitctrl','promoted')"#,
    )
    .bind(control_target)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("control target");
    let control_assignment = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind, arm, assigned_at,
            intended_template_id, propensity, context, prediction,
            contamination_estimate, is_interference_controllable, experiment_status,
            execution_status, experiment_kind)
           VALUES ($1,$2,$3,$4,'target_community','control',$5,'community-engager',0.9,
                   '{}'::jsonb,'{}'::jsonb,0.0,true,'active','control','randomized_holdout')"#,
    )
    .bind(&control_assignment)
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(control_target.to_string())
    .bind(assigned_at)
    .execute(&f.pool)
    .await
    .expect("control assignment");
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality,
            experiment_assignment_id)
           VALUES ($1,NULL,'community-engager:wait-control',$2,'r/waitctrl','reddit_post',
                   1,'control',0.9,false,2.0,1.0,'{}'::jsonb,'randomized_holdout',$3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(assigned_at)
    .bind(&control_assignment)
    .execute(&f.pool)
    .await
    .expect("control evidence");

    // Two treated units. `early` finishes its measurement first; `late` is the
    // one whose completion will resolve the control arm.
    let mut treated = Vec::new();
    for label in ["early", "late"] {
        let action_id =
            insert_dispatch(&f, &format!("community-engager:{label}"), assigned_at).await;
        let assignment_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO viryaos_experiment_assignments
               (id, workspace_id, experiment_uuid, unit_id, unit_kind, arm, assigned_at,
                intended_template_id, propensity, context, prediction,
                contamination_estimate, is_interference_controllable, experiment_status,
                execution_status, action_id, experiment_kind)
               VALUES ($1,$2,$3,$4,'target_community','treatment',$5,'community-engager',0.9,
                       '{}'::jsonb,'{}'::jsonb,0.0,true,'active','executed',$6,
                       'randomized_holdout')"#,
        )
        .bind(&assignment_id)
        .bind(f.workspace_id.into_uuid())
        .bind(experiment_uuid)
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(assigned_at)
        .bind(action_id)
        .execute(&f.pool)
        .await
        .expect("treatment assignment");
        sqlx::query(
            "UPDATE viryaos_growth_evidence SET experiment_assignment_id = $3 \
             WHERE workspace_id = $1 AND action_id = $2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(action_id)
        .bind(&assignment_id)
        .execute(&f.pool)
        .await
        .expect("link treatment evidence");
        treated.push(action_id);
    }
    let early = treated[0];
    let late = treated[1];

    // The early unit finishes everything it was waiting on.
    let early_measurement = queue_measurement(
        &f,
        early,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        assigned_at,
    )
    .await;
    // Twenty days in: the control arm's forty-four-day window has not elapsed,
    // so it cannot be measured yet. This is the production ordering.
    resolve_at(
        &f,
        &early_measurement,
        5.0,
        assigned_at + time::Duration::days(20),
    )
    .await;

    let (_, early_y14, _, early_resolved) = evidence_state(&f, early).await;
    assert_eq!(
        early_y14,
        Some(5.0),
        "the outcome itself lands as soon as it is measured"
    );
    assert!(
        early_resolved.is_none(),
        "but the row is not model-ready while its control arm is unmeasured — \
         replaying it now would contrast it against nothing and consume it"
    );

    // The late unit finishes, which is what resolves the control arm.
    let late_measurement = queue_measurement(
        &f,
        late,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        assigned_at,
    )
    .await;
    resolve(&f, &late_measurement, 7.0).await;

    let (_, _, _, early_resolved) = evidence_state(&f, early).await;
    let (_, _, _, late_resolved) = evidence_state(&f, late).await;
    let control_resolved = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "SELECT resolved_at FROM viryaos_growth_evidence \
         WHERE workspace_id=$1 AND experiment_assignment_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(&control_assignment)
    .fetch_one(&f.pool)
    .await
    .expect("control evidence state");

    assert!(control_resolved.is_some(), "the control arm was measured");
    assert!(
        early_resolved.is_some(),
        "the row held back earlier must be released by the control sweep, not \
         left waiting for a measurement that already completed"
    );
    assert!(
        late_resolved.is_some(),
        "and so must the one that triggered it"
    );

    // Same batch is the point: the contrast is computed per delta, so all three
    // must be visible to a single replay.
    let batch = f
        .repository
        .load_growth_evidence(f.workspace_id, Some(assigned_at))
        .await
        .expect("delta replay");
    let arms: Vec<_> = batch.iter().map(|ev| ev.treatment).collect();
    assert_eq!(
        arms.iter()
            .filter(|arm| **arm == crowdrelay_brain::TreatmentAssignment::Treatment)
            .count(),
        2,
        "both treated rows in one batch, got {arms:?}"
    );
    assert!(
        arms.contains(&crowdrelay_brain::TreatmentAssignment::Control),
        "with the control arm they are contrasted against, got {arms:?}"
    );

    // Consumed exactly once: a cursor past this batch returns nothing, so no
    // row can be applied to the posterior a second time.
    let after = batch
        .iter()
        .filter_map(|ev| ev.resolved_at)
        .max()
        .expect("resolved rows carry a cursor value");
    let replayed = f
        .repository
        .load_growth_evidence(f.workspace_id, Some(after))
        .await
        .expect("second delta replay");
    assert!(
        replayed.is_empty(),
        "advancing the cursor past the batch must consume it, got {} rows",
        replayed.len()
    );
}
