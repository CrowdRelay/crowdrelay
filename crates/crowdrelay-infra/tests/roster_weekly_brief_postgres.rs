//! The roster's weekly brief, against a real schema.
//!
//! Worth a database rather than a unit test for the usual reason this suite
//! exists: the org boundary is a join (`workspaces.organization_id`), the
//! queue predicates are the briefing's own — and none of it is checked at
//! compile time. A column that does not exist would compile, lint and pass
//! every unit test, then fail on the first request.
//!
//! What is asserted: each act's queue lands under the right act and never
//! under a labelmate or an outsider; a lapsed-window approval is dead work
//! rather than a pending decision; the two ways an ask dies stay distinct;
//! the ordering reads the way a manager reads — decisions first, then
//! drifting, then quiet; and an act that never issued a briefing reports
//! `None` rather than a fabricated date.

use std::time::Duration;

use crowdrelay_domain::roster_weekly_brief::{WINDOW_DAYS, compose};
use crowdrelay_infra::roster_weekly_brief::act_briefs;
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::{Date, OffsetDateTime};
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
        let name = format!("crowdrelay_rosterbrief_{}", Uuid::now_v7().simple());
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

/// The decision row an action's foreign key needs. `trace_id` is NOT NULL.
async fn decision(
    pool: &PgPool,
    workspace_id: Uuid,
    subject_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','content_suggestion',$4,
                  'outreach.target.request',7000,'require_approval',
                  'test decision','{}','{}','{}',now(),$1)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(format!("decision-{id}"))
    .bind(subject_id)
    .execute(pool)
    .await?;
    Ok(id)
}

/// An approval still waiting on a human — or one whose window already
/// lapsed, which is dead work the queue must not count as pending.
async fn pending_action(
    pool: &PgPool,
    workspace_id: Uuid,
    action_kind: &str,
    expires_at: Option<OffsetDateTime>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    let decision_id = decision(pool, workspace_id, subject_id).await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, available_at, approval_expires_at
        ) VALUES ($1,$2,$3,'booking_opportunity',$4,'content_suggestion',$5,
                  $6,$7,'awaiting_approval',now(),$8)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(subject_id)
    .bind(format!("action-{id}"))
    .bind(json!({"kind": action_kind}))
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(id)
}

/// An ask that died unanswered — `resolution` is `last_error_kind` verbatim:
/// `approval_expired` or `insufficient_evidence`.
async fn slipped_action(
    pool: &PgPool,
    workspace_id: Uuid,
    action_kind: &str,
    resolution: &str,
    finished_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    let decision_id = decision(pool, workspace_id, subject_id).await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, available_at, finished_at, last_error_kind
        ) VALUES ($1,$2,$3,'booking_opportunity',$4,'content_suggestion',$5,
                  $6,$7,'cancelled',now(),$8,$9)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(subject_id)
    .bind(format!("action-{id}"))
    .bind(json!({"kind": action_kind}))
    .bind(finished_at)
    .bind(resolution)
    .execute(pool)
    .await?;
    Ok(())
}

/// A briefing for `local_date`, as the sweep would have written it.
async fn briefing(
    pool: &PgPool,
    workspace_id: Uuid,
    local_date: Date,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_daily_briefings (workspace_id, local_date, title, body)
         VALUES ($1, $2, 'ViryaOS — morning briefing', 'a briefing body')",
    )
    .bind(workspace_id)
    .bind(local_date)
    .execute(pool)
    .await?;
    Ok(())
}

