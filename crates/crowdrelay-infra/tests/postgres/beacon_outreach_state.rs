//! The booker's "not now" and "not this pair" must reach the next cycle.
//!
//! Before 0395 the only way to say no to a ranked Beacon outreach was to
//! cancel the ask in the approval queue — a missing row the snapshot loader
//! never saw, so the next cycle recommended the same pair and the queue
//! taught the booker that saying no sticks to nothing.
//!
//! What is asserted here:
//!
//! - `defer` stamps `deferred_until`, cancels the waiting ask, and the
//!   campaign snapshot drops the pair until the defer lapses.
//! - `decline` writes `declined` with `declined_via = 'operator'` — the
//!   answer is distinguishable from a partner's reply — and cancels the
//!   waiting ask in the same transaction.
//! - A campaign already `partner`/`closed` is not demoted by either verb.
//! - An ask that was approved before the answer arrived cannot send on the
//!   stale yes: the executor re-reads the campaign row and fails closed.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotControlRepository, AutopilotDecisionRepository,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    autopilot::PostgresAutopilotRepository, beacon_signal::outreach_state,
    beacon_signal::outreach_state::OutreachStateRefusal, config::DatabaseConfig,
};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: Uuid,
    city_id: Uuid,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(format!("outreach-state-{}", workspace_id.simple()))
        .bind("Outreach State Tests")
        .execute(&pool)
        .await?;
    let suffix = &workspace_id.simple().to_string()[20..];
    sqlx::query("INSERT INTO cities (name, country_code, slug) VALUES ($1, 'PL', $2)")
        .bind(format!("Gorzów {suffix}"))
        .bind(format!("gorzow-{suffix}"))
        .execute(&pool)
        .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = $1")
        .bind(format!("gorzow-{suffix}"))
        .fetch_one(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        city_id,
    })
}

/// A verified local beacon plus a published show inside the outreach window.
/// Returns `(beacon_id, beacon_version, event_id)`.
async fn pair(
    f: &Fixture,
    starts_in_days: i64,
) -> Result<(Uuid, i64, Uuid), Box<dyn std::error::Error>> {
    let suffix = &Uuid::now_v7().simple().to_string()[20..];
    let event_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events
            (workspace_id, slug, title, status, starts_at, timezone, city_id, published_at)
        VALUES ($1, $2, 'Test night', 'published', $3, 'Europe/Warsaw', $4, now())
        RETURNING id
        "#,
    )
    .bind(f.workspace_id)
    .bind(format!("night-{suffix}"))
    .bind(OffsetDateTime::now_utc() + time::Duration::days(starts_in_days))
    .bind(f.city_id)
    .fetch_one(&f.pool)
    .await?;
    let (beacon_id, beacon_version) = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        INSERT INTO beacons
            (workspace_id, city_id, beacon_kind, display_name, contact_email,
             active, verified, accepts_outreach, relationship_score)
        VALUES ($1, $2, 'community', $3, $4, true, true, true, 50)
        RETURNING id, version
        "#,
    )
    .bind(f.workspace_id)
    .bind(f.city_id)
    .bind(format!("Partner {suffix}"))
    .bind(format!("partner-{suffix}@example.test"))
    .fetch_one(&f.pool)
    .await?;
    Ok((beacon_id, beacon_version, event_id))
}

/// The ask the ranked candidate becomes: a decision row for lineage and an
/// `awaiting_approval` action carrying the pair payload the verbs read.
async fn pending_ask(
    f: &Fixture,
    beacon_id: Uuid,
    beacon_version: i64,
    event_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'beacon','beacon',$4,'amplify_local_signal',9000,
                   'require_approval','seeded ranked ask','{}','{}','{}',now(),$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(beacon_id)
    .execute(&f.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class)
           VALUES ($1,$2,$3,'beacon','beacon.outreach.request','beacon',$4,$5,$6,
                   'awaiting_approval','third_party')"#,
    )
    .bind(action_id)
    .bind(f.workspace_id)
    .bind(decision_id)
    .bind(beacon_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({
        "kind": "request_beacon_outreach",
        "beacon_id": beacon_id,
        "event_id": event_id,
        "beacon_version": beacon_version,
        "phase": "initial",
        "template_key": "beacon.local_story.v1",
    }))
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

async fn campaign_row(
    f: &Fixture,
    beacon_id: Uuid,
    event_id: Uuid,
) -> Result<(String, Option<OffsetDateTime>, Option<String>), Box<dyn std::error::Error>> {
    Ok(
        sqlx::query_as::<_, (String, Option<OffsetDateTime>, Option<String>)>(
            "SELECT status, deferred_until, declined_via FROM beacon_campaigns
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
        )
        .bind(f.workspace_id)
        .bind(beacon_id)
        .bind(event_id)
        .fetch_one(&f.pool)
        .await?,
    )
}

