//! The play state machine against a real Postgres.
//!
//! Everything worth checking here is invisible from the Rust side. The
//! audience query decides who a campaign reaches, and the three ways it can be
//! wrong all look like a working system in a unit test: it can offer the same
//! fan every cycle and never finish, it can thank somebody for attending a show
//! they only expressed interest in, and it can hand a step's whole ceiling out
//! again while the first batch is still queued.
//!
//! The completion guard is the same kind of property: a play completed while a
//! step is still open strands that step for ever, and nothing in the type
//! system says otherwise.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotActionPayload, AutopilotActionRepository, AutopilotDecisionRepository,
    ClaimedAutopilotAction, PlayAnchorRef, PlayAudience, PlayStart, PlayStepPlan,
    PlayStepSettlement,
};
use crowdrelay_domain::{
    AutopilotActionId, EventId, FanId, WorkspaceId,
    action_class::ActionClass,
    plays::{PlayKind, PlayStepKind, StepSkipReason, step_schedule},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_play_starts_once_reaches_a_fan_once_and_only_finishes_when_every_step_is_settled()
-> Result<(), Box<dyn std::error::Error>> {
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
        .bind(format!("plays-e2e-{suffix}"))
        .bind("Plays E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let anchor_at = now + time::Duration::days(30);
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("plays-e2e-show-{suffix}"))
    .bind("Plays E2E show")
    .bind(anchor_at)
    .execute(&pool)
    .await?;

    let fan_id = FanId::new();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(fan_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("fan-{suffix}@example.test"))
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
    // Interest, not attendance. The announce ask accepts it and the post-show
    // ask must not.
    sqlx::query("INSERT INTO event_interests (workspace_id, event_id, fan_id) VALUES ($1,$2,$3)")
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .bind(fan_id.into_uuid())
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

    let anchors = repository
        .load_play_anchors(workspace_id, PlayKind::TrackUsAsk, now)
        .await?;
    let anchor = anchors
        .iter()
        .find(|anchor| anchor.anchor == PlayAnchorRef::Event { event_id })
        .ok_or("the published upcoming show is a play anchor")?;
    assert!(anchor.active, "a published show is an active anchor");
    assert!(
        (700..=725).contains(&anchor.hours_until),
        "a show thirty days out is about seven hundred and twenty hours away, got {}",
        anchor.hours_until
    );

    let start = PlayStart {
        kind: PlayKind::TrackUsAsk,
        anchor: PlayAnchorRef::Event { event_id },
        anchor_at,
        hypothesis: PlayKind::TrackUsAsk.hypothesis(),
        success_metric_platform: PlayKind::TrackUsAsk.success_metric().0,
        success_metric_key: PlayKind::TrackUsAsk.success_metric().1,
        steps: PlayKind::TrackUsAsk
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor_at);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: anchor_at + time::Duration::days(14),
    };
    assert!(repository.start_play(workspace_id, &start).await?);
    assert!(
        !repository.start_play(workspace_id, &start).await?,
        "a second cycle, or a restart mid-cycle, must leave exactly one campaign"
    );
    assert!(
        !repository
            .load_play_anchors(workspace_id, PlayKind::TrackUsAsk, now)
            .await?
            .iter()
            .any(|anchor| anchor.anchor == PlayAnchorRef::Event { event_id }),
        "an anchor that already carries a play is not offered again"
    );

    let snapshots = repository.load_play_snapshots(workspace_id, now).await?;
    let play = snapshots
        .iter()
        .find(|play| play.anchor == PlayAnchorRef::Event { event_id })
        .ok_or("the running play is read back")?;
    assert!(play.anchor_active);
    assert_eq!(play.steps.len(), 2);
    assert!(play.steps.iter().all(|step| !step.settled));
    assert!(play.steps.iter().all(|step| step.recipients_emitted == 0));
    assert_eq!(
        play.steps.first().map(|step| (step.kind, step.class)),
        Some((PlayStepKind::AnnounceAsk, ActionClass::OwnedAudience))
    );
    assert_eq!(
        play.audience,
        PlayAudience::Next {
            fan_id,
            remaining: 1
        },
        "a consented fan who registered interest is the announce ask's audience"
    );
    let play_id = play.play_id;

    // Settling step zero moves the audience to the post-show ask, which does
    // not accept interest. The fan did not buy a ticket, so there is nobody to
    // thank — and the play must say so rather than hold the window open.
    repository
        .settle_play_step(
            workspace_id,
            &PlayStepSettlement {
                play_id,
                step_index: 0,
                reason: StepSkipReason::WindowClosed,
            },
            now,
        )
        .await?;
    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.steps.first().map(|step| step.settled),
        Some(true),
        "the skip is written down"
    );
    assert_eq!(
        play.audience,
        PlayAudience::Exhausted,
        "interest is not attendance: the post-show ask has nobody"
    );

    // The completion guard, before the second step is settled.
    repository.complete_play(workspace_id, play_id, now).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM viryaos_plays WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(play_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        "running",
        "completing a play with an open step would strand that step for ever"
    );

    // Give the fan a paid ticket for this show. Now they attended, and the
    // post-show ask has an audience.
    insert_paid_ticket(&pool, workspace_id, event_id, fan_id, &suffix, now).await?;
    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Next {
            fan_id,
            remaining: 1
        },
        "a ticket buyer is the post-show ask's audience"
    );

    // A committed but undelivered send takes the fan out of the audience and
    // counts against the step's ceiling. Without this the play re-offers the
    // same fan every cycle and never progresses.
    let step_payload = AutopilotActionPayload::RunPlayStep {
        play_id,
        play_kind: PlayKind::TrackUsAsk,
        step_index: 1,
        step_kind: PlayStepKind::PostShowAsk,
        event_id: Some(event_id),
        fan_id: Some(fan_id),
        template_key: PlayStepKind::PostShowAsk.template_key().to_owned(),
    };
    let payload = serde_json::to_value(&step_payload)?;
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        )
        VALUES (
            $1,$2,$3,'plays','fan',$4,'run_play_step',9000,'require_approval',
            'test', '{}'::jsonb, '{}'::jsonb, $5
        )
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision:play-step:v1:{play_id}:1:{fan_id}"))
    .bind(fan_id.into_uuid())
    .bind(&payload)
    .execute(&pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class
        )
        VALUES ($1,$2,$3,'plays','play.step.run','fan',$4,$5,$6,'awaiting_approval','owned_audience')
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(fan_id.into_uuid())
    .bind(format!("action:play-step:{play_id}:1:{fan_id}"))
    .bind(&payload)
    .execute(&pool)
    .await?;
    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Exhausted,
        "a fan with a send already committed is not offered again"
    );
    assert_eq!(
        play.steps.get(1).map(|step| step.recipients_emitted),
        Some(1),
        "an awaiting-approval send has already spent the step's budget"
    );

    // Now execute it for real. This is the only place the dispatch query, the
    // recipient write and the outbox emission run together, and a mistake in
    // any of them is invisible from Rust.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions
         SET status='processing', attempt_count=1, started_at=now()
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(&pool)
    .await?;
    repository
        .execute_action(
            workspace_id,
            &ClaimedAutopilotAction {
                id: AutopilotActionId::from_uuid(action_id),
                payload: step_payload,
                attempt_number: 1,
            },
            now,
        )
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM viryaos_autopilot_actions WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .fetch_one(&pool)
        .await?,
        "succeeded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM viryaos_play_step_recipients AS recipient
             JOIN viryaos_play_steps AS step
               ON step.workspace_id = recipient.workspace_id AND step.id = recipient.step_id
             WHERE recipient.workspace_id=$1 AND step.play_id=$2 AND step.step_index=1
               AND recipient.fan_id=$3"
        )
        .bind(workspace_id.into_uuid())
        .bind(play_id.into_uuid())
        .bind(fan_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        1,
        "a dispatched send is recorded as having reached the fan"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM outbox_events
             WHERE workspace_id=$1 AND event_type='crowdrelay.play.step_requested'"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        1,
        "the send leaves through the existing outbox, not a new path"
    );

    repository
        .settle_play_step(
            workspace_id,
            &PlayStepSettlement {
                play_id,
                step_index: 1,
                reason: StepSkipReason::NoEligibleRecipients,
            },
            now,
        )
        .await?;
    repository.complete_play(workspace_id, play_id, now).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM viryaos_plays WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(play_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        "completed"
    );
    assert!(
        repository
            .load_play_snapshots(workspace_id, now)
            .await?
            .iter()
            .all(|play| play.anchor != PlayAnchorRef::Event { event_id }),
        "a completed play is not read as running work"
    );

    // No workspace cleanup here. `fan_consents` is append-only and its
    // workspace reference is `ON DELETE RESTRICT`, so a consent record cannot
    // be deleted by anything — including a test. The database is disposable;
    // the consent ledger is not.
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_cancelled_show_withdraws_its_play_anchor() -> Result<(), Box<dyn std::error::Error>> {
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
        .bind(format!("plays-cancel-{suffix}"))
        .bind("Plays cancellation E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let anchor_at = now + time::Duration::days(30);
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("plays-cancel-show-{suffix}"))
    .bind("Plays cancellation show")
    .bind(anchor_at)
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
    let start = PlayStart {
        kind: PlayKind::TrackUsAsk,
        anchor: PlayAnchorRef::Event { event_id },
        anchor_at,
        hypothesis: PlayKind::TrackUsAsk.hypothesis(),
        success_metric_platform: PlayKind::TrackUsAsk.success_metric().0,
        success_metric_key: PlayKind::TrackUsAsk.success_metric().1,
        steps: PlayKind::TrackUsAsk
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor_at);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: anchor_at + time::Duration::days(14),
    };
    assert!(repository.start_play(workspace_id, &start).await?);

    sqlx::query("UPDATE events SET status='cancelled' WHERE workspace_id=$1 AND id=$2")
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .execute(&pool)
        .await?;
    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert!(
        !play.anchor_active,
        "a cancelled show must not be promoted, and the play has to be able to see that"
    );

    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;
    Ok(())
}

