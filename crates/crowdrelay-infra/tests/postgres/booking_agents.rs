//! The booking-agent approach against a real Postgres (§4h-10, §12-5).
//!
//! What these tests exist for is what the schema enforces that the domain
//! gate cannot see from a unit test: the workspace scoping on every read and
//! write, the `booking_agent` vocabulary the widened CHECKs accept, the
//! `awaiting_approval` row the request files, the dispatch re-gate under the
//! row lock, and the `refused_until` / `do_not_contact` stamps a filed reply
//! writes. A unit test over the same code would pass while every one of
//! those constraints silently failed to migrate.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotBookingAgentStateRepository, AutopilotContext,
    AutopilotControlRepository, AutopilotDecisionRepository, AutopilotRuntimeRepository,
    ClaimExecution, ExecutorReportStatus, RecordBookingAgentReply, RecordExecutionReport,
};
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_domain::booking_agent::BookingAgentReplyDisposition;
use crowdrelay_domain::{AutopilotActionId, BookingAgentId, WorkspaceId};
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use crowdrelay_infra::booking_agents::{
    BookingAgentApproachOutcome, BookingAgentError, PostgresBookingAgentRepository,
};
use crowdrelay_infra::config::DatabaseConfig;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn test_pool() -> Result<(PgPool, String), Box<dyn std::error::Error>> {
    Ok(common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?)
}

async fn insert_workspace(pool: &PgPool) -> Result<Uuid, sqlx::Error> {
    let workspace_id = WorkspaceId::new().into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(format!("agent-e2e-{}", workspace_id.simple()))
        .bind("Agent E2E")
        .execute(pool)
        .await?;
    Ok(workspace_id)
}

fn key() -> IdempotencyKey {
    IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7())).unwrap()
}

fn autopilot(
    pool: &PgPool,
    url: &str,
) -> Result<PostgresAutopilotRepository, Box<dyn std::error::Error>> {
    Ok(PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    ))
}

/// An agent row the way a screened promotion writes it: a confirmed route
/// (`contact_verified_at`), active, no flags. The variant that never had a
/// route confirmed is the gate's own test below.
async fn insert_agent(
    pool: &PgPool,
    workspace_id: Uuid,
    verified: bool,
) -> Result<Uuid, sqlx::Error> {
    let agent_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO booking_agents \
            (workspace_id, id, name, agency, contact_email, contact_verified_at) \
         VALUES ($1, $2, $3, 'Agency Under Test', $4, \
                 CASE WHEN $5 THEN now() ELSE NULL END)",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .bind(format!("Agent {}", &agent_id.simple().to_string()[..8]))
    .bind(format!("agent-{}@example.test", agent_id.simple()))
    .bind(verified)
    .execute(pool)
    .await?;
    Ok(agent_id)
}

