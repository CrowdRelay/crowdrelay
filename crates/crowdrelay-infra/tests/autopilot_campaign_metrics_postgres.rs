//! Email-campaign measurement against a real Postgres.
//!
//! A communication campaign answers for what its send did to the fans the
//! delivery ledger says it reached: who bought a ticket inside their own
//! fourteen-day window, and who withdrew consent inside their own seven.
//! What fails here and nowhere else: a measurement that reads a fabricated
//! zero for a send that never left, a permanently abandoned measurement for
//! a send still in flight, a window that truncates a slow delivery, or a
//! scheduler that wedges a no-send milestone looking for a campaign row that
//! only send-bearing milestones create.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotMeasurementKind, AutopilotMeasurementRepository,
    AutopilotTeamStateRepository, ClaimedAutopilotMeasurement, UpsertReleasePlan,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_domain::release_autopilot::ReleaseTier;
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
        .bind(format!("campaign-metrics-{suffix}"))
        .bind("Campaign Metric Tests")
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

/// Queue a release milestone action and run it through the production
/// claim-and-execute path, the same way the worker picks it up.
async fn run_release_milestone(
    f: &Fixture,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    release_at: OffsetDateTime,
    milestone: &str,
) {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'release','release_plan',$4,'execute_release_milestone',
                   9000,'auto_execute','milestone due','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}-{milestone}"))
    .bind(release_id.into_uuid())
    .bind(f.now)
    .execute(&f.pool)
    .await
    .expect("decision");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'release','release.milestone.execute','release_plan',
                   $4,$5,$6,'queued',$7,'system:test',$7)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(release_id.into_uuid())
    .bind(format!("action:release:{release_id}:{milestone}"))
    .bind(serde_json::json!({
        "kind": "execute_release_milestone",
        "release_id": release_id.into_uuid(),
        "title": title,
        "release_at": release_at,
        "milestone": milestone,
    }))
    .bind(f.now)
    .execute(&f.pool)
    .await
    .expect("action");
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, f.now)
        .await
        .expect("claim");
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued milestone action is claimable");
    f.repository
        .execute_action(f.workspace_id, action, f.now)
        .await
        .expect("the milestone action must execute, not wedge");
}

