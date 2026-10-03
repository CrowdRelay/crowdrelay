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

use crate::common;
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
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_new_track_us_play_has_one_announce_step_and_finishes_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
    assert_eq!(
        start.steps.len(),
        1,
        "new track-us plays have one owner before the show"
    );
    assert_eq!(start.steps[0].kind, PlayStepKind::AnnounceAsk);
    assert!(repository.start_play(workspace_id, &start).await?);

    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Next {
            fan_id,
            remaining: 1
        },
        "interest is enough for the pre-show follow ask"
    );
    let play_id = play.play_id;

    repository
        .settle_play_step(
            workspace_id,
            &PlayStepSettlement {
                play_id,
                step_index: 0,
                reason: Some(StepSkipReason::WindowClosed),
            },
            now,
        )
        .await?;
    repository.complete_play(workspace_id, play_id, now).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM plays WHERE workspace_id=$1 AND id=$2")
            .bind(workspace_id.into_uuid())
            .bind(play_id.into_uuid())
            .fetch_one(&pool)
            .await?,
        "completed",
        "a one-step track-us play finishes without leaving a hidden post-show rung"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_legacy_post_show_step_uses_observed_attendance_only_when_show_growth_is_off()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("legacy-post-show-{suffix}"))
        .bind("Legacy post-show E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let anchor_at = now - time::Duration::hours(24);
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("legacy-post-show-{suffix}"))
    .bind("Legacy post-show")
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
    .bind(format!("legacy-{suffix}@example.test"))
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
    insert_paid_ticket(&pool, workspace_id, event_id, fan_id, &suffix, now).await?;

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
        steps: vec![
            PlayStepPlan {
                index: 0,
                kind: PlayStepKind::AnnounceAsk,
                class: PlayStepKind::AnnounceAsk.action_class(),
                due_at: anchor_at - time::Duration::days(14),
                expires_at: anchor_at - time::Duration::days(7),
            },
            PlayStepPlan {
                index: 1,
                kind: PlayStepKind::PostShowAsk,
                class: PlayStepKind::PostShowAsk.action_class(),
                due_at: anchor_at + time::Duration::hours(18),
                expires_at: anchor_at + time::Duration::days(3),
            },
        ],
        measurement_window_end: anchor_at + time::Duration::days(14),
    };
    assert!(repository.start_play(workspace_id, &start).await?);
    let play = one_play(&repository, workspace_id, now, event_id).await?;
    let play_id = play.play_id;
    repository
        .settle_play_step(
            workspace_id,
            &PlayStepSettlement {
                play_id,
                step_index: 0,
                reason: Some(StepSkipReason::WindowClosed),
            },
            now,
        )
        .await?;

    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Exhausted,
        "buying a ticket is not evidence that the fan was in the room"
    );

    let campaign_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concert_qr_campaigns
         (id, workspace_id, event_id, label, valid_from, valid_until)
         VALUES ($1,$2,$3,'legacy-test',$4,$5)",
    )
    .bind(campaign_id)
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(anchor_at - time::Duration::hours(2))
    .bind(anchor_at + time::Duration::hours(8))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO concert_checkins
         (workspace_id, event_id, campaign_id, fan_id, checked_in_at, identity_source)
         VALUES ($1,$2,$3,$4,$5,'session')",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(campaign_id)
    .bind(fan_id.into_uuid())
    .bind(anchor_at + time::Duration::hours(2))
    .execute(&pool)
    .await?;

    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1,'show_growth',true,'require_approval',14)
         ON CONFLICT (workspace_id, context)
         DO UPDATE SET enabled=true",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Exhausted,
        "Show Growth owns the observed room when that context is enabled"
    );

    sqlx::query(
        "UPDATE autopilot_policies SET enabled=false
         WHERE workspace_id=$1 AND context='show_growth'",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let play = one_play(&repository, workspace_id, now, event_id).await?;
    assert_eq!(
        play.audience,
        PlayAudience::Next {
            fan_id,
            remaining: 1
        },
        "a persisted legacy rung remains a safe fallback when Show Growth is disabled"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_cancelled_show_withdraws_its_play_anchor() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        , trace_id)
        VALUES ($1,$2,$3,'plays','event',$4,'run_play_step',9000,'auto_execute',
                'test','{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
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
        INSERT INTO autopilot_actions (
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
            "SELECT count(*) FROM play_step_recipients WHERE workspace_id=$1"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        0,
        "a sweep reaches nobody, and recording a recipient would be a contact that never happened"
    );
    let result = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT result FROM play_steps WHERE workspace_id=$1 AND play_id=$2 AND step_index=0",
    )
    .bind(workspace_id.into_uuid())
    .bind(play.play_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(
        result["missing"]
            .as_array()
            .is_some_and(|missing| missing.iter().any(|item| item == "ticket_url")),
        "a show with no ticket link is a finding the sweep reports, got {result}"
    );
    assert!(
        result.get("proposed_fix").is_none(),
        "no sale is open, so there is no link the sweep can honestly propose"
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
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        , trace_id)
        VALUES ($1,$2,$3,'plays','fan',$4,'run_play_step',9000,'auto_execute',
                'test','{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
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
        INSERT INTO autopilot_actions (
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
    let expected_follow_path = format!("/l/play-follow-{}", action_id.simple());
    assert_eq!(
        emitted
            .get("call_to_action_url")
            .and_then(|url| url.as_str()),
        Some(expected_follow_path.as_str()),
        "the send owns a deterministic redirect to the operator's follow destination"
    );
    let action_link: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT destination_url,action_id
         FROM smart_links
         WHERE workspace_id=$1 AND slug=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(expected_follow_path.trim_start_matches("/l/"))
    .fetch_one(&pool)
    .await?;
    assert_eq!(action_link.0, "https://example.test/follow");
    assert_eq!(
        action_link.1,
        Some(action_id),
        "click ownership is exact action identity, not a shared follow slug"
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

/// One listing sweep over one event: starts the play, dispatches the step,
/// executes it, and hands back the play id plus what the step recorded.
///
/// Factored because the propose/apply cycle runs twice below — once under
/// the default ask-first posture and once under `bounded_auto` — and the
/// difference between them is exactly one policy row, not two test bodies.
async fn run_listing_sweep(
    pool: &sqlx::PgPool,
    repository: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: EventId,
    anchor_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(Uuid, serde_json::Value), Box<dyn std::error::Error>> {
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
    repository.start_play(workspace_id, &start).await?;
    let play = one_play(repository, workspace_id, now, event_id).await?;
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
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
        VALUES ($1,$2,$3,'plays','event',$4,'run_play_step',9000,'auto_execute',
                'test','{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "decision:sweep:{}:{}",
        play.play_id,
        Uuid::now_v7()
    ))
    .bind(event_id.into_uuid())
    .bind(serde_json::to_value(&payload)?)
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
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
    .bind(format!("action:sweep:{}:{}", play.play_id, Uuid::now_v7()))
    .bind(serde_json::to_value(&payload)?)
    .execute(pool)
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
    let result = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT result FROM play_steps WHERE workspace_id=$1 AND play_id=$2 AND step_index=0",
    )
    .bind(workspace_id.into_uuid())
    .bind(play.play_id.into_uuid())
    .fetch_one(pool)
    .await?;
    Ok((play.play_id.into_uuid(), result))
}

/// The event a sweep checks: published, thirty days out, a live sale, and no
/// ticket link — the one gap the sweep is allowed to close itself.
async fn insert_sellable_unlinked_event(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    suffix: &str,
    anchor_at: OffsetDateTime,
) -> Result<EventId, Box<dyn std::error::Error>> {
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("fixable-show-{suffix}"))
    .bind("Fixable show")
    .bind(anchor_at)
    .execute(pool)
    .await?;
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
    sqlx::query(
        "INSERT INTO ticket_sales (
             id, workspace_id, event_id, admission_pool_id, capacity,
             sales_open_at, sales_close_at
         ) VALUES ($1,$2,$3,$4,100,$5,$6)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(pool_id)
    .bind(anchor_at - time::Duration::days(30))
    .bind(anchor_at - time::Duration::days(1))
    .execute(pool)
    .await?;
    Ok(event_id)
}

/// Queues the fix action a sweep proposed and runs it, the same path the
/// dispatcher takes after an approval click or a `bounded_auto` queue.
async fn approve_and_run_fix(
    pool: &sqlx::PgPool,
    repository: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: EventId,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let (fix_id, fix_payload) = sqlx::query_as::<_, (Uuid, serde_json::Value)>(
        "SELECT id, payload FROM autopilot_actions
         WHERE workspace_id=$1 AND action_kind='event.ticket_url.set' AND subject_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_one(pool)
    .await?;
    sqlx::query("UPDATE autopilot_actions SET status='queued' WHERE workspace_id=$1 AND id=$2")
        .bind(workspace_id.into_uuid())
        .bind(fix_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE autopilot_actions SET status='processing', attempt_count=1, started_at=now()
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(fix_id)
    .execute(pool)
    .await?;
    repository
        .execute_action(
            workspace_id,
            &ClaimedAutopilotAction {
                id: AutopilotActionId::from_uuid(fix_id),
                payload: serde_json::from_value(fix_payload)?,
                attempt_number: 1,
            },
            now,
        )
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_sweep_proposes_the_ticket_fix_in_ask_mode_and_applies_it_in_alone_mode()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("sweep-fix-{suffix}"))
        .bind("Sweep Fix E2E")
        .execute(&pool)
        .await?;
    // The link is built on the tenant's own site. Without this row it used to
    // be built on the first tenant's, through a shipped default.
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://sweep-fix.example')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let now = OffsetDateTime::now_utc();
    let anchor_at = now + time::Duration::days(30);
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    // Ask mode: the seeded 'plays' policy is require_approval, so the fix is
    // a proposal — recorded, attributable, and not yet applied.
    let event_id = insert_sellable_unlinked_event(&pool, workspace_id, &suffix, anchor_at).await?;
    let (_play_id, result) =
        run_listing_sweep(&pool, &repository, workspace_id, event_id, anchor_at, now).await?;
    assert_eq!(
        result
            .pointer("/proposed_fix/status")
            .and_then(|s| s.as_str()),
        Some("awaiting_approval"),
        "under ask-first posture the fix waits for a person, got {result}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM autopilot_actions
             WHERE workspace_id=$1 AND action_kind='event.ticket_url.set'"
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        "awaiting_approval"
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT ticket_url FROM events WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .fetch_one(&pool)
        .await?
        .is_none(),
        "a proposed fix has not written anything — the proposal is not the write"
    );

    approve_and_run_fix(&pool, &repository, workspace_id, event_id, now).await?;
    let expected_url = format!("https://sweep-fix.example/live/fixable-show-{suffix}");
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT ticket_url FROM events WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .fetch_one(&pool)
        .await?
        .as_deref(),
        Some(expected_url.as_str()),
        "an approved fix writes the show's own sale page as the ticket link"
    );

    // Alone mode: the same finding queues the fix itself, and applying it is
    // the dispatcher's next pass — the sweep never writes `events` itself.
    sqlx::query(
        "UPDATE autopilot_policies SET autonomy_level='bounded_auto'
         WHERE workspace_id=$1 AND context='plays'",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let second_event =
        insert_sellable_unlinked_event(&pool, workspace_id, &format!("{suffix}b"), anchor_at)
            .await?;
    let (_play_id, result) = run_listing_sweep(
        &pool,
        &repository,
        workspace_id,
        second_event,
        anchor_at,
        now,
    )
    .await?;
    assert_eq!(
        result
            .pointer("/proposed_fix/status")
            .and_then(|s| s.as_str()),
        Some("queued"),
        "under bounded_auto the fix queues without waiting on a person, got {result}"
    );
    approve_and_run_fix(&pool, &repository, workspace_id, second_event, now).await?;
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT ticket_url FROM events WHERE workspace_id=$1 AND id=$2"
        )
        .bind(workspace_id.into_uuid())
        .bind(second_event.into_uuid())
        .fetch_one(&pool)
        .await?
        .as_deref(),
        Some(format!("https://sweep-fix.example/live/fixable-show-{suffix}b").as_str()),
        "the queued fix applies on its own dispatch"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_third_party_step_parks_behind_its_own_capability()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("cap-split-{suffix}"))
        .bind("Capability Split")
        .execute(&pool)
        .await?;

    // Two parked play-step dispatches: an owned-audience rung and the one
    // step that reaches outside the workspace. The posture read must not
    // price them as the same missing lane.
    for (step_kind, key) in [
        (PlayStepKind::FollowAskFirst, "owned"),
        (PlayStepKind::ReleaseCuratorWave, "third-party"),
    ] {
        let payload = serde_json::to_value(AutopilotActionPayload::RunPlayStep {
            play_id: crowdrelay_domain::PlayId::new(),
            play_kind: PlayKind::ReleaseRunway,
            step_index: 0,
            step_kind,
            event_id: None,
            fan_id: None,
            template_key: step_kind.template_key().to_owned(),
        })?;
        let decision_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
            VALUES ($1,$2,$3,'plays','event',$4,'run_play_step',9000,'auto_execute',
                    'test','{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
            "#,
        )
        .bind(decision_id)
        .bind(workspace_id.into_uuid())
        .bind(format!("decision:cap:{key}:{suffix}"))
        .bind(Uuid::now_v7())
        .bind(&payload)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO autopilot_actions (
                id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
                idempotency_key, payload, status, action_class, last_error_kind
            )
            VALUES ($1,$2,$3,'plays','play.step.run','event',$4,$5,$6,'queued',
                    'first_party_reversible','awaiting_executor')
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(decision_id)
        .bind(Uuid::now_v7())
        .bind(format!("action:cap:{key}:{suffix}"))
        .bind(&payload)
        .execute(&pool)
        .await?;
    }

    let report = crowdrelay_infra::autopilot::executor_capability_posture(
        &pool,
        workspace_id,
        OffsetDateTime::now_utc(),
    )
    .await?;
    let missing: Vec<&str> = report
        .capabilities
        .iter()
        .filter(|cap| cap.state == "missing")
        .map(|cap| cap.capability.as_str())
        .collect();
    assert!(
        missing.contains(&"play.step"),
        "an owned-audience step parks behind play.step, got {missing:?}"
    );
    assert!(
        missing.contains(&"play.step.third_party"),
        "a curator wave parks behind its own name, got {missing:?}"
    );
    Ok(())
}
