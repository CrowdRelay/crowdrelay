use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
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
            "capture-plan-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Capture Plan Test")
        .execute(pool)
        .await?;
    Ok(())
}

async fn seed_member(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    member_key: &str,
    skills: &[&str],
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let member_id: Uuid = sqlx::query_scalar(
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
    .await?;
    let skills_csv = skills
        .iter()
        .map(|skill| format!("'{skill}'"))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::query(&format!(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, '{member_key}', true, ARRAY[{skills_csv}]::text[])"
    ))
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(pool)
    .await?;
    Ok(member_id)
}

async fn seed_team_email_executor(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        r#"INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-capture-plan-test','test','test-manifest',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-capture-plan-test','team.email','1',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_production_event(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    kind: &str,
    scheduled_for: time::Date,
    status: &str,
    event_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO production_events
             (id, workspace_id, kind, title, scheduled_for, status, event_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(kind)
    .bind(format!("{kind} day"))
    .bind(scheduled_for)
    .bind(status)
    .bind(event_id)
    .fetch_one(pool)
    .await?)
}

async fn seed_plan(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    production_event_id: Uuid,
    status: &str,
    assignee: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO capture_plans
             (id, workspace_id, production_event_id, items, assignee_member_id, status, issued_at)
         VALUES ($1,$2,$3,$4,$5,$6,CASE WHEN $6='issued' THEN now() END) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(production_event_id)
    .bind(json!([{"item": "coverage", "skill": "video"}]))
    .bind(assignee)
    .bind(status)
    .fetch_one(pool)
    .await?)
}

