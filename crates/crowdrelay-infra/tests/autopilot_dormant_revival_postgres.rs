//! Dormant revival against a real Postgres.
//!
//! Every rule that keeps this play from becoming a mailing machine is a
//! `NOT EXISTS` in one statement, and each one can be deleted without any type
//! noticing. What breaks then is not an error: it is the band writing to the
//! wrong people, which nobody finds out about from a log.
//!
//! Its own file rather than an addition to `autopilot_plays_postgres`, which is
//! already at the size the source ratchet reviews.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotDecisionRepository, PlayAnchorRef, PlayStart, PlayStepPlan,
};
use crowdrelay_domain::{
    EventId, FanId, WorkspaceId,
    plays::{PlayKind, step_schedule},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_revival_takes_only_fans_who_were_here_and_stopped()
-> Result<(), Box<dyn std::error::Error>> {
    // Every guard here is a `NOT EXISTS` somebody could delete without any
    // type noticing, and each deletion turns the play into a mailing machine
    // aimed at a different wrong group.
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

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("revival-e2e-{suffix}"))
        .bind("Revival E2E")
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();

    let past_event = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(past_event.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("revival-past-{suffix}"))
    .bind("Two years ago")
    .bind(now - time::Duration::days(730))
    .execute(&pool)
    .await?;

    let consented_fan = async |email: &str| -> Result<FanId, Box<dyn std::error::Error>> {
        let fan_id = FanId::new();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status)
             VALUES ($1,$2,$3,'active')",
        )
        .bind(fan_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(email)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
             VALUES ($1,$2,'marketing',true,'v1','test')",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .execute(&pool)
        .await?;
        Ok(fan_id)
    };

    // Dormant: interested two years ago, silent since.
    let dormant = consented_fan(&format!("dormant-{suffix}@example.test")).await?;
    sqlx::query(
        "INSERT INTO event_interests (workspace_id, event_id, fan_id, created_at)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(past_event.into_uuid())
    .bind(dormant.into_uuid())
    .bind(now - time::Duration::days(730))
    .execute(&pool)
    .await?;

    // Never did anything. A name on a list is not a dormant fan.
    let stranger = consented_fan(&format!("stranger-{suffix}@example.test")).await?;

    // Interested last month: the ladder's audience, not this one.
    let recent = consented_fan(&format!("recent-{suffix}@example.test")).await?;
    sqlx::query(
        "INSERT INTO event_interests (workspace_id, event_id, fan_id, created_at)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(past_event.into_uuid())
    .bind(recent.into_uuid())
    .bind(now - time::Duration::days(30))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    // Nothing coming up, so there is nothing to revive anybody with.
    assert!(
        repository
            .load_play_anchors(workspace_id, PlayKind::DormantRevival, now)
            .await?
            .is_empty(),
        "a revival with no upcoming date is 'hello, remember us'"
    );

    let upcoming = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(upcoming.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("revival-next-{suffix}"))
    .bind("A reason to write")
    .bind(now + time::Duration::days(40))
    .execute(&pool)
    .await?;

    let anchors = repository
        .load_play_anchors(workspace_id, PlayKind::DormantRevival, now)
        .await?;
    assert_eq!(
        anchors
            .iter()
            .map(|anchor| anchor.anchor)
            .collect::<Vec<_>>(),
        vec![PlayAnchorRef::Fan { fan_id: dormant }],
        "only the fan who was here and stopped: not the stranger, not the recent one"
    );
    // And the ladder takes the exact complement, so nobody is in both at once.
    sqlx::query(
        "INSERT INTO smart_links (workspace_id, slug, destination_url, active)
         VALUES ($1,'follow','https://example.test/follow',true)",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let ladder = repository
        .load_play_anchors(workspace_id, PlayKind::FollowAskLadder, now)
        .await?;
    assert_eq!(
        ladder
            .iter()
            .map(|anchor| anchor.anchor)
            .collect::<Vec<_>>(),
        vec![PlayAnchorRef::Fan { fan_id: recent }],
        "the ladder takes activity inside a year and the revival takes its complement"
    );
    assert_ne!(stranger, dormant);

    // A fan a play reached recently is left alone, whatever else is true of
    // them: the weekly envelope does not know they have just had a ladder.
    let start = PlayStart {
        kind: PlayKind::DormantRevival,
        anchor: PlayAnchorRef::Fan { fan_id: dormant },
        anchor_at: now,
        hypothesis: PlayKind::DormantRevival.hypothesis(),
        success_metric_platform: PlayKind::DormantRevival.success_metric().0,
        success_metric_key: PlayKind::DormantRevival.success_metric().1,
        steps: PlayKind::DormantRevival
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, now);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: now + time::Duration::days(90),
    };
    assert!(repository.start_play(workspace_id, &start).await?);
    assert!(
        repository
            .load_play_anchors(workspace_id, PlayKind::DormantRevival, now)
            .await?
            .is_empty(),
        "a fan who already has a revival is never offered a second one"
    );
    Ok(())
}
