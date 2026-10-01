//! The org-wide attention budget against a real Postgres (§4d-3).
//!
//! `contact_governor` already binds the seven-day cooldown across an
//! organization — one person, one roster, one window a week. What it cannot
//! do is count: one row per (workspace, contact), updated in place, holds no
//! answer to "of the four things this roster wanted to tell this person this
//! month, how many already went out". `contact_touches` is the
//! append-only half — one row per action that reserved a window — and
//! `reserve_contact_window` refuses once the trailing thirty days reach
//! `ORG_MONTHLY_CONTACT_BUDGET`.
//!
//! These tests run the reservation through the real path —
//! `execute_action` on a `RequestBookingOutreach` action — because the parts
//! that can be wrong are exactly the ones a copied predicate would not catch:
//! the count joining workspaces the same way the sibling gate does, the
//! ledger write sharing the reservation's transaction, a replayed action not
//! spending twice, and the refusal carrying `org_attention_budget` instead of
//! reading back as a generic `state_changed`.
//!
//! Every test runs against a disposable database it creates and drops itself;
//! the schema comes from `MIGRATOR.run`, so a migration that does not apply
//! fails here before it can fail a deploy.

use crate::common;

use std::time::Duration;

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, AutopilotActionRepository, ClaimedAutopilotAction,
    ORG_ATTENTION_BUDGET_ERROR_KIND,
};
use crowdrelay_domain::booking::BookingOutreachPhase;
use crowdrelay_domain::{AutopilotActionId, BookingTargetId, CityId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

fn repository(pool: &PgPool) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: "postgres://unused".to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn organization(pool: &PgPool, name: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("org-{}", id.simple()))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    name: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind(name)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code) VALUES ($1, $1, 'PL')
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?)
}

/// A booking target whose `contact_email` is the person the budget protects.
/// `active`, `accepts_booking` and `version = 1` are the table's defaults —
/// exactly what `lock_booking_target_for_execution` requires.
async fn target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    contact_email: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email, priority)
         VALUES ($1, $2, 'promoter', 'Promoter', $3, 50) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(contact_email)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// An executor advertising `booking.outreach`, the way the worker heartbeat
