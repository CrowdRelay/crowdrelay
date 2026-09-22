//! Business invariants for the generic metric write-back.
//!
//! The typed fan-growth columns (`observed_incremental_fans`,
//! `durable_fans_30d`, …) feed dedicated posteriors. Every other measured
//! outcome — ticket revenue, clicks, replies, engagement — lands in the
//! evidence row's `observed_metrics` map, which the metric posteriors replay.
//! These tests pin down the write side: the merge keeps earlier keys, each
//! kind lands under its own `learnable_metric_key`, and the typed kinds never
//! double-write into the map.

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
    HarmObservation, assess_measurement_effect,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("metric-learn-{suffix}"))
        .bind("Metric Learning Tests")
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
async fn insert_dispatch(
    f: &Fixture,
    opportunity_id: &str,
    dispatched_at: OffsetDateTime,
) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
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
        r#"INSERT INTO autopilot_actions
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
        r#"INSERT INTO growth_evidence
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
        r#"INSERT INTO autopilot_measurements
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
        due_at: action_finished_at + time::Duration::days(7),
        attempt_number: 1,
    }
}

/// Observes and completes one measurement the way the worker loop does, so the
/// tests exercise the real classification step rather than a hand-made effect.
async fn resolve(f: &Fixture, measurement: &ClaimedAutopilotMeasurement, observed: f64) {
    sqlx::query(
        "UPDATE autopilot_measurements SET status='processing', started_at=now() \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(measurement.id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("processing");
    let effect = assess_measurement_effect(measurement, observed, &HarmObservation::default())
        .expect("a measurement the worker can classify");
    f.repository
        .complete_measurement(
            f.workspace_id,
            measurement,
            observed,
            effect,
            Some(&HarmObservation::default()),
            f.now,
        )
        .await
        .expect("complete");
}

/// H: a measured outcome with no typed column still reaches the evidence row.
///
/// Ticket revenue, clicks, replies and friends used to resolve into
/// `autopilot_outcomes` and stop there — assessed, stored, and
/// invisible to every learner. `observed_metrics` is the general write-back:
/// each learnable kind lands under its own key, a merge preserves the keys
/// already present, and a second measurement of the same kind cannot rewrite
/// the first observation.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn generic_metrics_merge_into_evidence() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "opp-metrics", f.now - time::Duration::days(30)).await;

    async fn metrics(f: &Fixture, action_id: uuid::Uuid) -> serde_json::Value {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT observed_metrics FROM growth_evidence \
             WHERE workspace_id=$1 AND action_id=$2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("evidence metrics")
    }

    // Before anything resolves the map is empty — a dispatch does not
    // pre-populate zeros for metrics nobody measured yet.
    assert_eq!(metrics(&f, action_id).await, serde_json::json!({}));

    let revenue = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::TicketRevenue72h,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &revenue, 4200.0).await;
    assert_eq!(
        metrics(&f, action_id).await["ticket_revenue_minor"].as_f64(),
        Some(4200.0),
        "a revenue measurement lands under its learnable key"
    );

    // A different kind merges beside it rather than replacing the map.
    let clicks = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &clicks, 17.0).await;
    let merged = metrics(&f, action_id).await;
    assert_eq!(merged["ticket_revenue_minor"].as_f64(), Some(4200.0));
    assert_eq!(
        merged["show_growth_clicks"].as_f64(),
        Some(17.0),
        "a second metric merges without losing the first"
    );
    let metric_keys: Vec<&String> = merged
        .as_object()
        .expect("metrics object")
        .keys()
        .filter(|key| !key.starts_with("harm:"))
        .collect();
    assert_eq!(
        metric_keys.len(),
        2,
        "and no other metric lands in the map — the harm keys are the \
         separate ledger every completion writes"
    );
}

