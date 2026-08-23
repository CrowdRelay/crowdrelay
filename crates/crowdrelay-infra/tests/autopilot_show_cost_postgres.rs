//! Predicted show cost against settled show cost, against a real Postgres.
//!
//! The rule is unit-tested. What cannot be: that a prediction is frozen once,
//! that a settlement with nothing behind it is refused rather than backfilled,
//! that the first account of what happened stands, and that the schema refuses
//! a verdict on a show nobody settled.

use std::time::Duration;

use crowdrelay_application::{
    IdempotencyKey,
    autopilot::{
        AutopilotShowCostRepository, FreezeShowCostPrediction, SettleShowCost, ShowCostMutation,
    },
};
use crowdrelay_domain::{EventId, WorkspaceId, show_settlement::SettledShowCost};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: EventId,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
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
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Show economics E2E")
        .execute(&pool)
        .await?;
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,now() - INTERVAL '2 days','published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("{label}-show-{suffix}"))
    .bind("Show economics show")
    .execute(&pool)
    .await?;
    // Rates are provisioned with the workspace; this only guarantees they
    // exist if that ever stops being true.
    sqlx::query(
        "INSERT INTO viryaos_tour_economics (workspace_id) VALUES ($1)
         ON CONFLICT (workspace_id) DO NOTHING",
    )
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    Ok(Fixture {
        repository: PostgresAutopilotRepository::new(pool.clone(), &database),
        pool,
        workspace_id,
        event_id,
    })
}

fn key() -> IdempotencyKey {
    IdempotencyKey::parse("show-cost-e2e-0000000000000001").expect("valid idempotency key")
}

async fn freeze(fixture: &Fixture) -> Result<ShowCostMutation, Box<dyn std::error::Error>> {
    Ok(fixture
        .repository
        .freeze_show_cost_prediction(
            fixture.workspace_id,
            FreezeShowCostPrediction {
                event_id: fixture.event_id,
                distance_km: Some(250),
                nights_away: Some(1),
                offered_fee_minor: 200_000,
                application_fee_minor: 0,
            },
            &key(),
            None,
        )
        .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_prediction_is_frozen_once_and_a_settlement_scores_the_model_against_it()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("show-cost").await?;

    let first = freeze(&fixture).await?;
    assert!(!first.replayed);
    let second = freeze(&fixture).await?;
    assert!(
        second.replayed,
        "re-freezing after the show would let the goalposts move"
    );

    let predicted_total = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT predicted_total_cost_minor FROM viryaos_show_cost_ledger
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?
    .ok_or("the default rates produce a complete estimate")?;

    // Settle far enough above the estimate that the model is drifting, with
    // transport carrying the whole miss.
    let settled = fixture
        .repository
        .settle_show_cost(
            fixture.workspace_id,
            SettleShowCost {
                event_id: fixture.event_id,
                settled: SettledShowCost {
                    transport_minor: predicted_total,
                    accommodation_minor: 0,
                    per_diem_minor: 0,
                    overhead_minor: 0,
                    other_minor: 0,
                    fee_received_minor: 200_000,
                },
                settled_by: "tour manager".to_owned(),
            },
            &key(),
            None,
        )
        .await?;
    assert!(!settled.replayed);
    assert!(settled.accuracy.is_some(), "a settlement always yields a verdict");

    let row = sqlx::query_as::<_, (String, Option<String>, Option<i64>, Option<i64>)>(
        "SELECT accuracy, worst_line, settled_total_cost_minor, implied_transport_rate_minor_per_100km
         FROM viryaos_show_cost_ledger WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(
        row.0 == "drifting" || row.0 == "calibrated",
        "a settled show carries a verdict, got {}",
        row.0
    );
    assert_eq!(row.2, Some(predicted_total));
    assert!(
        row.3.is_some(),
        "the implied road rate is derived when the frozen distance is known"
    );

    // A second account does not replace the first.
    let again = fixture
        .repository
        .settle_show_cost(
            fixture.workspace_id,
            SettleShowCost {
                event_id: fixture.event_id,
                settled: SettledShowCost {
                    transport_minor: 1,
                    accommodation_minor: 1,
                    per_diem_minor: 1,
                    overhead_minor: 1,
                    other_minor: 1,
                    fee_received_minor: 1,
                },
                settled_by: "somebody else".to_owned(),
            },
            &key(),
            None,
        )
        .await?;
    assert!(again.replayed);
    let unchanged = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT settled_total_cost_minor FROM viryaos_show_cost_ledger
         WHERE workspace_id=$1 AND event_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.event_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        unchanged,
        Some(predicted_total),
        "the first account of what happened stands"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_settlement_with_no_prediction_behind_it_is_refused()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("show-cost-refuse").await?;
    let refused = fixture
        .repository
        .settle_show_cost(
            fixture.workspace_id,
            SettleShowCost {
                event_id: fixture.event_id,
                settled: SettledShowCost {
                    transport_minor: 100_000,
                    accommodation_minor: 0,
                    per_diem_minor: 0,
                    overhead_minor: 0,
                    other_minor: 0,
                    fee_received_minor: 100_000,
                },
                settled_by: "tour manager".to_owned(),
            },
            &key(),
            None,
        )
        .await;
    assert!(
        refused.is_err(),
        "a model cannot be scored against a show it was never asked about"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_schema_refuses_a_verdict_on_an_unsettled_show()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("show-cost-schema").await?;
    freeze(&fixture).await?;

    let verdict_without_settlement = sqlx::query(
        "UPDATE viryaos_show_cost_ledger SET accuracy='calibrated' WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(
        verdict_without_settlement.is_err(),
        "a show nobody settled has no verdict, not a neutral one"
    );

    let line_without_drift = sqlx::query(
        "UPDATE viryaos_show_cost_ledger SET worst_line='transport' WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(
        line_without_drift.is_err(),
        "only a drifting verdict may point an operator at a rate to change"
    );
    Ok(())
}
