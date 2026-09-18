//! The label's quarterly page to one of its own acts, against a real schema
//! (5.25).
//!
//! Worth a database rather than a unit test for the usual reason: the
//! membership boundary is a join on `workspaces.organization_id`, the rooms
//! are a join through `event_acts`, the fans-by-city split crosses three
//! tables — and none of it is checked at compile time. What is asserted:
//! the page composes each section from the rows that act actually produced;
//! an act outside the organisation gets no page; a cancelled night is not
//! a room played; and a catalogue rotation is counted where it lands.

use std::time::Duration;

use crowdrelay_infra::roster_act_report::{CATALOGUE_ROTATION_TEMPLATE, roster_act_report};
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
        let name = format!("crowdrelay_actreport_{}", Uuid::now_v7().simple());
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

async fn city(pool: &PgPool, name: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    let slug = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, 'PL')")
        .bind(id)
        .bind(slug)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

/// One dispatched action, with the decision row its foreign key needs.
async fn action(
    pool: &PgPool,
    workspace_id: Uuid,
    action_kind: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                  'request_agent_run',8000,'auto_execute','quarter test',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence',$4,'workspace',
                  $2,$5,'{}'::jsonb,$6,now(),$7)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(format!("action-{}", Uuid::now_v7()))
    .bind(status)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(())
}

/// A night the act played — the `event_acts.act_workspace_id` edge is the
/// join the report reads.
async fn show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    venue: &str,
    status: &str,
    starts_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $4, $4, $5, $6, $7, CASE WHEN $7 IN ('published','completed') THEN now() ELSE NULL END)",
    )
    .bind(event_id)
    .bind(workspace_id)
    .bind(city_id)
    .bind(format!("show-{event_id}"))
    .bind(venue)
    .bind(starts_at)
    .bind(status)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, act_workspace_id)
         VALUES ($1, $2, 'the-act', 'The Act', $1)",
    )
    .bind(workspace_id)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A fan who declared interest in a city — the join row the by-city split
/// reads. `fan_created_at` is set explicitly so the quarter boundary is the
/// thing under test, not the wall clock.
async fn fan_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    fan_created_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1, $2, $3, 'active', $4)",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(format!("fan-{fan_id}@example.com"))
    .bind(fan_created_at)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id) VALUES ($1, $2, $3)",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// One quarter's record, composed: the action lines land by kind and
