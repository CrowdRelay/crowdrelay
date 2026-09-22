//! The roster weekly brief's delivery, against a real schema.
//!
//! The measured half (`roster_weekly_brief_postgres.rs`) proves the page
//! reads right; this suite proves the page reaches somebody: one artifact
//! per organisation per ISO week, assignments to owner/admin members of the
//! measured workspaces, the team-email rail deduplicated per person across
//! workspaces, and every skip (not an org, not Monday, already issued, no
//! mail path) staying a quiet zero, and a mid-week catch-up landing under the same Monday key.
//!
//! The seams worth a database are the same ones the daily briefing's suite
//! earns: the org boundary is a join, the capability gate is a live
//! executor read, the dedupe is a partial unique index — none of it is
//! checked at compile time.

use crate::common;

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::json;
use sqlx::PgPool;
use time::{OffsetDateTime, macros::datetime};
use uuid::Uuid;

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    slug: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// A member row with an explicit role — the roster brief is management
/// information, so who receives it is the thing under test.
async fn member(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    role: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
         VALUES ($1, $2, $2, $3, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .bind(role)
    .fetch_one(pool)
    .await?)
}

/// A live `team.email` executor — the same heartbeat+capability pair the
/// daily briefing's suite seeds.
async fn team_email_executor(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let executor = format!("n8n-roster-{workspace_id}");
    sqlx::query(
        r#"INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,$2,'test','test-manifest',$3,$4)"#,
    )
    .bind(workspace_id)
    .bind(&executor)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,$2,'team.email','1',$3,$4)"#,
    )
    .bind(workspace_id)
    .bind(&executor)
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

