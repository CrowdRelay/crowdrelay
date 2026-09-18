//! The roster's pooled portfolio (5.1) against a real Postgres.
//!
//! Each act's eval publishes the candidate pool its selection was drawn from
//! via `replace_portfolio_pool`; the roster read pools the union and re-ranks
//! it under the organisation's stated limits. These tests exercise the real
//! write and the real read — a test that reimplemented either would prove
//! nothing about the paths that ship.
//!
//! Every test runs against a disposable database it creates and drops itself;
//! the schema comes from `MIGRATOR.run`, so a migration that does not apply
//! fails here before it can fail a deploy.

use std::time::Duration;

use crowdrelay_application::autopilot::{AutopilotDecisionRepository, PortfolioPoolEntry};
use crowdrelay_brain::{
    DecisionMode, DecisionValue, EstimationRegime, EvidenceQuality, OpportunityAction,
    OpportunityId, ResourceCost,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::organization_settings::{
    KEY_ROSTER_PORTFOLIO_MAX_DISPATCHES, OrganizationSettingsRepository,
};
use crowdrelay_infra::roster_portfolio::roster_portfolio_plan;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_51_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
            ..
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

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

fn decision_value(expected_fans: f64) -> DecisionValue {
    DecisionValue {
        expected_incremental_y30: expected_fans,
        uncertainty: 0.0,
        p_meaningful_effect: 0.9,
        estimation_regime: EstimationRegime::Y30Direct,
        evidence_quality: EvidenceQuality::Observational,
        sample_size: 20,
        uses_y30: true,
        bridge_confidence: 0,
        bridge_is_reliable: true,
        contamination: 0.0,
        resource_cost: ResourceCost::configured(1.0),
        pragmatic_value: expected_fans,
        risk_penalty: None,
        opportunity_cost: 0.0,
        decision_mode: DecisionMode::Exploit,
    }
}

/// One pool row as the eval publishes it: the opportunity identity whole
/// (its display form does not parse back), the decision value whole, and
/// what the act's own selection did with it.
fn entry(
    template: &str,
    target: &str,
    expected_fans: f64,
    audience: &str,
    selected: bool,
    rejection_reason: Option<&str>,
) -> PortfolioPoolEntry {
    let opportunity_id =
        OpportunityId::with_context_hash(template, target, OpportunityAction::Post, "ctx:test");
    PortfolioPoolEntry {
        opportunity_key: opportunity_id.to_string(),
        opportunity_id,
        audience_key: audience.to_owned(),
        source_context: "GrowthIntelligence".to_owned(),
        action_key: format!("decision:{template}:{target}"),
        decision_value: decision_value(expected_fans),
        is_experimental: false,
        selected,
        rejection_reason: rejection_reason.map(str::to_owned),
    }
}

/// The pooling payoff in one scenario: act A's pool holds a candidate its own
/// five-slot cap rejected; the roster's larger budget picks it up. The read
/// must say who proposed it and that the act itself did not select it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_locally_rejected_candidate_can_win_a_roster_slot()
-> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let org = organization(&db.pool, "Roster").await?;
    let act_a = workspace(&db.pool, "Act A", Some(org)).await?;
    let act_b = workspace(&db.pool, "Act B", Some(org)).await?;

    // Act A published a pool where its own cap rejected the best-looking
    // remainder; act B published one middling candidate.
    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_a),
            &[
                entry(
                    "community-engager",
                    "r/metal",
                    9.0,
                    "subreddit:r_metal",
                    true,
                    None,
                ),
                entry(
                    "community-engager",
                    "r/punk",
                    8.0,
                    "subreddit:r_punk",
                    false,
                    Some("max_dispatches_reached"),
                ),
            ],
            OffsetDateTime::now_utc(),
        )
        .await?;
    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_b),
            &[entry(
                "community-engager",
                "r/indie",
                4.0,
                "subreddit:r_indie",
                true,
                None,
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;

    let plan = roster_portfolio_plan(&db.pool, org)
        .await?
        .expect("an org with members returns a plan");

    // The ranked answer is act-attributed and orders by value: A's two
    // candidates outrank B's one — including the one A's own cap rejected.
    assert_eq!(plan.selected.len(), 3);
    assert_eq!(plan.selected[0].act, "Act A");
    assert_eq!(plan.selected[0].target, "r/metal");
    assert_eq!(plan.selected[1].act, "Act A");
    assert_eq!(plan.selected[1].target, "r/punk");
    assert!(
        !plan.selected[1].locally_selected,
        "the pooled winner must admit the act's own selection left it out"
    );
    assert_eq!(
        plan.selected[1].local_rejection_reason.as_deref(),
        Some("max_dispatches_reached")
    );
    assert_eq!(plan.selected[2].act, "Act B");

    // The act ledger names both acts, their pool sizes, and how many slots
    // each took home.
    let act_a_row = plan
        .acts
        .iter()
        .find(|act| act.workspace_id == act_a)
        .expect("act A is a member");
    assert_eq!(act_a_row.pool_size, 2);
    assert_eq!(act_a_row.selected, 2);
    assert_eq!(act_a_row.name, "Act A");

    db.drop_database().await;
    Ok(())
}

/// The stated org cap binds the pooled rank, and the response says the cap
/// was stated rather than defaulted.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_organisations_stated_cap_bounds_the_rank() -> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let org = organization(&db.pool, "Roster").await?;
    let act_a = workspace(&db.pool, "Act A", Some(org)).await?;

    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_a),
            &[
                entry(
                    "community-engager",
                    "r/one",
                    9.0,
                    "subreddit:one",
                    true,
                    None,
                ),
                entry(
                    "community-engager",
                    "r/two",
                    8.0,
                    "subreddit:two",
                    true,
                    None,
                ),
                entry(
                    "community-engager",
                    "r/three",
                    7.0,
                    "subreddit:three",
                    true,
                    None,
                ),
            ],
            OffsetDateTime::now_utc(),
        )
        .await?;

    OrganizationSettingsRepository::new(db.pool.clone())
        .set(org, KEY_ROSTER_PORTFOLIO_MAX_DISPATCHES, "2")
        .await?;

    let plan = roster_portfolio_plan(&db.pool, org)
        .await?
        .expect("an org with members returns a plan");

    assert_eq!(plan.max_dispatches, 2);
    assert_eq!(plan.max_dispatches_source, "stated");
    assert_eq!(
        plan.selected.len(),
        2,
        "the roster's stated cap binds the pooled rank"
    );
    assert_eq!(plan.rejected_count, 1);

    db.drop_database().await;
    Ok(())
}

