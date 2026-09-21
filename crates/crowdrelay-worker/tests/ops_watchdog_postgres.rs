//! Live-database proof that the watchdog survives a CrowdRelay-only schema.
//!
//! `agent_service_credentials` belongs to the agents service — no migration
//! here creates it. The snapshot's Reddit session readings are guarded by a
//! `to_regclass` probe: without the table they must read "no usable session"
//! rather than abort the whole cycle (and every condition with it). With the
//! table present, a dead credential beside queued drafts must raise
//! `publishing.session_dead`.

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::auto_post_platforms::{AutoPostPlatforms, PublishingPosture, RedditPosture};
use crowdrelay_worker::ops_watchdog::OpsWatchdogWorker;
use sqlx::PgPool;
use uuid::Uuid;

fn watchdog(pool: PgPool, workspace_id: WorkspaceId) -> OpsWatchdogWorker {
    OpsWatchdogWorker::new(
        pool,
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
        PublishingPosture {
            platforms: AutoPostPlatforms {
                telegram: true,
                discord: true,
                social: true,
            },
            reddit: RedditPosture::Publishes,
        },
    )
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("watchdog-{}", id.simple()))
        .bind("Watchdog")
        .execute(pool)
        .await
        .context("insert workspace")?;
    // The snapshot selects from `executor_instances` — one live
    // executor row, the shape a healthy workspace carries.
    sqlx::query(
        r#"INSERT INTO executor_instances
               (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
           VALUES ($1,'worker-1','1.0.0','abc123',now(),now() + interval '1 hour')"#,
    )
    .bind(id)
    .execute(pool)
    .await
    .context("insert executor instance")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A queued community draft — the posting demand `session_dead` reads.
async fn queued_draft(pool: &PgPool, workspace_id: WorkspaceId) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                  'community.engage',9000,'auto_execute','post to a community',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("engage-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                  'workspace',$4,$5,'{}'::jsonb,'succeeded',now(),$6)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert action")?;
    sqlx::query(
        r#"
        INSERT INTO community_posts
            (workspace_id, action_id, subreddit, title, body, status)
        VALUES ($1,$2,'r/test','title','body','pending')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert queued draft")?;
    Ok(())
}

/// The agents-service credential table, the columns the snapshot reads —
/// same convention as `agent_run_assignment_postgres.rs`.
async fn create_credentials_table(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_credentials (
            id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
            workspace_id uuid NOT NULL,
            provider text NOT NULL,
            status text NOT NULL DEFAULT 'active',
            last_validated_at timestamptz,
            last_validation_error text,
            created_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create foreign credentials table")?;
    Ok(())
}

async fn active_alerts(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT alert_key FROM ops_alert_state \
         WHERE workspace_id = $1 AND active ORDER BY alert_key",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .context("read alert state")
}

/// A deployment without the agents schema must still run the cycle: the
/// snapshot's Reddit readings default to "no usable session" and every other
/// condition still evaluates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_missing_credentials_table_does_not_blind_the_watchdog() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let ws = workspace(&db).await?;
        queued_draft(&db, ws).await?;
        // No create_credentials_table call — the relation does not exist.
        let transitions = watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"publishing.session_dead".to_owned()),
            "a queued draft with no credential service at all is a dead \
             session — got {alerts:?}"
        );
        assert!(transitions > 0, "the cycle ran and recorded the alert");
        Ok(())
    }
    .await
}

/// With the table present, an `invalid` credential beside queued drafts
/// fires the same alert — and an `active` one silences it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dead_credential_raises_session_dead_and_a_live_one_clears_it() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let ws = workspace(&db).await?;
        queued_draft(&db, ws).await?;
        create_credentials_table(&db).await?;
        sqlx::query(
            "INSERT INTO agent_service_credentials (workspace_id, provider, status, last_validation_error) \
             VALUES ($1,'reddit-browser','invalid','login rejected')",
        )
        .bind(ws.into_uuid())
        .execute(&db)
        .await
        .context("insert invalid credential")?;

        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"publishing.session_dead".to_owned()),
            "invalid credential + queued draft must fire: {alerts:?}"
        );

        sqlx::query(
            "UPDATE agent_service_credentials SET status='active', last_validated_at=now() \
             WHERE workspace_id=$1 AND provider='reddit-browser'",
        )
        .bind(ws.into_uuid())
        .execute(&db)
        .await
        .context("revive credential")?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            !alerts.contains(&"publishing.session_dead".to_owned()),
            "an active credential means the queue can be worked: {alerts:?}"
        );
        Ok(())
    }
    .await
}

