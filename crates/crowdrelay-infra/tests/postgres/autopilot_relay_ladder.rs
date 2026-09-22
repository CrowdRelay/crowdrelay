//! The post relay ladder against a real Postgres (P.5).
//!
//! One synced post fans out into a spread of rungs — the owned-audience push
//! and one community relay per admitted community — and each arrived as its
//! own approval. The ladder is the operator's one "yes" over the spread:
//! every parked rung sharing the post's `action:relay:{source}:` prefix
//! queues together, marked `operator:relay_ladder` so a revoke cancels
//! exactly what the ladder freed — never a rung a person approved on its
//! own, and never a different post's spread.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    source_id: Uuid,
    other_source_id: Uuid,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Relay ladder E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let source_id = Uuid::now_v7();
    let other_source_id = Uuid::now_v7();
    for (id, key) in [
        (source_id, format!("{label}-post-a-{suffix}")),
        (other_source_id, format!("{label}-post-b-{suffix}")),
    ] {
        sqlx::query(
            "INSERT INTO content_sources (
                 id, workspace_id, source_kind, source_key, title, occurred_at, expires_at
             ) VALUES ($1,$2,'social_post',$3,$4,$5,$6)",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(key)
        .bind("A post the band made")
        .bind(now - time::Duration::hours(2))
        .bind(now + time::Duration::days(30))
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
        source_id,
        other_source_id,
    })
}

/// What varies between the rungs of one post's spread — the rest is fixture.
struct Rung<'a> {
    suffix: String,
    action_kind: &'a str,
    subject_kind: &'a str,
    subject_id: Uuid,
    status: &'a str,
    approved_by: Option<&'a str>,
    action_class: &'a str,
}

/// One relay rung. `idempotency_key` is what ties a rung to its post:
/// `action:relay:{source}:signal_push` or `action:relay:{source}:community:{t}`.
async fn seed_rung(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
    rung: Rung<'_>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'content_supply',$4,$5,
                   'relay_owned_post',9000,'require_approval','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "decision:relay:{source_id}:{}:{decision_id}",
        rung.suffix
    ))
    .bind(rung.subject_kind)
    .bind(rung.subject_id)
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, action_class,
             approved_at, approved_by, approval_expires_at
         ) VALUES ($1,$2,$3,'content_supply',$4,$5,$6,$7,'{}'::jsonb,$8,$9,$10,$11,$12)",
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(rung.action_kind)
    .bind(rung.subject_kind)
    .bind(rung.subject_id)
    .bind(format!("action:relay:{source_id}:{}", rung.suffix))
    .bind(rung.status)
    .bind(rung.action_class)
    .bind(rung.approved_by.map(|_| now))
    .bind(rung.approved_by)
    .bind(if rung.status == "awaiting_approval" {
        Some(now + time::Duration::hours(72))
    } else {
        None
    })
    .execute(pool)
    .await?;
    Ok(action_id)
}

