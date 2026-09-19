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
        draft: crowdrelay_domain::outreach_letter::OutreachLetter {
            subject: format!("Act — {label}"),
            body: format!("letter {label}"),
        },
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
async fn only_active_non_filler_releases_anchor_waves() -> Result<(), Box<dyn std::error::Error>> {
    // The anchor query is where "a filler owes no vertical" is decided: if the
    // tier filter slips off it, a demo the band posted for free would draft
    // five press waves nobody approved. Same for a plan the operator retired.
    let fixture = fixture("wave-anchor").await?;
    let suffix = fixture.workspace_id.into_uuid().simple().to_string();
    let release_at = fixture.now + time::Duration::days(30);
    let single_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_release_plans (id, workspace_id, source_key, title, release_at, tier)
         VALUES ($1,$2,$3,$4,$5,'single')",
    )
    .bind(single_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("wave-anchor-single-{suffix}"))
    .bind("The real single")
    .bind(release_at)
    .execute(&fixture.pool)
    .await?;
    for (tier, active) in [("filler", true), ("single", false)] {
        sqlx::query(
            "INSERT INTO viryaos_release_plans (
                 id, workspace_id, source_key, title, release_at, tier, active
             ) VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(Uuid::now_v7())
        .bind(fixture.workspace_id.into_uuid())
        .bind(format!("wave-anchor-{tier}-{active}-{suffix}"))
        .bind(format!("{tier} active={active}"))
        .bind(release_at)
        .bind(tier)
        .bind(active)
        .execute(&fixture.pool)
        .await?;
    }

    let anchors = fixture
        .repository
        .load_outreach_wave_anchors(fixture.workspace_id, fixture.now)
        .await?;
    let release_anchors = anchors
        .iter()
        .filter(|anchor| matches!(anchor.anchor, WaveAnchor::Release { .. }))
        .count();
    assert_eq!(
        release_anchors, 5,
        "five free-reach kinds anchor on the one live non-filler plan"
    );
    assert!(
        anchors.iter().all(|anchor| {
            !matches!(anchor.anchor, WaveAnchor::Release { .. }) || anchor.anchor.id() == single_id
        }),
        "the filler and the retired plan stay out of the wave queue"
    );
    assert_eq!(
        anchors
            .iter()
            .filter(|anchor| anchor.anchor.id() == fixture.event_id)
            .count(),
        5,
        "the published show still anchors its five kinds beside it"
    );
    Ok(())
}