/// I: a fan-growth kind keeps its value in its typed column only.
///
/// The fan-growth kinds return `None` from `learnable_metric_key` on purpose:
/// they already write `observed_incremental_fans` / `observed_signal_installs`,
/// and a value stored in two places is a value a learner can count twice.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn typed_kinds_do_not_double_write_metrics() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "opp-typed", f.now - time::Duration::days(30)).await;

    let growth = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &growth, 5.0).await;

    let (incremental, metrics): (Option<f64>, serde_json::Value) = sqlx::query_as(
        "SELECT observed_incremental_fans, observed_metrics \
         FROM growth_evidence WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence");
    assert_eq!(
        incremental,
        Some(5.0),
        "the typed column still gets the value"
    );
    let metric_keys: Vec<&String> = metrics
        .as_object()
        .expect("metrics object")
        .keys()
        .filter(|key| !key.starts_with("harm:"))
        .collect();
    assert!(
        metric_keys.is_empty(),
        "a typed kind writes no metric keys — the harm keys are the \
         separate ledger every completion writes"
    );
}

/// Phase 2: standing is keyed `action_kind:identity` over every measured
/// action, not only agent templates. A show lever's own worsened outcomes
/// retire it exactly the way a worker template's do, an unmeasured lever is
/// simply absent (untested, never harmed), and an agent template's key cannot
/// collide with a lever that happens to share its name.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn action_standings_cover_levers_and_templates_alike() {
    use crowdrelay_application::autopilot::AutopilotDecisionRepository;
    use crowdrelay_domain::learning::Standing;

    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();

    // One worsened outcome under one action. Three in a row is the
    // retirement streak — measured outcomes only, which is what the
    // outcomes table carries.
    async fn seed_worsened_action(
        f: &Fixture,
        action_kind: &str,
        payload: serde_json::Value,
        worsened: usize,
    ) {
        for _ in 0..worsened {
            let decision_id = uuid::Uuid::now_v7();
            let action_id = uuid::Uuid::now_v7();
            let measurement_id = uuid::Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO autopilot_decisions
                   (id, workspace_id, decision_key, context, subject_kind,
                    subject_id, decision_kind, confidence_basis_points,
                    disposition, reason, input_snapshot, policy_snapshot,
                    recommendation, trace_id)
                   VALUES ($1,$2,$3,'show_growth','event',$4,'activate',9000,
                           'auto_execute','test','{}'::jsonb,'{}'::jsonb,
                           '{}'::jsonb,gen_random_uuid())"#,
            )
            .bind(decision_id)
            .bind(f.workspace_id.into_uuid())
            .bind(format!("key-{action_id}"))
            .bind(uuid::Uuid::now_v7())
            .execute(&f.pool)
            .await
            .expect("decision");
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    action_class, finished_at)
                   VALUES ($1,$2,$3,'show_growth',$4,'event',$5,$6,$7,
                           'succeeded','third_party',$8)"#,
            )
            .bind(action_id)
            .bind(f.workspace_id.into_uuid())
            .bind(decision_id)
            .bind(action_kind)
            .bind(uuid::Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(&payload)
            .bind(f.now)
            .execute(&f.pool)
            .await
            .expect("action");
            sqlx::query(
                r#"INSERT INTO autopilot_measurements
                   (id, workspace_id, action_id, measurement_kind, subject_id,
                    action_finished_at, baseline_value, due_at, status,
                    available_at, finished_at)
                   VALUES ($1,$2,$3,'show_growth_attributed_ticket_orders_7d',
                           $3,$4,0,now(),'succeeded',now(),now())"#,
            )
            .bind(measurement_id)
            .bind(f.workspace_id.into_uuid())
            .bind(action_id)
            .bind(f.now)
            .execute(&f.pool)
            .await
            .expect("measurement");
            sqlx::query(
                r#"INSERT INTO autopilot_outcomes
                   (workspace_id, decision_id, action_id, measurement_id,
                    metric_key, observed_value, baseline_value,
                    effect_assessment, delta_basis_points, observed_at)
                   VALUES ($1,$2,$3,$4,'effect.test',0,10,'worsened',-1000,
                           now())"#,
            )
            .bind(f.workspace_id.into_uuid())
            .bind(decision_id)
            .bind(action_id)
            .bind(measurement_id)
            .execute(&f.pool)
            .await
            .expect("outcome");
        }
    }

    seed_worsened_action(
        &f,
        "show.growth.request",
        serde_json::json!({"kind": "request_show_growth", "lever": "partner_cross_promo"}),
        3,
    )
    .await;
    seed_worsened_action(
        &f,
        "agent.run.request",
        serde_json::json!({"kind": "request_agent_run", "template_id": "social-post"}),
        3,
    )
    .await;

    let standings = f
        .repository
        .load_action_standings(f.workspace_id)
        .await
        .expect("standings");

    assert_eq!(
        standings.get("show.growth.request:partner_cross_promo"),
        Some(&Standing::Retired {
            reason: crowdrelay_domain::learning::RetirementReason::RepeatedlyWorsened,
        }),
        "a lever whose outcomes all worsened retires under its own key"
    );
    assert_eq!(
        standings.get("agent.run.request:social-post"),
        Some(&Standing::Retired {
            reason: crowdrelay_domain::learning::RetirementReason::RepeatedlyWorsened,
        }),
        "agent templates keep standing under the kind-qualified key"
    );
    assert!(
        !standings.contains_key("show.growth.request:grassroots_scene_relay"),
        "an unmeasured lever is absent — untested, never harmed"
    );

    // And nothing leaks across workspaces — the other tenant's worsened run
    // must not retire this workspace's lever.
    let other = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(other.into_uuid())
        .bind(format!("standing-{}", other.into_uuid().simple()))
        .bind("Other")
        .execute(&f.pool)
        .await
        .expect("other workspace");
    let other_standings = f
        .repository
        .load_action_standings(other)
        .await
        .expect("other standings");
    assert!(other_standings.is_empty(), "workspace isolation");

    let _ = workspace;
}