async fn rung_state(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<(String, Option<String>, OffsetDateTime), Box<dyn std::error::Error>> {
    let row: (String, Option<String>, OffsetDateTime) = sqlx::query_as(
        "SELECT status, approved_by, available_at FROM autopilot_actions \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

fn relay_rung(suffix: String, subject_id: Uuid) -> Rung<'static> {
    Rung {
        suffix,
        action_kind: "community.engage.request",
        subject_kind: "target_community",
        subject_id,
        status: "awaiting_approval",
        approved_by: None,
        action_class: "third_party",
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approving_a_post_ladder_queues_the_whole_spread_and_revoke_stops_it()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("relay-ladder").await?;
    let src = fixture.source_id;

    // The spread as the evaluator emits it: one push, two community relays.
    let push = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        Rung {
            suffix: "signal_push".to_owned(),
            action_kind: "signal.push.request",
            subject_kind: "content_source",
            subject_id: src,
            status: "awaiting_approval",
            approved_by: None,
            action_class: "owned_audience",
        },
    )
    .await?;
    let community_a = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7()),
    )
    .await?;
    let community_b = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7()),
    )
    .await?;
    // A rung a person already approved on its own — revoke must not take it.
    let individual = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        Rung {
            status: "queued",
            approved_by: Some("operator:admin_api_key"),
            ..relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7())
        },
    )
    .await?;
    // Another post's spread in the same workspace — the prefix is the ladder.
    let other_post = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        fixture.other_source_id,
        Rung {
            suffix: "signal_push".to_owned(),
            action_kind: "signal.push.request",
            subject_kind: "content_source",
            subject_id: fixture.other_source_id,
            status: "awaiting_approval",
            approved_by: None,
            action_class: "owned_audience",
        },
    )
    .await?;

    // A rung whose approval window already closed is not the ladder's to
    // release — it stays parked for a person to renew or let lapse.
    let expired = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        Rung {
            ..relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7())
        },
    )
    .await?;
    sqlx::query(
        "UPDATE autopilot_actions SET approval_expires_at=$3 \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(expired)
    .bind(OffsetDateTime::now_utc() - time::Duration::hours(1))
    .execute(&fixture.pool)
    .await?;

    // A rung already claimed and running under the ladder's provenance —
    // revoke must not reach back into work that started.
    let processing = seed_rung(
        &fixture.pool,
        fixture.workspace_id,
        src,
        Rung {
            status: "processing",
            approved_by: Some("operator:relay_ladder"),
            ..relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7())
        },
    )
    .await?;

    // A second tenant whose rungs happen to carry this source's key prefix —
    // only the workspace predicate protects them, on both directions.
    let foreign_workspace = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(foreign_workspace.into_uuid())
        .bind(format!(
            "relay-ladder-foreign-{}",
            foreign_workspace.into_uuid().simple()
        ))
        .bind("Foreign workspace")
        .execute(&fixture.pool)
        .await?;
    let foreign_parked = seed_rung(
        &fixture.pool,
        foreign_workspace,
        src,
        Rung {
            suffix: "signal_push".to_owned(),
            action_kind: "signal.push.request",
            subject_kind: "content_source",
            subject_id: src,
            status: "awaiting_approval",
            approved_by: None,
            action_class: "owned_audience",
        },
    )
    .await?;
    let foreign_released = seed_rung(
        &fixture.pool,
        foreign_workspace,
        src,
        Rung {
            status: "queued",
            approved_by: Some("operator:relay_ladder"),
            ..relay_rung(format!("community:{}", Uuid::now_v7()), Uuid::now_v7())
        },
    )
    .await?;

    let mutation = fixture
        .repository
        .approve_relay_ladder(
            fixture.workspace_id,
            src,
            &IdempotencyKey::parse("ladder-approve-1")?,
            None,
        )
        .await?;
    assert_eq!(
        mutation.status, "approved:3",
        "the whole spread moved at once"
    );

    for rung in [push, community_a, community_b] {
        let (status, approved_by, available_at) =
            rung_state(&fixture.pool, fixture.workspace_id, rung).await?;
        assert_eq!(status, "queued");
        assert_eq!(approved_by.as_deref(), Some("operator:relay_ladder"));
        assert!(
            available_at > OffsetDateTime::now_utc(),
            "the outward hold still applies — every relay class is outward"
        );
    }
    let (status, _, _) = rung_state(&fixture.pool, fixture.workspace_id, other_post).await?;
    assert_eq!(
        status, "awaiting_approval",
        "another post's ladder stays parked"
    );
    let (status, _, _) = rung_state(&fixture.pool, fixture.workspace_id, expired).await?;
    assert_eq!(
        status, "awaiting_approval",
        "an expired window is a person's call, not the ladder's"
    );
    let (status, _, _) = rung_state(&fixture.pool, foreign_workspace, foreign_parked).await?;
    assert_eq!(
        status, "awaiting_approval",
        "a foreign tenant's rung sharing the key prefix stays parked"
    );

    let replay = fixture
        .repository
        .approve_relay_ladder(
            fixture.workspace_id,
            src,
            &IdempotencyKey::parse("ladder-approve-1")?,
            None,
        )
        .await?;
    assert!(replay.replayed);
    assert_eq!(
        replay.status, "approved",
        "the replay records that the release happened, not a recount"
    );

    let mutation = fixture
        .repository
        .revoke_relay_ladder(
            fixture.workspace_id,
            src,
            &IdempotencyKey::parse("ladder-revoke-1")?,
            None,
        )
        .await?;
    assert_eq!(mutation.status, "revoked:3");

    for rung in [push, community_a, community_b] {
        let (status, _, _) = rung_state(&fixture.pool, fixture.workspace_id, rung).await?;
        assert_eq!(
            status, "cancelled",
            "a ladder-released rung stops with the ladder"
        );
    }
    let (status, approved_by, _) =
        rung_state(&fixture.pool, fixture.workspace_id, individual).await?;
    assert_eq!(status, "queued");
    assert_eq!(approved_by.as_deref(), Some("operator:admin_api_key"));
    let (status, _, _) = rung_state(&fixture.pool, fixture.workspace_id, other_post).await?;
    assert_eq!(
        status, "awaiting_approval",
        "another post's parked spread survives a revoke too"
    );
    let (status, _, _) = rung_state(&fixture.pool, fixture.workspace_id, processing).await?;
    assert_eq!(
        status, "processing",
        "work already running keeps its record"
    );
    let (status, _, _) = rung_state(&fixture.pool, foreign_workspace, foreign_released).await?;
    assert_eq!(
        status, "queued",
        "a foreign tenant's queued rung survives the revoke"
    );

    let replay = fixture
        .repository
        .revoke_relay_ladder(
            fixture.workspace_id,
            src,
            &IdempotencyKey::parse("ladder-revoke-1")?,
            None,
        )
        .await?;
    assert!(replay.replayed);
    assert_eq!(replay.status, "revoked");

    // A mistyped or foreign source id is a not-found, not a released zero.
    let result = fixture
        .repository
        .approve_relay_ladder(
            fixture.workspace_id,
            Uuid::now_v7(),
            &IdempotencyKey::parse("ladder-approve-missing")?,
            None,
        )
        .await;
    assert!(matches!(result, Err(RepositoryError::NotFound)));
    let result = fixture
        .repository
        .revoke_relay_ladder(
            fixture.workspace_id,
            Uuid::now_v7(),
            &IdempotencyKey::parse("ladder-revoke-missing")?,
            None,
        )
        .await;
    assert!(matches!(result, Err(RepositoryError::NotFound)));

    fixture.pool.close().await;
    Ok(())
}
