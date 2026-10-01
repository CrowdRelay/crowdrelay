//! The tracked-link backstop at dispatch, against a real Postgres.
//!
//! The type system is the first fence: every outreach composer takes a
//! `TrackedLink`, which only exists as `{member_site}/l/{slug}`. But the type
//! only covers what was composed *today* — a draft persisted before the gate
//! shipped, or a body a person edited after approval, reaches dispatch
//! carrying whatever it carries. `gate_outward_emission` is the last seam
//! before the letter leaves, so it scans a third-party draft's body and
//! refuses the send when a URL does not resolve through the tenant's own
//! redirect.
//!
//! These tests run the refusal through `execute_action` on a real
//! `booking.outreach.request` action — a copied predicate cannot catch a
//! gate wired to the wrong payload shape, and the booking path is the one
//! production walks. An owned-audience broadcast is deliberately out of
//! scope: an event announcement may carry the ticketing provider's own URL,
//! which is not ours to wrap in a redirect.

use std::time::Duration;

use crate::common;
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{AutopilotActionPayload, AutopilotActionRepository};
use crowdrelay_domain::booking::BookingOutreachPhase;
use crowdrelay_domain::{BookingTargetId, CityId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

pub(super) fn repository(pool: &PgPool, url: &str) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

pub(super) async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("linkgate-{}", id.simple()))
        .bind("Link Gate Tests")
        .execute(pool)
        .await?;
    Ok(id)
}

pub(super) async fn city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
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

