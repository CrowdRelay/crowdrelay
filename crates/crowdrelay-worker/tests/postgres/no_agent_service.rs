//! A tenant stack without an agent service.
//!
//! `agent_service_tasks` is the agent service's table; a stack without that
//! service (the demo tenants in production) has none. The three post
//! executors joined it inside their claim transaction, so every cycle there
//! failed with `relation "agent_service_tasks" does not exist` — three
//! warnings a minute per stack — and the abort also discarded the claim of
//! posts that were already pending. On a freshly migrated database, which
//! is exactly such a stack, each executor now completes a cycle.

use crate::common;

use anyhow::{Context, Result, bail, ensure};
use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionPayload, AutopilotActionRepository, AutopilotMeasurementKind,
        AutopilotMeasurementRepository, ClaimedAutopilotAction, ClaimedAutopilotMeasurement,
    },
};
use crowdrelay_domain::{AutopilotActionId, AutopilotMeasurementId, WorkspaceId};
use crowdrelay_infra::sensitive_response::SensitiveResponseKey;
use crowdrelay_worker::{
    discord_executor::DiscordExecutorWorker, social_post_executor::SocialPostExecutorWorker,
    telegram_executor::TelegramExecutorWorker,
};
use uuid::Uuid;

fn key() -> SensitiveResponseKey {
    SensitiveResponseKey::derive_from_secret(b"test-encryption-key")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_post_executors_run_without_an_agent_service() -> Result<()> {
    let database = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("create an isolated, migrated database")?;
    let result = run(&database.pool).await;
    database
        .drop()
        .await
        .context("drop the isolated database")?;
    result
}

async fn run(pool: &sqlx::PgPool) -> Result<()> {
    let absent: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('agent_service_tasks')::text")
            .fetch_one(pool)
            .await?;
    ensure!(
        absent.is_none(),
        "the premise: no migration creates the agent service's table"
    );
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'No agents')")
        .bind(id)
        .bind(format!("no-agents-{}", id.simple()))
        .execute(pool)
        .await?;
    let workspace_id = WorkspaceId::from_uuid(id);
    let origin = "https://example.test".to_owned();

    let social = SocialPostExecutorWorker::new(
        pool.clone(),
        workspace_id,
        true,
        None,
        origin.clone(),
        key(),
        false,
    )
    .context("build social executor")?;
    social
        .run_once()
        .await
        .context("social post executor cycle without agent_service_tasks")?;

    TelegramExecutorWorker::new(pool.clone(), workspace_id, true, key(), origin.clone())
        .context("build telegram executor")?
        .run_once()
        .await
        .context("telegram executor cycle without agent_service_tasks")?;

    DiscordExecutorWorker::new(pool.clone(), workspace_id, true, key(), origin)
        .context("build discord executor")?
        .run_once()
        .await
        .context("discord executor cycle without agent_service_tasks")?;

    // The autopilot's two other agent-service touchpoints degrade the same
    // way: a measurement that would join the absent task table abandons with
    // the named `no_agent_service` kind instead of `relation does not exist`,
    // and an agent-run dispatch fails the action the same named way rather
    // than crashing its execution transaction.
    let repository = crowdrelay_infra::autopilot::PostgresAutopilotRepository::new_with_timeouts(
        pool.clone(),
        std::time::Duration::from_secs(10),
    );
    let now = time::OffsetDateTime::now_utc();

    let agent_backed = ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::new(),
        action_id: AutopilotActionId::new(),
        kind: AutopilotMeasurementKind::ScannerDiscoveryQuality14d,
        subject_id: Uuid::now_v7(),
        baseline_value: 0.0,
        action_finished_at: now,
        due_at: now,
        attempt_number: 1,
    };
    match repository
        .observe_measurement(workspace_id, &agent_backed, now)
        .await
    {
        Err(RepositoryError::ConflictBecause(reason)) => ensure!(
            reason == AutopilotMeasurementKind::NO_AGENT_SERVICE,
            "agent-backed observation should abandon as no_agent_service, got {reason}"
        ),
        other => bail!("agent-backed observation should fail named, got {other:?}"),
    }

    // The guard is kind-scoped: a kind that never reads the agent tables
    // answers through its own path — here a real (if empty) count — not the
    // service's absence.
    let workspace_backed = ClaimedAutopilotMeasurement {
        kind: AutopilotMeasurementKind::AgentRunFanGrowth3d,
        ..agent_backed
    };
    match repository
        .observe_measurement(workspace_id, &workspace_backed, now)
        .await
    {
        Ok(observed) => ensure!(
            observed == 0.0,
            "an empty workspace counts zero new fans, got {observed}"
        ),
        other => bail!("a fan-count observation needs no agent tables, got {other:?}"),
    }

    let action = ClaimedAutopilotAction {
        id: AutopilotActionId::new(),
        payload: AutopilotActionPayload::RequestAgentRun {
            template_id: "community-scanner".to_owned(),
            prompt: "scan for communities".to_owned(),
            priority: 0,
            tier: crowdrelay_brain::AgentTier::Basic,
        },
        attempt_number: 1,
    };
    match repository.execute_action(workspace_id, &action, now).await {
        Err(RepositoryError::ConflictBecause(reason)) => ensure!(
            reason == AutopilotMeasurementKind::NO_AGENT_SERVICE,
            "an agent-run dispatch should fail as no_agent_service, got {reason}"
        ),
        other => bail!("an agent-run dispatch should fail named, got {other:?}"),
    }
    Ok(())
}