async fn action_status(f: &Fixture, action_id: Uuid) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT status FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(f.workspace_id)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?)
}

async fn pair_in_due_set(
    f: &Fixture,
    beacon_id: Uuid,
    event_id: Uuid,
) -> Result<bool, Box<dyn std::error::Error>> {
    let snapshots = f
        .repository
        .load_beacon_campaign_snapshots(
            WorkspaceId::from_uuid(f.workspace_id),
            OffsetDateTime::now_utc(),
        )
        .await?;
    Ok(snapshots.iter().any(|snapshot| {
        snapshot.beacon_id.into_uuid() == beacon_id && snapshot.event_id.into_uuid() == event_id
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_defer_holds_the_pair_out_of_the_due_set_until_it_lapses()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 30).await?;
    let action_id = pending_ask(&f, beacon_id, beacon_version, event_id).await?;
    assert!(
        pair_in_due_set(&f, beacon_id, event_id).await?,
        "a fresh verified pair must be in the due set before the answer"
    );

    let changed = outreach_state::defer_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        7,
        "test-defer-1",
        None,
    )
    .await?
    .map_err(|refusal| format!("defer refused: {refusal:?}"))?;

    assert_eq!(changed.status, "candidate");
    assert_eq!(changed.cancelled_actions, 1);
    assert!(!changed.preserved_stronger_state);
    assert!(
        changed
            .deferred_until
            .is_some_and(|until| until > OffsetDateTime::now_utc() + time::Duration::days(6)),
        "a seven-day defer should land about a week out"
    );
    assert_eq!(action_status(&f, action_id).await?, "cancelled");
    assert!(
        !pair_in_due_set(&f, beacon_id, event_id).await?,
        "a deferred pair must leave the due set"
    );
    // The operator answer is on the audit ledger in the same transaction.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM operator_actions
         WHERE workspace_id = $1 AND action = 'beacon_outreach_defer' AND target_id = $2",
    )
    .bind(f.workspace_id)
    .bind(beacon_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(audit, 1);

    // The defer lapses on its own — the pair re-enters without a new answer.
    sqlx::query(
        "UPDATE beacon_campaigns SET deferred_until = now() - INTERVAL '1 hour'
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
    )
    .bind(f.workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .execute(&f.pool)
    .await?;
    assert!(
        pair_in_due_set(&f, beacon_id, event_id).await?,
        "an elapsed defer must let the pair back into the due set"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_decline_is_the_operators_no_not_the_partners() -> Result<(), Box<dyn std::error::Error>>
{
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 30).await?;
    let action_id = pending_ask(&f, beacon_id, beacon_version, event_id).await?;

    let changed = outreach_state::decline_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        Some("wrong partner for this show"),
        "test-decline-1",
        None,
    )
    .await?
    .map_err(|refusal| format!("decline refused: {refusal:?}"))?;

    assert_eq!(changed.status, "declined");
    assert_eq!(changed.cancelled_actions, 1);
    let (status, deferred_until, declined_via) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "declined");
    assert_eq!(deferred_until, None);
    assert_eq!(declined_via.as_deref(), Some("operator"));
    assert_eq!(action_status(&f, action_id).await?, "cancelled");
    assert!(
        !pair_in_due_set(&f, beacon_id, event_id).await?,
        "a declined pair must not re-enter the due set next cycle"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn neither_verb_demotes_a_relationship_that_went_further()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, _, event_id) = pair(&f, 30).await?;
    sqlx::query(
        "INSERT INTO beacon_campaigns (workspace_id, beacon_id, event_id, status)
         VALUES ($1, $2, $3, 'partner')",
    )
    .bind(f.workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .execute(&f.pool)
    .await?;

    let declined = outreach_state::decline_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        None,
        "test-decline-partner",
        None,
    )
    .await?
    .map_err(|refusal| format!("decline refused: {refusal:?}"))?;
    assert!(declined.preserved_stronger_state);
    assert_eq!(declined.status, "partner");
    let (status, _, declined_via) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "partner");
    assert_eq!(
        declined_via, None,
        "a partner carries no decline provenance"
    );

    let deferred = outreach_state::defer_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        7,
        "test-defer-partner",
        None,
    )
    .await?
    .map_err(|refusal| format!("defer refused: {refusal:?}"))?;
    assert!(deferred.preserved_stronger_state);
    let (status, deferred_until, _) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "partner");
    assert_eq!(
        deferred_until, None,
        "a defer on a partner stamps nothing the loader could misread"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unknown_pair_cannot_be_answered() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (_, _, event_id) = pair(&f, 30).await?;
    let refusal = outreach_state::defer_beacon_outreach(
        &f.pool,
        f.workspace_id,
        Uuid::now_v7(),
        event_id,
        7,
        "test-defer-missing",
        None,
    )
    .await?
    .expect_err("a beacon the workspace does not have is a typo, not a decision");
    assert_eq!(refusal, OutreachStateRefusal::BeaconNotFound);
    let refusal = outreach_state::defer_beacon_outreach(
        &f.pool,
        f.workspace_id,
        Uuid::now_v7(),
        event_id,
        0,
        "test-defer-days",
        None,
    )
    .await?
    .expect_err("zero days is not a defer");
    assert_eq!(refusal, OutreachStateRefusal::BadDeferDays);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_ask_approved_before_the_answer_cannot_send_on_the_stale_yes()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 30).await?;
    let action_id = pending_ask(&f, beacon_id, beacon_version, event_id).await?;
    // Approve it the way the operator path does, then claim it the way the
    // worker would — the answer lands between approval and send.
    sqlx::query(
        "UPDATE autopilot_actions
         SET status = 'queued', approved_at = now(), approved_by = 'operator:test',
             available_at = now()
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(f.workspace_id)
    .bind(action_id)
    .execute(&f.pool)
    .await?;

    outreach_state::decline_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        Some("changed my mind"),
        "test-decline-stale",
        None,
    )
    .await?
    .map_err(|refusal| format!("decline refused: {refusal:?}"))?;

    // The decline cancelled only the awaiting_approval row — the queued ask
    // is past that gate, so the send-time re-check is what must refuse it.
    assert_eq!(action_status(&f, action_id).await?, "queued");
    let claimed = f
        .repository
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(f.workspace_id),
            8,
            OffsetDateTime::now_utc(),
        )
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the approved ask is claimable");
    let outcome = f
        .repository
        .execute_action(
            WorkspaceId::from_uuid(f.workspace_id),
            action,
            OffsetDateTime::now_utc(),
        )
        .await;
    assert!(
        outcome.is_err(),
        "a declined pair must fail the send-time re-check, not send"
    );

    // Nothing left the building: no outward event, no campaign touch.
    let emitted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM outbox_events
         WHERE workspace_id = $1 AND payload->>'action_id' = $2",
    )
    .bind(f.workspace_id)
    .bind(action_id.to_string())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(emitted, 0, "a refused send emits nothing");
    let (status, _, declined_via) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "declined");
    assert_eq!(declined_via.as_deref(), Some("operator"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn cancelling_the_ask_in_the_queue_is_the_same_no() -> Result<(), Box<dyn std::error::Error>>
{
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 20).await?;
    let action_id = pending_ask(&f, beacon_id, beacon_version, event_id).await?;
    assert!(pair_in_due_set(&f, beacon_id, event_id).await?);

    // Cancelling the waiting ask is the booker's "not this pair" — it must
    // land on the relationship ledger, or the next cycle re-recommends it.
    let mutation = f
        .repository
        .cancel_action(
            WorkspaceId::from_uuid(f.workspace_id),
            crowdrelay_domain::AutopilotActionId::from_uuid(action_id),
            &crowdrelay_application::IdempotencyKey::parse(format!(
                "beacon-cancel-{}",
                &action_id.simple().to_string()[20..]
            ))?,
            None,
        )
        .await?;
    assert_eq!(mutation.status, "cancelled");

    let (status, _, declined_via) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "declined");
    assert_eq!(declined_via.as_deref(), Some("operator"));
    assert!(
        !pair_in_due_set(&f, beacon_id, event_id).await?,
        "a cancelled pair must leave the due set"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_consumed_answer_key_conflicts_instead_of_merging()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 20).await?;
    let _ = pending_ask(&f, beacon_id, beacon_version, event_id).await?;
    let key = format!("answer-key-{}", &Uuid::now_v7().simple().to_string()[20..]);

    outreach_state::defer_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        5,
        &key,
        None,
    )
    .await?
    .expect("first answer lands");

    // A second answer on the same key must not merge silently into the
    // first one's audit row — that is what a network retry and a real
    // second decision look identical to.
    let replay = outreach_state::decline_beacon_outreach(
        &f.pool,
        f.workspace_id,
        beacon_id,
        event_id,
        None,
        &key,
        None,
    )
    .await?;
    assert!(matches!(
        replay,
        Err(OutreachStateRefusal::IdempotencyConflict)
    ));
    let (status, _, _) = campaign_row(&f, beacon_id, event_id).await?;
    assert_eq!(status, "candidate", "the refused replay leaves no mark");
    Ok(())
}
