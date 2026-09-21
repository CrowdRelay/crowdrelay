//! N.13 — the approval queue's losses are readable after the fact.
//!
//! Every property here is about which rows the read admits, which is a
//! `WHERE` clause and therefore something only a database can answer. The one
//! worth the fixture most is `awaiting_sweep`: an ask past its deadline that
//! the sweep has not reaped yet is filtered out of the pending read by
//! `approval_expires_at > now()` and is not cancelled either, so before this
//! read it appeared on no operator surface at all.

mod common;

use crowdrelay_infra::lapsed_approvals::lapsed_approvals;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_queue_reports_what_it_lost() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "act").await?;
    let neighbour = workspace(pool, "other-act").await?;

    // Nobody answered. The expensive one: work the machine did, spent.
    action(
        pool,
        act,
        "unanswered",
        "gig.outreach.request",
        Some("cancelled"),
        Some("approval_expired"),
        Some(now - time::Duration::days(2)),
        Some(now - time::Duration::days(2)),
        "Klub X has hosted four comparable acts this year",
        now,
    )
    .await?;
    // The machine withdrew its own ask. Not the operator's failure, and must
    // not read as one.
    action(
        pool,
        act,
        "withdrawn",
        "outreach.target.approve",
        Some("cancelled"),
        Some("insufficient_evidence"),
        Some(now - time::Duration::days(1)),
        Some(now - time::Duration::days(1)),
        "all three searches returned credential errors",
        now,
    )
    .await?;
    // Past its deadline, still sitting in the queue: invisible on both sides
    // before this read existed.
    action(
        pool,
        act,
        "awaiting-sweep",
        "content.publish",
        Some("awaiting_approval"),
        None,
        None,
        Some(now - time::Duration::hours(3)),
        "the arc's third beat is due",
        now,
    )
    .await?;
    // Older than the window. Real, and history rather than a loss anybody
    // could still have prevented.
    action(
        pool,
        act,
        "ancient",
        "gig.outreach.request",
        Some("cancelled"),
        Some("approval_expired"),
        Some(now - time::Duration::days(20)),
        Some(now - time::Duration::days(20)),
        "a city that went quiet",
        now,
    )
    .await?;
    // A different death entirely: the executor failed. Not a lapse, and
    // counting it as one would blame the queue for the sender's problem.
    action(
        pool,
        act,
        "executor-failure",
        "gig.outreach.request",
        Some("cancelled"),
        Some("executor_unavailable"),
        Some(now - time::Duration::days(1)),
        Some(now - time::Duration::days(1)),
        "a room worth asking about",
        now,
    )
    .await?;
    // Pending, closing tonight. The forward-looking count.
    action(
        pool,
        act,
        "closing-soon",
        "gig.outreach.request",
        Some("awaiting_approval"),
        None,
        None,
        Some(now + time::Duration::hours(6)),
        "a promoter who books this room",
        now,
    )
    .await?;
    // Pending, days away. Not urgent and must not be counted as such.
    action(
        pool,
        act,
        "closing-later",
        "gig.outreach.request",
        Some("awaiting_approval"),
        None,
        None,
        Some(now + time::Duration::days(3)),
        "another room, no hurry",
        now,
    )
    .await?;
    // Another workspace's lapse. One process serves one workspace; a
    // labelmate's losses are not this operator's queue.
    action(
        pool,
        neighbour,
        "not-ours",
        "gig.outreach.request",
        Some("cancelled"),
        Some("approval_expired"),
        Some(now - time::Duration::days(1)),
        Some(now - time::Duration::days(1)),
        "somebody else's night",
        now,
    )
    .await?;

    let read = lapsed_approvals(pool, act, now).await?;

    assert_eq!(
        read.total,
        3,
        "expected the unanswered ask, the withdrawn one and the unswept one, got {:?}",
        read.items
            .iter()
            .map(|item| (item.action_kind.clone(), item.cause.clone()))
            .collect::<Vec<_>>()
    );
    let causes: Vec<&str> = read.items.iter().map(|item| item.cause.as_str()).collect();
    assert!(
        causes.contains(&"approval_expired"),
        "the ask nobody answered is missing: {causes:?}"
    );
    assert!(
        causes.contains(&"insufficient_evidence"),
        "the machine's withdrawn ask is missing: {causes:?}"
    );
    assert!(
        causes.contains(&"awaiting_sweep"),
        "an ask past its deadline and not yet reaped is on no surface at all: {causes:?}"
    );
    assert!(
        !causes.contains(&"executor_unavailable"),
        "a failed send was counted as a lapsed approval: {causes:?}"
    );

    // Newest first, so the item at the top is the most recent loss.
    assert_eq!(read.items[0].action_kind, "content.publish");
    // The decision's own sentence travels with it, or the entry is a kind and
    // a timestamp and nobody can tell whether it was worth chasing.
    assert!(
        read.items
            .iter()
            .any(|item| item.reason.contains("four comparable acts")),
        "the decision's reason did not reach the read"
    );
    // The unswept one has no finished_at. Absent, not backfilled with now.
    let unswept = read
        .items
        .iter()
        .find(|item| item.cause == "awaiting_sweep")
        .expect("checked above");
    assert!(
        unswept.finished_at.is_none(),
        "an ask that was never cancelled reported a finish time"
    );

    assert_eq!(
        read.expiring_within_24h, 1,
        "one pending ask closes tonight and one closes in three days"
    );
    assert_eq!(read.window_days, 7);
    Ok(())
}

async fn workspace(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

/// One decision and the action it produced, in whatever end state the case
/// needs.
#[allow(clippy::too_many_arguments)]
async fn action(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
    action_kind: &str,
    status: Option<&str>,
    last_error_kind: Option<&str>,
    finished_at: Option<OffsetDateTime>,
    approval_expires_at: Option<OffsetDateTime>,
    reason: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1, $2, $3, 'booking_opportunity', 'city', $4,
                  'gig.proposal', 7000, 'require_approval', $5,
                  '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, $6, $7)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{key}"))
    .bind(Uuid::now_v7())
    .bind(reason)
    .bind(now - time::Duration::days(5))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at,
            last_error_kind, approval_expires_at, created_at
        ) VALUES ($1, $2, $3, 'booking_opportunity', $4, 'city', $5, $6,
                  '{}'::jsonb, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(Uuid::now_v7())
    .bind(format!("action-{key}"))
    .bind(status.unwrap_or("awaiting_approval"))
    .bind(finished_at)
    .bind(last_error_kind)
    .bind(approval_expires_at)
    .bind(now - time::Duration::days(5))
    .execute(pool)
    .await?;
    Ok(())
}