/// An approval still waiting on a human — the brief's pending section
/// needs at least one real queue row to name. `expires_at` is bound
/// explicitly because the sweep runs against a fixed Monday: a window
/// written against wall-clock `now()` can lapse before the pretend `now`
/// the sweep is asked about.
async fn pending_action(
    pool: &PgPool,
    workspace_id: Uuid,
    expires_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','content_suggestion',$4,
                  'outreach.target.request',7000,'require_approval',
                  'test decision','{}','{}','{}',now(),$1)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(subject_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, available_at, approval_expires_at
        ) VALUES ($1,$2,$3,'booking_opportunity','outreach.target.request',
                  'content_suggestion',$4,$5,$6,'awaiting_approval',now(),$7)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(subject_id)
    .bind(format!("action-{}", Uuid::now_v7()))
    .bind(json!({"kind": "outreach.target.request"}))
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn brief_rows(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<(Uuid, time::Date, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT id, local_date, body FROM roster_briefs
         WHERE organization_id = $1 ORDER BY local_date",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?)
}

async fn roster_assignments(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<(Uuid, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT assignee_member_id, status FROM team_assignments
         WHERE workspace_id = $1 AND source_kind = 'roster_weekly_brief'
         ORDER BY assignee_member_id",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?)
}

async fn roster_emails(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT payload->>'recipient_email' FROM autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'team.assignment.email'
           AND context = 'roster'
         ORDER BY id",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?)
}

/// One brief per organisation per week, to the people who run the roster:
/// owner/admin members of the measured workspaces get an assignment each,
/// a person who is admin of two workspaces gets one email, and `staff`
/// never sees a sibling act's queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_brief_issues_once_per_org_per_week() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_issue(&database).await
}

fn repository(pool: &PgPool) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: String::new(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn run_issue(database: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let pool = database;
    let repository = repository(database);

    let label = organization(pool, "roster-label").await?;
    // Names fix member order: "alpha-act" precedes "beta-act", so alpha's
    // worker is the deterministic first sender for shared recipients.
    let alpha = workspace(pool, "alpha-act", Some(label)).await?;
    let beta = workspace(pool, "beta-act", Some(label)).await?;
    let outsider = workspace(pool, "outsider", None).await?;

    // alpha: the label manager (owner) and local crew (staff — excluded).
    let alpha_manager = member(pool, alpha, "manager@label.test", "owner").await?;
    member(pool, alpha, "crew@alpha.test", "staff").await?;
    // beta: the same manager (admin here) plus beta's own admin.
    let beta_manager = member(pool, beta, "manager@label.test", "admin").await?;
    let beta_admin = member(pool, beta, "admin@beta.test", "admin").await?;

    team_email_executor(pool, alpha).await?;
    team_email_executor(pool, beta).await?;
    pending_action(pool, alpha, datetime!(2026-10-08 00:00 UTC)).await?;

    // 2026-10-05 is a Monday; the workspace has no crew_timezone, so the
    // shipped default UTC decides the morning.
    let sunday = datetime!(2026-10-04 10:00 UTC);
    assert_eq!(
        repository
            .issue_roster_weekly_briefs(WorkspaceId::from_uuid(alpha), sunday)
            .await?,
        0,
        "Sunday is not the brief's day"
    );

    let too_early = datetime!(2026-10-05 07:00 UTC);
    assert_eq!(
        repository
            .issue_roster_weekly_briefs(WorkspaceId::from_uuid(alpha), too_early)
            .await?,
        0,
        "Monday before the brief's morning hour is still too early"
    );

    // The Wednesday catch-up issues under the same Monday key — a worker
    // that missed Monday still delivers the week it belongs to.
    let monday = datetime!(2026-10-05 09:00 UTC);
    let wednesday = datetime!(2026-10-07 11:00 UTC);
    let issued = repository
        .issue_roster_weekly_briefs(WorkspaceId::from_uuid(alpha), wednesday)
        .await?;
    assert_eq!(
        issued, 3,
        "alpha's owner, beta's two admins — staff gets nothing"
    );

    let briefs = brief_rows(pool, label).await?;
    assert_eq!(briefs.len(), 1);
    let (_, local_date, body) = &briefs[0];
    assert_eq!(*local_date, monday.date());
    assert!(
        body.contains("alpha-act") && body.contains("beta-act"),
        "every measured act is on the page: {body}"
    );
    assert!(
        body.contains("waiting on a decision"),
        "alpha's pending ask is named: {body}"
    );
    assert!(
        !body.contains("outsider"),
        "a non-member workspace must not leak into the page: {body}"
    );

    // Assignments: alpha's owner only (staff excluded), beta's two admins.
    let alpha_assignments = roster_assignments(pool, alpha).await?;
    assert_eq!(
        alpha_assignments,
        vec![(alpha_manager, "open".to_owned())],
        "staff is not roster management"
    );
    let mut beta_holders: Vec<Uuid> = roster_assignments(pool, beta)
        .await?
        .into_iter()
        .map(|(member_id, _)| member_id)
        .collect();
    beta_holders.sort();
    let mut expected_beta = vec![beta_manager, beta_admin];
    expected_beta.sort();
    assert_eq!(beta_holders, expected_beta);

    // Emails: the manager is emailed once — via alpha, the first
    // email-capable workspace in member order — while the assignment in
    // beta still lands. beta's own admin is emailed via beta.
    let alpha_emails = roster_emails(pool, alpha).await?;
    assert_eq!(alpha_emails, vec!["manager@label.test".to_owned()]);
    let beta_emails = roster_emails(pool, beta).await?;
    assert_eq!(beta_emails, vec!["admin@beta.test".to_owned()]);

    // The same Monday from the other member's worker: the week is spoken
    // for — no second artifact, no second round of handoffs.
    assert_eq!(
        repository
            .issue_roster_weekly_briefs(WorkspaceId::from_uuid(beta), monday)
            .await?,
        0
    );
    assert_eq!(brief_rows(pool, label).await?.len(), 1);
    assert_eq!(roster_assignments(pool, beta).await?.len(), 2);
    assert_eq!(roster_emails(pool, beta).await?.len(), 1);

    // And the outsider's worker has no roster at all.
    assert_eq!(
        repository
            .issue_roster_weekly_briefs(WorkspaceId::from_uuid(outsider), monday)
            .await?,
        0
    );
    Ok(())
}

/// A member workspace without a live `team.email` capability cannot be
/// told, so its people get no assignment — the same reasoning the daily
/// briefing applies, one workspace at a time. The rest of the org is
/// still briefed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_workspace_without_email_gets_no_assignments() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_no_email(&database).await
}

async fn run_no_email(database: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let pool = database;
    let repository = repository(database);

    let label = organization(pool, "roster-label").await?;
    let alpha = workspace(pool, "alpha-act", Some(label)).await?;
    let gamma = workspace(pool, "gamma-act", Some(label)).await?;
    member(pool, alpha, "manager@label.test", "owner").await?;
    member(pool, gamma, "owner@gamma.test", "owner").await?;
    // Only alpha can send.
    team_email_executor(pool, alpha).await?;

    let monday = datetime!(2026-10-05 09:00 UTC);
    let issued = repository
        .issue_roster_weekly_briefs(WorkspaceId::from_uuid(alpha), monday)
        .await?;
    assert_eq!(issued, 1, "only alpha's owner can be told");
    assert_eq!(brief_rows(pool, label).await?.len(), 1);
    assert_eq!(roster_assignments(pool, alpha).await?.len(), 1);
    assert_eq!(
        roster_assignments(pool, gamma).await?.len(),
        0,
        "no mail path means no unread task — the daily briefing's own rule"
    );
    assert_eq!(roster_emails(pool, alpha).await?.len(), 1);
    Ok(())
}

/// An organisation where no member workspace can email gets no artifact
/// at all — a brief written for nobody is not written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_org_with_no_email_path_issues_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_no_path(&database).await
}

async fn run_no_path(database: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let pool = database;
    let repository = repository(database);

    let label = organization(pool, "roster-label").await?;
    let alpha = workspace(pool, "alpha-act", Some(label)).await?;
    member(pool, alpha, "manager@label.test", "owner").await?;
    // No executor anywhere.

    let monday = datetime!(2026-10-05 09:00 UTC);
    assert_eq!(
        repository
            .issue_roster_weekly_briefs(WorkspaceId::from_uuid(alpha), monday)
            .await?,
        0
    );
    assert!(
        brief_rows(pool, label).await?.is_empty(),
        "no delivery path, no artifact"
    );
    Ok(())
}