/// Phase 4: attendance is a show outcome, and a show that never happened has
/// no outcome to measure.
///
/// `show_attendance_rate_14d` observes redeemed admission passes over every
/// pass valid for entry — revoked passes were taken back and are no show's
/// fault. A cancelled event fails the measurement outright: zero attendance
/// on a show that was never held is the absence of an outcome, not a zero
/// rate, and writing it would teach the learner the lever failed.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn show_attendance_rate_and_cancelled_event_guards() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();

    async fn insert_event(f: &Fixture, status: &str) -> uuid::Uuid {
        let event_id = uuid::Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status)
               VALUES ($1,$2,$3,'Gig',$4,$5)"#,
        )
        .bind(event_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("gig-{}", event_id.simple()))
        .bind(f.now - time::Duration::days(20))
        .bind(status)
        .execute(&f.pool)
        .await
        .expect("event");
        event_id
    }

    async fn insert_passes(f: &Fixture, event_id: uuid::Uuid, statuses: &[&str]) {
        let pool_id = uuid::Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO admission_pools (id, workspace_id, event_id, name, capacity, slug)
               VALUES ($1,$2,$3,'GA',100,$4)"#,
        )
        .bind(pool_id)
        .bind(f.workspace_id.into_uuid())
        .bind(event_id)
        .bind(format!("ga-{}", pool_id.simple()))
        .execute(&f.pool)
        .await
        .expect("pool");
        for (i, status) in statuses.iter().enumerate() {
            let fan_id = uuid::Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO fans (id, workspace_id, normalized_email, status)
                   VALUES ($1,$2,$3,'active')"#,
            )
            .bind(fan_id)
            .bind(f.workspace_id.into_uuid())
            .bind(format!("pass-holder-{i}-{}", event_id.simple()))
            .execute(&f.pool)
            .await
            .expect("fan");
            // The pass lifecycle is constrained per status: `issued` holds a
            // live claim token, `claimed`/`redeemed` have consumed it.
            sqlx::query(
                r#"INSERT INTO admission_passes
                   (id, workspace_id, event_id, admission_pool_id, fan_id,
                    issuance_method, public_reference, claim_expires_at,
                    status, claim_token_hash, claim_token_consumed_at,
                    claimed_at, redeemed_at)
                   VALUES (gen_random_uuid(),$1,$2,$3,$4,'first_come',$5,
                           now() + interval '30 days',$6,
                           CASE WHEN $6 = 'issued' THEN gen_random_bytes(32) END,
                           CASE WHEN $6 IN ('claimed','redeemed') THEN now() END,
                           CASE WHEN $6 IN ('claimed','redeemed') THEN now() END,
                           CASE WHEN $6 = 'redeemed' THEN now() END)"#,
            )
            .bind(f.workspace_id.into_uuid())
            .bind(event_id)
            .bind(pool_id)
            .bind(fan_id)
            .bind(format!("PASS-{}-{i}", event_id.simple()))
            .bind(*status)
            .execute(&f.pool)
            .await
            .expect("pass");
        }
    }

    let action_id = insert_dispatch(&f, "opp-attendance", f.now - time::Duration::days(30)).await;

    let claim = |event_id: uuid::Uuid| ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
        action_id: AutopilotActionId::from(action_id),
        kind: AutopilotMeasurementKind::ShowAttendanceRate14d,
        subject_id: event_id,
        baseline_value: 0.0,
        action_finished_at: f.now - time::Duration::days(20),
        due_at: f.now,
        attempt_number: 1,
    };

    // 2 redeemed of 4 valid (issued + expired count; the revoked pass was
    // taken back before the show and is nobody's no-show).
    let event_id = insert_event(&f, "completed").await;
    insert_passes(
        &f,
        event_id,
        &["redeemed", "redeemed", "issued", "expired", "revoked"],
    )
    .await;
    let observed = f
        .repository
        .observe_measurement(f.workspace_id, &claim(event_id), f.now)
        .await
        .expect("attendance observation");
    assert!(
        (observed - 0.5).abs() < 1e-9,
        "redeemed over valid-for-entry: got {observed}"
    );

    // A cancelled event abandons the measurement rather than reporting a
    // zero the learner would blame on the action.
    let cancelled = insert_event(&f, "cancelled").await;
    insert_passes(&f, cancelled, &["issued", "issued"]).await;
    let error = f
        .repository
        .observe_measurement(f.workspace_id, &claim(cancelled), f.now)
        .await
        .expect_err("a cancelled show has no outcome");
    assert!(
        matches!(
            error,
            crowdrelay_application::RepositoryError::ConflictBecause(kind)
                if kind == AutopilotMeasurementKind::EVENT_CANCELLED
        ),
        "cancelled events abandon as event_cancelled, got {error:?}"
    );

    // An event that never ticketed through the platform has no denominator —
    // the measurement cannot be read at all, and is not a zero rate.
    let unticketed = insert_event(&f, "completed").await;
    let error = f
        .repository
        .observe_measurement(f.workspace_id, &claim(unticketed), f.now)
        .await
        .expect_err("no passes means nothing to observe");
    assert!(
        matches!(
            error,
            crowdrelay_application::RepositoryError::ConflictBecause(kind)
                if kind == AutopilotMeasurementKind::NO_ISSUED_PASSES
        ),
        "pass-free events abandon as no_issued_passes, got {error:?}"
    );

    let _ = workspace;
}