/// One finished cycle's North Star reading on one day — the same row shape
/// the cycle close writes, which `daily_north_star` collapses per day.
async fn north_star_day(
    pool: &PgPool,
    workspace_id: Uuid,
    started_at: OffsetDateTime,
    value: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_autopilot_cycle_runs
             (id, workspace_id, trigger, started_at, finished_at, outcome, north_star_value)
         VALUES ($1, $2, 'scheduled', $3, $3 + INTERVAL '1 minute', 'succeeded', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(started_at)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Two acts, one outsider, and every queue shape at once: decisions pending
/// under the right act, deaths counted by resolution, the ordering a manager
/// reads, and briefing staleness stated or null — never fabricated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn each_acts_week_lands_under_that_act() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_roster(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_roster(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let today = now.date();
    let label = organization(pool, "roster-label").await?;
    let busy = workspace(pool, "busy-act", Some(label)).await?;
    let drifting = workspace(pool, "drifting-act", Some(label)).await?;
    let quiet = workspace(pool, "quiet-act", Some(label)).await?;
    let outsider = workspace(pool, "outsider", None).await?;

    // busy: two live asks (one of them nearly out of window), one ask whose
    // window already closed — dead work, not a decision — and three deaths
    // inside the week, plus one too old to count.
    pending_action(
        pool,
        busy,
        "outreach.target.request",
        Some(now + time::Duration::days(2)),
    )
    .await?;
    pending_action(pool, busy, "content.draft.review", None).await?;
    pending_action(
        pool,
        busy,
        "stale.window",
        Some(now - time::Duration::hours(1)),
    )
    .await?;
    slipped_action(
        pool,
        busy,
        "outreach.target.request",
        "approval_expired",
        now - time::Duration::days(1),
    )
    .await?;
    slipped_action(
        pool,
        busy,
        "outreach.target.request",
        "approval_expired",
        now - time::Duration::days(3),
    )
    .await?;
    slipped_action(
        pool,
        busy,
        "content.draft.review",
        "insufficient_evidence",
        now - time::Duration::days(2),
    )
    .await?;
    slipped_action(
        pool,
        busy,
        "outreach.target.request",
        "approval_expired",
        now - time::Duration::days(i64::from(WINDOW_DAYS) + 3),
    )
    .await?;

    // busy's queue is loud but its week is also growing — rising North Star
    // so the headline's weakest is a measured minimum, not the only signal.
    for (day, value) in [(6, 10), (5, 20), (4, 30), (3, 40), (2, 50), (1, 60)] {
        north_star_day(pool, busy, now - time::Duration::days(day), value).await?;
    }

    // drifting: nothing pending, but six days of a falling North Star — the
    // brain's own verdict is regressing, which is bucket two.
    for (day, value) in [(6, 100), (5, 90), (4, 80), (3, 30), (2, 20), (1, 10)] {
        north_star_day(pool, drifting, now - time::Duration::days(day), value).await?;
    }
    briefing(pool, drifting, today - time::Duration::days(2)).await?;
    briefing(pool, drifting, today - time::Duration::days(5)).await?;

    // quiet: six flat days is `learning`, not a fault — the honest posture
    // of a young system, and the third bucket.
    for day in 1..=6 {
        north_star_day(pool, quiet, now - time::Duration::days(day), 10).await?;
    }

    // The outsider's queue is full and its week is terrible. None of it is
    // this organisation's business, and the only way to prove the boundary
    // is to give it something worth leaking.
    pending_action(pool, outsider, "outreach.target.request", None).await?;
    slipped_action(
        pool,
        outsider,
        "outreach.target.request",
        "approval_expired",
        now - time::Duration::days(1),
    )
    .await?;
    for (day, value) in [(6, 100), (5, 90), (4, 80), (3, 30), (2, 20), (1, 10)] {
        north_star_day(pool, outsider, now - time::Duration::days(day), value).await?;
    }
    briefing(pool, outsider, today).await?;

    let brief = compose(label, now, act_briefs(pool, label, now).await?);

    assert_eq!(brief.organization_id, label);
    assert_eq!(brief.window_days, WINDOW_DAYS);
    assert_eq!(
        brief.acts.len(),
        3,
        "three member acts and no outsider: {:?}",
        brief
            .acts
            .iter()
            .map(|act| act.name.as_str())
            .collect::<Vec<_>>()
    );

    // Ordered the way the manager reads: decisions, then drifting, then quiet.
    let order: Vec<(&str, Uuid)> = brief
        .acts
        .iter()
        .map(|act| (act.name.as_str(), act.workspace_id.into_uuid()))
        .collect();
    assert_eq!(
        order,
        [
            ("busy-act", busy),
            ("drifting-act", drifting),
            ("quiet-act", quiet)
        ],
    );

    let busy_brief = &brief.acts[0];
    assert_eq!(
        busy_brief.pending_decisions, 2,
        "two live asks — the lapsed-window one is dead work, not pending"
    );
    assert_eq!(busy_brief.pending.len(), 2);
    assert!(
        busy_brief
            .pending
            .iter()
            .all(|item| item.action_kind != "stale.window"),
        "an ask whose window closed must not be listed as needing a decision"
    );
    assert_eq!(
        busy_brief.slipped, 3,
        "three deaths inside the window; the ten-day-old one is last week's"
    );
    let resolutions: Vec<&str> = busy_brief
        .slipped_items
        .iter()
        .map(|item| item.resolution.as_str())
        .collect();
    assert_eq!(
        resolutions
            .iter()
            .filter(|r| **r == "approval_expired")
            .count(),
        2,
        "the two ways an ask dies stay distinct: {resolutions:?}"
    );
    assert!(resolutions.contains(&"insufficient_evidence"));
    assert_eq!(
        busy_brief.posture, "improving",
        "six rising days is the brain calling itself improving"
    );
    assert_eq!(busy_brief.latest_briefing_date, None);

    let drifting_brief = &brief.acts[1];
    assert_eq!(drifting_brief.posture, "regressing");
    assert!(drifting_brief.posture_needs_attention);
    assert_eq!(drifting_brief.days_observed, 6);
    assert_eq!(
        drifting_brief.latest_briefing_date,
        Some(today - time::Duration::days(2)),
        "the newest briefing's own day, so staleness reads as staleness"
    );

    let quiet_brief = &brief.acts[2];
    assert_eq!(quiet_brief.posture, "learning");
    assert!(!quiet_brief.posture_needs_attention);
    assert_eq!(quiet_brief.pending_decisions, 0);
    assert_eq!(quiet_brief.slipped, 0);
    assert_eq!(quiet_brief.latest_briefing_date, None);

    // 5.12 — the roster's north star is a distribution: the headline is the
    // weakest act's growth, not the roster's sum. drifting fell 90 fans over
    // the window (10 ← 100), quiet held at zero, busy gained 50 — the
    // headline names drifting, the total is the second line, and nobody is
    // unreadable.
    assert_eq!(drifting_brief.north_star_delta, Some(-90));
    assert_eq!(quiet_brief.north_star_delta, Some(0));
    assert_eq!(busy_brief.north_star_delta, Some(50));
    assert_eq!(
        brief.headline.weakest_act_id,
        Some(drifting_brief.workspace_id)
    );
    assert_eq!(
        brief.headline.weakest_act_name.as_deref(),
        Some("drifting-act")
    );
    assert_eq!(brief.headline.weakest_act_growth, Some(-90));
    assert_eq!(brief.headline.total_growth, Some(-40));
    assert_eq!(brief.headline.acts_without_signal, 0);
    Ok(())
}

/// The honest empty states: an organisation with no members is an empty
/// page, and a member with nothing recorded is a zeroed entry — both real
/// answers, never errors and never invented numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_empty_org_and_a_silent_act_both_render() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_empty(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_empty(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let empty_org = organization(pool, "empty-org").await?;
    let brief = compose(empty_org, now, act_briefs(pool, empty_org, now).await?);
    assert!(
        brief.acts.is_empty(),
        "an org with no member workspaces is an empty page, not an error"
    );

    let quiet_org = organization(pool, "quiet-org").await?;
    let member = workspace(pool, "silent-act", Some(quiet_org)).await?;
    let brief = compose(quiet_org, now, act_briefs(pool, quiet_org, now).await?);
    assert_eq!(brief.acts.len(), 1);
    let act = &brief.acts[0];
    assert_eq!(act.workspace_id.into_uuid(), member);
    assert_eq!(act.name, "silent-act");
    assert_eq!(act.pending_decisions, 0);
    assert!(act.pending.is_empty());
    assert_eq!(act.slipped, 0);
    assert!(act.slipped_items.is_empty());
    assert_eq!(act.posture, "initializing");
    assert_eq!(act.days_observed, 0);
    assert_eq!(act.latest_briefing_date, None);
    assert_eq!(act.north_star_delta, None);
    assert_eq!(brief.headline.weakest_act_id, None);
    assert_eq!(brief.headline.weakest_act_growth, None);
    assert_eq!(brief.headline.total_growth, None);
    assert_eq!(brief.headline.acts_without_signal, 1);
    Ok(())
}
