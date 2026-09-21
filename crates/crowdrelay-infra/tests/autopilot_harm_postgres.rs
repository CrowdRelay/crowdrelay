//! Harm observation against a real Postgres.
//!
//! Every measurement answers two questions, not one: what the action earned
//! and what it cost. The cost side counts consent withdrawals, suppressions,
//! complaints, refunds and cancellations attributable to the action inside
//! the measurement's own window — last-touch across every contact channel
//! the ledger can see. What fails here and nowhere else: a withdrawal
//! pinned on the wrong send, harm counted outside the window, a terminal
//! failure that loses the harm it already observed, or an operator's manual
//! send quietly blamed on the autopilot.

use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
    HarmObservation, assess_measurement_effect,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_domain::performance::EffectAssessment;
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
        .bind(format!("harm-{suffix}"))
        .bind("Harm Tests")
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

/// A decision, an action whose payload names the event it promoted, and the
/// growth evidence row a dispatch writes.
async fn insert_dispatch(
    f: &Fixture,
    opportunity_id: &str,
    event_id: uuid::Uuid,
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
           VALUES ($1,$2,$3,'growth_metrics','audience.campaign.request','target_community',
                   $4,$5,$6,'succeeded','owned_audience',$7)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(serde_json::json!({
        "kind": "request_audience_campaign",
        "event_id": event_id,
        "phase": "announcement",
        "template_key": "event.announcement.v1"
    }))
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("action");
    sqlx::query(
        r#"INSERT INTO growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality)
           VALUES ($1,$2,$3,$4,'recipient','email',100,'treatment',0.9,false,
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

/// A campaign dispatched through the outbox — `dispatch_event_id` carries
/// the owning action, the same chain the production senders write.
async fn insert_campaign(
    f: &Fixture,
    segment_id: uuid::Uuid,
    slug: &str,
    action_id: Option<uuid::Uuid>,
    dispatched_at: OffsetDateTime,
) -> uuid::Uuid {
    let campaign_id = uuid::Uuid::now_v7();
    let outbox_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events
           (id, workspace_id, event_type, event_version, payload, action_id, available_at)
           VALUES ($1,$2,'communication.campaign_due',1,'{}'::jsonb,$3,$4)"#,
    )
    .bind(outbox_id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("outbox");
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key,
            status, scheduled_at, dispatch_event_id)
           VALUES ($1,$2,$3,$4,$5,'email','event.announcement.v1','scheduled',$6,$7)"#,
    )
    .bind(campaign_id)
    .bind(f.workspace_id.into_uuid())
    .bind(segment_id)
    .bind(slug)
    .bind(format!("{slug} campaign"))
    .bind(dispatched_at)
    .bind(outbox_id)
    .execute(&f.pool)
    .await
    .expect("campaign");
    campaign_id
}

async fn insert_fan(f: &Fixture, tag: &str) -> uuid::Uuid {
    let fan_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,'active')",
    )
    .bind(fan_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("{tag}-{}@x.test", fan_id.simple()))
    .execute(&f.pool)
    .await
    .expect("fan");
    fan_id
}

