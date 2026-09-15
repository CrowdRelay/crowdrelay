//! 1G.12 — the T+7 post-show report is a system artifact, not a human chore.
//!
//! `post_show_report` escalation resolves into `issue_post_show_report`:
//! labelled first-party numbers, the campaigns the system ran with their
//! receipts, and a recipient list (band members + the event counterparty)
//! emitted as `crowdrelay.show.post_show_report_due` on the show.escalation
//! capability. The checklist item is marked done in the same transaction so
//! the report never double-sends.

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
        .bind(format!("t7-report-{suffix}"))
        .bind("T+7 Report Tests")
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

async fn advertise_show_escalation(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-report-test','test','test-manifest',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-report-test','show.escalation','1',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

/// Seeds an event eight days in the past with a counterparty, one session
/// check-in, one email-claim check-in on a freshly created fan, a paid order,
/// an act-attributed click, a delivered recap campaign, and a band member.
async fn seed_show(f: &Fixture, now: OffsetDateTime) -> Result<Uuid, Box<dyn std::error::Error>> {
    let event_id = Uuid::now_v7();
    let city_id = Uuid::now_v7();
    let starts_at = now - time::Duration::days(8);
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) VALUES ($1, 'krakow', 'Kraków', 'PL')
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(city_id)
    .execute(&f.pool)
    .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'krakow'",
    )
    .fetch_one(&f.pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, venue, timezone, starts_at,
            status, published_at, counterparty_name, counterparty_email
        ) VALUES (
            $1, $2, $3, 'krakow-live-2026', 'Virya live', 'Klub Testowy',
            'Europe/Warsaw', $4, 'published', $5, 'Promoter Jan', 'promoter@example.test'
        )
        "#,
    )
    .bind(event_id)
    .bind(f.workspace_id.into_uuid())
    .bind(city_id)
    .bind(starts_at)
    .bind(starts_at - time::Duration::days(20))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position)
         VALUES ($1, $2, 'virya', 'Virya', 0)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .execute(&f.pool)
    .await?;

    // A band member the report is addressed to.
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
         VALUES ($1, 'band@example.test', 'Virya Band', 'owner', 'active')",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;

    // Room evidence: one long-known fan on a session scan, one stranger who
    // claimed an email identity at the door (fan record created at show time).
    let qr_campaign_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concert_qr_campaigns (workspace_id, event_id, id, label, valid_from, valid_until)
         VALUES ($1, $2, $3, 'poster', $4, $5)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .bind(qr_campaign_id)
    .bind(starts_at - time::Duration::days(7))
    .bind(starts_at + time::Duration::days(7))
    .execute(&f.pool)
    .await?;

    let known_fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1, $2, 'known@example.test', 'active', $3)",
    )
    .bind(known_fan)
    .bind(f.workspace_id.into_uuid())
    .bind(starts_at - time::Duration::days(60))
    .execute(&f.pool)
    .await?;
    let new_fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1, $2, 'stranger@example.test', 'active', $3)",
    )
    .bind(new_fan)
    .bind(f.workspace_id.into_uuid())
    .bind(starts_at + time::Duration::hours(1))
    .execute(&f.pool)
    .await?;
    for (fan_id, source) in [(known_fan, "session"), (new_fan, "email_claim")] {
        sqlx::query(
            "INSERT INTO concert_checkins (workspace_id, event_id, campaign_id, fan_id, checked_in_at, identity_source)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(event_id)
        .bind(qr_campaign_id)
        .bind(fan_id)
        .bind(starts_at + time::Duration::hours(1))
        .bind(source)
        .execute(&f.pool)
        .await?;
    }

    // Proxy signals: a paid ticket order and an act-attributed ticket click.
    let pool_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admission_pools (id, workspace_id, event_id, name, capacity, slug)
         VALUES ($1, $2, $3, 'General', 200, 'general')",
    )
    .bind(pool_id)
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .execute(&f.pool)
    .await?;
    let sale_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ticket_sales (id, workspace_id, event_id, admission_pool_id, capacity, sales_open_at, sales_close_at)
         VALUES ($1, $2, $3, $4, 200, $5, $6)",
    )
    .bind(sale_id)
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .bind(pool_id)
    .bind(starts_at - time::Duration::days(30))
    .bind(starts_at)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO ticket_orders (
               workspace_id, ticket_sale_id, public_reference, buyer_email,
               currency, amount_gross_minor, amount_net_minor,
               amount_vat_minor, vat_rate_basis_points, reservation_key,
               request_hash, checkout_token_hash, status, paid_at, expires_at
           ) VALUES (
               $1, $2, 'VRY-ORD-0123456789ABCDEF', 'buyer@example.test',
               'PLN', 10000, 8130, 1870, 2300, 'res-test-1',
               decode(repeat('ab', 32), 'hex'), decode(repeat('cd', 32), 'hex'),
               'paid', $3, now() + interval '1 hour'
           )"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(sale_id)
    .bind(starts_at - time::Duration::days(10))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_action_events (workspace_id, event_id, action, occurred_at, act_slug)
         VALUES ($1, $2, 'ticket_click', $3, 'virya')",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .bind(starts_at - time::Duration::days(5))
    .execute(&f.pool)
    .await?;

    // The recap the system sent next morning, with its delivery receipt.
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audience_segments (workspace_id, id, slug, name, filter, active)
         VALUES ($1, $2, 'viryaos-krakow-live-2026-post-show-recap', 'recap', '{}', true)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(segment_id)
    .execute(&f.pool)
    .await?;
    let dispatch_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO outbox_events (workspace_id, event_type, payload)
         VALUES ($1, 'communication.campaign_due', '{}') RETURNING id",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO communication_campaigns (
               workspace_id, segment_id, slug, name, channel, template_key, content,
               status, scheduled_at, dispatch_event_id,
               recipient_count, delivered_count, failed_count, completed_at
           ) VALUES (
               $1, $2, 'viryaos-krakow-live-2026-post-show-recap', 'recap', 'email',
               'post_show_recap', $3, 'completed', $4, $5, 2, 2, 0, $4
           )"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(segment_id)
    .bind(json!({"event_id": event_id, "lever": "post_show_recap"}))
    .bind(starts_at + time::Duration::hours(15))
    .bind(dispatch_id)
    .execute(&f.pool)
    .await?;
    Ok(event_id)
}

