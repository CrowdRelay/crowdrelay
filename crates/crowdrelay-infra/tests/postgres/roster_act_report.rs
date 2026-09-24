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

use crate::common;

use crowdrelay_infra::roster_act_report::{CATALOGUE_ROTATION_TEMPLATE, roster_act_report};
use sqlx::PgPool;
use time::{OffsetDateTime, macros::datetime};
use uuid::Uuid;

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(crate::common::unique_slug(slug, id))
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
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(crate::common::unique_slug(slug, id))
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
        .bind(crate::common::unique_slug(&slug, id))
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
        INSERT INTO autopilot_decisions (
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
        INSERT INTO autopilot_actions (
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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_report(&database).await
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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = database.clone();

    async move {
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
    .await
}

/// The release-collision warning on the per-act page: same calendar the
/// org-level endpoint serves, filtered to this act — the labelmate's side
/// of the week is named, the shared-fan count is the measured overlap, and
/// an outsider releasing the same week is none of this roster's business.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_shared_release_week_warns_on_the_acts_page() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_collision(&database).await
}

async fn release_plan(
    pool: &PgPool,
    workspace_id: Uuid,
    title: &str,
    release_at: OffsetDateTime,
    tier: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO release_plans
            (workspace_id, source_key, title, release_at, tier, assets_ready, active)
        VALUES ($1, $2, $3, $4, $5, true, true)
        "#,
    )
    .bind(workspace_id)
    .bind(format!("src-{title}"))
    .bind(title)
    .bind(release_at)
    .bind(tier)
    .execute(pool)
    .await?;
    Ok(())
}

async fn run_collision(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    // A fixed Wednesday inside the lookahead so the two release dates are
    // a stable ISO week apart from `now`.
    let now = datetime!(2026-10-07 12:00 UTC);
    let org = organization(pool, "roster-label").await?;
    let act = workspace(pool, "the-act", Some(org)).await?;
    let labelmate = workspace(pool, "labelmate", Some(org)).await?;
    let outsider = workspace(pool, "outsider", None).await?;

    // The act's Track and the labelmate's Single share the ISO week of
    // 2026-10-12; the Single outranks, so the act is the one asked to move.
    release_plan(
        pool,
        act,
        "Act Single B-side",
        datetime!(2026-10-15 12:00 UTC),
        "track",
    )
    .await?;
    release_plan(
        pool,
        labelmate,
        "Labelmate Lead Single",
        datetime!(2026-10-17 12:00 UTC),
        "single",
    )
    .await?;
    // Same week, different organisation — not this collision's business.
    release_plan(
        pool,
        outsider,
        "Outsider Record",
        datetime!(2026-10-16 12:00 UTC),
        "single",
    )
    .await?;

    // Two fans are on both acts' books — the measurable cost of the clash.
    for email in ["shared-1@example.test", "shared-2@example.test"] {
        for ws in [act, labelmate] {
            sqlx::query(
                "INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1, $2, 'active')",
            )
            .bind(ws)
            .bind(email)
            .execute(pool)
            .await?;
        }
    }

    let report = roster_act_report(pool, org, act, now)
        .await?
        .expect("a member act gets a page");

    assert_eq!(report.release_collisions.len(), 1);
    let collision = &report.release_collisions[0];
    assert_eq!(
        collision.week_start,
        time::Date::from_calendar_date(2026, time::Month::October, 12)?,
        "the ISO Monday of the shared week"
    );
    assert_eq!(collision.release_title, "Act Single B-side");
    assert_eq!(collision.other_act, "labelmate");
    assert_eq!(collision.other_release_title, "Labelmate Lead Single");
    assert!(
        collision.this_act_moves,
        "the Track yields the week to the Single"
    );
    assert_eq!(
        collision.shared_fans,
        Some(2),
        "the measured overlap, cited"
    );
    assert!(collision.reason.contains("Move it a week later"));

    // And the labelmate's own page reads the same collision from its side.
    let labelmate_report = roster_act_report(pool, org, labelmate, now)
        .await?
        .expect("the labelmate is a member too");
    assert_eq!(labelmate_report.release_collisions.len(), 1);
    assert!(
        !labelmate_report.release_collisions[0].this_act_moves,
        "the Single keeps the week from the labelmate's side"
    );
    assert_eq!(labelmate_report.release_collisions[0].other_act, "the-act");
    Ok(())
}
