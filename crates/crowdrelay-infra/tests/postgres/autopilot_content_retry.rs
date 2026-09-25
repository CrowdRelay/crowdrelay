//! A failed content artifact is retried under a new key, not frozen.
//!
//! On 2026-09-25 two `live_listing` requests hit Discord's rate limit and
//! failed. The next cycles asked for the same artifact under the failed
//! action's own idempotency key, which deduplicated onto the failed row and
//! wrote nothing, so neither source got another artifact. What fails here and
//! nowhere else: the snapshot read that reports failures per artifact and
//! source version, and the persist step that must accept the retry's key
//! while still refusing the old one.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    ActionSubject, AutopilotActionPayload, AutopilotContext, AutopilotDecisionRepository,
    DecisionCandidate,
};
use crowdrelay_domain::{
    ContentSourceId, TraceContext, WorkspaceId,
    autonomy::{Confidence, PolicyDisposition},
    content_supply::{
        ContentArtifactKind, ContentSupplyDecision, ContentSupplyPolicy, evaluate_content_supply,
    },
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;

fn candidate(source_id: ContentSourceId, source_version: i64, retry: &str) -> DecisionCandidate {
    // The keys `content_candidates` writes; the application's unit test
    // `a_retried_artifact_gets_its_own_keys` pins the same format.
    DecisionCandidate {
        context: AutopilotContext::ContentSupply,
        subject: ActionSubject::ContentSource(source_id),
        decision_kind: "request_content_artifact",
        confidence: Confidence::saturating_from_basis_points(9_500),
        disposition: PolicyDisposition::RequireApproval,
        reason: "trusted source is missing one required deterministic content artifact",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({}),
        action: AutopilotActionPayload::RequestContentArtifact {
            source_id,
            source_version,
            artifact: ContentArtifactKind::SignalPush,
            template_key: ContentArtifactKind::SignalPush.template_key().to_owned(),
        },
        decision_key: format!(
            "decision:content:v1:{source_id}:sv{source_version}:SignalPush{retry}"
        ),
        action_idempotency_key: format!(
            "action:content:{source_id}:sv{source_version}:SignalPush{retry}"
        ),
    }
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn a_failed_artifact_is_retried_under_a_new_key() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
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
    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "content-retry-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Content retry")
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();
    let (source_uuid, version): (uuid::Uuid, i64) = sqlx::query_as(
        "INSERT INTO content_sources
             (workspace_id, source_kind, source_key, title, occurred_at, expires_at)
         VALUES ($1,'video','retry-video','A video',$2,$3)
         RETURNING id, version",
    )
    .bind(workspace_id.into_uuid())
    .bind(now - time::Duration::days(1))
    .bind(now + time::Duration::days(30))
    .fetch_one(&pool)
    .await?;
    let source_id = ContentSourceId::from_uuid(source_uuid);
    let trace = TraceContext::root(workspace_id);

    // The first request, then its failure at the executor two hours ago.
    // A failure for another version of the same source must not count; it
    // goes in second, because persist refuses a second action for a subject
    // that already has one in flight.
    // Through the transitions the action ledger allows: approved, run, failed.
    let fail_everything = || async {
        let mut failed = 0;
        for (from, to) in [
            ("awaiting_approval", "queued"),
            ("queued", "processing"),
            ("processing", "failed"),
        ] {
            failed = sqlx::query(
                "UPDATE autopilot_actions
                 SET status = $3, last_error_kind = CASE WHEN $3 = 'failed'
                         THEN 'provider_rejected' END,
                     finished_at = $4, updated_at = $4
                 WHERE workspace_id = $1 AND context = 'content_supply' AND status = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(from)
            .bind(to)
            .bind(now - time::Duration::hours(2))
            .execute(&pool)
            .await?
            .rows_affected();
        }
        Ok::<u64, sqlx::Error>(failed)
    };
    for version in [version, version + 5] {
        let written = repository
            .persist_candidate(workspace_id, &candidate(source_id, version, ""), &trace)
            .await?;
        assert!(written.action_created);
        assert_eq!(fail_everything().await?, 1);
    }

    let snapshot = |snapshots: Vec<crowdrelay_domain::content_supply::ContentSupplySnapshot>| {
        snapshots
            .into_iter()
            .find(|snapshot| snapshot.source_id == source_id)
            .expect("the source is listed")
    };
    let loaded = snapshot(
        repository
            .load_content_supply_snapshots(workspace_id, now)
            .await?,
    );
    assert_eq!(
        loaded.failed_artifacts.len(),
        1,
        "{:?}",
        loaded.failed_artifacts
    );
    assert_eq!(
        loaded.failed_artifacts[0].artifact,
        ContentArtifactKind::SignalPush
    );
    assert_eq!(
        loaded.failed_artifacts[0].failures, 1,
        "only this version's failure counts"
    );
    assert!(loaded.in_flight_artifacts.is_empty());
    assert!(matches!(
        evaluate_content_supply(&loaded, ContentSupplyPolicy::default(), now),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::SignalPush,
            attempt: 1,
            ..
        }
    ));

    // The old key still dedupes onto the failed action: this was the freeze.
    let replay = repository
        .persist_candidate(workspace_id, &candidate(source_id, version, ""), &trace)
        .await?;
    assert!(!replay.action_created);
    // The retry's key writes a new action, which is then in flight.
    let retry = repository
        .persist_candidate(
            workspace_id,
            &candidate(source_id, version, ":attempt1"),
            &trace,
        )
        .await?;
    assert!(retry.decision_created);
    assert!(retry.action_created, "the retry must write a new action");

    let loaded = snapshot(
        repository
            .load_content_supply_snapshots(workspace_id, now)
            .await?,
    );
    assert_eq!(
        loaded.in_flight_artifacts,
        vec![ContentArtifactKind::SignalPush]
    );
    assert_eq!(loaded.failed_artifacts[0].failures, 1);
    assert!(!matches!(
        evaluate_content_supply(&loaded, ContentSupplyPolicy::default(), now),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::SignalPush,
            ..
        }
    ));
    Ok(())
}