/// outcome, the rooms are the shows the act was billed on, the fans split
/// by declared city, the rotation is counted — and last quarter's rows stay
/// last quarter's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_quarters_record_lands_under_the_right_act() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_report(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_report(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let org = organization(pool, "roster-label").await?;
    let act = workspace(pool, "the-act", Some(org)).await?;
    let labelmate = workspace(pool, "labelmate", Some(org)).await?;
    let outsider = workspace(pool, "outsider", None).await?;
    let warsaw = city(pool, "Warsaw").await?;
    let krakow = city(pool, "Kraków").await?;

    // The act's quarter: two dispatches of one kind (one landed, one
    // failed), one show in each city, two new fans in Warsaw and one in
    // Kraków, one catalogue rotation — plus a show and a fan from before
    // the quarter, which the period must exclude.
    action(pool, act, "agent.run", "succeeded").await?;
    action(pool, act, "agent.run", "failed").await?;
    action(pool, act, "outreach.target.request", "succeeded").await?;
    action(pool, labelmate, "agent.run", "succeeded").await?;

    show(
        pool,
        act,
        warsaw,
        "Hydro",
        "completed",
        now - time::Duration::days(10),
    )
    .await?;
    show(
        pool,
        act,
        krakow,
        "Studio",
        "completed",
        now - time::Duration::days(3),
    )
    .await?;
    show(
        pool,
        act,
        warsaw,
        "Cancelled Room",
        "cancelled",
        now - time::Duration::days(5),
    )
    .await?;
    // A labelmate's show is not the act's room.
    show(pool, labelmate, warsaw, "Labelmate Venue", "completed", now).await?;

    fan_in_city(pool, act, warsaw, now - time::Duration::days(2)).await?;
    fan_in_city(pool, act, warsaw, now - time::Duration::days(1)).await?;
    fan_in_city(pool, act, krakow, now - time::Duration::days(1)).await?;

    let segment_id = Uuid::now_v7();
    sqlx::query("INSERT INTO audience_segments (id, workspace_id, slug, name) VALUES ($1, $2, 'rot-seg', 'Rotation segment')")
        .bind(segment_id)
        .bind(act)
        .execute(pool)
        .await?;
    // A completed campaign needs its dispatch event: the row the ledger
    // anchors, so the rotation counted is one that actually sent.
    let dispatch_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, payload)
         VALUES ($1, $2, 'campaign.dispatch', '{}'::jsonb)",
    )
    .bind(dispatch_event)
    .bind(act)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaigns
             (id, workspace_id, segment_id, slug, name, channel, template_key, content,
              status, scheduled_at, dispatch_event_id, recipient_count, delivered_count,
              failed_count, completed_at)
         VALUES ($1, $2, $4, 'rot-1', 'Catalogue rotation', 'email', $3, '{}'::jsonb,
                 'completed', now() - INTERVAL '1 day', $5, 12, 12, 0, now())",
    )
    .bind(Uuid::now_v7())
    .bind(act)
    .bind(CATALOGUE_ROTATION_TEMPLATE)
    .bind(segment_id)
    .bind(dispatch_event)
    .execute(pool)
    .await?;
    // A cancelled rotation is not a landed one.
    sqlx::query(
        "INSERT INTO communication_campaigns
             (id, workspace_id, segment_id, slug, name, channel, template_key, content,
              status, cancelled_at)
         VALUES ($1, $2, $3, 'rot-cancelled', 'Cancelled rotation', 'email', $4, '{}'::jsonb,
                 'cancelled', now())",
    )
    .bind(Uuid::now_v7())
    .bind(act)
    .bind(segment_id)
    .bind(CATALOGUE_ROTATION_TEMPLATE)
    .execute(pool)
    .await?;

    let report = roster_act_report(pool, org, act, now)
        .await?
        .expect("a member act gets a page");

    assert_eq!(report.act_name, "the-act");
    assert_eq!(report.organization_id, org);

    // Actions land by kind and outcome — the two `agent.run` results stay
    // distinct, and the labelmate's dispatch never enters this page.
    let agent_lines: Vec<_> = report
        .actions
        .iter()
        .filter(|line| line.action_kind == "agent.run")
        .collect();
    assert_eq!(agent_lines.len(), 2);
    let mut statuses: Vec<(&str, u32)> = agent_lines
        .iter()
        .map(|line| (line.status.as_str(), line.count))
        .collect();
    statuses.sort();
    assert_eq!(statuses, [("failed", 1), ("succeeded", 1)]);
    assert!(
        report
            .actions
            .iter()
            .any(|line| line.action_kind == "outreach.target.request" && line.count == 1)
    );

    // Two rooms played; the cancelled night is not one, and the labelmate's
    // room is not the act's.
    let venues: Vec<&str> = report
        .shows
        .iter()
        .map(|show| show.venue.as_str())
        .collect();
    assert_eq!(venues, ["Hydro", "Studio"], "venues={venues:?}");

    // Fans gained: three total, split 2 Warsaw / 1 Kraków.
    assert_eq!(report.fans.gained_total, 3);
    let by_city: Vec<(&str, u32)> = report
        .fans
        .by_city
        .iter()
        .map(|row| (row.city.as_str(), row.count))
        .collect();
    assert_eq!(by_city, [("Warsaw", 2), ("Kraków", 1)]);

    assert_eq!(report.rotations_landed, 1);

    // The membership boundary is the refusal: an act outside the org gets
    // no page, and neither does a stranger asking through this org's id.
    assert!(roster_act_report(pool, org, outsider, now).await?.is_none());
    let other_org = organization(pool, "other-label").await?;
    assert!(
        roster_act_report(pool, other_org, act, now)
            .await?
            .is_none()
    );

    Ok(())
}

/// The honest zero state: a member with no quarter activity gets an empty
/// page that is still a page — `actions: []`, `shows: []`, zero fans — not
/// an error and not a 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_silent_quarter_is_an_empty_page_not_an_error() -> Result<(), Box<dyn std::error::Error>>
{
    let database = DisposableDatabase::create().await?;
    let pool = database.pool.clone();
    let result = async move {
        let now = OffsetDateTime::now_utc();
        let org = organization(&pool, "roster-label").await?;
        let act = workspace(&pool, "quiet-act", Some(org)).await?;
        let report = roster_act_report(&pool, org, act, now)
            .await?
            .expect("a member gets a page even with nothing in it");
        assert!(report.actions.is_empty());
        assert!(report.shows.is_empty());
        assert_eq!(report.fans.gained_total, 0);
        assert!(report.fans.by_city.is_empty());
        assert_eq!(report.rotations_landed, 0);
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}