async fn seed_report_action(
    f: &Fixture,
    event_id: Uuid,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'show_operations','event',$4,'escalate_show_task',10000,
                   'auto_execute','T+7 report due','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(event_id)
    .bind(now)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'show_operations','show.task.escalate','event',$4,$5,$6,
                   'queued',$7,'system:test',$7)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id)
    .bind(format!("action:show:{event_id}:PostShowReport:escalate"))
    .bind(json!({
        "kind": "escalate_show_task",
        "event_id": event_id,
        "task": "post_show_report"
    }))
    .bind(now)
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn post_show_report_escalation_emits_labelled_artifact_and_closes_task()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    advertise_show_escalation(&f.pool, f.workspace_id, now).await?;
    let event_id = seed_show(&f, now).await?;
    let action_id = seed_report_action(&f, event_id, now).await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("report escalation must be claimable under show.escalation");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    // Exactly one report event, carrying the labelled numbers.
    let (event_type, payload): (String, Value) = sqlx::query_as(
        "SELECT event_type, payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.show.post_show_report_due'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(event_type, "crowdrelay.show.post_show_report_due");

    let observed = &payload["report"]["observed"];
    assert_eq!(observed["room_checkins_total"], 2);
    assert_eq!(observed["room_checkins_by_session"], 1);
    assert_eq!(observed["room_checkins_by_email_claim"], 1);
    // Only the fan created at show time counts — not the long-known one.
    assert_eq!(observed["new_fan_records_at_show"], 1);
    assert_eq!(observed["admission_passes_redeemed"], 0);

    let inferred = &payload["report"]["inferred"];
    assert_eq!(inferred["paid_ticket_buyers"], 1);
    assert_eq!(inferred["ticket_link_clicks"], 1);
    assert_eq!(
        inferred["ticket_link_clicks_by_act"][0]["act_slug"],
        "virya"
    );

    // The recap the system sent is reported with its receipt, not implied.
    assert_eq!(
        payload["report"]["campaigns"][0]["slug"],
        "viryaos-krakow-live-2026-post-show-recap"
    );
    assert_eq!(payload["report"]["campaigns"][0]["delivered"], 2);

    // Recipients: the active band member and the recorded counterparty.
    assert_eq!(
        payload["recipients"]["band"][0]["email"],
        "band@example.test"
    );
    assert_eq!(
        payload["recipients"]["counterparty"]["email"],
        "promoter@example.test"
    );
    assert!(
        payload["report"]["evidence_gaps"]
            .as_array()
            .expect("gaps array")
            .is_empty()
    );

    // The task is done — re-evaluation holds instead of re-sending.
    let status: String = sqlx::query_scalar(
        "SELECT status FROM show_checklist_items
         WHERE workspace_id = $1 AND event_id = $2 AND item_key = 'post_show_report'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(status, "done");

    // And the show is registered as harvestable material.
    let source_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM viryaos_content_sources
         WHERE workspace_id = $1 AND source_kind = 'show_completed'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(source_count, 1);

    // A show with no counterparty and no room evidence states its gaps.
    let bare_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, timezone, starts_at, status, published_at)
         VALUES ($1, $2, 'quiet-show', 'Quiet', 'UTC', $3, 'published', $4)",
    )
    .bind(bare_event)
    .bind(f.workspace_id.into_uuid())
    .bind(now - time::Duration::days(8))
    .bind(now - time::Duration::days(20))
    .execute(&f.pool)
    .await?;
    let bare_action = seed_report_action(&f, bare_event, now).await?;
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == bare_action)
        .expect("second report action claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.show.post_show_report_due'
           AND payload->'event'->>'slug' = 'quiet-show'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    let gaps: Vec<&str> = payload["report"]["evidence_gaps"]
        .as_array()
        .expect("gaps array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(gaps.contains(&"room_attendance_unverified"));
    assert!(gaps.contains(&"no_counterparty_on_record"));
    assert!(gaps.contains(&"no_event_campaigns_on_record"));
    assert!(payload["recipients"]["counterparty"].is_null());

    f.pool.close().await;
    Ok(())
}

/// A terminally failed escalation must still advance `last_escalated_at`:
/// the idempotency epoch derives from it, so a snapshot that ignored dead
/// actions would regenerate the same action key forever and the one report
/// that exists could never retry.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failed_report_escalation_advances_the_retry_epoch()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::AutopilotDecisionRepository;
    use crowdrelay_domain::show_operations::ShowTaskKind;

    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, timezone, starts_at, status, published_at)
         VALUES ($1, $2, 'failed-report', 'Failed', 'UTC', $3, 'published', $4)",
    )
    .bind(event_id)
    .bind(f.workspace_id.into_uuid())
    .bind(now - time::Duration::days(8))
    .bind(now - time::Duration::days(20))
    .execute(&f.pool)
    .await?;

    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'show_operations','event',$4,'escalate_show_task',10000,
                   'auto_execute','T+7 report due','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(event_id)
    .bind(now)
    .execute(&f.pool)
    .await?;
    let failed_at = now - time::Duration::hours(2);
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at,
            last_error_kind, approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'show_operations','show.task.escalate','event',$4,$5,$6,
                   'failed',$7,'test_failure',$8,'system:test',$8)"#,
    )
    .bind(Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id)
    .bind(format!("action:show:{event_id}:PostShowReport:escalate:0"))
    .bind(json!({
        "kind": "escalate_show_task",
        "event_id": event_id,
        "task": "post_show_report"
    }))
    .bind(failed_at)
    .bind(now)
    .execute(&f.pool)
    .await?;

    let snapshots = f
        .repository
        .load_show_task_snapshots(f.workspace_id, now)
        .await?;
    let report = snapshots
        .iter()
        .find(|s| s.event_id.into_uuid() == event_id && s.task == ShowTaskKind::PostShowReport)
        .expect("report task must be in the snapshot while the show is in window");
    assert_eq!(
        report.last_escalated_at.map(|at| at.unix_timestamp()),
        Some(failed_at.unix_timestamp()),
        "a dead escalation must move the epoch so the retry gets a fresh key"
    );

    f.pool.close().await;
    Ok(())
}
