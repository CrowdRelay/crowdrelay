//! Free-reach waves against a real Postgres.
//!
//! The domain is unit-tested and the state machine is not what fails here. What
//! fails here is that every one of these reads is a statement nothing checks at
//! compile time: an anchor query over two tables, a pitch count that reads a
//! JSON key, and an approval that has to move a wave and its whole batch
//! together or not at all.
//!
//! The last of those is the test worth keeping: half an approved batch is the
//! one state an operator cannot reason about, because the thing they approved
//! was the batch.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotActionPayload, AutopilotControlRepository, AutopilotDecisionRepository,
    OutreachWaveStart, OutreachWaveTransition,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{
    OutreachOpportunityId, OutreachTargetId, WorkspaceId,
    free_reach::{WaveAnchor, WaveState},
    outreach::{OutreachPhase, OutreachTargetKind},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: Uuid,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
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
        .bind(format!("{label}-{suffix}"))
        .bind("Waves E2E")
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("{label}-show-{suffix}"))
    .bind("Waves E2E show")
    .bind(now + time::Duration::days(30))
    .execute(&pool)
    .await?;
    // Four verified press targets, so the anchor clears the minimum wave size.
    for index in 0..4 {
        sqlx::query(
            "INSERT INTO viryaos_outreach_targets (
                 workspace_id, target_kind, display_name, contact_email,
                 active, verified, accepts_outreach
             ) VALUES ($1,'press',$2,$3,true,true,true)",
        )
        .bind(workspace_id.into_uuid())
        .bind(format!("Press {index}"))
        .bind(format!("press-{index}-{suffix}@example.test"))
        .execute(&pool)
        .await?;
    }

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        event_id,
        now,
    })
}

async fn open_press_wave(fixture: &Fixture) -> Result<Uuid, Box<dyn std::error::Error>> {
    let anchors = fixture
        .repository
        .load_outreach_wave_anchors(fixture.workspace_id, fixture.now)
        .await?;
    let anchor = anchors
        .iter()
        .find(|anchor| {
            anchor.anchor.id() == fixture.event_id
                && anchor.target_kind == OutreachTargetKind::Press
        })
        .ok_or("the published show is a press-wave anchor")?;
    assert_eq!(anchor.eligible_targets, 4);
    assert!(
        (700..=725).contains(&anchor.hours_until),
        "a show thirty days out is about seven hundred and twenty hours away"
    );
    assert!(
        fixture
            .repository
            .open_outreach_wave(
                fixture.workspace_id,
                &OutreachWaveStart {
                    anchor: anchor.anchor,
                    anchor_at: anchor.anchor_at,
                    target_kind: anchor.target_kind,
                    capacity: 4,
                },
            )
            .await?
    );
    assert!(
        !fixture
            .repository
            .open_outreach_wave(
                fixture.workspace_id,
                &OutreachWaveStart {
                    anchor: anchor.anchor,
                    anchor_at: anchor.anchor_at,
                    target_kind: anchor.target_kind,
                    capacity: 4,
                },
            )
            .await?,
        "one wave per kind per anchor, whatever the cycle does"
    );
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_outreach_waves WHERE workspace_id=$1 AND anchor_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id)
    .fetch_one(&fixture.pool)
    .await?)
}

/// One pitch in the wave, awaiting approval like every wave pitch is.
async fn queue_pitch(
    fixture: &Fixture,
    wave_id: Option<Uuid>,
    label: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let payload = AutopilotActionPayload::RequestOutreach {
        opportunity_id: OutreachOpportunityId::new(),
        target_id: OutreachTargetId::new(),
        target_version: 1,
        target_name: label.to_owned(),
        phase: OutreachPhase::Initial,
        template_key: "outreach.press.v1".to_owned(),
        wave_id,
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        , trace_id)
        VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,
                'request_relationship_outreach',9000,'require_approval','test',
                '{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision:outreach:{label}:{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class
        )
        VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',$4,$5,$6,
                'awaiting_approval','third_party')
        "#,
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("action:outreach:{label}:{action_id}"))
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;
    Ok(action_id)
}