pub(super) async fn target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email, priority)
         VALUES ($1, $2, 'promoter', 'Promoter', $3, 50) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(format!("gate-{}@example.test", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// The executor advertisement a claim needs — the same rows a worker
/// heartbeat writes.
pub(super) async fn advertise(pool: &PgPool, workspace_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let executor = format!("n8n-linkgate-{}", Uuid::now_v7().simple());
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

/// A queued `booking.outreach.request` action whose approved draft is the
/// body under test — seeded the way the evaluator's persist path writes it.
pub(super) async fn outreach_action(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    target_id: Uuid,
    body: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'booking_opportunity','city',$4,
                 'request_booking_outreach',9000,'require_approval','seeded link-gate proposal',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("linkgate-decision-{}", Uuid::now_v7()))
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
            body: body.to_owned(),
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
    .bind(format!("linkgate-action-{}", Uuid::now_v7()))
    .bind(payload)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn execute(
    pool: &PgPool,
    url: &str,
    workspace_id: Uuid,
    action_id: Uuid,
) -> Result<(), RepositoryError> {
    let repo = repository(pool, url);
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
    repo.execute_action(
        WorkspaceId::from_uuid(workspace_id),
        &action,
        OffsetDateTime::now_utc(),
    )
    .await
}

async fn live_link(
    pool: &PgPool,
    workspace_id: Uuid,
    slug: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO smart_links (workspace_id, slug, destination_url, active)
         VALUES ($1, $2, 'https://listen.example/music', true)
         ON CONFLICT (workspace_id, slug) DO UPDATE SET active = true",
    )
    .bind(workspace_id)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn emitted_for(pool: &PgPool, workspace_id: Uuid, action_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM outbox_events
         WHERE workspace_id = $1 AND payload->>'action_id' = $2",
    )
    .bind(workspace_id)
    .bind(action_id.to_string())
    .fetch_one(pool)
    .await
    .expect("read outbox emissions")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stranger_letter_with_an_untracked_link_is_refused_at_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example')",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    // The historical shape: a draft carrying a raw YouTube link — clicked,
    // counted nowhere. The composer cannot produce this any more; the gate
    // exists for everything that still can.
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "Letter for you.\n\nListen: https://www.youtube.com/watch?v=abc123",
    )
    .await?;

    let outcome = execute(&pool, &url, workspace_id, action_id).await;
    assert!(
        matches!(outcome, Err(RepositoryError::ConflictBecause(_))),
        "an untracked URL in a stranger letter must refuse, got {outcome:?}"
    );
    assert_eq!(
        emitted_for(&pool, workspace_id, action_id).await,
        0,
        "a refused letter must leave no outbox emission"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stranger_letter_with_a_tracked_link_passes_the_gate()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example')",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "Letter for you.\n\nListen: https://band.example/l/release-rytual",
    )
    .await?;

    live_link(&pool, workspace_id, "release-rytual").await?;
    execute(&pool, &url, workspace_id, action_id).await?;
    assert_eq!(
        emitted_for(&pool, workspace_id, action_id).await,
        1,
        "a tracked letter emits exactly once"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stranger_letter_with_no_member_site_refuses_any_url()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    // No tenant_settings row at all: no link can verify, so even the tracked
    // shape refuses — the gate fails closed, never waves an unverifiable URL
    // through.
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "Letter for you.\n\nListen: https://band.example/l/release-rytual",
    )
    .await?;

    let outcome = execute(&pool, &url, workspace_id, action_id).await;
    assert!(
        matches!(outcome, Err(RepositoryError::ConflictBecause(_))),
        "an unverifiable site must fail closed, got {outcome:?}"
    );
    assert_eq!(emitted_for(&pool, workspace_id, action_id).await, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stranger_letter_with_no_links_sends_as_before() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "Letter for you. No links, no pitch URL — a plain ask.",
    )
    .await?;

    execute(&pool, &url, workspace_id, action_id).await?;
    assert_eq!(emitted_for(&pool, workspace_id, action_id).await, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn only_live_workspace_redirects_may_leave_in_a_letter()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let other_workspace = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    advertise(&pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example')",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    live_link(&pool, other_workspace, "foreign").await?;
    live_link(&pool, workspace_id, "inactive").await?;
    sqlx::query("UPDATE smart_links SET active = false WHERE workspace_id = $1 AND slug = 'inactive'")
        .bind(workspace_id)
        .execute(&pool)
        .await?;

    for slug in ["missing", "foreign", "inactive", "bad.slug", "site/extra", ""] {
        let target_id = target(&pool, workspace_id, city_id).await?;
        let action_id = outreach_action(
            &pool,
            workspace_id,
            city_id,
            target_id,
            &format!("A personal ask. https://band.example/l/{slug}"),
        )
        .await?;
        let outcome = execute(&pool, &url, workspace_id, action_id).await;
        assert!(
            matches!(outcome, Err(RepositoryError::ConflictBecause(_))),
            "{slug:?} must refuse before emission, got {outcome:?}"
        );
        assert_eq!(emitted_for(&pool, workspace_id, action_id).await, 0);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn restoring_a_redirect_allows_the_same_claim_without_a_new_send_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = workspace(&pool).await?;
    let city_id = city(&pool, &format!("linkgate-{}", Uuid::now_v7().simple())).await?;
    let target_id = target(&pool, workspace_id, city_id).await?;
    advertise(&pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example')",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await?;
    let action_id = outreach_action(
        &pool,
        workspace_id,
        city_id,
        target_id,
        "A personal ask. https://band.example/l/restored",
    )
    .await?;
    let repo = repository(&pool, &url);
    let workspace = WorkspaceId::from_uuid(workspace_id);
    let now = OffsetDateTime::now_utc();
    let claimed = repo.claim_due_autonomous_actions(workspace, 8, now).await?;
    let action = claimed
        .iter()
        .find(|action| action.id.into_uuid() == action_id)
        .expect("the queued letter is claimable");
    assert!(matches!(
        repo.execute_action(workspace, action, now).await,
        Err(RepositoryError::ConflictBecause(_))
    ));
    assert_eq!(emitted_for(&pool, workspace_id, action_id).await, 0);
    live_link(&pool, workspace_id, "restored").await?;
    repo.execute_action(workspace, action, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(emitted_for(&pool, workspace_id, action_id).await, 1);
    Ok(())
}
