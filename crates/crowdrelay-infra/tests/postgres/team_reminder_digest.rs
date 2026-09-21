//! One crew member, one email per sweep.
//!
//! Reported from a real inbox: four emails at 09:31, all subjected "VIRYA —
//! przypomnienie: Zatwierdź artefakt treści", all with the same opening
//! sentence. They were four different approvals — the subject is composed from
//! the action kind, so tasks of one kind are indistinguishable — and a person
//! reading that cannot tell four tasks from one task sent four times. Either
//! reading teaches them to stop opening VIRYA mail, which costs more than every
//! one of those tasks was worth.
//!
//! The property is the sweep's, not a formatter's: it comes from grouping rows
//! inside one transaction, so only a database can show it.

use crate::common;

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn four_due_tasks_are_one_email() -> Result<(), Box<dyn std::error::Error>> {
    let (database, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database, &url).await
}

async fn run(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let workspace = workspace(pool).await?;
    advertise_team_email(pool, workspace, now).await?;

    // Crew mail keeps the tenant's night quiet (UTC when `crew_timezone` is
    // unset) — a dispatch instant inside the quiet window is the next test's
    // business. Twelve hours forward from any quiet hour lands mid-morning.
    let dispatch_now = if (8..21).contains(&now.hour()) {
        now
    } else {
        now + time::Duration::hours(12)
    };

    let alice = member(pool, workspace, "alice").await?;
    let bogdan = member(pool, workspace, "bogdan").await?;

    // Alice's four, all due and all of a kind — the reported inbox.
    for index in 0..4 {
        assignment(pool, workspace, alice, now, index).await?;
    }
    // Bogdan's one. A second person still gets their own email; grouping is per
    // recipient, not per sweep.
    assignment(pool, workspace, bogdan, now, 9).await?;

    let database = DatabaseConfig {
        url: url.to_owned(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let queued = repository
        .dispatch_team_handoff_reminders(WorkspaceId::from_uuid(workspace), dispatch_now)
        .await?;
    assert_eq!(queued, 2, "one email each for two people, not five");

    let emails = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT action_kind, payload FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'team.assignment.email'",
    )
    .bind(workspace)
    .fetch_all(pool)
    .await?;
    assert_eq!(emails.len(), 2, "one action per person");

    let alices = emails
        .iter()
        .find(|(_, payload)| {
            payload["recipient_email"]
                .as_str()
                .is_some_and(|email| email.starts_with("alice"))
        })
        .ok_or("alice got no email at all")?;
    let detail = alices.1["task_detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("jeszcze 3 zadania") || detail.contains("3 more tasks"),
        "the digest did not name the other three tasks: {detail}"
    );

    // Every assignment in the digest advances its own bookkeeping — each was
    // named in the body, so none may be reminded about again as if it had been
    // silent.
    let unreminded = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_team_assignments
         WHERE workspace_id = $1 AND reminder_count = 0",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        unreminded, 0,
        "an assignment named in the digest was left unmarked and will be chased again"
    );

    // And the ladder is finite: a second sweep at the same instant sends
    // nothing, because every rung of every assignment is still in the future.
    let again = repository
        .dispatch_team_handoff_reminders(WorkspaceId::from_uuid(workspace), dispatch_now)
        .await?;
    assert_eq!(again, 0, "the same sweep sent a second round of mail");
    Ok(())
}

/// Crew mail keeps the tenant's night quiet: a reminder due at 03:00 on the
/// tenant's clock does not send at 03:00 — the row stays due, uncounted, and
/// the first waking sweep sends it unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reminder_due_overnight_waits_for_morning() -> Result<(), Box<dyn std::error::Error>> {
    let (database, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_quiet(&database, &url).await
}

async fn run_quiet(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    // The workspace carries no `crew_timezone` row — the shipped default is
    // UTC, so the quiet window is 21:00–08:00 UTC and the test can name its
    // own hours without depending on the wall clock.
    let quiet_now = OffsetDateTime::now_utc()
        .replace_hour(3)
        .expect("03:00 is a valid hour");
    let morning = quiet_now + time::Duration::hours(6);

    let workspace = workspace(pool).await?;
    // The capability heartbeat compares `expires_at` to the database's own
    // clock, not the sweep's `now` — advertise it against the real wall clock
    // so it is still live when the deferred send runs a moment later.
    advertise_team_email(pool, workspace, OffsetDateTime::now_utc()).await?;
    let alice = member(pool, workspace, "night-owl").await?;
    assignment(pool, workspace, alice, quiet_now, 0).await?;

    let database = DatabaseConfig {
        url: url.to_owned(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    let sent_at_night = repository
        .dispatch_team_handoff_reminders(WorkspaceId::from_uuid(workspace), quiet_now)
        .await?;
    assert_eq!(sent_at_night, 0, "a 03:00 reminder mailed anyway");

    // Nothing was consumed: the row is still due and still unreminded, so the
    // morning sweep sees the same assignment the night sweep declined to mail.
    let (still_due, unreminded) = sqlx::query_as::<_, (bool, i32)>(
        "SELECT next_reminder_at IS NOT NULL, reminder_count
         FROM viryaos_team_assignments WHERE workspace_id = $1",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert!(
        still_due,
        "the quiet sweep cleared a reminder it never sent"
    );
    assert_eq!(
        unreminded, 0,
        "the quiet sweep counted a mail it never sent"
    );

    let sent_in_the_morning = repository
        .dispatch_team_handoff_reminders(WorkspaceId::from_uuid(workspace), morning)
        .await?;
    assert_eq!(
        sent_in_the_morning, 1,
        "the deferred reminder never arrived"
    );
    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Crew test')")
        .bind(id)
        .bind(format!("crew-{}", id.simple()))
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'crew_locale', 'pl')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn member(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
         VALUES ($1, $2, $3, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("{key}-{}@example.test", workspace_id.simple()))
    .bind(format!("Crew {key}"))
    .fetch_one(pool)
    .await?)
}

/// Without an executor advertising `team.email` the sweep queues nothing and
/// the test would pass for the wrong reason.
async fn advertise_team_email(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-crew-test','test','test-manifest',$2,$3)"#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-crew-test','team.email','1',$2,$3)"#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

/// One open assignment whose reminder is due now, with room left before the
/// deadline for the ladder to have another rung.
async fn assignment(
    pool: &PgPool,
    workspace_id: Uuid,
    member_id: Uuid,
    now: OffsetDateTime,
    index: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"INSERT INTO viryaos_team_assignments (
            workspace_id, source_kind, source_id, assignee_member_id, required_skill,
            status, due_at, assigned_at, next_reminder_at, reminder_count
        ) VALUES ($1,'opportunity',$2,$3,'operations','open',$4,$5,$6,0)"#,
    )
    .bind(workspace_id)
    .bind(Uuid::now_v7())
    .bind(member_id)
    .bind(now + time::Duration::days(5) + time::Duration::minutes(i64::from(index)))
    .bind(now - time::Duration::days(1))
    .bind(now - time::Duration::minutes(1))
    .execute(pool)
    .await?;
    Ok(())
}