/// Phase 5: a release milestone's own outcomes reach the evidence row.
///
/// The milestone executor guarantees the campaign link exists before it
/// sends, and these kinds read back through exactly that binding:
/// acquisitions and clicks join `campaigns.release_plan_id`, conversions
/// join the release-acquired fan to a paid order by address, and the
/// channel lift reads the release's own metric series against the fourteen
/// days before the milestone ran. A release nobody heard reads zeros — a
/// release that never had a campaign row reads zeros too, because the
/// tracked link is the attribution boundary.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn release_milestone_metrics_reach_the_evidence_row() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(14);
    let release_id = uuid::Uuid::now_v7();

    sqlx::query(
        r#"INSERT INTO release_plans
           (id, workspace_id, source_key, title, release_at, tier)
           VALUES ($1,$2,$3,'Signal Lost',$4,'single')"#,
    )
    .bind(release_id)
    .bind(workspace)
    .bind(format!("release-{release_id}"))
    .bind(anchor)
    .execute(&f.pool)
    .await
    .expect("release plan");

    let campaign_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO campaigns (id, workspace_id, name, release_plan_id)
           VALUES ($1,$2,'release-campaign',$3)"#,
    )
    .bind(campaign_id)
    .bind(workspace)
    .bind(release_id)
    .execute(&f.pool)
    .await
    .expect("campaign");

    // Two fans acquired through the release's campaign inside the window —
    // one of whom bought a ticket in it — plus a click on the tracked link.
    let buyer_fan = uuid::Uuid::now_v7();
    for (fan_id, email) in [
        (buyer_fan, "release-buyer@x.test"),
        (uuid::Uuid::now_v7(), "release-lurker@x.test"),
    ] {
        sqlx::query(
            r#"INSERT INTO fans (id, workspace_id, normalized_email, status)
               VALUES ($1,$2,$3,'active')"#,
        )
        .bind(fan_id)
        .bind(workspace)
        .bind(email)
        .execute(&f.pool)
        .await
        .expect("fan");
        sqlx::query(
            r#"INSERT INTO fan_acquisition_events
               (workspace_id, fan_id, campaign_id, source, request_id, occurred_at)
               VALUES ($1,$2,$3,'release_link',$4,$5)"#,
        )
        .bind(workspace)
        .bind(fan_id)
        .bind(campaign_id)
        .bind(format!("req-{fan_id}"))
        .bind(anchor + time::Duration::days(2))
        .execute(&f.pool)
        .await
        .expect("acquisition");
    }

    let smart_link_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO smart_links (id, workspace_id, campaign_id, slug, destination_url)
           VALUES ($1,$2,$3,$4,'https://example.test/listen')"#,
    )
    .bind(smart_link_id)
    .bind(workspace)
    .bind(campaign_id)
    .bind(format!("rel-{}", release_id.simple()))
    .execute(&f.pool)
    .await
    .expect("smart link");
    sqlx::query(
        r#"INSERT INTO click_events (workspace_id, smart_link_id, campaign_id, occurred_at)
           VALUES ($1,$2,$3,$4),($1,$2,$3,$4)"#,
    )
    .bind(workspace)
    .bind(smart_link_id)
    .bind(campaign_id)
    .bind(anchor + time::Duration::days(3))
    .execute(&f.pool)
    .await
    .expect("clicks");

    // The buyer's paid order inside the window — sale rows need the event
    // and pool they sell from.
    let event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
           VALUES ($1,$2,$3,'Album show',$4,'published',$5)"#,
    )
    .bind(event_id)
    .bind(workspace)
    .bind(format!("show-{}", event_id.simple()))
    .bind(f.now + time::Duration::days(30))
    .bind(anchor - time::Duration::days(40))
    .execute(&f.pool)
    .await
    .expect("event");
    let pool_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO admission_pools (id, workspace_id, event_id, name, capacity, slug)
           VALUES ($1,$2,$3,'GA',100,$4)"#,
    )
    .bind(pool_id)
    .bind(workspace)
    .bind(event_id)
    .bind(format!("ga-{}", pool_id.simple()))
    .execute(&f.pool)
    .await
    .expect("pool");
    let sale_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO ticket_sales
           (id, workspace_id, event_id, admission_pool_id, capacity,
            sales_open_at, sales_close_at)
           VALUES ($1,$2,$3,$4,100,$5,$6)"#,
    )
    .bind(sale_id)
    .bind(workspace)
    .bind(event_id)
    .bind(pool_id)
    .bind(anchor - time::Duration::days(30))
    .bind(f.now + time::Duration::days(60))
    .execute(&f.pool)
    .await
    .expect("sale");
    sqlx::query(
        r#"INSERT INTO ticket_orders
           (workspace_id, ticket_sale_id, public_reference, buyer_email, status,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, reservation_key, request_hash,
            checkout_token_hash, expires_at, paid_at)
           VALUES ($1,$2,$3,'release-buyer@x.test','paid','PLN',
                   5000,4630,370,800,$4,gen_random_bytes(32),gen_random_bytes(32),
                   now() + interval '1 day',$5)"#,
    )
    .bind(workspace)
    .bind(sale_id)
    .bind(format!("VRY-ORD-{:016X}", f.now.unix_timestamp() as u64))
    .bind(format!("res-{}", sale_id.simple()))
    .bind(anchor + time::Duration::days(5))
    .execute(&f.pool)
    .await
    .expect("order");

    // The release's own channel series: flat-ish before the milestone
    // (100 → 110), a real bump after (110 → 160). Lift = 50 − 10 = 40.
    let series_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO growth_metric_series
           (id, workspace_id, platform, metric_key, subject_kind, subject_id,
            display_name)
           VALUES ($1,$2,'youtube','views','release_plan',$3,'Signal Lost views')"#,
    )
    .bind(series_id)
    .bind(workspace)
    .bind(release_id)
    .execute(&f.pool)
    .await
    .expect("series");
    for (days, value) in [(-20_i64, 100_i64), (-1, 110), (12, 160)] {
        sqlx::query(
            r#"INSERT INTO growth_metric_points
               (workspace_id, series_id, captured_at, value, source)
               VALUES ($1,$2,$3,$4,'test')"#,
        )
        .bind(workspace)
        .bind(series_id)
        .bind(anchor + time::Duration::days(days))
        .bind(value)
        .execute(&f.pool)
        .await
        .expect("point");
    }

    let action_id = insert_dispatch(&f, "opp-release", anchor).await;
    let claim = |kind: AutopilotMeasurementKind| ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
        action_id: AutopilotActionId::from(action_id),
        kind,
        subject_id: release_id,
        baseline_value: 0.0,
        action_finished_at: anchor,
        due_at: f.now,
        attempt_number: 1,
    };

    // Observe through the real queries, then complete so the write-back
    // lands on the evidence row.
    for (kind, expected) in [
        (AutopilotMeasurementKind::ReleaseBoundAcquisition14d, 2.0),
        (AutopilotMeasurementKind::ReleaseLinkClicks14d, 2.0),
        (AutopilotMeasurementKind::ReleaseFanConversion14d, 1.0),
        (AutopilotMeasurementKind::ReleaseChannelLift14d, 40.0),
    ] {
        let claim = claim(kind);
        let observed = f
            .repository
            .observe_measurement(f.workspace_id, &claim, f.now)
            .await
            .expect("release observation");
        assert!(
            (observed - expected).abs() < 1e-9,
            "{} observed {observed}, expected {expected}",
            kind.as_str()
        );
        sqlx::query(
            r#"INSERT INTO autopilot_measurements
               (id, workspace_id, action_id, measurement_kind, subject_id,
                action_finished_at, baseline_value, due_at, available_at)
               VALUES ($1,$2,$3,$4,$5,$6,0.0,$7,$7)"#,
        )
        .bind(claim.id.into_uuid())
        .bind(workspace)
        .bind(action_id)
        .bind(kind.as_str())
        .bind(release_id)
        .bind(anchor)
        .bind(f.now)
        .execute(&f.pool)
        .await
        .expect("measurement row");
        resolve(&f, &claim, observed).await;
    }

    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence \
         WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence metrics");
    assert_eq!(metrics["release_acquisitions"].as_f64(), Some(2.0));
    assert_eq!(metrics["release_link_clicks"].as_f64(), Some(2.0));
    assert_eq!(metrics["release_fan_conversions"].as_f64(), Some(1.0));
    assert_eq!(metrics["release_channel_lift"].as_f64(), Some(40.0));
}