async fn insert_delivery(
    f: &Fixture,
    campaign_id: uuid::Uuid,
    fan_id: uuid::Uuid,
    delivered_at: OffsetDateTime,
) {
    sqlx::query(
        r#"INSERT INTO communication_campaign_recipients
           (workspace_id, campaign_id, fan_id) VALUES ($1,$2,$3)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(f.workspace_id.into_uuid())
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
    .bind(f.workspace_id.into_uuid())
    .bind(campaign_id)
    .bind(fan_id)
    .bind(format!("attempt-{campaign_id}-{fan_id}"))
    .bind(delivered_at)
    .execute(&f.pool)
    .await
    .expect("delivery");
}

async fn withdraw(f: &Fixture, fan_id: uuid::Uuid, recorded_at: OffsetDateTime) {
    sqlx::query(
        r#"INSERT INTO fan_consents
           (id, workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
           VALUES ($1,$2,$3,'marketing',false,'v1','unsubscribe',$4)"#,
    )
    .bind(uuid::Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(recorded_at)
    .execute(&f.pool)
    .await
    .expect("withdrawal");
}

fn claimed(
    action_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    action_finished_at: OffsetDateTime,
    due_at: OffsetDateTime,
) -> ClaimedAutopilotMeasurement {
    ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
        action_id: AutopilotActionId::from(action_id),
        kind: AutopilotMeasurementKind::CampaignUnsubscribe7d,
        subject_id,
        baseline_value: 0.0,
        action_finished_at,
        due_at,
        attempt_number: 1,
    }
}

/// Last-touch attribution across both contact channels: a withdrawal lands
/// on the send the fan most recently received — campaign deliveries bound
/// through the dispatch outbox, signal pushes named by `source_id` — and a
/// deletion counts as a suppression. Complaints and refunds ride their own
/// joins: the reach ledger for the outreach target, the promoted event's
/// payload key for the refund and the cancellation.
#[tokio::test]
#[ignore = "postgres"]
async fn harm_counts_each_source_against_the_action_that_caused_it() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(5);

    let event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
           VALUES ($1,$2,$3,'Promoted show',$4,'cancelled',$5)"#,
    )
    .bind(event_id)
    .bind(workspace)
    .bind(format!("show-{}", event_id.simple()))
    .bind(f.now + time::Duration::days(30))
    .bind(anchor - time::Duration::days(40))
    .execute(&f.pool)
    .await
    .expect("event");

    let action_id = insert_dispatch(&f, "opp-harm", event_id, anchor).await;
    let sibling_action = insert_dispatch(&f, "opp-harm-sibling", event_id, anchor).await;

    let segment_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO audience_segments (id, workspace_id, slug, name, description, filter, active)
           VALUES ($1,$2,$3,'harm audience','test segment','{}'::jsonb,true)"#,
    )
    .bind(segment_id)
    .bind(workspace)
    .bind(format!("seg-{segment_id}"))
    .execute(&f.pool)
    .await
    .expect("segment");

    let campaign = insert_campaign(&f, segment_id, "camp-harm", Some(action_id), anchor).await;
    let sibling = insert_campaign(&f, segment_id, "camp-later", Some(sibling_action), anchor).await;

    // Withdrawer: delivered by our campaign, then rescinded marketing consent.
    let withdrawer = insert_fan(&f, "wd").await;
    insert_delivery(&f, campaign, withdrawer, anchor + time::Duration::days(1)).await;
    withdraw(&f, withdrawer, anchor + time::Duration::days(3)).await;

    // Suppressed: delivered by our campaign, then the account was deleted —
    // the deletion CHECK wants the whole tombstone shape, not just the stamp.
    let suppressed = insert_fan(&f, "sup").await;
    insert_delivery(&f, campaign, suppressed, anchor + time::Duration::days(1)).await;
    sqlx::query(
        "UPDATE fans SET status='suppressed', display_name=NULL, locale=NULL, \
         normalized_email=$4, deleted_at=$3 \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(suppressed)
    .bind(anchor + time::Duration::days(2))
    .bind(format!("deleted-{suppressed}@account.invalid"))
    .execute(&f.pool)
    .await
    .expect("suppress");

    // Stolen: our send first, the sibling's send after, then the withdrawal —
    // the last touch owns it.
    let stolen = insert_fan(&f, "stolen").await;
    insert_delivery(&f, campaign, stolen, anchor + time::Duration::days(1)).await;
    insert_delivery(&f, sibling, stolen, anchor + time::Duration::days(2)).await;
    withdraw(&f, stolen, anchor + time::Duration::days(3)).await;

    // Signal push: `source_id` names the action directly.
    let pushed = insert_fan(&f, "push").await;
    let endpoint_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO fan_push_endpoints
           (id, workspace_id, fan_id, installation_id, transport, endpoint_address)
           VALUES ($5,$1,$2,$3,'android_fcm',$4)"#,
    )
    .bind(workspace)
    .bind(pushed)
    .bind(format!("install-{}", pushed.simple()))
    .bind(format!("fcm-token-{}-0123456789abcdef", pushed.simple()))
    .bind(endpoint_id)
    .execute(&f.pool)
    .await
    .expect("endpoint");
    sqlx::query(
        r#"INSERT INTO fan_push_deliveries
           (workspace_id, fan_id, endpoint_id, source_kind, source_id,
            title, body, target_path, status, delivered_at)
           VALUES ($1,$2,$3,'agent_signal_push',$4,'t','b','/home','delivered',$5)"#,
    )
    .bind(workspace)
    .bind(pushed)
    .bind(endpoint_id)
    .bind(action_id)
    .bind(anchor + time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("push");
    withdraw(&f, pushed, anchor + time::Duration::days(2)).await;

    // A fan nobody's send touched withdrew anyway — baseline churn is not
    // this action's harm.
    let stranger = insert_fan(&f, "stranger").await;
    withdraw(&f, stranger, anchor + time::Duration::days(3)).await;

    // Complaint on an outreach target the action reached.
    let target_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outreach_targets
           (id, workspace_id, target_kind, display_name, contact_email)
           VALUES ($1,$2,'press','Weekly Rag',$3)"#,
    )
    .bind(target_id)
    .bind(workspace)
    .bind(format!("desk-{}@rag.test", target_id.simple()))
    .execute(&f.pool)
    .await
    .expect("target");
    sqlx::query(
        r#"INSERT INTO reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel,
            template_id, estimated_reach, status, sent_at)
           VALUES ($1,$2,'outreach_target',$3,'email','pitch-sender',1,'sent',$4)"#,
    )
    .bind(workspace)
    .bind(action_id)
    .bind(target_id.to_string())
    .bind(anchor + time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("reach");
    sqlx::query(
        r#"INSERT INTO outreach_delivery_faults
           (workspace_id, target_id, fault, occurred_at)
           VALUES ($1,$2,'complaint',$3)"#,
    )
    .bind(workspace)
    .bind(target_id)
    .bind(anchor + time::Duration::days(2))
    .execute(&f.pool)
    .await
    .expect("fault");

    // A refund against the promoted event — the measurement's subject is
    // the campaign, the payload's `event_id` is the join.
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
    let order_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO ticket_orders
           (id, workspace_id, ticket_sale_id, public_reference, buyer_email, status,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, reservation_key, request_hash,
            checkout_token_hash, expires_at, paid_at)
           VALUES ($1,$2,$3,'VRY-ORD-0000000000ABCD01','buyer@x.test','paid','PLN',
                   5000,4630,370,800,$4,gen_random_bytes(32),gen_random_bytes(32),
                   now() + interval '1 day',$5)"#,
    )
    .bind(order_id)
    .bind(workspace)
    .bind(sale_id)
    .bind(format!("res-harm-{}", sale_id.simple()))
    .bind(anchor + time::Duration::days(2))
    .execute(&f.pool)
    .await
    .expect("order");
    sqlx::query(
        r#"INSERT INTO ticket_accounting_entries
           (workspace_id, ticket_order_id, event_id, stripe_event_id,
            entry_kind, occurred_at, currency, vat_rate_basis_points,
            amount_gross_minor, amount_net_minor, amount_vat_minor)
           VALUES ($1,$2,$3,$4,'refund',$5,'PLN',800,-5000,-4630,-370)"#,
    )
    .bind(workspace)
    .bind(order_id)
    .bind(event_id)
    .bind(format!("evt-refund-{}", order_id.simple()))
    .bind(anchor + time::Duration::days(4))
    .execute(&f.pool)
    .await
    .expect("refund");

    let measurement = claimed(
        action_id,
        campaign,
        anchor,
        anchor + time::Duration::days(7),
    );
    let harm = f
        .repository
        .observe_action_harm(f.workspace_id, &measurement, f.now)
        .await
        .expect("harm observation");

    // The withdrawer and the pushed fan are ours; the stolen fan belongs to
    // the sibling send and the stranger to nobody.
    assert_eq!(harm.unsubscribes, 2.0, "last-touch owns the withdrawals");
    assert_eq!(harm.fan_suppressions, 1.0, "the deletion is a suppression");
    assert_eq!(harm.complaints, 1.0, "the complaint rides the reach ledger");
    assert_eq!(
        harm.refunds, 1.0,
        "the refund joins through the payload event"
    );
    assert_eq!(harm.show_cancellations, 1.0, "the promoted show cancelled");
    assert_eq!(harm.fan_equivalent_loss(), 3.0);

    // The sibling action's own measurement sees the stolen withdrawal.
    let sibling_measurement = claimed(
        sibling_action,
        sibling,
        anchor,
        anchor + time::Duration::days(7),
    );
    let sibling_harm = f
        .repository
        .observe_action_harm(f.workspace_id, &sibling_measurement, f.now)
        .await
        .expect("sibling harm");
    assert_eq!(sibling_harm.unsubscribes, 1.0, "the last touch owns it");
    assert_eq!(sibling_harm.fan_suppressions, 0.0);
    assert_eq!(sibling_harm.complaints, 0.0);

    // The harm keys land on the evidence row's observed_metrics when the
    // measurement completes — zeros included, a clean source is evidence.
    sqlx::query(
        r#"INSERT INTO autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, status,
            started_at, attempt_count)
           VALUES ($1,$2,$3,$4,$5,$6,0.0,$7,$7,'processing',$8,1)"#,
    )
    .bind(measurement.id.into_uuid())
    .bind(workspace)
    .bind(action_id)
    .bind(measurement.kind.as_str())
    .bind(campaign)
    .bind(anchor)
    .bind(anchor + time::Duration::days(7))
    .bind(anchor)
    .execute(&f.pool)
    .await
    .expect("measurement row");
    let effect = assess_measurement_effect(&measurement, 0.02, &harm).expect("assess");
    f.repository
        .complete_measurement(
            f.workspace_id,
            &measurement,
            0.02,
            effect,
            Some(&harm),
            f.now,
        )
        .await
        .expect("complete");
    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence metrics");
    assert_eq!(metrics["harm:unsubscribes"].as_f64(), Some(2.0));
    assert_eq!(metrics["harm:fan_suppressions"].as_f64(), Some(1.0));
    assert_eq!(metrics["harm:complaints"].as_f64(), Some(1.0));
    assert_eq!(metrics["harm:refunds"].as_f64(), Some(1.0));
    assert_eq!(metrics["harm:show_cancellations"].as_f64(), Some(1.0));
}