/// A lead that already answered is served, not fresh: the eligible count stops
/// promising it, and the send lock refuses the pitch outright — the reply may
/// have landed after the opportunity was seeded, so the check lives at
/// dispatch, not only at seed time.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_served_lead_cannot_be_re_pitched() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("wave-served").await?;
    // Three of the four press targets answered: one yes, one no, one reply the
    // classifier has not read yet. All three are served — only the silent one
    // is still a lead.
    for (name, disposition) in [
        ("Press 0", "positive"),
        ("Press 1", "declined"),
        ("Press 2", "received"),
    ] {
        sqlx::query(
            "UPDATE viryaos_outreach_targets
             SET last_reply_at=now(), last_reply_disposition=$3
             WHERE workspace_id=$1 AND display_name=$2",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(name)
        .bind(disposition)
        .execute(&fixture.pool)
        .await?;
    }

    let anchors = fixture
        .repository
        .load_outreach_wave_anchors(fixture.workspace_id, fixture.now)
        .await?;
    let press = anchors
        .iter()
        .find(|anchor| {
            anchor.anchor.id() == fixture.event_id
                && anchor.target_kind == OutreachTargetKind::Press
        })
        .ok_or("the published show is still a press-wave anchor")?;
    assert_eq!(
        press.eligible_targets, 1,
        "only the lead that never answered still counts"
    );

    // And the dispatch lock refuses the served lead even though a stale
    // opportunity row points at it — the gate is the last word.
    let served_target = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_outreach_targets WHERE workspace_id=$1 AND display_name='Press 0'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let opportunity_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_outreach_opportunities (
             workspace_id, target_id, source, subject_kind, subject_key, template_key,
             relevance_basis_points, confidence_basis_points, observed_at, expires_at
         ) VALUES ($1,$2,'manual','event',$3,'event.press.v1',9000,9000,$4,$5)
         RETURNING id",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(served_target)
    .bind(format!("event:{}", fixture.event_id))
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::days(30))
    .fetch_one(&fixture.pool)
    .await?;
    let payload = AutopilotActionPayload::RequestOutreach {
        opportunity_id: OutreachOpportunityId::from_uuid(opportunity_id),
        target_id: OutreachTargetId::from_uuid(served_target),
        target_version: 1,
        target_name: "Press 0".to_owned(),
        phase: OutreachPhase::Initial,
        template_key: "event.press.v1".to_owned(),
        wave_id: None,
        draft: crowdrelay_domain::outreach_letter::OutreachLetter {
            subject: "Act — pitch".to_owned(),
            body: format!("letter {}", Uuid::now_v7()),
        },
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
        VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,
                'request_relationship_outreach',9000,'require_approval','test',
                '{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision:outreach:served:{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class
         ) VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',$4,$5,$6,
                   'queued','third_party')",
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("action:outreach:served:{action_id}"))
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;

    // An executor that can actually send: without the advertised capability the
    // claim parks the pitch as `awaiting_executor` and the dispatch gate under
    // test is never reached.
    sqlx::query(
        "INSERT INTO viryaos_executor_instances (
             workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
         ) VALUES ($1,'n8n-served-test','test','test-manifest',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities (
             workspace_id, executor_id, capability, capability_version, observed_at, expires_at
         ) VALUES ($1,'n8n-served-test','outreach.send','1',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;

    use crowdrelay_application::autopilot::AutopilotActionRepository;
    // `available_at` defaulted to the insert's now(), which is later than the
    // fixture's — claiming at the fixture's clock would see it as not yet due.
    let claim_now = OffsetDateTime::now_utc();
    let claimed = fixture
        .repository
        .claim_due_actions(fixture.workspace_id, 8, claim_now)
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .ok_or("the queued pitch is claimable")?;
    let outcome = fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await;
    assert!(
        matches!(outcome, Err(RepositoryError::Conflict)),
        "a served lead must refuse the send, got {outcome:?}"
    );
    let outbox_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events
         WHERE workspace_id=$1 AND payload->>'action_id'=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(action_id.to_string())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(outbox_rows, 0, "the refused send left no outbox intent");
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

/// A pitch that does dispatch emits the approved letter verbatim — the
/// executor receives the words the operator read, not a template key it
/// would have to resolve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dispatched_pitch_carries_the_approved_letter() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = fixture("wave-draft").await?;
    let target = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_outreach_targets WHERE workspace_id=$1 AND display_name='Press 0'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let opportunity_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_outreach_opportunities (
             workspace_id, target_id, source, subject_kind, subject_key, template_key,
             relevance_basis_points, confidence_basis_points, observed_at, expires_at
         ) VALUES ($1,$2,'manual','event',$3,'event.press.v1',9000,9000,$4,$5)
         RETURNING id",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target)
    .bind(format!("event:{}", fixture.event_id))
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::days(30))
    .fetch_one(&fixture.pool)
    .await?;
    let draft_body = format!("approved letter {}", Uuid::now_v7());
    let payload = AutopilotActionPayload::RequestOutreach {
        opportunity_id: OutreachOpportunityId::from_uuid(opportunity_id),
        target_id: OutreachTargetId::from_uuid(target),
        target_version: 1,
        target_name: "Press 0".to_owned(),
        phase: OutreachPhase::Initial,
        template_key: "event.press.v1".to_owned(),
        wave_id: None,
        draft: crowdrelay_domain::outreach_letter::OutreachLetter {
            subject: "Waves E2E — our new single".to_owned(),
            body: draft_body.clone(),
        },
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
        VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,
                'request_relationship_outreach',9000,'require_approval','test',
                '{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision:outreach:draft:{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class
         ) VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',$4,$5,$6,
                   'queued','third_party')",
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("action:outreach:draft:{action_id}"))
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;

    sqlx::query(
        "INSERT INTO viryaos_executor_instances (
             workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
         ) VALUES ($1,'n8n-draft-test','test','test-manifest',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities (
             workspace_id, executor_id, capability, capability_version, observed_at, expires_at
         ) VALUES ($1,'n8n-draft-test','outreach.send','1',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;

    use crowdrelay_application::autopilot::AutopilotActionRepository;
    let claim_now = OffsetDateTime::now_utc();
    let claimed = fixture
        .repository
        .claim_due_actions(fixture.workspace_id, 8, claim_now)
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .ok_or("the queued pitch is claimable")?;
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await?;

    let emitted: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id=$1 AND event_type='crowdrelay.outreach.requested'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        emitted["draft"]["subject"].as_str(),
        Some("Waves E2E — our new single")
    );
    assert_eq!(
        emitted["draft"]["body"].as_str(),
        Some(draft_body.as_str()),
        "the executor receives the approved words verbatim"
    );
    assert_eq!(
        emitted["contact_email"].as_str().map(|e| e.contains('@')),
        Some(true)
    );
    Ok(())
}

