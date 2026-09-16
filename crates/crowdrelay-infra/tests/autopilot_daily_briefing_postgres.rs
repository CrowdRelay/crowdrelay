use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::{OffsetDateTime, macros::datetime};
use uuid::Uuid;

async fn repository()
-> Result<(PostgresAutopilotRepository, sqlx::PgPool), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    Ok((
        PostgresAutopilotRepository::new(pool.clone(), &database),
        pool,
    ))
}

async fn seed_workspace(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "daily-briefing-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Daily Briefing Test")
        .execute(pool)
        .await?;
    Ok(())
}

/// A member with a team profile (the routing roster can see them).
async fn seed_member(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    member_key: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let member_id = seed_member_row(pool, workspace_id, member_key).await?;
    sqlx::query(
        "INSERT INTO viryaos_team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, $3, true, ARRAY['video']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .bind(member_key)
    .execute(pool)
    .await?;
    Ok(member_id)
}

/// The member row only — no team profile, so the routing roster never
/// sees them. The briefing must still reach them.
async fn seed_member_row(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    member_key: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
         VALUES ($1, $2, $3, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "{member_key}-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .bind(format!("Crew {member_key}"))
    .fetch_one(pool)
    .await?)
}

async fn seed_team_email_executor(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        r#"INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-briefing-test','test','test-manifest',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO viryaos_executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-briefing-test','team.email','1',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

async fn briefing_rows(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(Uuid, time::Date, String, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT id, local_date, title, body FROM viryaos_daily_briefings
         WHERE workspace_id = $1 ORDER BY local_date",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

async fn briefing_assignments(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(Uuid, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT assignee_member_id, status FROM viryaos_team_assignments
         WHERE workspace_id = $1 AND source_kind = 'daily_briefing'
         ORDER BY assignee_member_id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

async fn briefing_emails(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'team.assignment.email'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// One briefing per tenant-local day, to every active member — including
/// the member the routing roster cannot see. This workspace deliberately
/// has NO team profiles at all, so `load_team_routing` returns empty and
/// the sweep's early return would skip a briefing that waited for the
/// roster — the ordering this test exists to pin. Repeated sweeps the
/// same day issue nothing more.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_briefing_issues_once_a_day_to_every_active_member()
-> Result<(), Box<dyn std::error::Error>> {
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let routed = seed_member_row(&pool, workspace_id, "routed").await?;
    // No team profile: invisible to the routing roster, still a reader.
    let unprofiled = seed_member_row(&pool, workspace_id, "unprofiled").await?;
    seed_team_email_executor(&pool, workspace_id).await?;

    let morning = datetime!(2026-10-05 10:00 UTC);
    let issued = repository
        .reconcile_team_handoffs(workspace_id, morning)
        .await?;
    assert_eq!(issued, 2);

    let briefings = briefing_rows(&pool, workspace_id).await?;
    assert_eq!(briefings.len(), 1);
    let (_, local_date, title, body) = &briefings[0];
    assert_eq!(*local_date, morning.date());
    assert!(
        title.contains("2026-10-05"),
        "title carries the day: {title}"
    );
    assert!(
        body.contains("arc") || body.contains("Łuk"),
        "arc section present: {body}"
    );

    let assignments = briefing_assignments(&pool, workspace_id).await?;
    assert_eq!(assignments.len(), 2);
    assert!(assignments.iter().all(|(_, status)| status == "open"));
    let mut holders = assignments
        .iter()
        .map(|(member, _)| *member)
        .collect::<Vec<_>>();
    holders.sort();
    let mut expected = vec![routed, unprofiled];
    expected.sort();
    assert_eq!(holders, expected);
    assert_eq!(briefing_emails(&pool, workspace_id).await?, 2);

    // The same sweep, an hour later: nothing new — the day is spoken for.
    let issued = repository
        .reconcile_team_handoffs(workspace_id, morning + time::Duration::hours(1))
        .await?;
    assert_eq!(issued, 0);
    assert_eq!(briefing_rows(&pool, workspace_id).await?.len(), 1);
    assert_eq!(briefing_assignments(&pool, workspace_id).await?.len(), 2);
    assert_eq!(briefing_emails(&pool, workspace_id).await?, 2);
    Ok(())
}

/// The briefing is a morning cadence, not an any-time one: before the
/// local hour nothing issues, after it the whole day is covered — and a
/// workspace with nothing to report still hears that, once.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_briefing_waits_for_the_local_hour_and_says_so_when_empty()
-> Result<(), Box<dyn std::error::Error>> {
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    seed_member(&pool, workspace_id, "solo").await?;
    seed_team_email_executor(&pool, workspace_id).await?;

    // 06:00 UTC — before the 08:00 gate (test workspaces default to UTC).
    let early = datetime!(2026-10-05 06:00 UTC);
    assert_eq!(
        repository
            .reconcile_team_handoffs(workspace_id, early)
            .await?,
        0
    );
    assert!(briefing_rows(&pool, workspace_id).await?.is_empty());

    // 09:00 the same day — the gate has passed and the day is briefed.
    let morning = datetime!(2026-10-05 09:00 UTC);
    assert_eq!(
        repository
            .reconcile_team_handoffs(workspace_id, morning)
            .await?,
        1
    );

    let briefings = briefing_rows(&pool, workspace_id).await?;
    assert_eq!(briefings.len(), 1);
    let (_, _, _, body) = &briefings[0];
    // An empty workspace still gets the honest answer, once.
    assert!(
        body.contains("arc") || body.contains("Łuk"),
        "the arc line is always present: {body}"
    );
    Ok(())
}

/// A new day brings a new briefing and closes yesterday's — a stale
/// "read me" is noise, and noise is what the briefing exists to replace.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_new_day_supersedes_yesterdays_briefing() -> Result<(), Box<dyn std::error::Error>> {
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    seed_member(&pool, workspace_id, "solo").await?;
    seed_team_email_executor(&pool, workspace_id).await?;

    let day_one = datetime!(2026-10-05 09:00 UTC);
    assert_eq!(
        repository
            .reconcile_team_handoffs(workspace_id, day_one)
            .await?,
        1
    );

    let day_two = datetime!(2026-10-06 09:00 UTC);
    assert_eq!(
        repository
            .reconcile_team_handoffs(workspace_id, day_two)
            .await?,
        1
    );

    let briefings = briefing_rows(&pool, workspace_id).await?;
    assert_eq!(briefings.len(), 2);
    let assignments = briefing_assignments(&pool, workspace_id).await?;
    assert_eq!(assignments.len(), 2);
    let cancelled = assignments.iter().filter(|(_, s)| s == "cancelled").count();
    let open = assignments.iter().filter(|(_, s)| s == "open").count();
    assert_eq!(cancelled, 1, "yesterday's briefing is superseded");
    assert_eq!(open, 1, "today's briefing is the live one");
    assert_eq!(briefing_emails(&pool, workspace_id).await?, 2);
    Ok(())
}