/// A draw that clears the floor: two played shows in the window, twenty
/// distinct paid buyers, three tickets a head — the numbers an application
/// may honestly cite. Each figure is written as the ledger records it, so a
/// reading of `None` is only ever the system's failure to look.
async fn seed_agent_draw(pool: &PgPool, workspace_id: Uuid) -> Result<(), sqlx::Error> {
    let city_slug = format!("draw-city-{}", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'Draw City', 'PL')")
        .bind(&city_slug)
        .execute(pool)
        .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(&city_slug)
    .fetch_one(pool)
    .await?;

    for show in 0..2_i32 {
        let event_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events \
                (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at) \
             VALUES ($1, $2, $3, $4, 'Draw Night', 'Klub Draw', \
                     now() - interval '20 days' - ($5::int * interval '20 days'), \
                     'completed', now() - interval '60 days')",
        )
        .bind(event_id)
        .bind(workspace_id)
        .bind(city_id)
        .bind(format!("draw-night-{}", Uuid::now_v7().simple()))
        .bind(show)
        .execute(pool)
        .await?;
        let pool_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO admission_pools (id, workspace_id, event_id, slug, name, capacity) \
             VALUES ($1,$2,$3,$4,'ga',200)",
        )
        .bind(pool_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(format!("ga-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await?;
        let sale_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO ticket_sales \
                (id, workspace_id, event_id, admission_pool_id, capacity, \
                 sales_open_at, sales_close_at) \
             VALUES ($1,$2,$3,$4,200, now() - interval '50 days', now() - interval '21 days')",
        )
        .bind(sale_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(pool_id)
        .execute(pool)
        .await?;
        let type_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO ticket_types \
                (id, workspace_id, ticket_sale_id, slug, name, price_gross_minor) \
             VALUES ($1,$2,$3,$4,'GA',10800)",
        )
        .bind(type_id)
        .bind(workspace_id)
        .bind(sale_id)
        .bind(format!("ga-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await?;
        // Twenty buyers a show — distinct people, three tickets each.
        let salt = Uuid::now_v7().simple().to_string();
        sqlx::query(
            r#"
            INSERT INTO ticket_orders (
                workspace_id, ticket_sale_id, public_reference, status, buyer_email,
                currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
                vat_rate_basis_points, reservation_key, request_hash, checkout_token_hash,
                expires_at, paid_at
            )
            SELECT $1, $2,
                   'VRY-ORD-' || upper(lpad(to_hex(g + $4::int), 16, '0')),
                   'paid', 'buyer-' || g || '-' || $3 || '@example.test',
                   'PLN', 32400, 30000, 2400, 800,
                   'resv-' || $3 || '-' || g,
                   sha256(('req-' || $3 || '-' || g)::bytea),
                   sha256(('chk-' || $3 || '-' || g)::bytea),
                   now() + interval '1 day', now() - interval '30 days'
            FROM generate_series(1, 20) AS g
            "#,
        )
        .bind(workspace_id)
        .bind(sale_id)
        .bind(&salt)
        .bind(show * 1_000_000)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO ticket_order_items (
                workspace_id, ticket_order_id, ticket_type_id, quantity,
                unit_gross_minor, unit_net_minor, unit_vat_minor,
                total_gross_minor, total_net_minor, total_vat_minor
            )
            SELECT $1, o.id, $3, 3, 10800, 10000, 800, 32400, 30000, 2400
            FROM ticket_orders o
            WHERE o.workspace_id = $1 AND o.ticket_sale_id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(sale_id)
        .bind(type_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The capability the dispatch half is gated on — registered the way a live
/// executor registers, so the claim path exercises the real check rather
/// than the no-registry fail-open.
async fn advertise_approach_capability(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<(), sqlx::Error> {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO executor_instances \
            (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at) \
         VALUES ($1, 'n8n-agent-test', 'test', 'test-manifest', $2, $3)",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO executor_capabilities \
            (workspace_id, executor_id, capability, capability_version, observed_at, expires_at) \
         VALUES ($1, 'n8n-agent-test', 'booking_agent.approach', '1', $2, $3)",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

/// §4h-10 — the registry's own approach path: tenant-scoped, gated on a
/// verified route and real draw, filed as an approval, and one letter per
/// season either way.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_registry_approach_queues_once_and_spends_the_season()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let other_workspace = insert_workspace(&pool).await?;
    let repo = PostgresBookingAgentRepository::new(pool.clone());
    let autopilot = autopilot(&pool, &url)?;

    // The workspace provisioned its policy on insert — posture exists for
    // the context the same day the tenant does.
    let provisioned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM autopilot_policies \
         WHERE workspace_id = $1 AND context = 'booking_agent' \
           AND autonomy_level = 'require_approval')",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert!(provisioned, "the booking_agent policy was not provisioned");

    // The provisioned row must read back through the eval's policy load —
    // production lost every cycle for three hours the day a context the
    // CHECK accepts could not be parsed by the storage mapping.
    let policies = autopilot.load_policies(workspace_id.into()).await?;
    assert!(
        policies
            .iter()
            .any(|policy| policy.context == AutopilotContext::BookingAgent),
        "load_policies dropped the booking_agent row"
    );

    let agent_id = insert_agent(&pool, workspace_id, true).await?;

    // Tenant isolation: another workspace sees no agents and cannot ask
    // for one it does not own.
    assert!(
        repo.list_agents(other_workspace).await?.is_empty(),
        "another tenant's registry leaked"
    );
    assert!(matches!(
        repo.request_approach(other_workspace, agent_id, None, &key())
            .await,
        Err(BookingAgentError::NotFound),
    ));

    // No draw on the books: a measured zero refuses like an absent reading
    // — the pitch would be a claim, not evidence.
    match repo
        .request_approach(workspace_id, agent_id, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("evidence"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a draw refusal, got {other:?}"),
    }
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(queued, 0, "a thin pitch queued an approach anyway");

    seed_agent_draw(&pool, workspace_id).await?;

    // A route nobody confirmed refuses before the numbers are read — the
    // route is a standing gate, not a property of the season.
    let unverified = insert_agent(&pool, workspace_id, false).await?;
    match repo
        .request_approach(workspace_id, unverified, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("confirmed"), "unexpected refusal: {reason}")
        }
        other => panic!("expected an unverified-route refusal, got {other:?}"),
    }

    // Real draw and a confirmed route: the request queues an approval, and
    // the evidence snapshot it was decided on rides inside the payload.
    let idempotency_key = key();
    let action_id = match repo
        .request_approach(
            workspace_id,
            agent_id,
            Some("the Testowice run is ours"),
            &idempotency_key,
        )
        .await?
    {
        BookingAgentApproachOutcome::Queued { action_id } => action_id,
        other => panic!("expected a queued approach, got {other:?}"),
    };
    let (status, context, action_class): (String, String, String) = sqlx::query_as(
        "SELECT status, context, action_class FROM autopilot_actions \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (status.as_str(), context.as_str(), action_class.as_str()),
        ("awaiting_approval", "booking_agent", "third_party"),
        "the request must wait on a person before it leaves"
    );
    let evidence_ok: bool = sqlx::query_scalar(
        "SELECT (payload->'evidence'->>'paid_tickets_12m')::int >= 50 \
         FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert!(evidence_ok, "the approval payload carries no real draw");
    let draft_ready: bool = sqlx::query_scalar(
        "SELECT length(trim(payload->'draft'->>'subject')) > 0 \
         AND length(trim(payload->'draft'->>'body')) > 0 \
         FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        draft_ready,
        "the approval payload must carry the finished letter, not a promise of one"
    );

    // A retried submit replays the same action; a different key is a
    // different ask, and the pending one already spent it.
    match repo
        .request_approach(workspace_id, agent_id, None, &idempotency_key)
        .await?
    {
        BookingAgentApproachOutcome::Replayed {
            action_id: replayed,
            ..
        } => {
            assert_eq!(replayed, action_id)
        }
        other => panic!("expected a replayed approach, got {other:?}"),
    }
    match repo
        .request_approach(workspace_id, agent_id, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("waiting"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a pending refusal, got {other:?}"),
    }

    // The read surface shows the pending ask — the band sees the door
    // before they knock again, and the address never leaves the row.
    let listed = repo.list_agents(workspace_id).await?;
    let listed_agent = listed
        .iter()
        .find(|row| row.agent_id == agent_id)
        .expect("the agent lists");
    assert!(listed_agent.approach_pending);
    assert!(listed_agent.route_verified);

    // Approve and dispatch: the letter emits, the season is spent on the
    // registry row and the ledger, and the reply measurement is scheduled.
    advertise_approach_capability(&pool, workspace_id).await?;
    autopilot
        .approve_action(
            WorkspaceId::from_uuid(workspace_id),
            AutopilotActionId::from_uuid(action_id),
            &key(),
            None::<&RequestId>,
            None,
        )
        .await?;
    // The approach is an outward class, so approval parks it in the two-minute
    // hold window (O.2) — the window the operator cancels inside when they
    // spot a mistake. Claiming at `now` must find nothing due; claiming once
    // the window has lapsed must find it.
    let in_window = autopilot
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(workspace_id),
            8,
            OffsetDateTime::now_utc(),
        )
        .await?;
    assert!(
        in_window
            .iter()
            .all(|claimed| claimed.id.into_uuid() != action_id),
        "the approved approach was claimable inside its hold window"
    );
    let claimed = autopilot
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(workspace_id),
            8,
            OffsetDateTime::now_utc() + Duration::from_secs(121),
        )
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .expect("the approved approach is claimable once the hold lapses");
    autopilot
        .execute_action(
            WorkspaceId::from_uuid(workspace_id),
            action,
            OffsetDateTime::now_utc(),
        )
        .await?;

    let stamped: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT approached_at FROM booking_agents \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .fetch_one(&pool)
    .await?;
    assert!(stamped.is_some(), "the send never spent the season");
    let (ledger_row, reach_row, outbox_row): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM booking_agent_interactions \
              WHERE workspace_id = $1 AND agent_id = $2 \
                AND direction = 'outbound' AND phase = 'approach'), \
            (SELECT count(*) FROM reach_events \
              WHERE workspace_id = $1 AND recipient_kind = 'booking_agent' \
                AND recipient_id = $2::text), \
            (SELECT count(*) FROM outbox_events \
              WHERE workspace_id = $1 \
                AND event_type = 'crowdrelay.booking_agent.approach_requested')",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (ledger_row, reach_row, outbox_row),
        (1, 1, 1),
        "the send must leave the ledger, the reach event and the outbox intent"
    );
    // The reply window is scheduled only when the executor's provider
    // confirms the send — an outbox intent alone is not evidence the letter
    // left. The executor claims the work, then files its receipt.
    let claim = autopilot
        .claim_execution(
            WorkspaceId::from_uuid(workspace_id),
            ClaimExecution {
                action_id: AutopilotActionId::from_uuid(action_id),
                executor_id: "n8n-agent-test".to_owned(),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await?;
    autopilot
        .record_execution_report(
            WorkspaceId::from_uuid(workspace_id),
            RecordExecutionReport {
                action_id: AutopilotActionId::from_uuid(action_id),
                receipt_key: format!("agent-receipt-{action_id}"),
                executor_id: "n8n-agent-test".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: claim.claim_token,
                provider_reference: Some("agent-msg-1".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await?;
    let measurement: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_measurements \
         WHERE workspace_id = $1 AND measurement_kind = 'booking_agent_reply_30d'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(measurement, 1, "the reply window was never scheduled");

    // The spent season refuses the second letter — and would for 120 days.
    match repo
        .request_approach(workspace_id, agent_id, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("season"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a season refusal, got {other:?}"),
    }
    Ok(())
}

/// A gate that moved between approve and send fails the dispatch, not the
/// request: the letter does not leave, the season is not spent, and the
/// failure says which gate moved.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_moved_gate_fails_the_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let repo = PostgresBookingAgentRepository::new(pool.clone());
    let autopilot = autopilot(&pool, &url)?;
    seed_agent_draw(&pool, workspace_id).await?;
    let agent_id = insert_agent(&pool, workspace_id, true).await?;

    let action_id = match repo
        .request_approach(workspace_id, agent_id, None, &key())
        .await?
    {
        BookingAgentApproachOutcome::Queued { action_id } => action_id,
        other => panic!("expected a queued approach, got {other:?}"),
    };
    advertise_approach_capability(&pool, workspace_id).await?;
    autopilot
        .approve_action(
            WorkspaceId::from_uuid(workspace_id),
            AutopilotActionId::from_uuid(action_id),
            &key(),
            None::<&RequestId>,
            None,
        )
        .await?;

    // The agent asked not to be contacted after the approval was filed —
    // exactly the move the dispatch re-gate exists to catch.
    sqlx::query(
        "UPDATE booking_agents SET do_not_contact = true \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .execute(&pool)
    .await?;

    // The approval parked the approach in the two-minute outward hold window
    // (O.2); claiming once the window has lapsed reaches the dispatch re-gate,
    // which is the check this test exists for.
    let claimed = autopilot
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(workspace_id),
            8,
            OffsetDateTime::now_utc() + Duration::from_secs(121),
        )
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .expect("the approved approach is claimable once the hold lapses");
    let outcome = autopilot
        .execute_action(
            WorkspaceId::from_uuid(workspace_id),
            action,
            OffsetDateTime::now_utc(),
        )
        .await;
    assert!(
        matches!(
            outcome,
            Err(crowdrelay_application::RepositoryError::ConflictBecause(_))
        ),
        "a stale approval must not send: {outcome:?}"
    );

    let (stamped, outbox_row): (Option<OffsetDateTime>, i64) = sqlx::query_as(
        "SELECT \
            (SELECT approached_at FROM booking_agents \
              WHERE workspace_id = $1 AND id = $2), \
            (SELECT count(*) FROM outbox_events \
              WHERE workspace_id = $1 \
                AND event_type = 'crowdrelay.booking_agent.approach_requested')",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .fetch_one(&pool)
    .await?;
    assert!(stamped.is_none(), "the refused send still spent the season");
    assert_eq!(outbox_row, 0, "the refused send left an outbox intent");
    Ok(())
}

/// What the agent answered is filed on the registry row every other path
/// reads: a decline closes the season's door from the day they answered, a
/// do-not-contact is the wall the governor shares, and any reply re-proves
/// the route.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_reply_writes_the_door() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();
    let (pool, url) = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let repo = PostgresBookingAgentRepository::new(pool.clone());
    let autopilot = autopilot(&pool, &url)?;
    seed_agent_draw(&pool, workspace_id).await?;

    // A decline stamps refused_until from the day they answered, not the
    // day it was filed — the season is the answer's, not the filing's.
    let declined_id = insert_agent(&pool, workspace_id, true).await?;
    let answered = OffsetDateTime::now_utc() - time::Duration::days(3);
    autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(declined_id),
                disposition: BookingAgentReplyDisposition::Declined,
                occurred_at: answered,
            },
            &key(),
            None::<&RequestId>,
        )
        .await?;
    let door: Option<time::Date> = sqlx::query_scalar(
        "SELECT refused_until FROM booking_agents \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(declined_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        door,
        Some(
            answered.date()
                + time::Duration::days(crowdrelay_domain::booking_agent::APPROACH_SEASON_DAYS)
        ),
        "a decline must close the door for one season from the answer"
    );
    match repo
        .request_approach(workspace_id, declined_id, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("declined"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a closed-door refusal, got {other:?}"),
    }

    // The inbound reply itself is on the agent's own ledger.
    let replies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM booking_agent_interactions \
         WHERE workspace_id = $1 AND agent_id = $2 \
           AND direction = 'inbound' AND disposition = 'declined'",
    )
    .bind(workspace_id)
    .bind(declined_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(replies, 1);

    // Do-not-contact is the wall: the flag lands on the row and on the
    // shared governor so every other route to the same address honours it.
    let walled_id = insert_agent(&pool, workspace_id, true).await?;
    autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(walled_id),
                disposition: BookingAgentReplyDisposition::DoNotContact,
                occurred_at: OffsetDateTime::now_utc(),
            },
            &key(),
            None::<&RequestId>,
        )
        .await?;
    let (flag, governor): (bool, bool) = sqlx::query_as(
        "SELECT \
            (SELECT do_not_contact FROM booking_agents \
              WHERE workspace_id = $1 AND id = $2), \
            (SELECT EXISTS (SELECT 1 FROM contact_governor g \
              JOIN booking_agents a \
                ON a.workspace_id = g.workspace_id \
               AND lower(btrim(a.contact_email)) = g.normalized_contact \
              WHERE a.workspace_id = $1 AND a.id = $2 AND g.do_not_contact))",
    )
    .bind(workspace_id)
    .bind(walled_id)
    .fetch_one(&pool)
    .await?;
    assert!(flag && governor, "the wall must bind every route");
    match repo
        .request_approach(workspace_id, walled_id, None, &key())
        .await
    {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(
                reason.contains("not to be contacted"),
                "unexpected refusal: {reason}"
            )
        }
        other => panic!("expected a do-not-contact refusal, got {other:?}"),
    }

    // A reply also re-proves the route: an agent who wrote back is an
    // agent whose address was real — the registry row gains
    // contact_verified_at it never had.
    let unverified_id = insert_agent(&pool, workspace_id, false).await?;
    autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(unverified_id),
                disposition: BookingAgentReplyDisposition::Received,
                occurred_at: OffsetDateTime::now_utc(),
            },
            &key(),
            None::<&RequestId>,
        )
        .await?;
    let verified_at: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT contact_verified_at FROM booking_agents \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(unverified_id)
    .fetch_one(&pool)
    .await?;
    assert!(verified_at.is_some(), "a reply did not re-prove the route");

    // The filing is idempotent — a retried receipt carries the same body
    // (the client owns `occurred_at`, so a replay is byte-identical) and
    // replays the recorded operation rather than writing a second
    // interaction.
    let reply_key = key();
    let replay_at = OffsetDateTime::now_utc();
    let replay_id = insert_agent(&pool, workspace_id, true).await?;
    let first = autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(replay_id),
                disposition: BookingAgentReplyDisposition::Positive,
                occurred_at: replay_at,
            },
            &reply_key,
            None::<&RequestId>,
        )
        .await?;
    let second = autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(replay_id),
                disposition: BookingAgentReplyDisposition::Positive,
                occurred_at: replay_at,
            },
            &reply_key,
            None::<&RequestId>,
        )
        .await?;
    assert!(second.replayed);
    assert_eq!(first.operation_id, second.operation_id);
    Ok(())
}