/// writes it — without one the claim parks the action instead of claiming it.
async fn advertise(pool: &PgPool, workspace_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let executor = format!("n8n-budget-{}", Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO executor_instances
            (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
         VALUES ($1,$2,'1','sha',$3,$4)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(&executor)
    .bind(now)
    .bind(now + Duration::from_secs(1800))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO executor_capabilities
            (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
         VALUES ($1,$2,'booking.outreach','1',$3,$4)",
    )
    .bind(workspace_id)
    .bind(&executor)
    .bind(now)
    .bind(now + Duration::from_secs(1800))
    .execute(pool)
    .await?;
    Ok(())
}

/// A queued booking-outreach action for `target_id`, seeded the way the
/// evaluator's persist path writes it: decision row first — `trace_id` is
/// NOT NULL — then the action with the typed payload serialized to JSONB.
async fn outreach_action(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    target_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'booking_opportunity','city',$4,
                 'request_booking_outreach',9000,'require_approval','seeded budget proposal',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("budget-decision-{}", Uuid::now_v7()))
    .bind(city_id)
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    let payload = serde_json::to_value(AutopilotActionPayload::RequestBookingOutreach {
        city_id: CityId::from_uuid(city_id),
        target_id: BookingTargetId::from_uuid(target_id),
        target_version: 1,
        target_name: "Promoter".to_owned(),
        score: 71,
        phase: BookingOutreachPhase::Initial,
        proposed_window: None,
        additional_recipients: vec![],
        venue_evidence: None,
        draft: crowdrelay_domain::booking_letter::BookingLetter {
            subject: "booking".to_owned(),
            // The identical-draft refusal compares letters across actions —
            // a fixture that always writes the same body would collide with
            // itself the second time the budget test seeds one.
            body: format!("letter {}", Uuid::now_v7()),
        },
    })?;
    sqlx::query_scalar(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, action_class)
         VALUES ($1,$2,$3,'booking_opportunity','booking.outreach.request','city',
                 $4,$5,$6,'queued','third_party') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(city_id)
    .bind(format!("budget-action-{}", Uuid::now_v7()))
    .bind(payload)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Claims `action_id` and executes it at `now`, the way the worker loop does.
/// The claim runs at real time — `available_at` is stamped at insert — while
/// the execution clock is the test's, which is what lands in `touched_at`.
async fn execute(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    now: OffsetDateTime,
) -> Result<ClaimedAutopilotAction, RepositoryError> {
    let repo = repository(pool);
    let claimed = repo
        .claim_due_autonomous_actions(
            WorkspaceId::from_uuid(workspace_id),
            8,
            OffsetDateTime::now_utc(),
        )
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the seeded outreach is claimable")
        .clone();
    repo.execute_action(WorkspaceId::from_uuid(workspace_id), &action, now)
        .await?;
    Ok(action)
}

/// A contact address of this run's own. The ledger is keyed on the contact
/// across every workspace — that is the point of it — and the suite shares
/// one database, so a fixed address inherits every earlier run's touches and
/// the budget refuses before the test's own first send.
fn contact(label: &str) -> String {
    format!("{}@example.com", common::unique_slug(label, Uuid::now_v7()))
}

/// The ledger count for one contact across every workspace — the figure the
/// budget reads, asserted directly rather than through a second predicate.
async fn touches(pool: &PgPool, contact: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM contact_touches WHERE normalized_contact = $1",
    )
    .bind(contact)
    .fetch_one(pool)
    .await
}

/// What the worker does with a failed execute: marks the action with the kind
/// the error mapped to. Besides being the real flow, it frees the in-flight
/// subject slot so the next ask to the same person can be seeded.
async fn fail(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    kind: &'static str,
) -> Result<(), RepositoryError> {
    repository(pool)
        .fail_action(
            WorkspaceId::from_uuid(workspace_id),
            AutopilotActionId::from_uuid(action_id),
            1,
            kind,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
}

/// The action row's terminal record — `(status, last_error_kind)`.
async fn action_outcome(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
) -> Result<(String, Option<String>), sqlx::Error> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, last_error_kind FROM autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_one(pool)
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_roster_shares_one_monthly_attention_budget() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let fan = &contact("shared-fan");

    let label = organization(pool, "Test roster").await?;
    let act_a = workspace(pool, "Act A", Some(label)).await?;
    let act_b = workspace(pool, "Act B", Some(label)).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    // One person behind two acts: each workspace books them through its own
    // target row, keyed to the same normalized contact.
    let target_a = target(pool, act_a, wroclaw, fan).await?;
    let target_b = target(pool, act_b, wroclaw, fan).await?;
    advertise(pool, act_a).await?;
    advertise(pool, act_b).await?;

    // Three touches inside the trailing month, split across the roster:
    // act A, act A again a week on, then act B once A's window lapses.
    let first = outreach_action(pool, act_a, wroclaw, target_a).await?;
    execute(pool, act_a, first, now).await?;
    let second = outreach_action(pool, act_a, wroclaw, target_a).await?;
    execute(pool, act_a, second, now + time::Duration::days(8)).await?;
    let third = outreach_action(pool, act_b, wroclaw, target_b).await?;
    execute(pool, act_b, third, now + time::Duration::days(16)).await?;
    assert_eq!(touches(pool, fan).await?, 3);

    // The fourth is refused whichever sibling sends it — the spend is the
    // roster's, not the act's. `fail_action` is what the worker runs next;
    // its `last_error_kind` is what the lapsed/attention reads see.
    let fourth = outreach_action(pool, act_b, wroclaw, target_b).await?;
    let outcome = execute(pool, act_b, fourth, now + time::Duration::days(24)).await;
    assert!(
        matches!(
            outcome,
            Err(RepositoryError::ConflictBecause(
                ORG_ATTENTION_BUDGET_ERROR_KIND
            ))
        ),
        "a fourth touch inside the month must refuse as {ORG_ATTENTION_BUDGET_ERROR_KIND}, got {outcome:?}"
    );
    fail(pool, act_b, fourth, ORG_ATTENTION_BUDGET_ERROR_KIND).await?;
    assert_eq!(
        action_outcome(pool, act_b, fourth).await?,
        (
            "failed".to_owned(),
            Some(ORG_ATTENTION_BUDGET_ERROR_KIND.to_owned())
        ),
        "the refusal must read as the budget, not as a generic state change"
    );
    let fifth = outreach_action(pool, act_a, wroclaw, target_a).await?;
    let outcome = execute(pool, act_a, fifth, now + time::Duration::days(25)).await;
    assert!(
        matches!(
            outcome,
            Err(RepositoryError::ConflictBecause(
                ORG_ATTENTION_BUDGET_ERROR_KIND
            ))
        ),
        "the refusal binds the sending sibling too, got {outcome:?}"
    );
    fail(pool, act_a, fifth, ORG_ATTENTION_BUDGET_ERROR_KIND).await?;

    // A refused send spends nothing: no touch row, no new governor window, no
    // emission — the transaction took all of it down together. The governor
    // holds one row per (workspace, contact), so three touches across two
    // acts are two rows — the ledger is the place that counts actions.
    assert_eq!(touches(pool, fan).await?, 3);
    let reserved = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM contact_governor
         WHERE workspace_id IN ($1, $2) AND normalized_contact = $3",
    )
    .bind(act_a)
    .bind(act_b)
    .bind(fan)
    .fetch_one(pool)
    .await?;
    assert_eq!(reserved, 2, "one governor row per workspace+contact");
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM outbox_events
         WHERE workspace_id IN ($1, $2)
           AND event_type = 'crowdrelay.booking.outreach_requested'",
    )
    .bind(act_a)
    .bind(act_b)
    .fetch_one(pool)
    .await?;
    assert_eq!(emitted, 3, "a refused budget sends no letter");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_different_organization_shares_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let fan = &contact("stranger-fan");

    let label = organization(pool, "First roster").await?;
    let other_label = organization(pool, "Second roster").await?;
    let act_a = workspace(pool, "Act A", Some(label)).await?;
    let outsider = workspace(pool, "Outsider", Some(other_label)).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    let target_a = target(pool, act_a, wroclaw, fan).await?;
    let target_out = target(pool, outsider, wroclaw, fan).await?;
    advertise(pool, act_a).await?;
    advertise(pool, outsider).await?;

    // First roster spends its whole monthly share.
    for day in [0_i64, 8, 16] {
        let action = outreach_action(pool, act_a, wroclaw, target_a).await?;
        execute(pool, act_a, action, now + time::Duration::days(day)).await?;
    }
    assert_eq!(touches(pool, fan).await?, 3);

    // The other organization's workspace touches the same person a day later:
    // no cooldown crosses the boundary and no spend does either — the ledger
    // records the touch while the budget stays each roster's own.
    let action = outreach_action(pool, outsider, wroclaw, target_out).await?;
    execute(pool, outsider, action, now + time::Duration::days(1)).await?;
    assert_eq!(
        touches(pool, fan).await?,
        4,
        "the ledger counts every roster's touch; the budget binds only its own"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_lone_workspace_keeps_the_cooldown_and_gains_the_cap()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let fan = &contact("solo-fan");

    // Every tenant today has no organization — the cap must hold for them too,
    // and the cooldown must keep meaning what it meant.
    let solo = workspace(pool, "Solo act", None).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    let target_id = target(pool, solo, wroclaw, fan).await?;
    advertise(pool, solo).await?;

    let first = outreach_action(pool, solo, wroclaw, target_id).await?;
    execute(pool, solo, first, now).await?;

    // A day later the seven-day window still refuses — as a plain conflict,
    // not as the budget: the person was reachable this month, just not today.
    let next_day = outreach_action(pool, solo, wroclaw, target_id).await?;
    let outcome = execute(pool, solo, next_day, now + time::Duration::days(1)).await;
    assert!(
        matches!(outcome, Err(RepositoryError::Conflict)),
        "the weekly cooldown refuses as before, got {outcome:?}"
    );
    fail(pool, solo, next_day, "state_changed").await?;
    assert_eq!(touches(pool, fan).await?, 1);

    // Weeks pass; the cooldown keeps admitting and the ledger keeps counting.
    for day in [8_i64, 16] {
        let action = outreach_action(pool, solo, wroclaw, target_id).await?;
        execute(pool, solo, action, now + time::Duration::days(day)).await?;
    }
    assert_eq!(touches(pool, fan).await?, 3);

    let fourth = outreach_action(pool, solo, wroclaw, target_id).await?;
    let outcome = execute(pool, solo, fourth, now + time::Duration::days(24)).await;
    assert!(
        matches!(
            outcome,
            Err(RepositoryError::ConflictBecause(
                ORG_ATTENTION_BUDGET_ERROR_KIND
            ))
        ),
        "a roster of one is still one sender: the fourth touch refuses, got {outcome:?}"
    );
    fail(pool, solo, fourth, ORG_ATTENTION_BUDGET_ERROR_KIND).await?;
    assert_eq!(
        action_outcome(pool, solo, fourth).await?,
        (
            "failed".to_owned(),
            Some(ORG_ATTENTION_BUDGET_ERROR_KIND.to_owned())
        ),
        "the budget refusal must not read as a cooldown"
    );
    assert_eq!(touches(pool, fan).await?, 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_replayed_action_does_not_spend_twice() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let fan = &contact("replayed-fan");

    let label = organization(pool, "Test roster").await?;
    let act = workspace(pool, "Act", Some(label)).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    let target_id = target(pool, act, wroclaw, fan).await?;
    advertise(pool, act).await?;

    // Fill the month: this action's touch plus two more.
    let first = outreach_action(pool, act, wroclaw, target_id).await?;
    let first_action = execute(pool, act, first, now).await?;
    for day in [8_i64, 16] {
        let action = outreach_action(pool, act, wroclaw, target_id).await?;
        execute(pool, act, action, now + time::Duration::days(day)).await?;
    }
    assert_eq!(touches(pool, fan).await?, 3);

    // Re-executing the first action must not spend again — its touch row is
    // already counted, and a budget the action already paid cannot refuse it
    // a second time. Execution now refuses before touching the budget at the
    // claim guard — the action is `succeeded`, not `processing` — which is
    // a plain `Conflict`, never the budget's kind.
    let replay = repository(pool)
        .execute_action(
            WorkspaceId::from_uuid(act),
            &first_action,
            now + time::Duration::days(20),
        )
        .await;
    assert!(
        matches!(replay, Err(RepositoryError::Conflict)),
        "a replay may fail at the status guard but never at the budget, got {replay:?}"
    );
    assert_eq!(
        touches(pool, fan).await?,
        3,
        "a replayed action spent the same person twice"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn touches_older_than_thirty_days_stop_counting() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let fan = &contact("old-fan");

    let label = organization(pool, "Test roster").await?;
    let act = workspace(pool, "Act", Some(label)).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    let target_id = target(pool, act, wroclaw, fan).await?;
    advertise(pool, act).await?;

    // Three touches, all spent more than thirty days ago: the ledger keeps
    // them — history does not vanish — but the trailing window no longer
    // counts them.
    for days_ago in [60_i64, 50, 40] {
        let action = outreach_action(pool, act, wroclaw, target_id).await?;
        execute(pool, act, action, now - time::Duration::days(days_ago)).await?;
    }
    assert_eq!(touches(pool, fan).await?, 3);

    let fresh = outreach_action(pool, act, wroclaw, target_id).await?;
    execute(pool, act, fresh, now).await?;
    assert_eq!(
        touches(pool, fan).await?,
        4,
        "a contact untouched for a month has a fresh share"
    );

    Ok(())
}
