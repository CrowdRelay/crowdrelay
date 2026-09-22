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

use crate::common;

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
        resource_cost: ResourceCost::configured(1.0),
        pragmatic_value: expected_fans,
        risk_penalty: None,
        opportunity_cost: 0.0,
        economic_value_fans: None,
        revenue_model_source: None,
        harm_fans: None,
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
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Roster").await?;
    let act_a = workspace(&db, "Act A", Some(org)).await?;
    let act_b = workspace(&db, "Act B", Some(org)).await?;

    // Act A published a pool where its own cap rejected the best-looking
    // remainder; act B published one middling candidate.
    repository(&db)
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
    repository(&db)
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

    let plan = roster_portfolio_plan(&db, org)
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

    Ok(())
}

/// The stated org cap binds the pooled rank, and the response says the cap
/// was stated rather than defaulted.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_organisations_stated_cap_bounds_the_rank() -> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Roster").await?;
    let act_a = workspace(&db, "Act A", Some(org)).await?;

    repository(&db)
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

    OrganizationSettingsRepository::new(db.clone())
        .set(org, KEY_ROSTER_PORTFOLIO_MAX_DISPATCHES, "2")
        .await?;

    let plan = roster_portfolio_plan(&db, org)
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

    Ok(())
}

/// An act that has never run a cycle is named in the ledger with an empty
/// pool rather than absent from it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_act_with_no_pool_is_named_not_silent() -> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Roster").await?;
    let act_a = workspace(&db, "Act A", Some(org)).await?;
    let quiet = workspace(&db, "Quiet Act", Some(org)).await?;

    repository(&db)
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

    let plan = roster_portfolio_plan(&db, org)
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

    Ok(())
}

/// No members is not an empty ranking — it is a roster that does not exist
/// yet, and the read says `None` so the response can say so.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_organisation_with_no_acts_has_nothing_to_pool() -> Result<(), Box<dyn std::error::Error>>
{
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Empty Roster").await?;

    assert!(roster_portfolio_plan(&db, org).await?.is_none());

    Ok(())
}

/// A fresh pool write replaces the previous one wholesale: the table is
/// current state, so a candidate that left the pool leaves the read with it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_new_cycle_replaces_the_pool_whole() -> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Roster").await?;
    let act_a = workspace(&db, "Act A", Some(org)).await?;

    repository(&db)
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
    repository(&db)
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

    let plan = roster_portfolio_plan(&db, org)
        .await?
        .expect("an org with members returns a plan");
    assert_eq!(
        plan.selected.len(),
        1,
        "the previous pool must not linger beside the new one"
    );
    assert_eq!(plan.selected[0].target, "r/new");

    Ok(())
}

/// One spent dispatch — the action row the fairness term counts. The decision
/// parent exists because the actions table anchors to it.
async fn spent_dispatch(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                   'request_agent_run',8000,'auto_execute','test dispatch',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)",
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, finished_at, trace_id
         ) VALUES ($1,$2,$3,'growth_intelligence','agent.run','workspace',
                   $4,$5,'{}'::jsonb,'succeeded',now(),$6)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("action-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(())
}

/// 5.2's promise: the act whose posteriors win every week cannot hold every
/// slot forever. A busy act's sixth dispatch pays the fairness damp — five
/// prior actions at 0.9 decay — and the quiet act's merely-good candidate
/// outranks its best one. A new member has not failed to do anything.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_permanently_outbid_act_stops_being_outbid() -> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let org = organization(&db, "Roster").await?;
    let busy = workspace(&db, "Busy Act", Some(org)).await?;
    let quiet = workspace(&db, "Quiet Act", Some(org)).await?;

    // The busy act already spent five dispatches this week.
    for _ in 0..5 {
        spent_dispatch(&db, busy).await?;
    }

    // Its best candidate beats the quiet act's on intrinsic value — 9.0 vs
    // 7.0 — but pays 0.9^5 for the share it already holds: 5.31 < 7.0.
    repository(&db)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(busy),
            &[entry(
                "community-engager",
                "r/big",
                9.0,
                "subreddit:big",
                true,
                None,
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;
    repository(&db)
        .replace_portfolio_pool(
            WorkspaceId::from_uuid(quiet),
            &[entry(
                "community-engager",
                "r/small",
                7.0,
                "subreddit:small",
                true,
                None,
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;

    let plan = roster_portfolio_plan(&db, org)
        .await?
        .expect("an org with members returns a plan");

    assert_eq!(plan.fairness_decay, 0.9, "unstated uses the roster default");
    assert_eq!(plan.fairness_decay_source, "roster_default");
    assert_eq!(
        plan.selected[0].act, "Quiet Act",
        "the starved act must outrank the act that already spent its week"
    );
    assert_eq!(plan.selected[0].act_recent_dispatches, 0);
    assert_eq!(plan.selected[1].act, "Busy Act");
    assert_eq!(plan.selected[1].act_recent_dispatches, 5);
    assert!(
        plan.selected[1].fairness_adjustment < 0.0,
        "the busy act's slot must show what its share cost"
    );

    Ok(())
}