/// A measurement that dies terminal still leaves its harm on the evidence
/// row — the window elapsed even though the primary metric never read — and
/// a retryable miss leaves the retried completion owning the merge.
#[tokio::test]
#[ignore = "postgres"]
async fn terminal_failure_keeps_the_harm_it_observed() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(5);

    let event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
           VALUES ($1,$2,$3,'Show',$4,'published',$5)"#,
    )
    .bind(event_id)
    .bind(workspace)
    .bind(format!("show-{}", event_id.simple()))
    .bind(f.now + time::Duration::days(30))
    .bind(anchor - time::Duration::days(40))
    .execute(&f.pool)
    .await
    .expect("event");

    let action_id = insert_dispatch(&f, "opp-harm-fail", event_id, anchor).await;
    let segment_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO audience_segments (id, workspace_id, slug, name, description, filter, active)
           VALUES ($1,$2,$3,'harm audience','test segment','{}'::jsonb,true)"#,
    )
    .bind(segment_id)
    .bind(workspace)
    .bind(format!("seg-{segment_id}"))
    .execute(&f.pool)
    .await
    .expect("segment");
    let campaign = insert_campaign(&f, segment_id, "camp-fail", Some(action_id), anchor).await;
    let withdrawer = insert_fan(&f, "wd-fail").await;
    insert_delivery(&f, campaign, withdrawer, anchor + time::Duration::days(1)).await;
    withdraw(&f, withdrawer, anchor + time::Duration::days(3)).await;

    let measurement = claimed(
        action_id,
        campaign,
        anchor,
        anchor + time::Duration::days(7),
    );
    let harm = f
        .repository
        .observe_action_harm(f.workspace_id, &measurement, f.now)
        .await
        .expect("harm observation");
    assert_eq!(harm.unsubscribes, 1.0);

    // Retryable miss first: the row returns to pending and no harm lands —
    // the retried completion owns the merge.
    sqlx::query(
        r#"INSERT INTO autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, status,
            started_at, attempt_count)
           VALUES ($1,$2,$3,$4,$5,$6,0.0,$7,$7,'processing',$8,1)"#,
    )
    .bind(measurement.id.into_uuid())
    .bind(workspace)
    .bind(action_id)
    .bind(measurement.kind.as_str())
    .bind(campaign)
    .bind(anchor)
    .bind(anchor + time::Duration::days(7))
    .bind(anchor)
    .execute(&f.pool)
    .await
    .expect("measurement row");
    f.repository
        .fail_measurement(
            f.workspace_id,
            &measurement,
            "unavailable",
            true,
            Some(&harm),
            f.now,
        )
        .await
        .expect("retryable fail");
    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence metrics");
    assert!(
        metrics["harm:unsubscribes"].is_null(),
        "a pending retry must not merge harm early"
    );

    // Terminal failure: the harm the measurement already observed lands with
    // the row's close — the retry path is exhausted at attempt 3.
    sqlx::query(
        "UPDATE autopilot_measurements \
         SET status='processing', started_at=now(), attempt_count=3 \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace)
    .bind(measurement.id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("reprocess");
    f.repository
        .fail_measurement(
            f.workspace_id,
            &measurement,
            "unavailable",
            true,
            Some(&harm),
            f.now,
        )
        .await
        .expect("terminal fail");
    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence metrics");
    assert_eq!(
        metrics["harm:unsubscribes"].as_f64(),
        Some(1.0),
        "terminal failure still leaves the harm it observed"
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM autopilot_measurements \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(measurement.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("status");
    assert_eq!(status, "failed");
}

/// Harm under a flat primary is Worsened — nothing gained while fans left
/// is the definition of a step backwards — while harm under real growth
/// keeps the primary verdict and prices the loss in `harm_fans` instead.
#[test]
fn harm_classification_is_bounded_by_the_primary_verdict() {
    let now = OffsetDateTime::now_utc();
    let measurement = ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
        action_id: AutopilotActionId::from(uuid::Uuid::now_v7()),
        kind: AutopilotMeasurementKind::CampaignTicketConversion14d,
        subject_id: uuid::Uuid::now_v7(),
        baseline_value: 0.0,
        action_finished_at: now - time::Duration::days(14),
        due_at: now,
        attempt_number: 1,
    };
    let harm = HarmObservation {
        unsubscribes: 3.0,
        ..HarmObservation::default()
    };
    let flat = assess_measurement_effect(&measurement, 0.0, &harm).expect("flat");
    assert_eq!(flat.assessment, EffectAssessment::Worsened);
    let grew = assess_measurement_effect(&measurement, 4.0, &harm).expect("grew");
    assert_eq!(grew.assessment, EffectAssessment::Improved);
}

/// A collector that never ran is not a clean reading: a terminal failure
/// carrying `None` must leave the row without `harm:*` keys rather than
/// write zeros a broken observation did not earn.
#[tokio::test]
#[ignore = "postgres"]
async fn unobserved_harm_writes_no_keys() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();
    let anchor = f.now - time::Duration::days(5);

    let event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
           VALUES ($1,$2,$3,'Show',$4,'published',$5)"#,
    )
    .bind(event_id)
    .bind(workspace)
    .bind(format!("show-{}", event_id.simple()))
    .bind(f.now + time::Duration::days(30))
    .bind(anchor - time::Duration::days(40))
    .execute(&f.pool)
    .await
    .expect("event");

    let action_id = insert_dispatch(&f, "opp-harm-none", event_id, anchor).await;
    let measurement = claimed(
        action_id,
        uuid::Uuid::now_v7(),
        anchor,
        anchor + time::Duration::days(7),
    );
    sqlx::query(
        r#"INSERT INTO autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, status,
            started_at, attempt_count)
           VALUES ($1,$2,$3,$4,$5,$6,0.0,$7,$7,'processing',$8,3)"#,
    )
    .bind(measurement.id.into_uuid())
    .bind(workspace)
    .bind(action_id)
    .bind(measurement.kind.as_str())
    .bind(measurement.subject_id)
    .bind(anchor)
    .bind(anchor + time::Duration::days(7))
    .bind(anchor)
    .execute(&f.pool)
    .await
    .expect("measurement row");

    f.repository
        .fail_measurement(
            f.workspace_id,
            &measurement,
            "unavailable",
            true,
            None,
            f.now,
        )
        .await
        .expect("terminal fail");

    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence metrics");
    for key in [
        "harm:unsubscribes",
        "harm:complaints",
        "harm:refunds",
        "harm:fan_suppressions",
        "harm:show_cancellations",
    ] {
        assert!(
            metrics[key].is_null(),
            "no {key} may be written for an observation that never ran"
        );
    }
}
