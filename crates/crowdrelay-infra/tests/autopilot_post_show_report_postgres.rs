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
        INSERT INTO executor_instances (
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
        INSERT INTO executor_capabilities (
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
         VALUES ($1, $2, 'crowdrelay-krakow-live-2026-post-show-recap', 'recap', '{}', true)",
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
               $1, $2, 'crowdrelay-krakow-live-2026-post-show-recap', 'recap', 'email',
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

/// The catalogue id for a city slug the fixture already created.
async fn city_id_for(pool: &sqlx::PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = $1 ORDER BY id LIMIT 1")
            .bind(slug)
            .fetch_one(pool)
            .await?,
    )
}

async fn seed_report_action(
    f: &Fixture,
    event_id: Uuid,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
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
        r#"INSERT INTO autopilot_actions
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
        "crowdrelay-krakow-live-2026-post-show-recap"
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

    // §4f-1: the room is a counterparty — the artifact carries the venue's
    // own registry numbers. 'Klub Testowy' was marked by the trigger on the
    // published event, so it resolves: this show among the room's record,
    // this workspace among its contributors, and a draw only over shows
    // that were actually ticketed. The database is shared across runs, so
    // the pin is the shape — our show counted — not a count that other
    // runs legitimately moved.
    let venue = &payload["report"]["venue"];
    assert_eq!(venue["on_record"], true);
    assert_eq!(venue["name"], "Klub Testowy");
    assert!(venue["shows_on_record"].as_i64().expect("count") >= 1);
    assert!(venue["contributors"].as_i64().expect("count") >= 1);
    assert!(venue["repeat_attenders"].as_i64().expect("count") >= 0);

    // An ordinary show's counterparty is a promoter — the kind travels with
    // the recipient and the report, and no festival name is claimed.
    assert_eq!(payload["report"]["counterparty_kind"], "promoter");
    assert_eq!(payload["recipients"]["counterparty"]["kind"], "promoter");
    assert!(payload["event"]["festival_name"].is_null());

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
        "SELECT count(*)::bigint FROM content_sources
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

    // A festival slot reports to its organiser: the counterparty kind is
    // the festival's, the event carries the festival's own name, and the
    // stage's own numbers ride along — the trigger already marked it, so
    // the festival's stage reads on_record with one show, while an
    // unticketed night keeps its draw NULL rather than quoting a zero the
    // room never earned.
    let festival_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, venue, timezone,
                             starts_at, status, published_at,
                             counterparty_name, counterparty_email, festival_name)
         VALUES ($1, $2, $3, 'off-festival-slot', 'OFF Festival slot', 'Festival Grounds',
                 'Europe/Warsaw', $4, 'published', $5,
                 'OFF Organiser', 'organiser@example.test', 'OFF Festival')",
    )
    .bind(festival_event)
    .bind(f.workspace_id.into_uuid())
    .bind(city_id_for(&f.pool, "krakow").await?)
    .bind(now - time::Duration::days(8))
    .bind(now - time::Duration::days(20))
    .execute(&f.pool)
    .await?;
    let festival_action = seed_report_action(&f, festival_event, now).await?;
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == festival_action)
        .expect("festival report action claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.show.post_show_report_due'
           AND payload->'event'->>'slug' = 'off-festival-slot'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(payload["report"]["counterparty_kind"], "festival");
    assert_eq!(payload["event"]["festival_name"], "OFF Festival");
    assert_eq!(
        payload["recipients"]["counterparty"]["kind"], "festival",
        "the organiser reads the festival's own kind of artifact"
    );
    let venue = &payload["report"]["venue"];
    assert_eq!(venue["on_record"], true);
    assert_eq!(venue["name"], "Festival Grounds");
    assert!(venue["shows_on_record"].as_i64().expect("count") >= 1);
    assert!(
        venue["typical_draw_paid_orders"].is_null(),
        "an unticketed festival slot must not read as a zero-draw room"
    );

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
        r#"INSERT INTO autopilot_decisions
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
        r#"INSERT INTO autopilot_actions
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

async fn advertise_show_growth(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-growth-test','test','test-manifest',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-growth-test','show.growth','1',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

/// §6.4 — post-festival follow-up, performer side. The T+1 recap for the
/// room names the festival and lists every act on the slot's bill — "acts
/// you saw" is the whole bill, not just the tenant — and the T+7 report
/// labels the room's numbers the same way it does for any show: a festival
/// date is evidence of record, not a special pipeline.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn festival_post_show_follow_up_labels_acts_and_room()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    advertise_show_growth(&f.pool, f.workspace_id, now).await?;
    advertise_show_escalation(&f.pool, f.workspace_id, now).await?;
    // First-party levers only send when the tenant has campaigns on.
    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled)
         VALUES ($1, 'communication_campaigns_enabled', true)",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;

    let starts_at = now - time::Duration::days(8);
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) VALUES ($1, 'wroclaw', 'Wrocław', 'PL')
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(Uuid::now_v7())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, venue, timezone,
                             starts_at, status, published_at,
                             counterparty_name, counterparty_email, festival_name)
         VALUES ($1, $2, $3, 'off-fest-2026', 'Virya at OFF Festival', 'Main Stage',
                 'Europe/Warsaw', $4, 'published', $5,
                 'OFF Organiser', 'organiser@example.test', 'OFF Festival')",
    )
    .bind(event_id)
    .bind(f.workspace_id.into_uuid())
    .bind(city_id_for(&f.pool, "wroclaw").await?)
    .bind(starts_at)
    .bind(starts_at - time::Duration::days(20))
    .execute(&f.pool)
    .await?;
    // The festival bill: the tenant plus two bill-mates, in play order.
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position)
         VALUES ($1, $2, 'virya', 'Virya', 0),
                ($1, $2, 'scene-local', 'Scene Local', 1),
                ($1, $2, 'quiet-riot', 'Quiet Riot Tribute', 2)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .execute(&f.pool)
    .await?;

    // Room evidence: a long-known fan on a session scan and a stranger who
    // claimed an email identity at the door — the festival's new-follows
    // number is the second of these.
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
         VALUES ($1, $2, 'festival-known@example.test', 'active', $3)",
    )
    .bind(known_fan)
    .bind(f.workspace_id.into_uuid())
    .bind(starts_at - time::Duration::days(60))
    .execute(&f.pool)
    .await?;
    let new_fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1, $2, 'festival-stranger@example.test', 'active', $3)",
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

    // The T+1 recap lever, seeded the way the evaluator's persist path writes
    // it — a queued owned-audience action on the show.growth capability.
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'show_operations','event',$4,'request_show_growth',10000,
                   'auto_execute','T+1 recap due','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(event_id)
    .bind(now)
    .execute(&f.pool)
    .await?;
    let recap_action = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'show_operations','show.growth.request','event',$4,$5,$6,
                   'queued','owned_audience',$7,'system:test',$7)"#,
    )
    .bind(recap_action)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id)
    .bind(format!("action:show:{event_id}:PostShowRecap"))
    .bind(json!({
        "kind": "request_show_growth",
        "event_id": event_id,
        "lever": "post_show_recap",
        "template_key": "post_show_recap"
    }))
    .bind(now)
    .execute(&f.pool)
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == recap_action)
        .expect("festival recap action claimable under show.growth");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    // The recap the room gets: the festival is the memory hook, and "acts you
    // saw" is every act on the slot's bill, in play order.
    let content: Value = sqlx::query_scalar(
        "SELECT content FROM communication_campaigns
         WHERE workspace_id = $1 AND slug = 'crowdrelay-off-fest-2026-post-show-recap'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(content["festival_name"], "OFF Festival");
    assert_eq!(content["venue"], "Main Stage");
    let acts: Vec<&str> = content["acts"]
        .as_array()
        .expect("acts array")
        .iter()
        .filter_map(|act| act["slug"].as_str())
        .collect();
    assert_eq!(acts, ["virya", "scene-local", "quiet-riot"]);
    let rules: Vec<&str> = content["email_contract"]["rules"]
        .as_array()
        .expect("rules array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(rules.contains(&"name_only_acts_on_the_announced_bill"));

    // The T+7 report on the same festival date labels the room's numbers —
    // the band-side "new follows from this room" is `new_fan_records_at_show`.
    let report_action = seed_report_action(&f, event_id, now).await?;
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == report_action)
        .expect("festival report action claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.show.post_show_report_due'
           AND payload->'event'->>'slug' = 'off-fest-2026'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(payload["event"]["festival_name"], "OFF Festival");
    assert_eq!(payload["report"]["observed"]["room_checkins_total"], 2);
    assert_eq!(payload["report"]["observed"]["new_fan_records_at_show"], 1);
    let report_acts: Vec<&str> = payload["event"]["acts"]
        .as_array()
        .expect("event acts array")
        .iter()
        .filter_map(|act| act["slug"].as_str())
        .collect();
    assert_eq!(report_acts, ["virya", "scene-local", "quiet-riot"]);

    f.pool.close().await;
    Ok(())
}

/// The announce beat exists only where a live campaign does — no mint, no
/// task — and its proof is first-party: the flag, or the first scan landing.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_qr_announce_beat_follows_the_campaign_not_the_calendar()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::AutopilotDecisionRepository;
    use crowdrelay_domain::show_operations::ShowTaskKind;

    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,'QR beat show',$4,'published',$4 - interval '7 days') RETURNING id",
    )
    .bind(event_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("qr-beat-{}", f.workspace_id.into_uuid().simple()))
    .bind(now + time::Duration::days(1))
    .execute(&f.pool)
    .await?;

    let snapshots = f
        .repository
        .load_show_task_snapshots(f.workspace_id, now)
        .await?;
    assert!(
        !snapshots
            .iter()
            .any(|s| s.event_id.into_uuid() == event_id && s.task == ShowTaskKind::QrFromStage),
        "no campaign minted yet — there is nothing to announce"
    );

    let campaign_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concert_qr_campaigns (id, workspace_id, event_id, label, valid_from, valid_until)
         VALUES ($1,$2,$3,'Door',$4,$5)",
    )
    .bind(campaign_id)
    .bind(f.workspace_id.into_uuid())
    .bind(event_id)
    .bind(now - time::Duration::hours(4))
    .bind(now + time::Duration::days(2))
    .execute(&f.pool)
    .await?;

    let snapshots = f
        .repository
        .load_show_task_snapshots(f.workspace_id, now)
        .await?;
    let beat = snapshots
        .iter()
        .find(|s| s.event_id.into_uuid() == event_id && s.task == ShowTaskKind::QrFromStage)
        .expect("a live campaign makes the beat appear");
    assert!(!beat.verifiable_fact, "minted is not announced");
    assert!(!beat.already_done);

    sqlx::query("UPDATE concert_qr_campaigns SET announced_from_stage = true WHERE id = $1")
        .bind(campaign_id)
        .execute(&f.pool)
        .await?;
    let snapshots = f
        .repository
        .load_show_task_snapshots(f.workspace_id, now)
        .await?;
    let beat = snapshots
        .iter()
        .find(|s| s.event_id.into_uuid() == event_id && s.task == ShowTaskKind::QrFromStage)
        .expect("the task stays in the snapshot");
    assert!(
        beat.verifiable_fact,
        "the stage flag is first-party proof the beat happened"
    );

    // A revoked campaign with no flag and no scans is a dead QR — the beat
    // must not complete against it. `now()` is the database clock: the
    // CHECK requires revoked_at >= created_at and the test process clock
    // can lag the server's by a tick.
    sqlx::query(
        "UPDATE concert_qr_campaigns
         SET announced_from_stage = false, active = false, revoked_at = now()
         WHERE id = $1",
    )
    .bind(campaign_id)
    .execute(&f.pool)
    .await?;
    let snapshots = f
        .repository
        .load_show_task_snapshots(f.workspace_id, now)
        .await?;
    assert!(
        !snapshots
            .iter()
            .any(|s| s.event_id.into_uuid() == event_id && s.task == ShowTaskKind::QrFromStage),
        "a revoked campaign retires the beat with it"
    );

    f.pool.close().await;
    Ok(())
}
