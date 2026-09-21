//! The retired reminder lane leaves no schedule behind.
//!
//! Reported from a real inbox: a "VIRYA — przypomnienie" mail repeated, one
//! task at a time, what the morning briefing had already aggregated — the same
//! fact twice a day. The lane is retired: nothing writes `next_reminder_at`,
//! and the drain sweep clears whatever was stamped before the retirement so a
//! leftover schedule can never fire a mail.
//!
//! The property is the sweep's, not a formatter's: it comes from the UPDATE
//! inside one transaction, so only a database can show it.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    url: String,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_remindrain_{}", Uuid::now_v7().simple());
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
            url,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
            ..
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
async fn the_drain_clears_schedules_without_mailing() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool, &database.url).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let workspace = workspace(pool).await?;
    let alice = member(pool, workspace, "alice").await?;
    let bogdan = member(pool, workspace, "bogdan").await?;

    // The pre-retirement backlog: schedules already past their fire time and
    // ones still in the future — the drain owes neither an email.
    for index in 0..4 {
        assignment(pool, workspace, alice, now, index).await?;
    }
    assignment(pool, workspace, bogdan, now, 9).await?;
    // A row with no schedule is untouched and uncounted.
    sqlx::query(
        r#"INSERT INTO viryaos_team_assignments (
            workspace_id, source_kind, source_id, assignee_member_id, required_skill,
            status, due_at, assigned_at
        ) VALUES ($1,'opportunity',$2,$3,'operations','open',$4,$5)"#,
    )
    .bind(workspace)
    .bind(Uuid::now_v7())
    .bind(alice)
    .bind(now + time::Duration::days(5))
    .bind(now - time::Duration::days(1))
    .execute(pool)
    .await?;

    let database = DatabaseConfig {
        url: url.to_owned(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let cleared = repository
        .drain_team_reminder_schedule(WorkspaceId::from_uuid(workspace))
        .await?;
    assert_eq!(cleared, 5, "the drain did not clear every stamped schedule");

    let emails = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'team.assignment.email'",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(emails, 0, "the drain mailed somebody");

    let still_scheduled = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_team_assignments
         WHERE workspace_id = $1 AND next_reminder_at IS NOT NULL",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(still_scheduled, 0, "a reminder schedule survived the drain");

    // The lane stays drained: a second sweep has nothing left to clear.
    let again = repository
        .drain_team_reminder_schedule(WorkspaceId::from_uuid(workspace))
        .await?;
    assert_eq!(again, 0, "the drain found new schedules to clear");
    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Crew test')")
        .bind(id)
        .bind(format!("crew-{}", id.simple()))
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

/// One open assignment carrying a stamped reminder schedule — the shape rows
/// written before the lane retired still have in production.
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