async fn statuses(fixture: &Fixture) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE workspace_id=$1 ORDER BY id",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_sealed_wave_is_approved_whole_and_a_drafting_one_is_not()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("wave-approve").await?;
    let wave_id = open_press_wave(&fixture).await?;
    for index in 0..3 {
        queue_pitch(&fixture, Some(wave_id), &format!("in-{index}")).await?;
    }
    // A standing pitch outside the wave, which approving the wave must not
    // touch: the operator said yes to a batch, not to the queue.
    let outsider = queue_pitch(&fixture, None, "outsider").await?;

    let live = fixture
        .repository
        .load_outreach_waves(fixture.workspace_id, fixture.now)
        .await?;
    let wave = live.first().ok_or("the wave is live")?;
    assert_eq!(wave.snapshot.state, WaveState::Drafting);
    assert_eq!(wave.snapshot.pitches, 3, "counted from the action ledger");
    assert!(wave.snapshot.anchor_active);
    assert_eq!(
        wave.snapshot.anchor,
        WaveAnchor::Event {
            event_id: crowdrelay_domain::EventId::from_uuid(fixture.event_id)
        }
    );

    // Drafting is not approvable: it would grow after somebody read it.
    assert!(matches!(
        fixture
            .repository
            .approve_outreach_wave(
                fixture.workspace_id,
                wave_id,
                &IdempotencyKey::parse("wave-early").expect("valid key"),
                None,
            )
            .await,
        Err(RepositoryError::Conflict)
    ));

    fixture
        .repository
        .transition_outreach_wave(
            fixture.workspace_id,
            wave_id,
            OutreachWaveTransition::Seal,
            fixture.now,
        )
        .await?;
    let released = fixture
        .repository
        .approve_outreach_wave(
            fixture.workspace_id,
            wave_id,
            &IdempotencyKey::parse("wave-approve").expect("valid key"),
            None,
        )
        .await?;
    assert_eq!(released.status, "approved:3");
    let statuses = statuses(&fixture).await?;
    assert_eq!(
        statuses.iter().filter(|status| *status == "queued").count(),
        3,
        "the whole batch moved together"
    );
    let outsider_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM viryaos_autopilot_actions WHERE id=$1")
            .bind(outsider)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(
        outsider_status, "awaiting_approval",
        "approving a wave says yes to the batch, not to the queue"
    );

    // And the wave is settled, so a second approval is not a second release.
    assert!(matches!(
        fixture
            .repository
            .approve_outreach_wave(
                fixture.workspace_id,
                wave_id,
                &IdempotencyKey::parse("wave-again").expect("valid key"),
                None,
            )
            .await,
        Err(RepositoryError::Conflict)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_expiring_wave_takes_its_unapproved_pitches_with_it()
-> Result<(), Box<dyn std::error::Error>> {
    // Left queued, they send a release-week pitch a month late, one at a time,
    // with nobody having decided to.
    let fixture = fixture("wave-expire").await?;
    let wave_id = open_press_wave(&fixture).await?;
    for index in 0..2 {
        queue_pitch(&fixture, Some(wave_id), &format!("doomed-{index}")).await?;
    }
    let outsider = queue_pitch(&fixture, None, "outsider").await?;

    fixture
        .repository
        .transition_outreach_wave(
            fixture.workspace_id,
            wave_id,
            OutreachWaveTransition::Expire {
                reason: crowdrelay_domain::free_reach::WaveExpiry::TooFewPitches,
            },
            fixture.now,
        )
        .await?;
    let settled = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, expiry_reason FROM viryaos_outreach_waves WHERE id=$1",
    )
    .bind(wave_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        settled,
        ("expired".to_owned(), Some("too_few_pitches".to_owned()))
    );
    let cancelled = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM viryaos_autopilot_actions
         WHERE workspace_id=$1 AND status='cancelled'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(cancelled, 2);
    let outsider_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM viryaos_autopilot_actions WHERE id=$1")
            .bind(outsider)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(outsider_status, "awaiting_approval");

    // A settled wave is never read into a cycle again, and the anchor is free
    // for nothing: one wave per kind per anchor, for ever.
    assert!(
        fixture
            .repository
            .load_outreach_waves(fixture.workspace_id, fixture.now)
            .await?
            .is_empty()
    );
    assert!(
        !fixture
            .repository
            .load_outreach_wave_anchors(fixture.workspace_id, fixture.now)
            .await?
            .iter()
            .any(|anchor| anchor.anchor.id() == fixture.event_id
                && anchor.target_kind == OutreachTargetKind::Press)
    );
    Ok(())
}