/// A release plan with no listenable URL never gets a campaign, so its
/// funnel measurements observe a funnel that was never instrumented. Each
/// counter must abandon with `no_release_link` rather than record a
/// fabricated zero — missing evidence is not a measured failure.
#[tokio::test]
#[ignore = "postgres"]
async fn release_funnel_abandons_when_no_link_was_tracked() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(14);
    let release_id = uuid::Uuid::now_v7();

    sqlx::query(
        r#"INSERT INTO release_plans
           (id, workspace_id, source_key, title, release_at, tier)
           VALUES ($1,$2,$3,'Unlinked Single',$4,'single')"#,
    )
    .bind(release_id)
    .bind(workspace)
    .bind(format!("release-{release_id}"))
    .bind(anchor)
    .execute(&f.pool)
    .await
    .expect("release plan");

    let action_id = insert_dispatch(&f, "opp-release-nolink", anchor).await;
    for kind in [
        AutopilotMeasurementKind::ReleaseBoundAcquisition14d,
        AutopilotMeasurementKind::ReleaseLinkClicks14d,
        AutopilotMeasurementKind::ReleaseFanConversion14d,
    ] {
        let claim = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind,
            subject_id: release_id,
            baseline_value: 0.0,
            action_finished_at: anchor,
            due_at: f.now,
            attempt_number: 1,
        };
        let outcome = f
            .repository
            .observe_measurement(f.workspace_id, &claim, f.now)
            .await;
        match outcome {
            Err(crowdrelay_application::RepositoryError::ConflictBecause(reason)) => {
                assert_eq!(reason, AutopilotMeasurementKind::NO_RELEASE_LINK);
            }
            other => panic!(
                "expected no_release_link abandon for {}, got {other:?}",
                kind.as_str()
            ),
        }
    }
}