/// An act that has never run a cycle is named in the ledger with an empty
/// pool rather than absent from it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_act_with_no_pool_is_named_not_silent() -> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let org = organization(&db.pool, "Roster").await?;
    let act_a = workspace(&db.pool, "Act A", Some(org)).await?;
    let quiet = workspace(&db.pool, "Quiet Act", Some(org)).await?;

    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_a),
            &[entry(
                "community-engager",
                "r/one",
                9.0,
                "subreddit:one",
                true,
                None,
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;

    let plan = roster_portfolio_plan(&db.pool, org)
        .await?
        .expect("an org with members returns a plan");

    let quiet_row = plan
        .acts
        .iter()
        .find(|act| act.workspace_id == quiet)
        .expect("the quiet act is a member");
    assert_eq!(quiet_row.pool_size, 0);
    assert_eq!(quiet_row.selected, 0);
    assert!(quiet_row.refreshed_at.is_none());
    assert_eq!(quiet_row.name, "Quiet Act");

    db.drop_database().await;
    Ok(())
}

/// No members is not an empty ranking — it is a roster that does not exist
/// yet, and the read says `None` so the response can say so.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_organisation_with_no_acts_has_nothing_to_pool() -> Result<(), Box<dyn std::error::Error>>
{
    let db = DisposableDatabase::create().await?;
    let org = organization(&db.pool, "Empty Roster").await?;

    assert!(roster_portfolio_plan(&db.pool, org).await?.is_none());

    db.drop_database().await;
    Ok(())
}

/// A fresh pool write replaces the previous one wholesale: the table is
/// current state, so a candidate that left the pool leaves the read with it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_new_cycle_replaces_the_pool_whole() -> Result<(), Box<dyn std::error::Error>> {
    let db = DisposableDatabase::create().await?;
    let org = organization(&db.pool, "Roster").await?;
    let act_a = workspace(&db.pool, "Act A", Some(org)).await?;

    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_a),
            &[
                entry(
                    "community-engager",
                    "r/old",
                    9.0,
                    "subreddit:old",
                    true,
                    None,
                ),
                entry(
                    "community-engager",
                    "r/older",
                    8.0,
                    "subreddit:older",
                    true,
                    None,
                ),
            ],
            OffsetDateTime::now_utc(),
        )
        .await?;
    repository(&db.pool)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(act_a),
            &[entry(
                "community-engager",
                "r/new",
                5.0,
                "subreddit:new",
                true,
                None,
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;

    let plan = roster_portfolio_plan(&db.pool, org)
        .await?
        .expect("an org with members returns a plan");
    assert_eq!(
        plan.selected.len(),
        1,
        "the previous pool must not linger beside the new one"
    );
    assert_eq!(plan.selected[0].target, "r/new");

    db.drop_database().await;
    Ok(())
}