/// An approval cancelled unanswered must reach the operator, against a real
/// schema.
///
/// The unit tests prove the condition's predicate. This proves the reading
/// behind it: three subqueries over `autopilot_actions`, one of them an
/// `EXTRACT(EPOCH …)` that returns `numeric` on PostgreSQL 14+ and has to be cast
/// before sqlx can decode it. An uncast one compiles, lints and passes every unit
/// test, then aborts the whole snapshot — and with it all eighteen conditions —
/// on first contact with a server.
///
/// Also pins the distinction the alarm turns on: an approval merely waiting is
/// the system working, and only one already discarded is the finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_approval_cancelled_unanswered_reaches_the_operator() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let ws = workspace(&db).await?;

        // An approval still outstanding, well inside its window.
        approval_action(&db, ws, "awaiting_approval", Some(48), None).await?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            !alerts.contains(&"approval.expired_unanswered".to_owned()),
            "work in the queue is the system working, not a finding: {alerts:?}"
        );

        // One the sweep cancelled because nobody answered it.
        approval_action(&db, ws, "cancelled", Some(-1), Some("approval_expired")).await?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"approval.expired_unanswered".to_owned()),
            "an approval discarded unanswered must be reported: {alerts:?}"
        );
        Ok(())
    }
    .await
}

/// One approval action, with its decision. `expires_in_hours` may be negative to
/// place the deadline in the past; `last_error_kind` is what separates an expiry
/// from an operator's own rejection.
async fn approval_action(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    status: &str,
    expires_in_hours: Option<i32>,
    last_error_kind: Option<&str>,
) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','workspace',$4,
                  'apply_live_opportunity',7700,'require_approval','a festival',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("live-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approval_expires_at, last_error_kind, finished_at, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','apply_live_opportunity',
                  'workspace',$4,$5,'{}'::jsonb,$6,
                  CASE WHEN $7::int IS NULL THEN NULL
                       ELSE now() + make_interval(hours => $7::int) END,
                  $8,
                  CASE WHEN $6 = 'cancelled' THEN now() ELSE NULL END,
                  $9)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(status)
    .bind(expires_in_hours)
    .bind(last_error_kind)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert approval action")?;
    Ok(())
}

/// A capability that went dark while work still needed it must reach the
/// operator — the `team.email` shape from production: the registry stays
/// live, one advertisement just stops.
///
/// The unit tests prove the predicate. This proves the reading against a
/// real schema: the parked marker is `last_error_kind='awaiting_executor'`
/// on a `queued` row, the cancellation is `no_executor` on a `cancelled`
/// one, and the action-kind rollup must decode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn work_parked_on_a_dark_capability_reaches_the_operator() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let ws = workspace(&db).await?;
        live_executor(&db, ws).await?;

        // Nothing waiting: a live registry alone is not the finding.
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            !alerts.contains(&"executor.capability_unadvertised".to_owned()),
            "a live registry with nothing parked is healthy: {alerts:?}"
        );

        parked_action(&db, ws).await?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"executor.capability_unadvertised".to_owned()),
            "parked work beside a live registry must be reported: {alerts:?}"
        );

        // The capability returns: the row unparks and the finding clears.
        sqlx::query(
            "UPDATE autopilot_actions SET last_error_kind=NULL \
             WHERE workspace_id=$1 AND status='queued' \
               AND last_error_kind='awaiting_executor'",
        )
        .bind(ws.into_uuid())
        .execute(&db)
        .await
        .context("unpark action")?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            !alerts.contains(&"executor.capability_unadvertised".to_owned()),
            "work unparked when its capability returns: {alerts:?}"
        );
        Ok(())
    }
    .await
}

