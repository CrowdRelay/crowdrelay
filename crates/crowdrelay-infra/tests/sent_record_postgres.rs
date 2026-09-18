//! O.4 and O.7 — what left, and what never did.
//!
//! Both reads answer from rows written by other code paths: the emitted
//! payload in `outbox_events`, tied to its action by
//! `viryaos_autopilot_action_emissions`. Nothing read them back until now, so
//! the console could say a send failed and never say who did not hear from the
//! tenant. Only a database can show that the join works.

use std::time::Duration;

use crowdrelay_infra::sent_record::{failed_sends, sent_record};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_sentrec_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn what_left_and_what_never_did() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "act").await?;
    let neighbour = workspace(pool, "other-act").await?;

    // One letter that went out, with its words and its addresses.
    let sent = action(pool, act, "sent", "succeeded", "third_party", None, now).await?;
    emit(
        pool,
        act,
        sent,
        "crowdrelay.gig.outreach_requested",
        serde_json::json!({
            "draft": {"subject": "Virya x Klub X — show proposal", "body": "Hi,\n\nFour comparable acts…"},
            "recipients": [
                {"target_id": Uuid::now_v7(), "contact_email": "anna@example.test"},
                {"target_id": Uuid::now_v7(), "contact_email": "bogdan@example.test"}
            ]
        }),
        now,
    )
    .await?;
    report(pool, act, sent, "succeeded", Some("gmail-message-1"), now).await?;

    // One that failed after emitting — somebody did not hear from the band.
    let failed = action(
        pool,
        act,
        "failed",
        "failed",
        "third_party",
        Some("provider_rejected"),
        now,
    )
    .await?;
    emit(
        pool,
        act,
        failed,
        "crowdrelay.outreach.requested",
        serde_json::json!({"recipient_email": "press@example.test"}),
        now,
    )
    .await?;

    // A first-party failure. The system's own problem, already an alert, and
    // not a send anybody outside was waiting for.
    action(
        pool,
        act,
        "internal",
        "failed",
        "first_party_reversible",
        Some("database_unavailable"),
        now,
    )
    .await?;
    // Another workspace's failure. One process serves one workspace.
    action(
        pool,
        neighbour,
        "theirs",
        "failed",
        "third_party",
        Some("provider_rejected"),
        now,
    )
    .await?;

    // A report that went out — the post-show and release reports name their
    // recipients as an object keyed by audience, not an array. Before the read
    // understood that shape this row answered "sent to nobody" for a send that
    // named the whole band and the promoter across the table.
    let reported = action(
        pool,
        act,
        "reported",
        "succeeded",
        "owned_audience",
        None,
        now,
    )
    .await?;
    emit(
        pool,
        act,
        reported,
        "crowdrelay.show.post_show_report_due",
        serde_json::json!({
            "recipients": {
                "band": [
                    {"email": "wiktor@example.test", "name": "Wiktor"},
                    {"email": "ola@example.test", "name": "Ola"}
                ],
                "counterparty": {"name": "Klub X", "email": "booking@klubx.test"}
            },
            "report": {"kind": "post_show_t7"}
        }),
        now,
    )
    .await?;

    let record = sent_record(pool, act, sent).await?.ok_or("no record")?;
    assert_eq!(
        record.subject.as_deref(),
        Some("Virya x Klub X — show proposal")
    );
    assert!(
        record
            .body
            .as_deref()
            .unwrap_or_default()
            .starts_with("Hi,"),
        "the words that left are not readable: {:?}",
        record.body
    );
    assert_eq!(
        record.recipients,
        vec![
            "anna@example.test".to_owned(),
            "bogdan@example.test".to_owned()
        ],
        "the addresses the letter went to are not readable"
    );
    assert_eq!(record.executor_status.as_deref(), Some("succeeded"));
    assert_eq!(
        record.provider_reference.as_deref(),
        Some("gmail-message-1")
    );

    let report_record = sent_record(pool, act, reported)
        .await?
        .ok_or("no record for the report")?;
    assert_eq!(
        report_record.recipients,
        vec![
            "wiktor@example.test".to_owned(),
            "ola@example.test".to_owned(),
            "booking@klubx.test".to_owned(),
        ],
        "the report's audience-keyed recipients read back as sent to nobody"
    );

    let failures = failed_sends(pool, act, now).await?;
    assert_eq!(
        failures.total,
        1,
        "expected one outward failure, got {:?}",
        failures
            .items
            .iter()
            .map(|item| (item.action_kind.clone(), item.error_kind.clone()))
            .collect::<Vec<_>>()
    );
    let item = &failures.items[0];
    assert_eq!(item.error_kind.as_deref(), Some("provider_rejected"));
    // The point of naming them: this is the address that never heard anything.
    assert_eq!(item.recipients, vec!["press@example.test".to_owned()]);
    assert_eq!(failures.window_days, 7);
    Ok(())
}

async fn workspace(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("{slug}-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

/// One decision and the action it produced, in the end state the case needs.
async fn action(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
    status: &str,
    action_class: &str,
    error_kind: Option<&str>,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1, $2, $3, 'booking_opportunity', 'city', $4,
                  'gig.proposal', 7000, 'require_approval', 'a reason',
                  '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, $5, $6)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{key}-{}", workspace_id.simple()))
    .bind(Uuid::now_v7())
    .bind(now - time::Duration::days(1))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            finished_at, last_error_kind, created_at
        ) VALUES ($1, $2, $3, 'booking_opportunity', 'gig.outreach.request', 'city',
                  $4, $5, '{}'::jsonb, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("action-{key}-{}", workspace_id.simple()))
    .bind(status)
    .bind(action_class)
    .bind(now - time::Duration::hours(2))
    .bind(error_kind)
    .bind(now - time::Duration::days(1))
    .execute(pool)
    .await?;
    Ok(action_id)
}

/// The emission the dispatch arm would have written.
async fn emit(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let outbox_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, available_at)
        VALUES ($1, $2, 1, $3, $4)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(now)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_action_emissions
            (workspace_id, action_id, emission_key, outbox_event_id, emitted_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(format!("emission-{action_id}"))
    .bind(outbox_id)
    .bind(now - time::Duration::hours(2))
    .execute(pool)
    .await?;
    Ok(())
}

/// The executor's own report — provider-confirmed evidence, separate from the
/// action's status by design.
async fn report(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    status: &str,
    provider_reference: Option<&str>,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_execution_reports
            (workspace_id, action_id, receipt_key, executor_id, status,
             provider_reference, occurred_at)
        VALUES ($1, $2, $3, 'n8n-test', $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(format!("receipt-{action_id}"))
    .bind(status)
    .bind(provider_reference)
    .bind(now - time::Duration::hours(1))
    .execute(pool)
    .await?;
    Ok(())
}