/// A communication campaign answers for what the send did: delivered fans
/// who bought after their own receipt convert (last touch — a newer send to
/// the same fan owns the outcome), and a consent withdrawal is harm measured
/// as a rate. The window bounds on each fan's delivery, so a slow send still
/// gets its full fourteen days. A dispatched campaign still awaiting ledger
/// results retries; one that never reached anyone abandons rather than
/// reading a fabricated zero.
#[tokio::test]
#[ignore = "postgres"]
async fn campaign_measurements_read_the_delivery_ledger() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(14);

    let segment_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO audience_segments (id, workspace_id, slug, name, description, filter, active)
           VALUES ($1,$2,$3,'release audience','test segment','{}'::jsonb,true)"#,
    )
    .bind(segment_id)
    .bind(workspace)
    .bind(format!("seg-{segment_id}"))
    .execute(&f.pool)
    .await
    .expect("segment");
    let campaign_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key, status)
           VALUES ($1,$2,$3,'camp-main','Signal Lost · release_day','email',
                   'release.release_day.v1','draft')"#,
    )
    .bind(campaign_id)
    .bind(workspace)
    .bind(segment_id)
    .execute(&f.pool)
    .await
    .expect("send campaign");

    // The send a fan gets after ours owns what happens next — the sibling
    // campaign exists only so the last-touch guard has someone to lose to.
    let sibling_campaign = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key, status)
           VALUES ($1,$2,$3,'camp-sibling','Signal Lost · sustain','email',
                   'release.sustain.v1','draft')"#,
    )
    .bind(sibling_campaign)
    .bind(workspace)
    .bind(segment_id)
    .execute(&f.pool)
    .await
    .expect("sibling campaign");

    // Six delivered fans: a buyer, a buyer whose receipt lands ten days in
    // (its window runs past the action-anchored horizon an early
    // implementation would have used), a quitter, a fan whose order a newer
    // send owns, a fan whose withdrawal a newer send owns, and one who does
    // nothing.
    let buyer = uuid::Uuid::now_v7();
    let late_buyer = uuid::Uuid::now_v7();
    let quitter = uuid::Uuid::now_v7();
    let stolen_buyer = uuid::Uuid::now_v7();
    let stolen_quitter = uuid::Uuid::now_v7();
    let quiet = uuid::Uuid::now_v7();
    for (i, (fan_id, delivered_at)) in [
        (buyer, 1),
        (late_buyer, 10),
        (quitter, 1),
        (stolen_buyer, 1),
        (stolen_quitter, 1),
        (quiet, 1),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            r#"INSERT INTO fans (id, workspace_id, normalized_email, status)
               VALUES ($1,$2,$3,'active')"#,
        )
        .bind(fan_id)
        .bind(workspace)
        .bind(format!("camp-fan-{i}@x.test"))
        .execute(&f.pool)
        .await
        .expect("fan");
        sqlx::query(
            r#"INSERT INTO communication_campaign_recipients
               (workspace_id, campaign_id, fan_id) VALUES ($1,$2,$3)"#,
        )
        .bind(workspace)
        .bind(campaign_id)
        .bind(fan_id)
        .execute(&f.pool)
        .await
        .expect("recipient");
        sqlx::query(
            r#"INSERT INTO communication_campaign_deliveries
               (workspace_id, campaign_id, fan_id, attempt_key, status, completed_at)
               VALUES ($1,$2,$3,$4,'delivered',$5)"#,
        )
        .bind(workspace)
        .bind(campaign_id)
        .bind(fan_id)
        .bind(format!("attempt-{fan_id}"))
        .bind(anchor + time::Duration::days(delivered_at))
        .execute(&f.pool)
        .await
        .expect("delivery");
    }

    // The sibling send reaches the two stolen fans a day after ours did.
    for fan_id in [stolen_buyer, stolen_quitter] {
        sqlx::query(
            r#"INSERT INTO communication_campaign_recipients
               (workspace_id, campaign_id, fan_id) VALUES ($1,$2,$3)"#,
        )
        .bind(workspace)
        .bind(sibling_campaign)
        .bind(fan_id)
        .execute(&f.pool)
        .await
        .expect("sibling recipient");
        sqlx::query(
            r#"INSERT INTO communication_campaign_deliveries
               (workspace_id, campaign_id, fan_id, attempt_key, status, completed_at)
               VALUES ($1,$2,$3,$4,'delivered',$5)"#,
        )
        .bind(workspace)
        .bind(sibling_campaign)
        .bind(fan_id)
        .bind(format!("attempt-sibling-{fan_id}"))
        .bind(anchor + time::Duration::days(2))
        .execute(&f.pool)
        .await
        .expect("sibling delivery");
    }

    // Ticket orders: the buyer inside their window, the late buyer inside
    // theirs (past the action-anchored horizon), the stolen buyer after the
    // sibling send reached them.
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
    for (idx, paid_at) in [
        anchor + time::Duration::days(4),
        anchor + time::Duration::days(18),
        anchor + time::Duration::days(5),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            r#"INSERT INTO ticket_orders
               (workspace_id, ticket_sale_id, public_reference, buyer_email, status,
                currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
                vat_rate_basis_points, reservation_key, request_hash,
                checkout_token_hash, expires_at, paid_at)
               VALUES ($1,$2,$3,$4,'paid','PLN',
                       5000,4630,370,800,$5,gen_random_bytes(32),gen_random_bytes(32),
                       now() + interval '1 day',$6)"#,
        )
        .bind(workspace)
        .bind(sale_id)
        .bind(format!(
            "VRY-ORD-{:016X}",
            f.now.unix_timestamp() as u64 + idx as u64
        ))
        .bind(format!("camp-fan-{}@x.test", [0usize, 1, 3][idx]))
        .bind(format!("res-{idx}-{}", sale_id.simple()))
        .bind(paid_at)
        .execute(&f.pool)
        .await
        .expect("order");
    }

    // The quitter withdraws marketing consent after their delivery; the
    // stolen quitter withdraws after the sibling reached them.
    for (fan_id, recorded_at) in [
        (quitter, anchor + time::Duration::days(3)),
        (stolen_quitter, anchor + time::Duration::days(3)),
    ] {
        sqlx::query(
            r#"INSERT INTO fan_consents
               (id, workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
               VALUES ($1,$2,$3,'marketing',false,'v1','unsubscribe',$4)"#,
        )
        .bind(uuid::Uuid::now_v7())
        .bind(workspace)
        .bind(fan_id)
        .bind(recorded_at)
        .execute(&f.pool)
        .await
        .expect("consent withdrawal");
    }

    let action_id = insert_dispatch(&f, "opp-campaign", anchor).await;
    for (kind, expected) in [
        // Buyer + late buyer; the stolen buyer's order belongs to the
        // sibling send.
        (AutopilotMeasurementKind::CampaignTicketConversion14d, 2.0),
        // One attributed withdrawal in six delivered.
        (AutopilotMeasurementKind::CampaignUnsubscribe7d, 1.0 / 6.0),
    ] {
        let claim = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind,
            subject_id: campaign_id,
            baseline_value: 0.0,
            action_finished_at: anchor,
            attempt_number: 1,
        };
        let observed = f
            .repository
            .observe_measurement(f.workspace_id, &claim, f.now)
            .await
            .expect("campaign observation");
        assert!(
            (observed - expected).abs() < 1e-9,
            "{} observed {observed}, expected {expected}",
            kind.as_str()
        );
    }

    // A dispatched send whose ledger results have not landed yet retries —
    // it can still reach someone.
    let outbox_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events (id, workspace_id, event_type, payload)
           VALUES ($1,$2,'communication.campaign_due','{}'::jsonb)"#,
    )
    .bind(outbox_id)
    .bind(workspace)
    .execute(&f.pool)
    .await
    .expect("outbox");
    let inflight_campaign = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key,
            status, scheduled_at, dispatch_event_id)
           VALUES ($1,$2,$3,'camp-inflight','in-flight','email',
                   'release.wrap.v1','scheduled',$4,$5)"#,
    )
    .bind(inflight_campaign)
    .bind(workspace)
    .bind(segment_id)
    .bind(anchor)
    .bind(outbox_id)
    .execute(&f.pool)
    .await
    .expect("in-flight campaign");
    for kind in [
        AutopilotMeasurementKind::CampaignTicketConversion14d,
        AutopilotMeasurementKind::CampaignUnsubscribe7d,
    ] {
        let claim = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind,
            subject_id: inflight_campaign,
            baseline_value: 0.0,
            action_finished_at: anchor,
            attempt_number: 1,
        };
        let outcome = f
            .repository
            .observe_measurement(f.workspace_id, &claim, f.now)
            .await;
        match outcome {
            Err(crowdrelay_application::RepositoryError::Unavailable) => {}
            other => panic!(
                "expected retryable unavailable for in-flight {}, got {other:?}",
                kind.as_str()
            ),
        }
    }

    // A campaign that never delivered has no outcomes — every kind abandons
    // rather than reporting zeros for a send that never happened.
    let dead_campaign = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key, status)
           VALUES ($1,$2,$3,'camp-dead','never sent','email','release.wrap.v1','draft')"#,
    )
    .bind(dead_campaign)
    .bind(workspace)
    .bind(segment_id)
    .execute(&f.pool)
    .await
    .expect("dead campaign");
    for kind in [
        AutopilotMeasurementKind::CampaignTicketConversion14d,
        AutopilotMeasurementKind::CampaignUnsubscribe7d,
    ] {
        let claim = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind,
            subject_id: dead_campaign,
            baseline_value: 0.0,
            action_finished_at: anchor,
            attempt_number: 1,
        };
        let outcome = f
            .repository
            .observe_measurement(f.workspace_id, &claim, f.now)
            .await;
        match outcome {
            Err(crowdrelay_application::RepositoryError::ConflictBecause(reason)) => {
                assert_eq!(reason, AutopilotMeasurementKind::NEVER_PUBLISHED);
            }
            other => panic!(
                "expected never_published abandon for {}, got {other:?}",
                kind.as_str()
            ),
        }
    }
}

