//! The supply read against a real Postgres.
//!
//! Every property this query has that matters is one a unit test cannot reach:
//! the per-sweep window, the difference between a sweep that found nothing
//! admissible and a sweep that was never answered, and the fact that the
//! barren count is a run ending at the most recent sweep rather than a total.
//! Getting any of those wrong makes the agent either stop asking too early or
//! never stop, and both look identical from the Rust side.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn supply_counts_the_run_of_barren_sweeps_not_the_total()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("supply-e2e-{}", workspace_id.into_uuid().simple()))
        .bind("Outreach supply E2E")
        .execute(&pool)
        .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let now = OffsetDateTime::now_utc();

    let empty = repository
        .load_outreach_supply_snapshot(workspace_id, now)
        .await?;
    assert_eq!(empty.pitchable_targets, 0);
    assert_eq!(empty.admitted_candidates, 0);
    assert_eq!(empty.last_sweep_requested_at, None);
    assert_eq!(empty.consecutive_barren_sweeps, 0);

    // Three sweeps, oldest to newest. The oldest found nothing admissible, the
    // middle one found a keeper, and the newest was barren again. The run
    // ending at the newest sweep is one, not two: a total would have the agent
    // give up while the source is still producing.
    let old = seed_sweep(&pool, workspace_id, now - time::Duration::days(9)).await?;
    let middle = seed_sweep(&pool, workspace_id, now - time::Duration::days(6)).await?;
    let recent = seed_sweep(&pool, workspace_id, now - time::Duration::days(3)).await?;

    // Each sweep was answered. The ingestion is what says so — candidate rows
    // cannot, because a sweep that reported "nothing found" leaves none either.
    for (at, status) in [
        (old, Some("refused")),
        (middle, Some("admitted")),
        (recent, Some("refused")),
    ] {
        seed_ingestion(&pool, workspace_id, at + time::Duration::hours(1)).await?;
        if let Some(status) = status {
            seed_candidate(&pool, workspace_id, status, at + time::Duration::hours(1)).await?;
        }
    }

    let snapshot = repository
        .load_outreach_supply_snapshot(workspace_id, now)
        .await?;
    assert_eq!(
        snapshot.consecutive_barren_sweeps, 1,
        "only the run ending at the most recent sweep counts"
    );
    assert_eq!(snapshot.admitted_candidates, 1);
    assert_eq!(
        snapshot
            .last_sweep_requested_at
            .map(|at| at.unix_timestamp()),
        Some(recent.unix_timestamp())
    );
    assert_eq!(
        snapshot.candidates_since_last_sweep, 1,
        "the per-sweep window must not credit an older sweep with a later candidate"
    );

    // An adapter that answered with an empty batch found nothing admissible,
    // which is a real answer and extends the barren run to two.
    let answered_empty = seed_sweep(&pool, workspace_id, now - time::Duration::hours(20)).await?;
    seed_ingestion(
        &pool,
        workspace_id,
        answered_empty + time::Duration::minutes(5),
    )
    .await?;
    let dry = repository
        .load_outreach_supply_snapshot(workspace_id, now)
        .await?;
    assert_eq!(
        dry.consecutive_barren_sweeps, 2,
        "an empty batch is a report that the source is dry, not silence"
    );

    // A sweep nobody answered at all is an integration failure, not a dry
    // source. If it counted as barren, one broken workflow would permanently
    // disable discovery — so it breaks the run instead of extending it.
    seed_sweep(&pool, workspace_id, now - time::Duration::hours(6)).await?;
    let unanswered = repository
        .load_outreach_supply_snapshot(workspace_id, now)
        .await?;
    assert_eq!(
        unanswered.consecutive_barren_sweeps, 0,
        "a sweep with no ingestion at all breaks the barren run"
    );
    assert_eq!(unanswered.candidates_since_last_sweep, 0);

    // Only a target that can actually be pitched today counts as supply.
    for (email, do_not_contact, active) in [
        ("keeper@example.test", false, true),
        ("blocked@example.test", true, true),
        ("retired@example.test", false, false),
    ] {
        sqlx::query(
            "INSERT INTO viryaos_outreach_targets
             (workspace_id, target_kind, display_name, contact_email, active, do_not_contact)
             VALUES ($1, 'playlist', $2, $2, $3, $4)",
        )
        .bind(workspace_id.into_uuid())
        .bind(email)
        .bind(active)
        .bind(do_not_contact)
        .execute(&pool)
        .await?;
    }
    let stocked = repository
        .load_outreach_supply_snapshot(workspace_id, now)
        .await?;
    assert_eq!(
        stocked.pitchable_targets, 1,
        "do-not-contact and inactive targets are not supply"
    );

    // No teardown: `operator_actions` is append-only, so cascading the
    // workspace away is refused by design. The database this runs against is
    // disposable and the workspace id is unique per run.
    Ok(())
}

async fn seed_sweep(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    created_at: OffsetDateTime,
) -> Result<OffsetDateTime, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions
         (id, workspace_id, context, subject_kind, subject_id, decision_kind,
          confidence_basis_points, disposition, reason, input_snapshot,
          policy_snapshot, recommendation, decision_key)
         VALUES ($1, $2, 'outreach_supply', 'workspace', $2, 'replenish_outreach_supply',
                 10000, 'auto_execute', 'test', '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, $3)",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision:test:{decision_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions
         (workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
          idempotency_key, payload, status, action_class, created_at, finished_at)
         VALUES ($1, $2, 'outreach_supply', 'outreach.discovery.request', 'workspace', $1,
                 $3, '{}'::jsonb, 'succeeded', 'first_party_reversible', $4, $4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(format!("action:test:{decision_id}"))
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(created_at)
}

/// Records the adapter's answer, exactly as the ingest route does.
///
/// `actor_type` is `admin_api_key` because the ledger has no value for an
/// executor yet; the action name is what identifies this as executor work.
/// Threading a real actor through all thirty-one call sites is its own change.
async fn seed_ingestion(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    created_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO operator_actions
         (id, workspace_id, action, target_type, target_id, actor_type,
          idempotency_key, details, created_at)
         VALUES ($1, $2, 'ingest_autopilot_outreach_candidates',
                 'outreach_candidate_batch', $2, 'admin_api_key', $3, '{}'::jsonb, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("sweep-report-{}", Uuid::now_v7()))
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_candidate(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    status: &str,
    created_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let route = format!("curator-{}@example.test", Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO viryaos_outreach_candidates
         (workspace_id, target_kind, display_name, source, source_reference, evidence,
          route_kind, route_value, route_is_published, fit_basis_points, status,
          refusal_reason, pitch_class, screened_at, created_at)
         VALUES ($1, 'playlist', 'Curator', 'playlist_description', 'https://example.test',
                 'submissions to …', 'email', $2, true, 8000, $3,
                 CASE WHEN $3 = 'refused' THEN 'poor_fit' END,
                 CASE WHEN $3 = 'refused' THEN NULL ELSE 'third_party' END,
                 $4, $4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(route)
    .bind(status)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}