async fn one_play(
    repository: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    event_id: EventId,
) -> Result<crowdrelay_application::autopilot::PlayRunSnapshot, Box<dyn std::error::Error>> {
    repository
        .load_play_snapshots(workspace_id, now)
        .await?
        .into_iter()
        .find(|play| play.anchor == PlayAnchorRef::Event { event_id })
        .ok_or_else(|| "the running play is read back".into())
}

async fn insert_paid_ticket(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    event_id: EventId,
    fan_id: FanId,
    suffix: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admission_pools (id, workspace_id, event_id, slug, name, capacity)
         VALUES ($1,$2,$3,$4,'General',100)",
    )
    .bind(pool_id)
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(format!("pool-{suffix}"))
    .execute(pool)
    .await?;
    let sale_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ticket_sales (
             id, workspace_id, event_id, admission_pool_id, capacity,
             sales_open_at, sales_close_at
         ) VALUES ($1,$2,$3,$4,100,$5,$6)",
    )
    .bind(sale_id)
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(pool_id)
    .bind(now - time::Duration::days(30))
    .bind(now + time::Duration::days(29))
    .execute(pool)
    .await?;
    let email = sqlx::query_scalar::<_, String>(
        "SELECT normalized_email FROM fans WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO ticket_orders (
            workspace_id, ticket_sale_id, public_reference, status, buyer_email,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, reservation_key, request_hash, checkout_token_hash,
            expires_at, paid_at
        ) VALUES (
            $1,$2,$3,'paid',$4,'PLN',10800,10000,800,800,$5,
            sha256($6::bytea), sha256($7::bytea), $8, $9
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(sale_id)
    .bind(format!("VRY-ORD-{}", suffix[..16].to_uppercase()))
    .bind(email)
    .bind(format!("reservation-{suffix}"))
    .bind(format!("request-{suffix}").into_bytes())
    .bind(format!("checkout-{suffix}").into_bytes())
    .bind(now + time::Duration::days(1))
    .bind(now - time::Duration::hours(1))
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_sweep_play_runs_once_for_its_show_and_reaches_nobody()
-> Result<(), Box<dyn std::error::Error>> {
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
        .bind(format!("sweep-e2e-{suffix}"))
        .bind("Sweep E2E")
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();
    let anchor_at = now + time::Duration::days(30);
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("sweep-e2e-show-{suffix}"))
    .bind("Sweep E2E show")
    .bind(anchor_at)
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
    let kind = PlayKind::ListingCompletenessSweep;
    let start = PlayStart {
        kind,
        anchor: PlayAnchorRef::Event { event_id },
        anchor_at,
        hypothesis: kind.hypothesis(),
        success_metric_platform: kind.success_metric().0,
        success_metric_key: kind.success_metric().1,
        steps: kind
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor_at);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: anchor_at + time::Duration::days(14),
    };
    assert!(repository.start_play(workspace_id, &start).await?);

    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(play.kind, kind);
    assert_eq!(
        play.steps.first().map(|step| step.class),
        Some(ActionClass::FirstPartyReversible),
        "the sweep is first-party work and the class ceiling should treat it so"
    );
    assert_eq!(
        play.audience,
        PlayAudience::NotRequired,
        "a step that needs nobody must not be measured against an audience it does not have"
    );

    // Dispatch it with no recipient at all.
    let payload = AutopilotActionPayload::RunPlayStep {
        play_id: play.play_id,
        play_kind: kind,
        step_index: 0,
        step_kind: PlayStepKind::ListingSweep,
        event_id: Some(event_id),
        fan_id: None,
        template_key: PlayStepKind::ListingSweep.template_key().to_owned(),
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        )
        VALUES ($1,$2,$3,'plays','event',$4,'run_play_step',9000,'auto_execute',
                'test','{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision:play-step:v1:{}:0:anchor", play.play_id))
    .bind(event_id.into_uuid())
    .bind(serde_json::to_value(&payload)?)
    .execute(&pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, attempt_count, started_at
        )
        VALUES ($1,$2,$3,'plays','play.step.run','event',$4,$5,$6,'processing',
                'first_party_reversible',1,now())
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id.into_uuid())
    .bind(format!("action:play-step:{}:0:anchor", play.play_id))
    .bind(serde_json::to_value(&payload)?)
    .execute(&pool)
    .await?;

    repository
        .execute_action(
            workspace_id,
            &ClaimedAutopilotAction {
                id: AutopilotActionId::from_uuid(action_id),
                payload,
                attempt_number: 1,
            },
            now,
        )
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM outbox_events
             WHERE workspace_id=$1 AND event_type='crowdrelay.play.step_requested'"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM viryaos_play_step_recipients WHERE workspace_id=$1"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        0,
        "a sweep reaches nobody, and recording a recipient would be a contact that never happened"
    );
    // No workspace cleanup: the emitted outbox event holds a RESTRICT
    // reference, which is the delivery ledger refusing to lose a dispatched
    // intent. The database is disposable; the ledger is not.
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_ladder_is_anchored_on_one_engaged_fan_and_needs_a_tracked_link()
-> Result<(), Box<dyn std::error::Error>> {
    // Everything that could go quietly wrong here is in the SQL. A fan anchor
    // read through the show query returns nothing and the play looks like it
    // ran; an engagement filter that matches everybody turns the ladder into a
    // mailing list; and a missing link turns the one call to action into a
    // message with nowhere to go.
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
        .bind(format!("ladder-e2e-{suffix}"))
        .bind("Ladder E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("ladder-e2e-show-{suffix}"))
    .bind("Ladder E2E show")
    .bind(now - time::Duration::days(60))
    .execute(&pool)
    .await?;

    // Engaged: a paid ticket inside the last year.
    let engaged = FanId::new();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(engaged.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("engaged-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1,$2,'marketing',true,'v1','test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(engaged.into_uuid())
    .execute(&pool)
    .await?;
    insert_paid_ticket(&pool, workspace_id, event_id, engaged, &suffix, now).await?;

    // Consented but inert: on the list, has never done anything. The ladder is
    // for people who came, not for everybody who can be written to.
    let inert = FanId::new();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(inert.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("inert-{suffix}@example.test"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1,$2,'marketing',true,'v1','test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(inert.into_uuid())
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

    // No tracked link yet, so there is nothing to ask people to click.
    assert!(
        repository
            .load_play_anchors(workspace_id, PlayKind::FollowAskLadder, now)
            .await?
            .is_empty(),
        "without the operator's tracked link the ladder has nowhere to send anybody"
    );

    sqlx::query(
        "INSERT INTO smart_links (workspace_id, slug, destination_url, active)
         VALUES ($1,'follow','https://example.test/follow',true)",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let anchors = repository
        .load_play_anchors(workspace_id, PlayKind::FollowAskLadder, now)
        .await?;
    assert_eq!(
        anchors
            .iter()
            .map(|anchor| anchor.anchor)
            .collect::<Vec<_>>(),
        vec![PlayAnchorRef::Fan { fan_id: engaged }],
        "only the fan who actually did something is a ladder anchor"
    );
    let anchor = anchors.first().copied().ok_or("one anchor")?;
    assert!(anchor.active);
    assert_eq!(
        anchor.hours_until, 0,
        "the anchor is the moment they qualified, not a date in the future"
    );

    let start = PlayStart {
        kind: PlayKind::FollowAskLadder,
        anchor: anchor.anchor,
        anchor_at: anchor.anchor_at,
        hypothesis: PlayKind::FollowAskLadder.hypothesis(),
        success_metric_platform: PlayKind::FollowAskLadder.success_metric().0,
        success_metric_key: PlayKind::FollowAskLadder.success_metric().1,
        steps: PlayKind::FollowAskLadder
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor.anchor_at);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: anchor.anchor_at + time::Duration::days(150),
    };
    assert!(repository.start_play(workspace_id, &start).await?);
    assert!(
        !repository.start_play(workspace_id, &start).await?,
        "one ladder per fan, for ever"
    );

    let snapshots = repository.load_play_snapshots(workspace_id, now).await?;
    let play = snapshots
        .iter()
        .find(|play| play.anchor == PlayAnchorRef::Fan { fan_id: engaged })
        .ok_or("the ladder is a running play")?;
    assert!(play.anchor_active);
    assert_eq!(
        play.audience,
        PlayAudience::Next {
            fan_id: engaged,
            remaining: 1
        },
        "the anchor fan is the whole audience of their own ladder"
    );

    // Dispatch the first rung, and the emitted intent must carry the tracked
    // link and no show.
    let payload = AutopilotActionPayload::RunPlayStep {
        play_id: play.play_id,
        play_kind: PlayKind::FollowAskLadder,
        step_index: 0,
        step_kind: PlayStepKind::FollowAskFirst,
        event_id: None,
        fan_id: Some(engaged),
        template_key: PlayStepKind::FollowAskFirst.template_key().to_owned(),
    };
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        )
        VALUES ($1,$2,$3,'plays','fan',$4,'run_play_step',9000,'auto_execute',
                'test','{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "decision:play-step:v1:{}:0:{engaged}",
        play.play_id
    ))
    .bind(engaged.into_uuid())
    .bind(serde_json::to_value(&payload)?)
    .execute(&pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, attempt_count, started_at
        )
        VALUES ($1,$2,$3,'plays','play.step.run','fan',$4,$5,$6,'processing',
                'owned_audience',1,now())
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(engaged.into_uuid())
    .bind(format!("action:play-step:{}:0:{engaged}", play.play_id))
    .bind(serde_json::to_value(&payload)?)
    .execute(&pool)
    .await?;
    repository
        .execute_action(
            workspace_id,
            &ClaimedAutopilotAction {
                id: AutopilotActionId::from_uuid(action_id),
                payload,
                attempt_number: 1,
            },
            now,
        )
        .await?;
    let emitted = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM outbox_events
         WHERE workspace_id=$1 AND event_type='crowdrelay.play.step_requested'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        emitted
            .get("call_to_action_url")
            .and_then(|url| url.as_str()),
        Some("/l/follow"),
        "the one call to action is the operator's tracked link"
    );
    assert!(
        emitted.get("event").is_some_and(serde_json::Value::is_null),
        "a ladder has no show, and rendering one would be an ask about the wrong thing"
    );

    // The rung has now been committed to, so the anchor fan is no longer
    // eligible for it and the ladder waits rather than re-sending.
    let advanced = repository.load_play_snapshots(workspace_id, now).await?;
    let play = advanced
        .iter()
        .find(|play| play.anchor == PlayAnchorRef::Fan { fan_id: engaged })
        .ok_or("still running")?;
    assert_eq!(play.audience, PlayAudience::Exhausted);

    // Withdrawing consent withdraws the anchor: the remaining rungs are the
    // agent's to skip, not to send.
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1,$2,'marketing',false,'v1','test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(engaged.into_uuid())
    .execute(&pool)
    .await?;
    let withdrawn = repository.load_play_snapshots(workspace_id, now).await?;
    let play = withdrawn
        .iter()
        .find(|play| play.anchor == PlayAnchorRef::Fan { fan_id: engaged })
        .ok_or("still running")?;
    assert!(
        !play.anchor_active,
        "a fan who withdrew consent is a withdrawn anchor, exactly like a cancelled show"
    );
    Ok(())
}
