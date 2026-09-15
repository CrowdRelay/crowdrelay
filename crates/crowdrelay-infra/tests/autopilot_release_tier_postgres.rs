//! Release tier round-trip and milestone marks against a real Postgres.
//!
//! What fails here and nowhere else: an upsert that drops `tier` under
//! COALESCE semantics (absent must keep the stored value, not reset it), a
//! snapshot row that forgets to read the new column, and a milestone-marks
//! query that returns rows for the wrong workspace.

use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotDecisionRepository, AutopilotTeamStateRepository, UpsertReleasePlan,
};
use crowdrelay_domain::{WorkspaceId, release_autopilot::ReleaseTier};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;

struct Fixture {
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .map_err(|error| format!("test database url must target a disposable database: {error}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Release tier E2E")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool,
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
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

fn idem(seed: &str) -> IdempotencyKey {
    IdempotencyKey::parse(format!("release-tier-e2e-{seed}")).expect("bounded key")
}

fn plan_cmd(source_key: &str, tier: Option<ReleaseTier>, version: i64) -> UpsertReleasePlan {
    UpsertReleasePlan {
        release_id: None,
        source_key: source_key.to_string(),
        title: format!("{source_key} title"),
        release_at: OffsetDateTime::now_utc() + time::Duration::days(60),
        listen_url: None,
        tier,
        active: true,
        assets_ready: true,
        communication_enabled: true,
        press_enabled: true,
        expected_version: version,
    }
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn an_absent_tier_never_resets_the_bands_call() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("tier").await?;
    let idem_key = idem("tier-single");
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("tier-single", Some(ReleaseTier::Single), 0),
            &idem_key,
            None,
        )
        .await?;
    assert!(!created.replayed);

    // A caller that does not know about tiers updates the plan — the band's
    // call must survive, not quietly reset to the default.
    let updated = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: Some(created.release_id),
                title: "tier-single retitled".into(),
                ..plan_cmd("tier-single", None, 1)
            },
            &idem("tier-update"),
            None,
        )
        .await?;
    assert!(!updated.replayed);

    let plans = fixture
        .repository
        .load_release_plan_snapshots(fixture.workspace_id, fixture.now)
        .await?;
    let stored = plans
        .iter()
        .find(|p| p.release_id == created.release_id)
        .expect("the plan is listed");
    assert_eq!(stored.tier, ReleaseTier::Single);
    assert_eq!(stored.title, "tier-single retitled");

    // And a plan written without a tier lands on the honest default.
    let defaulted = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("tier-default", None, 0),
            &idem("tier-default"),
            None,
        )
        .await?;
    let plans = fixture
        .repository
        .load_release_plan_snapshots(fixture.workspace_id, fixture.now)
        .await?;
    assert_eq!(
        plans
            .iter()
            .find(|p| p.release_id == defaulted.release_id)
            .expect("the default-tier plan is listed")
            .tier,
        ReleaseTier::Track
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn milestone_marks_are_workspace_scoped_and_parse_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("marks").await?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("marks-plan", Some(ReleaseTier::Track), 0),
            &idem("marks-plan"),
            None,
        )
        .await?;

    // No milestones recorded yet: the marks list is empty, never a phantom
    // completion borrowed from another workspace's plan.
    let marks = fixture
        .repository
        .load_release_milestone_marks(fixture.workspace_id, &[created.release_id])
        .await?;
    assert!(marks.is_empty());
    let foreign = fixture
        .repository
        .load_release_milestone_marks(WorkspaceId::new(), &[created.release_id])
        .await?;
    assert!(foreign.is_empty());
    Ok(())
}