/// The application the organiser receives is the one the operator read —
/// the payload carries `draft` verbatim, and dispatch refuses a row that
/// predates the field.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dispatched_application_carries_the_approved_letter()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("apply-draft").await?;
    let suffix = fixture.workspace_id.into_uuid().simple().to_string();
    let opportunity_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_team_opportunities (
             workspace_id, opportunity_kind, source, external_key, title,
             organization, contact_email, verified_destination, eligible,
             fit_basis_points, reputation_basis_points,
             confidence_basis_points, status
         ) VALUES ($1,'festival','scout',$2,$3,$4,$5,true,true,8000,
                   5000, 5000, 'awaiting_approval')
         RETURNING id",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("apply-{suffix}"))
    .bind("Summerfest 2027 open call")
    .bind("Summerfest")
    .bind(format!("bookings-{suffix}@example.test"))
    .fetch_one(&fixture.pool)
    .await?;
    let draft_body = format!("approved application {}", Uuid::now_v7());
    let payload = AutopilotActionPayload::ApplyLiveOpportunity {
        opportunity_id: crowdrelay_domain::TeamOpportunityId::from_uuid(opportunity_id),
        opportunity_kind: crowdrelay_domain::live_opportunities::LiveOpportunityKind::Festival,
        score: 80,
        draft: crowdrelay_domain::application_letter::ApplicationLetter {
            subject: "VIRYA — application for Summerfest 2027 open call".to_owned(),
            body: draft_body.clone(),
        },
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
        VALUES ($1,$2,$3,'live_opportunity','team_opportunity',$4,
                'apply_live_opportunity',9000,'require_approval','test',
                '{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision:apply:draft:{decision_id}"))
    .bind(opportunity_id)
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class
         ) VALUES ($1,$2,$3,'live_opportunity','apply_live_opportunity','team_opportunity',$4,$5,$6,
                   'queued','third_party')",
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(opportunity_id)
    .bind(format!("action:apply:draft:{action_id}"))
    .bind(serde_json::to_value(&payload)?)
    .execute(&fixture.pool)
    .await?;

    sqlx::query(
        "INSERT INTO viryaos_executor_instances (
             workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
         ) VALUES ($1,'n8n-draft-test','test','test-manifest',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities (
             workspace_id, executor_id, capability, capability_version, observed_at, expires_at
         ) VALUES ($1,'n8n-draft-test','opportunity.application','1',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;

    use crowdrelay_application::autopilot::AutopilotActionRepository;
    let claimed = fixture
        .repository
        .claim_due_actions(fixture.workspace_id, 8, OffsetDateTime::now_utc())
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .ok_or("the queued application is claimable")?;
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await?;

    let emitted: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id=$1 AND event_type='crowdrelay.opportunity.application_requested'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        emitted["draft"]["body"].as_str(),
        Some(draft_body.as_str()),
        "the executor receives the approved application verbatim"
    );
    assert_eq!(
        emitted["contact_email"].as_str().map(|e| e.contains('@')),
        Some(true)
    );
    Ok(())
}

/// The language pin for the composition fix: production rows carry
/// `country_code` and leave `travel_band` NULL, so a Polish organiser must
/// still get Polish words — the band is a cost hint, not a locale.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_application_speaks_the_organisers_language() -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::{ActionSubject, AutopilotContext, DecisionCandidate};
    use crowdrelay_domain::autonomy::{Confidence, PolicyDisposition};
    use crowdrelay_domain::live_opportunities::LiveOpportunityKind;
    use crowdrelay_domain::{TeamOpportunityId, TraceContext};

    let fixture = fixture("apply-lang").await?;
    let suffix = fixture.workspace_id.into_uuid().simple().to_string();

    async fn seed_opportunity(
        fixture: &Fixture,
        suffix: &str,
        country_code: Option<&str>,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO viryaos_team_opportunities (
                 workspace_id, opportunity_kind, source, external_key, title,
                 organization, contact_email, verified_destination, eligible,
                 fit_basis_points, reputation_basis_points,
                 confidence_basis_points, status, country_code
             ) VALUES ($1,'festival','scout',$2,$3,$4,$5,true,true,8000,
                       5000, 5000, 'new', $6)
             RETURNING id",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(format!("apply-{country_code:?}-{suffix}"))
        .bind(format!("Festiwal {country_code:?} {suffix}"))
        .bind(format!("Org {country_code:?}"))
        .bind(format!("bookings-{suffix}@example.test"))
        .bind(country_code)
        .fetch_one(&fixture.pool)
        .await
        .map_err(Into::into)
    }

    async fn persisted_subject(
        fixture: &Fixture,
        opportunity_id: Uuid,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let trace = TraceContext::root(fixture.workspace_id);
        let candidate = DecisionCandidate {
            context: AutopilotContext::LiveOpportunity,
            subject: ActionSubject::TeamOpportunity(TeamOpportunityId::from_uuid(opportunity_id)),
            decision_kind: "apply_live_opportunity",
            confidence: Confidence::saturating_from_basis_points(9_000),
            disposition: PolicyDisposition::RequireApproval,
            reason: "test candidate",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::ApplyLiveOpportunity {
                opportunity_id: TeamOpportunityId::from_uuid(opportunity_id),
                opportunity_kind: LiveOpportunityKind::Festival,
                score: 80,
                draft: Default::default(),
            },
            decision_key: format!("decision:apply-lang:{opportunity_id}"),
            action_idempotency_key: format!("action:apply-lang:{opportunity_id}"),
        };
        let persisted = fixture
            .repository
            .persist_candidate(fixture.workspace_id, &candidate, &trace)
            .await?;
        assert!(
            persisted.action_created,
            "the candidate must persist an action"
        );
        sqlx::query_scalar::<_, String>(
            "SELECT payload->'draft'->>'subject' FROM viryaos_autopilot_actions
             WHERE workspace_id=$1 AND payload->>'opportunity_id'=$2",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(opportunity_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .map_err(Into::into)
    }

    // The production shape: a Polish destination with no travel band set.
    let polish = seed_opportunity(&fixture, &suffix, Some("PL")).await?;
    let subject = persisted_subject(&fixture, polish).await?;
    assert!(
        subject.contains("zgłoszenie"),
        "a PL organiser gets the Polish application, got: {subject}"
    );

    let german = seed_opportunity(&fixture, &suffix, Some("DE")).await?;
    let subject = persisted_subject(&fixture, german).await?;
    assert!(
        subject.contains("application"),
        "a non-PL organiser gets the English application, got: {subject}"
    );

    // And no locale at all still fails safe to English rather than guessing.
    let unknown = seed_opportunity(&fixture, &suffix, None).await?;
    let subject = persisted_subject(&fixture, unknown).await?;
    assert!(
        subject.contains("application"),
        "a locale-less organiser gets the English application, got: {subject}"
    );
    Ok(())
}