/// The press milestone sends no email — it seeds outreach targets, so no
/// `communication_campaigns` row exists for its slug. A scheduler that reads
/// that absence as an error wedges the whole milestone action terminally
/// (the transaction rolls back, the milestone stays due, and the dead action
/// row absorbs every re-emitted decision). What fails here and nowhere else:
/// a regression that reintroduces a hard lookup on a row only send-bearing
/// milestones create.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn the_press_milestone_schedules_funnel_metrics_without_a_send()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await.expect("fixture");
    let release_at = OffsetDateTime::from_unix_timestamp(f.now.unix_timestamp() + 20 * 86_400)
        .expect("release_at");
    let created = f
        .repository
        .upsert_release_plan(
            f.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "press-measure".into(),
                title: "press-measure title".into(),
                release_at,
                listen_url: None,
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            &IdempotencyKey::parse("campaign-metrics-press").expect("bounded key"),
            None,
        )
        .await
        .expect("release plan");

    run_release_milestone(
        &f,
        created.release_id,
        "press-measure title",
        release_at,
        "start_press",
    )
    .await;

    // The funnel measurements land on the release; nothing campaign-bound
    // can exist for a milestone that never sent.
    let kinds = sqlx::query_scalar::<_, String>(
        "SELECT measurement_kind FROM viryaos_autopilot_measurements \
         WHERE workspace_id = $1 AND subject_id = $2 ORDER BY measurement_kind",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(created.release_id.into_uuid())
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(
        kinds,
        vec![
            "release_bound_acquisition_14d",
            "release_fan_conversion_14d",
            "release_link_clicks_14d",
        ],
        "start_press should schedule exactly the release funnel kinds"
    );

    // And the milestone is durably marked — a rolled-back transaction would
    // leave the milestone due and the release ladder stalled at press.
    let marked = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM viryaos_release_milestones
             WHERE workspace_id = $1 AND release_id = $2 AND milestone = 'start_press'
         )",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(created.release_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert!(
        marked,
        "the press milestone must commit its completion mark"
    );
    Ok(())
}