/// A live heartbeat row so `executor_active > 0` — the condition's other half.
async fn live_executor(pool: &PgPool, workspace_id: WorkspaceId) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-heartbeat','1.0.0','manifest',now(),now() + interval '10 minutes')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await
    .context("insert live executor")?;
    Ok(())
}

/// One queued action parked waiting on a capability nobody advertises.
async fn parked_action(pool: &PgPool, workspace_id: WorkspaceId) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','workspace',$4,
                  'apply_live_opportunity',7700,'require_approval','a festival',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("live-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at,
            last_error_kind, trace_id
        ) VALUES ($1,$2,$3,'live_opportunity','team.email.send',
                  'workspace',$4,$5,'{}'::jsonb,'queued',now(),
                  'awaiting_executor',$6)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert parked action")?;
    Ok(())
}

/// A refused letter must reach the operator regardless of its event type.
///
/// The predicate is the payload, not the event name: `contact_email` means a
/// specific human was the addressee. Production proved why the type list could
/// not be trusted — on 2026-09-15 an approved `opportunity.application_requested`
/// and a `post_show_report_due` both died 422 cancelled while a two-type list
/// watched neither. This test refuses a letter under an event type that does not
/// exist in any list, and the alarm still has to fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_refused_letter_under_an_unknown_event_type_still_raises_attention() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let ws = workspace(&db).await?;
        refused_delivery(
            &db,
            ws,
            "crowdrelay.festival.application_requested",
            serde_json::json!({"action_id": Uuid::now_v7(), "contact_email": "fest@example.org"}),
        )
        .await?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"delivery.growth_event_refused".to_owned()),
            "a letter refused under an unlisted event type must still alarm: {alerts:?}"
        );

        // And a refused event with no named recipient is the warning, not the
        // critical alarm — the two must not bleed into each other.
        refused_delivery(
            &db,
            ws,
            "crowdrelay.ops.status_changed",
            serde_json::json!({"status": "degraded"}),
        )
        .await?;
        watchdog(db.clone(), ws).run_once().await?;
        let alerts = active_alerts(&db, ws).await?;
        assert!(
            alerts.contains(&"delivery.event_refused".to_owned()),
            "a refused non-letter delivery warns: {alerts:?}"
        );
        Ok(())
    }
    .await
}

/// One outbox event + one endpoint + one `cancelled` delivery joining them.
async fn refused_delivery(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let ws = workspace_id.into_uuid();
    let endpoint_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO webhook_endpoints (workspace_id, name, url, signing_secret_ref, max_attempts) \
         VALUES ($1,$2,'https://consumer.invalid/hook','ref',12) \
         ON CONFLICT (workspace_id, name) DO UPDATE SET url = EXCLUDED.url \
         RETURNING id",
    )
    .bind(ws)
    .bind(format!("ep-{}", Uuid::now_v7()))
    .fetch_one(pool)
    .await
    .context("insert endpoint")?;
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO outbox_events (workspace_id, event_type, payload, status, delivered_at) \
         VALUES ($1,$2,$3,'delivered',now()) RETURNING id",
    )
    .bind(ws)
    .bind(event_type)
    .bind(payload)
    .fetch_one(pool)
    .await
    .context("insert outbox event")?;
    // The outbox event's own status is not what the watchdog reads — the
    // delivery is the cancelled one. `delivered` satisfies its CHECK without
    // pretending the letter arrived.
    sqlx::query(
        "INSERT INTO webhook_deliveries (workspace_id, outbox_event_id, endpoint_id, \
         status, max_attempts, cancelled_at, last_response_status, last_error_kind) \
         VALUES ($1,$2,$3,'cancelled',12,now(),422,'http_permanent_status')",
    )
    .bind(ws)
    .bind(event_id)
    .bind(endpoint_id)
    .execute(pool)
    .await
    .context("insert refused delivery")?;
    Ok(())
}
