//! Beacon discovery seed entities against a real Postgres.
//!
//! The bill is already known data — a discovery sweep that ignores the
//! announced acts and the host venue sends the executor scouting blind for
//! entities we could have named. The risky part of seeding is the join that
//! decides which bill-mates count: the workspace's own act must never seed
//! itself, and a roster sibling (same organization) keeps the org-internal
//! path instead of entering cold discovery. Neither rule is visible to a unit
//! test — only real rows decide it.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{AutopilotActionPayload, AutopilotActionRepository};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn discovery_request_seeds_non_sibling_bill_mates_and_the_venue()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let now = OffsetDateTime::now_utc();

    // Foreign table owned by crowdrelay-agents. The production path now
    // delegates research directly to this scheduler rather than an unrouted
    // webhook.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_tasks (
            id uuid PRIMARY KEY,
            workspace_id uuid NOT NULL,
            template_id text NOT NULL,
            model_id text NOT NULL,
            prompt text NOT NULL,
            status text NOT NULL,
            tier text NOT NULL DEFAULT 'basic',
            metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
            created_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(&pool)
    .await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    let org_a = Uuid::now_v7();
    let org_b = Uuid::now_v7();
    for (org_id, slug) in [(org_a, "org-a"), (org_b, "org-b")] {
        sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(format!("{slug}-{suffix}"))
            .bind(slug)
            .execute(&pool)
            .await?;
    }
    // Own workspace in org A, a roster sibling in org A, an unrelated tenant
    // workspace in org B.
    let sibling_ws = Uuid::now_v7();
    let tenant_ws = Uuid::now_v7();
    for (ws, org, slug) in [
        (workspace_id.into_uuid(), org_a, "own"),
        (sibling_ws, org_a, "sibling"),
        (tenant_ws, org_b, "tenant"),
    ] {
        sqlx::query(
            "INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(ws)
        .bind(format!("seed-{slug}-{suffix}"))
        .bind(slug)
        .bind(org)
        .execute(&pool)
        .await?;
    }

    let city_id = Uuid::now_v7();
    sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, 'PL')")
        .bind(city_id)
        .bind(format!("seed-city-{suffix}"))
        .bind("Seed City")
        .execute(&pool)
        .await?;

    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at) \
         VALUES ($1, $2, $3, $4, 'Seed Night', 'Klub Seed', $5, 'published', $6)",
    )
    .bind(event_id)
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .bind(format!("seed-night-{suffix}"))
    .bind(now + time::Duration::days(30))
    .bind(now)
    .execute(&pool)
    .await?;

    let peer_act_id = Uuid::now_v7();
    sqlx::query("INSERT INTO place_peer_acts (id, name_key, display_name) VALUES ($1, $2, $3)")
        .bind(peer_act_id)
        .bind(format!("peer-act-{suffix}"))
        .bind("Peer Act")
        .execute(&pool)
        .await?;

    // The whole resolution matrix on one bill: own act, org sibling, unrelated
    // tenant act, a peer-identity act and a bare-name unclaimed act.
    for (slug, name, position, act_ws, peer) in [
        (
            "own-act",
            "Own Act",
            0,
            Some(workspace_id.into_uuid()),
            None,
        ),
        ("sibling-act", "Sibling Act", 1, Some(sibling_ws), None),
        ("tenant-act", "Tenant Act", 2, Some(tenant_ws), None),
        ("peer-act", "Peer Act", 3, None, Some(peer_act_id)),
        ("unclaimed-act", "Unclaimed Act", 4, None, None),
    ] {
        sqlx::query(
            "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, \
             act_workspace_id, peer_act_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .bind(slug)
        .bind(name)
        .bind(position)
        .bind(act_ws)
        .bind(peer)
        .execute(&pool)
        .await?;
    }

    // Research is in-process agent work now, so no n8n/executor capability is
    // required to let the Brain act on the discovery decision.
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'beacon','event',$4,$5,10000,
                  'auto_execute','seed test','{}','{}','{}',$6,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(event_id)
    .bind("test.beacon.discovery.request")
    .bind(now)
    .execute(&pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, approved_at, approved_by, available_at
        ) VALUES ($1,$2,$3,'beacon','beacon.discovery.request','event',$4,$5,$6,
                  'queued',$7,'system:test',$7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id)
    .bind(format!("action-{action_id}"))
    .bind(serde_json::to_value(
        AutopilotActionPayload::RequestBeaconDiscovery {
            event_id: crowdrelay_domain::EventId::from_uuid(event_id),
            target_count: 5,
        },
    )?)
    .bind(now)
    .execute(&pool)
    .await?;

    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(1),
        },
    );

    let claimed = repository.claim_due_actions(workspace_id, 8, now).await?;
    assert_eq!(claimed.len(), 1);
    repository
        .execute_action(workspace_id, &claimed[0], now)
        .await?;

    let (template_id, tier, prompt, metadata) =
        sqlx::query_as::<_, (String, String, String, serde_json::Value)>(
            r#"
            SELECT template_id,tier,prompt,metadata
            FROM agent_service_tasks
            WHERE workspace_id=$1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?;

    assert_eq!(template_id, "event-network-scout");
    assert_eq!(tier, "premium");
    assert_eq!(metadata["action_id"], action_id.to_string());
    assert!(prompt.contains(&event_id.to_string()));
    assert!(prompt.contains(&city_id.to_string()));
    assert!(prompt.contains("Seed City"));
    assert!(prompt.contains("Klub Seed"));
    assert!(prompt.contains("Tenant Act"));
    assert!(prompt.contains("Peer Act"));
    assert!(prompt.contains("Unclaimed Act"));
    assert!(!prompt.contains("Own Act"));
    assert!(!prompt.contains("Sibling Act"));
    assert!(prompt.contains("local_artists_craftspeople_and_alternative_culture_nodes"));
    assert!(prompt.contains("do not contact anybody"));

    Ok(())
}