async fn seed_assignment(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    plan_id: Uuid,
    member_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO team_assignments
             (id, workspace_id, source_kind, source_id, assignee_member_id, required_skill)
         VALUES ($1,$2,'capture_plan',$3,$4,'video')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(plan_id)
    .bind(member_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_source(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    source_key: &str,
    occurred_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO content_sources
             (workspace_id, source_kind, source_key, title, occurred_at, expires_at)
         VALUES ($1,'video',$2,$3,$4,$5)",
    )
    .bind(workspace_id.into_uuid())
    .bind(source_key)
    .bind(format!("Material {source_key}"))
    .bind(occurred_at)
    .bind(occurred_at + time::Duration::days(45))
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_show_becomes_a_production_day_and_the_plan_reaches_the_camera_holder()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    seed_team_email_executor(&pool, workspace_id).await?;
    // Two camera-capable members so the six checklist tasks the same show
    // routes cannot starve the plan of an assignee — the competition is
    // the honest load, two holders is the honest roster.
    seed_member(&pool, workspace_id, "crew-a", &["video", "photography"]).await?;
    seed_member(&pool, workspace_id, "crew-b", &["video", "social"]).await?;

    // Tonight's published gig — `date_trunc` keeps the fixture on today's
    // date whatever hour the suite runs at.
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, date_trunc('day', now()) + interval '20 hours', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("show-{}", workspace_id.into_uuid().simple()))
    .bind("Tonight's gig")
    .fetch_one(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    repo.reconcile_team_handoffs(workspace_id, now).await?;

    // The gig projected into a production day.
    let day: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, kind FROM production_events
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&pool)
    .await?;
    let (day_id, kind) = day.expect("a published show projects a production day");
    assert_eq!(kind, "show");

    // The day carries an issued plan with a real shot list and an
    // assignee — the baseline coverage shot exists even with no needs.
    let plan: Option<(Uuid, String, Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, status, assignee_member_id, items FROM capture_plans
         WHERE workspace_id=$1 AND production_event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(day_id)
    .fetch_optional(&pool)
    .await?;
    let (plan_id, plan_status, assignee, items) =
        plan.expect("the sweep issues a plan for the day");
    assert_eq!(plan_status, "issued");
    assert!(assignee.is_some(), "a camera holder was found");
    assert!(
        items.as_array().is_some_and(|list| !list.is_empty()),
        "the plan carries a shot list"
    );

    // The assignment points at the plan and the email is queued.
    let assignment: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT status, id FROM team_assignments
         WHERE workspace_id=$1 AND source_kind='capture_plan' AND source_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(plan_id)
    .fetch_optional(&pool)
    .await?;
    let (assignment_status, assignment_id) = assignment.expect("the plan routed to a member");
    assert_eq!(assignment_status, "open");
    let email_actions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM autopilot_actions
         WHERE workspace_id=$1 AND action_kind='team.assignment.email' AND subject_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(assignment_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(email_actions, 1, "the shot list was emailed once");

    // The checklist's bare item is satisfied by the real plan, not nagged
    // beside it — and no bare capture_plan task was assigned either.
    let checklist: Option<String> = sqlx::query_scalar(
        "SELECT status FROM show_checklist_items
         WHERE workspace_id=$1 AND event_id=$2 AND item_key='capture_plan'",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&pool)
    .await?;
    assert_eq!(checklist.as_deref(), Some("done"));
    let bare_tasks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_assignments
         WHERE workspace_id=$1 AND source_kind='show_task' AND source_ref='capture_plan'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(bare_tasks, 0);

    // A draft waiting for a holder routes the moment the roster can hold
    // it — a member joining mid-week must not lose the day.
    let today = now.date();
    let photo_day =
        seed_production_event(&pool, workspace_id, "photoshoot", today, "scheduled", None).await?;
    let draft_plan = seed_plan(&pool, workspace_id, photo_day, "draft", None).await?;

    // Second sweep: the show's day is planned already — no twin plan, no
    // twin assignment — while the waiting draft gets issued and routed.
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let plans: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM capture_plans
         WHERE workspace_id=$1 AND production_event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(day_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(plans, 1, "the open plan dedups the next sweep");
    let retried: Option<(String, Option<Uuid>)> =
        sqlx::query_as("SELECT status, assignee_member_id FROM capture_plans WHERE id=$1")
            .bind(draft_plan)
            .fetch_optional(&pool)
            .await?;
    let (retried_status, retried_assignee) = retried.expect("the draft plan row exists");
    assert_eq!(retried_status, "issued", "the draft was offered again");
    assert!(retried_assignee.is_some());
    let draft_assignment: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_assignments
         WHERE workspace_id=$1 AND source_kind='capture_plan' AND source_id=$2 AND status='open'",
    )
    .bind(workspace_id.into_uuid())
    .bind(draft_plan)
    .fetch_one(&pool)
    .await?;
    assert_eq!(draft_assignment, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_harvest_counts_sources_and_settles_plans() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    // Settling runs before the roster check, so a bare member for the
    // assignment foreign keys is enough — no profile, no executor.
    let member_id = seed_member(&pool, workspace_id, "lens", &["video"]).await?;
    // No team profile: the member exists for the FK, but the routing
    // roster stays empty and the sweep still settles — the point of
    // running housekeeping before the team check.
    sqlx::query("DELETE FROM team_profiles WHERE workspace_id=$1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;

    let today = OffsetDateTime::now_utc().date();

    // A — the day yielded the target: done.
    let day_a = seed_production_event(
        &pool,
        workspace_id,
        "shoot",
        today - time::Duration::days(1),
        "scheduled",
        None,
    )
    .await?;
    let plan_a = seed_plan(&pool, workspace_id, day_a, "issued", Some(member_id)).await?;
    seed_assignment(&pool, workspace_id, plan_a, member_id).await?;
    for i in 0..3 {
        seed_source(
            &pool,
            workspace_id,
            &format!("footage:a-{i}"),
            OffsetDateTime::now_utc() - time::Duration::days(1),
        )
        .await?;
    }

    // B — window lapsed eight days back with only machine-written
    // projection rows inside it: a show happening is not footage of the
    // show, and a release dropping nearby is not either — the calendar
    // and discography kinds never count. Far enough back that A's
    // sources stay outside the window.
    let gig_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() - interval '8 days', 'published', now() - interval '12 days')
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("gig-{}", workspace_id.into_uuid().simple()))
    .bind("Last week's gig")
    .fetch_one(&pool)
    .await?;
    let day_b = seed_production_event(
        &pool,
        workspace_id,
        "show",
        today - time::Duration::days(8),
        "scheduled",
        Some(gig_id),
    )
    .await?;
    let plan_b = seed_plan(&pool, workspace_id, day_b, "issued", Some(member_id)).await?;
    seed_assignment(&pool, workspace_id, plan_b, member_id).await?;
    sqlx::query(
        "INSERT INTO content_sources
             (workspace_id, source_kind, source_key, title, occurred_at, expires_at)
         VALUES ($1,'show_completed',$2,'The gig itself',$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("show_completed:{gig_id}"))
    .bind(OffsetDateTime::now_utc() - time::Duration::days(5))
    .bind(OffsetDateTime::now_utc() + time::Duration::days(40))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO content_sources
             (workspace_id, source_kind, source_key, title, occurred_at, expires_at)
         VALUES ($1,'release',$2,'An album drop, not footage',$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("release:{gig_id}"))
    .bind(OffsetDateTime::now_utc() - time::Duration::days(6))
    .bind(OffsetDateTime::now_utc() + time::Duration::days(39))
    .execute(&pool)
    .await?;

    // C — the day was cancelled: abandoned regardless of yield.
    let day_c = seed_production_event(
        &pool,
        workspace_id,
        "studio",
        today + time::Duration::days(1),
        "cancelled",
        None,
    )
    .await?;
    let plan_c = seed_plan(&pool, workspace_id, day_c, "issued", Some(member_id)).await?;
    seed_assignment(&pool, workspace_id, plan_c, member_id).await?;

    // D — inside the window, under target: still waiting. Scheduled today
    // so the older scenarios' sources all predate its window.
    let day_d =
        seed_production_event(&pool, workspace_id, "rehearsal", today, "scheduled", None).await?;
    let plan_d = seed_plan(&pool, workspace_id, day_d, "issued", Some(member_id)).await?;
    seed_assignment(&pool, workspace_id, plan_d, member_id).await?;
    seed_source(
        &pool,
        workspace_id,
        "footage:d-0",
        OffsetDateTime::now_utc(),
    )
    .await?;

    // E — a draft that outlived its day was never issued: abandoned.
    let day_e = seed_production_event(
        &pool,
        workspace_id,
        "photoshoot",
        today - time::Duration::days(1),
        "scheduled",
        None,
    )
    .await?;
    let plan_e = seed_plan(&pool, workspace_id, day_e, "draft", None).await?;

    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;

    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM capture_plans WHERE workspace_id=$1")
            .bind(workspace_id.into_uuid())
            .fetch_all(&pool)
            .await?;
    let status_of = |plan: Uuid| {
        statuses
            .iter()
            .find(|(id, _)| *id == plan)
            .map(|(_, status)| status.as_str())
    };
    assert_eq!(status_of(plan_a), Some("done"), "target met settles");
    assert_eq!(
        status_of(plan_b),
        Some("abandoned"),
        "the auto-projection row is not footage"
    );
    assert_eq!(
        status_of(plan_c),
        Some("abandoned"),
        "cancelled is cancelled"
    );
    assert_eq!(
        status_of(plan_d),
        Some("issued"),
        "the window is still open"
    );
    assert_eq!(
        status_of(plan_e),
        Some("abandoned"),
        "a stale draft cannot hold the slot"
    );

    // Assignments follow their plans: done for the filmed day, cancelled
    // for the lost ones, still open for the one inside its window.
    let assignments: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT source_id, status FROM team_assignments
         WHERE workspace_id=$1 AND source_kind='capture_plan'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&pool)
    .await?;
    let assignment_of = |plan: Uuid| {
        assignments
            .iter()
            .find(|(id, _)| *id == plan)
            .map(|(_, status)| status.as_str())
    };
    assert_eq!(assignment_of(plan_a), Some("done"));
    assert_eq!(assignment_of(plan_b), Some("cancelled"));
    assert_eq!(assignment_of(plan_c), Some("cancelled"));
    assert_eq!(assignment_of(plan_d), Some("open"));

    // The yield persists on the plan — the operator reads what a day
    // produced, and an unmeasured draft is NULL, never a guessed zero.
    let yields: Vec<(Uuid, Option<i32>)> =
        sqlx::query_as("SELECT id, sources_landed FROM capture_plans WHERE workspace_id=$1")
            .bind(workspace_id.into_uuid())
            .fetch_all(&pool)
            .await?;
    let yield_of = |plan: Uuid| {
        yields
            .iter()
            .find(|(id, _)| *id == plan)
            .map(|(_, landed)| *landed)
    };
    assert_eq!(
        yield_of(plan_a),
        Some(Some(4)),
        "the filmed day kept its count — its own three plus D's footage inside the shared window"
    );
    assert_eq!(
        yield_of(plan_b),
        Some(Some(0)),
        "measured and empty — the machine rows were not footage"
    );
    assert_eq!(
        yield_of(plan_d),
        Some(Some(1)),
        "the running yield stays live"
    );
    assert_eq!(yield_of(plan_e), Some(None), "a draft was never measured");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_moved_or_cancelled_gig_carries_its_production_day_with_it()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    seed_team_email_executor(&pool, workspace_id).await?;
    seed_member(&pool, workspace_id, "crew-a", &["video", "photography"]).await?;

    // Tomorrow's gig projects a production day and issues a plan.
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, date_trunc('day', now()) + interval '44 hours', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("moved-{}", workspace_id.into_uuid().simple()))
    .bind("The gig that will move")
    .fetch_one(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;

    let day: Option<(Uuid, time::Date)> = sqlx::query_as(
        "SELECT id, scheduled_for FROM production_events
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&pool)
    .await?;
    let (day_id, first_day) = day.expect("the gig projected a production day");

    // The gig moves a week out and gets retitled: the projected day and
    // the member's deadline follow instead of pointing at a day that no
    // longer exists.
    sqlx::query(
        "UPDATE events SET title='The moved gig', starts_at = starts_at + interval '7 days'
         WHERE id=$1",
    )
    .bind(event_id)
    .execute(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let moved: Option<(time::Date, String)> =
        sqlx::query_as("SELECT scheduled_for, title FROM production_events WHERE id=$1")
            .bind(day_id)
            .fetch_optional(&pool)
            .await?;
    let (moved_day, moved_title) = moved.expect("the day row is still there");
    assert_eq!(moved_day, first_day + time::Duration::days(7));
    assert_eq!(moved_title, "The moved gig");
    let moved_due: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT assignment.due_at FROM team_assignments assignment
         JOIN capture_plans plan ON plan.id = assignment.source_id
         WHERE assignment.workspace_id=$1 AND plan.production_event_id=$2
           AND assignment.source_kind='capture_plan'",
    )
    .bind(workspace_id.into_uuid())
    .bind(day_id)
    .fetch_optional(&pool)
    .await?;
    assert_eq!(
        moved_due.map(|due| due.date()),
        Some(moved_day + time::Duration::days(1)),
        "the member's deadline moved with the day"
    );

    // The gig is cancelled outright: the day dies, the plan abandons and
    // the open assignment cancels — nobody gets reminded to film a show
    // that is not happening.
    sqlx::query("UPDATE events SET status='cancelled' WHERE id=$1")
        .bind(event_id)
        .execute(&pool)
        .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let day_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM production_events WHERE id=$1")
            .bind(day_id)
            .fetch_optional(&pool)
            .await?;
    assert_eq!(day_status.as_deref(), Some("cancelled"));
    let plan_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM capture_plans WHERE production_event_id=$1")
            .bind(day_id)
            .fetch_optional(&pool)
            .await?;
    assert_eq!(plan_status.as_deref(), Some("abandoned"));
    let assignment_status: Option<String> = sqlx::query_scalar(
        "SELECT assignment.status FROM team_assignments assignment
         JOIN capture_plans plan ON plan.id = assignment.source_id
         WHERE assignment.workspace_id=$1 AND plan.production_event_id=$2
           AND assignment.source_kind='capture_plan'",
    )
    .bind(workspace_id.into_uuid())
    .bind(day_id)
    .fetch_optional(&pool)
    .await?;
    assert_eq!(assignment_status.as_deref(), Some("cancelled"));
    Ok(())
}

/// 6.3: a festival slot is a production day of its own kind. The mark on
/// the gig is what decides — a show marked before it projects lands as
/// `festival`, a mark added after flips the day's kind through the same
/// sync that carries moves, and unmarking carries it back.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_festival_slot_projects_a_festival_production_day()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // The slot as booked — still an ordinary show.
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, date_trunc('day', now()) + interval '20 hours', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("fest-{}", workspace_id.into_uuid().simple()))
    .bind("The festival slot")
    .fetch_one(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT kind FROM production_events
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(kind.as_deref(), Some("show"));

    // The festival confirmation lands — the day's kind follows it.
    sqlx::query("UPDATE events SET festival_name='OFF Festival' WHERE id=$1")
        .bind(event_id)
        .execute(&pool)
        .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM production_events WHERE event_id=$1")
            .bind(event_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(kind.as_deref(), Some("festival"));

    // A second slot booked marked projects as festival from the start.
    let marked_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at, festival_name)
         VALUES ($1,$2,$3,$4, date_trunc('day', now()) + interval '30 hours', 'published', now(), 'OFF Festival')
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("fest2-{}", workspace_id.into_uuid().simple()))
    .bind("The marked slot")
    .fetch_one(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM production_events WHERE event_id=$1")
            .bind(marked_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(kind.as_deref(), Some("festival"));

    // Losing the mark carries the day back to a show.
    sqlx::query("UPDATE events SET festival_name=NULL WHERE id=$1")
        .bind(event_id)
        .execute(&pool)
        .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM production_events WHERE event_id=$1")
            .bind(event_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(kind.as_deref(), Some("show"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_published_show_mints_its_door_campaign_once_and_only_once()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // A published gig inside the projection window, plus a draft and a
    // far-out published gig that must not mint anything yet.
    let show_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() + interval '10 days', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("door-{}", workspace_id.into_uuid().simple()))
    .bind("The door gig")
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status)
         VALUES ($1,$2,$3,$4, now() + interval '10 days', 'draft')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("draft-{}", workspace_id.into_uuid().simple()))
    .bind("Unpublished gig")
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() + interval '90 days', 'published', now())",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("far-{}", workspace_id.into_uuid().simple()))
    .bind("Far out gig")
    .execute(&pool)
    .await?;

    // A night that finished yesterday: inside the projection's own two-day
    // look-back, and outside the door's window. Minting for it would create a
    // QR that is expired the moment it exists — a scan page that says "this
    // campaign has closed" where the honest answer is that the night is over.
    let past_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() - interval '30 hours', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("past-{}", workspace_id.into_uuid().simple()))
    .bind("Last night's gig")
    .fetch_one(&pool)
    .await?;

    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;

    let past_campaigns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concert_qr_campaigns WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(past_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        past_campaigns, 0,
        "a show whose door closed yesterday was given a QR born expired"
    );

    let campaigns: Vec<(String, bool)> = sqlx::query_as(
        "SELECT label, active FROM concert_qr_campaigns
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        campaigns.as_slice(),
        &[("Door".to_owned(), true)],
        "one door campaign, minted by the sweep"
    );

    // The window is the door's: opens before start, closes into the night.
    let (valid_from, valid_until): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT valid_from, valid_until FROM concert_qr_campaigns
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_one(&pool)
    .await?;
    let starts_at: OffsetDateTime = sqlx::query_scalar("SELECT starts_at FROM events WHERE id=$1")
        .bind(show_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(valid_from, starts_at - time::Duration::hours(4));
    assert_eq!(valid_until, starts_at + time::Duration::hours(12));

    // A second sweep mints nothing — the row itself is the idempotency mark.
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concert_qr_campaigns
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);

    // The mint is on the audit trail as the machine's own act.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events
         WHERE workspace_id=$1 AND action='concert_qr.auto_minted'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(audit, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_operator_campaign_blocks_the_auto_mint_and_revoke_sticks()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    let show_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() + interval '10 days', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("hand-{}", workspace_id.into_uuid().simple()))
    .bind("Hand-entered gig")
    .fetch_one(&pool)
    .await?;
    // The operator made their own campaign first — the machine must not
    // stack a second one on top of it.
    sqlx::query(
        "INSERT INTO concert_qr_campaigns (
             id, workspace_id, event_id, label, valid_from, valid_until
         ) VALUES ($1,$2,$3,'Merch table', now(), now() + interval '10 days')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .execute(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concert_qr_campaigns
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1, "the operator's campaign stands alone");

    // A revoked campaign is a decision, not a gap — revoke the auto-minted
    // one on the sibling fixture and confirm the sweep does not re-mint.
    let revoked_show: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() + interval '12 days', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("revoked-{}", workspace_id.into_uuid().simple()))
    .bind("Revoked gig")
    .fetch_one(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    sqlx::query(
        "UPDATE concert_qr_campaigns SET active=false, revoked_at=now()
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(revoked_show)
    .execute(&pool)
    .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let (count, active): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE active)
         FROM concert_qr_campaigns WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(revoked_show)
    .fetch_one(&pool)
    .await?;
    assert_eq!((count, active), (1, 0), "a revoked door stays revoked");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_moved_show_moves_its_door_window_until_the_first_checkin()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    let show_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4, now() + interval '10 days', 'published', now())
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("moved-{}", workspace_id.into_uuid().simple()))
    .bind("The moved gig")
    .fetch_one(&pool)
    .await?;

    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;

    // The gig moves a week out — the minted window follows the show, so a
    // reprinted QR works instead of every scan refusing on a dead window.
    sqlx::query("UPDATE events SET starts_at = starts_at + interval '7 days' WHERE id=$1")
        .bind(show_id)
        .execute(&pool)
        .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let (valid_from, valid_until): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT valid_from, valid_until FROM concert_qr_campaigns
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_one(&pool)
    .await?;
    let starts_at: OffsetDateTime = sqlx::query_scalar("SELECT starts_at FROM events WHERE id=$1")
        .bind(show_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(valid_from, starts_at - time::Duration::hours(4));
    assert_eq!(valid_until, starts_at + time::Duration::hours(12));

    // Once somebody has scanned, the window is history — a further move
    // leaves it alone.
    let campaign_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM concert_qr_campaigns WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .fetch_one(&pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "scanner-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO concert_checkins (id, workspace_id, event_id, campaign_id, fan_id)
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(show_id)
    .bind(campaign_id)
    .bind(fan_id)
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE events SET starts_at = starts_at + interval '2 days' WHERE id=$1")
        .bind(show_id)
        .execute(&pool)
        .await?;
    repo.reconcile_team_handoffs(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let still: OffsetDateTime =
        sqlx::query_scalar("SELECT valid_until FROM concert_qr_campaigns WHERE id=$1")
            .bind(campaign_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(still, valid_until, "a scanned campaign's window is history");
    Ok(())
}

/// A `team.assignment.email` action shares `context='content_supply'` and the
/// source as `subject_id` with the artifact builds it rides alongside — and
/// its payload carries no `artifact` key. Without the guard the array column
/// picks up NULL and the whole snapshot load fails the eval, as it did in
/// production the morning the first assignment email aged in.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_keyless_action_on_a_source_does_not_break_the_supply_read()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::AutopilotDecisionRepository;

    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    seed_source(
        &pool,
        workspace_id,
        "src-keyless",
        OffsetDateTime::now_utc(),
    )
    .await?;
    let source_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM content_sources WHERE workspace_id=$1 AND source_key='src-keyless'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;

    // The row that killed production: a succeeded, emitted, never-reported
    // action on the source whose payload is the assignment email, not an
    // artifact build.
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'content_supply','content_source',$4,
                   'request_content_artifact',8000,'auto_execute','ask for the artifact',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(source_id)
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, finished_at, trace_id
         ) VALUES ($1,$2,$3,'content_supply','team.assignment.email','content_source',
                   $4,$5,$6,'succeeded',now(),$7)",
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(source_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind":"send_team_assignment_email","task_title":"Approve the post"}))
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_action_emissions
             (workspace_id, action_id, emission_key, outbox_event_id)
         VALUES ($1,$2,$3,NULL)",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("emission-{action_id}"))
    .execute(&pool)
    .await?;

    let snapshots = repo
        .load_content_supply_snapshots(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.source_id.into_uuid() == source_id)
        .expect("the seeded source is missing from the supply read");
    assert!(
        snapshot.in_flight_artifacts.is_empty() && snapshot.completed_artifacts.is_empty(),
        "the assignment email leaked into the artifact inventory"
    );
    Ok(())
}