/// §4h-10 — the reply lane: an agent who wrote back surfaces as waiting on
/// an answer, the ask queues one approval card carrying the scaffold, and
/// the send closes the loop without spending the season.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_reply_lane_surfaces_waits_and_sends_the_approved_answer()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let other_workspace = insert_workspace(&pool).await?;
    let repo = PostgresBookingAgentRepository::new(pool.clone());
    let autopilot = autopilot(&pool, &url)?;

    let agent_id = insert_agent(&pool, workspace_id, true).await?;

    // The season's letter went out three days ago — the contact governor
    // holds a window that has four more days on it. A reply must still send:
    // the cooldown spaces what we initiate, not the answer the agent is
    // waiting on. The DNC wall and the org budget still bind.
    sqlx::query(
        "INSERT INTO contact_governor \
            (workspace_id, normalized_contact, last_context, last_action_id, \
             last_outbound_at, next_contact_after) \
         SELECT $1, lower(btrim(contact_email)), 'booking_agent', NULL, \
                now() - interval '3 days', now() + interval '4 days' \
         FROM booking_agents WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .execute(&pool)
    .await?;

    // Before any reply, the ask refuses — there is nothing to answer.
    match repo.request_reply(workspace_id, agent_id, &key()).await {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("waiting"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a nothing-waiting refusal, got {other:?}"),
    }

    // The operator files what the agent said — the inbound half of the
    // conversation.
    let replied_at = OffsetDateTime::now_utc() - Duration::from_secs(3600);
    autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(agent_id),
                disposition: BookingAgentReplyDisposition::Positive,
                occurred_at: replied_at,
            },
            &key(),
            None::<&RequestId>,
        )
        .await?;

    // The registry row now says the conversation waits on the band.
    let listed = repo.list_agents(workspace_id).await?;
    let row = listed
        .iter()
        .find(|row| row.agent_id == agent_id)
        .expect("the agent lists");
    assert!(row.awaiting_reply, "a filed positive reply did not surface");
    assert_eq!(row.reply_waiting_disposition.as_deref(), Some("positive"));
    assert!(!row.reply_pending, "nothing is queued yet");

    // Tenant isolation: another workspace cannot ask to answer an agent it
    // does not own, and sees nothing waiting.
    assert!(matches!(
        repo.request_reply(other_workspace, agent_id, &key()).await,
        Err(BookingAgentError::NotFound),
    ));

    // The ask queues the approval card with the scaffold inside — the
    // operator completes the words on the card before it can leave.
    let reply_key = key();
    let action_id = match repo
        .request_reply(workspace_id, agent_id, &reply_key)
        .await?
    {
        crowdrelay_infra::booking_agents::BookingAgentReplyOutcome::Queued { action_id } => {
            action_id
        }
        other => panic!("expected a queued reply, got {other:?}"),
    };
    let (status, action_kind): (String, String) = sqlx::query_as(
        "SELECT status, action_kind FROM autopilot_actions \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (status.as_str(), action_kind.as_str()),
        ("awaiting_approval", "booking_agent.reply.request"),
        "the reply must wait on a person like every letter here"
    );
    let draft_ready: bool = sqlx::query_scalar(
        "SELECT length(trim(payload->'draft'->>'subject')) > 0 \
         AND length(trim(payload->'draft'->>'body')) > 0 \
         AND (payload->>'reply_interaction_id') IS NOT NULL \
         FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        draft_ready,
        "the card must carry the scaffold and the reply it answers"
    );

    // A retried click replays the card; a different key refuses — one open
    // answer card per agent.
    match repo
        .request_reply(workspace_id, agent_id, &reply_key)
        .await?
    {
        crowdrelay_infra::booking_agents::BookingAgentReplyOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id),
        other => panic!("expected a replayed reply, got {other:?}"),
    }
    match repo.request_reply(workspace_id, agent_id, &key()).await {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(reason.contains("approval"), "unexpected refusal: {reason}")
        }
        other => panic!("expected an already-queued refusal, got {other:?}"),
    }

    let listed = repo.list_agents(workspace_id).await?;
    let row = listed
        .iter()
        .find(|row| row.agent_id == agent_id)
        .expect("the agent lists");
    assert!(
        row.reply_pending && row.awaiting_reply,
        "the queued answer must show on the row while the reply still waits"
    );

    // Approve, ride out the outward hold, dispatch: the outbox event, the
    // ledger's outbound half and the reach row all land — and the season
    // stamp does not move, because an answer is not a new ask.
    advertise_approach_capability(&pool, workspace_id).await?;
    autopilot
        .approve_action(
            WorkspaceId::from_uuid(workspace_id),
            AutopilotActionId::from_uuid(action_id),
            &key(),
            None::<&RequestId>,
            None,
        )
        .await?;
    let claimed = autopilot
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(workspace_id),
            8,
            OffsetDateTime::now_utc() + Duration::from_secs(121),
        )
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .expect("the approved reply is claimable once the hold lapses");
    autopilot
        .execute_action(
            WorkspaceId::from_uuid(workspace_id),
            action,
            OffsetDateTime::now_utc(),
        )
        .await?;

    let (ledger_row, answers_row, reach_row, outbox_row): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM booking_agent_interactions \
              WHERE workspace_id = $1 AND agent_id = $2 \
                AND direction = 'outbound' AND phase = 'reply'), \
            (SELECT count(*) FROM booking_agent_interactions \
              WHERE workspace_id = $1 AND agent_id = $2 \
                AND direction = 'outbound' AND phase = 'reply' \
                AND metadata->>'answers_interaction_id' IS NOT NULL), \
            (SELECT count(*) FROM reach_events \
              WHERE workspace_id = $1 AND recipient_kind = 'booking_agent' \
                AND recipient_id = $2::text AND template_id = 'booking_agent_reply'), \
            (SELECT count(*) FROM outbox_events \
              WHERE workspace_id = $1 \
                AND event_type = 'crowdrelay.booking_agent.reply_requested')",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (ledger_row, answers_row, reach_row, outbox_row),
        (1, 1, 1, 1),
        "the send must leave the answered ledger row, the reach event and the outbox intent"
    );
    let still_unspent: bool = sqlx::query_scalar(
        "SELECT approached_at IS NULL FROM booking_agents \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(agent_id)
    .fetch_one(&pool)
    .await?;
    assert!(still_unspent, "an answer must not spend the season's ask");

    // The loop is closed: the row no longer waits, and a fresh ask refuses
    // honestly rather than double-answering.
    let listed = repo.list_agents(workspace_id).await?;
    let row = listed
        .iter()
        .find(|row| row.agent_id == agent_id)
        .expect("the agent lists");
    assert!(!row.awaiting_reply, "the send did not close the loop");

    // A decline asks nothing — the newest inbound being `declined` leaves
    // the row without a waiting reply.
    let declined_id = insert_agent(&pool, workspace_id, true).await?;
    autopilot
        .record_booking_agent_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(declined_id),
                disposition: BookingAgentReplyDisposition::Declined,
                occurred_at: OffsetDateTime::now_utc(),
            },
            &key(),
            None::<&RequestId>,
        )
        .await?;
    let listed = repo.list_agents(workspace_id).await?;
    let declined_row = listed
        .iter()
        .find(|row| row.agent_id == declined_id)
        .expect("the declined agent lists");
    assert!(
        !declined_row.awaiting_reply,
        "a decline must not read as a reply waiting on an answer"
    );

    // The wall binds the reply lane too — no answer composes to a
    // do-not-contact agent, and an unconfirmed route refuses the same way.
    let walled_id = insert_agent(&pool, workspace_id, true).await?;
    sqlx::query("UPDATE booking_agents SET do_not_contact = true WHERE id = $1")
        .bind(walled_id)
        .execute(&pool)
        .await?;
    match repo.request_reply(workspace_id, walled_id, &key()).await {
        Err(BookingAgentError::Refused(reason)) => {
            assert!(
                reason.contains("not to be contacted"),
                "unexpected refusal: {reason}"
            )
        }
        other => panic!("expected a do-not-contact refusal, got {other:?}"),
    }
    Ok(())
}
