//! Reply probability model integration tests — verifies the full loop:
//! outreach interaction → outcome loading → model update → prediction.
//!
//! These tests prove the Beta-Bernoulli reply model actually learns from
//! real data in `viryaos_outreach_interactions` and produces non-prior
//! predictions. The model is a shadow advisory signal; these tests verify
//! the learning machinery, not ranking influence.
//!
//! A: A cold-start model (no outcomes) returns the global prior for every
//!    target — fail-closed behavior.
//! B: A no-reply outcome (outbound with no inbound, 30+ days ago) loads as
//!    a negative observation and moves the posterior away from the prior.
//! C: A positive reply outcome loads as a positive observation and moves
//!    the posterior in the opposite direction from a no-reply.
//! D: Delta replay loads only outcomes whose observation window closed
//!    after the checkpoint, not those whose outbound preceded it.
//! E: The checkpoint persists and reloads, so the next cycle does not
//!    replay outcomes the model has already learned from.

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_brain::ReplyProbabilityModel;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use crowdrelay_infra::config::DatabaseConfig;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
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
        .bind(format!("reply-model-{suffix}"))
        .bind("Reply Model Tests")
        .execute(&pool)
        .await?;
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
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

/// Inserts an outreach target and returns its UUID.
async fn insert_target(f: &Fixture, kind: &str, display_name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_outreach_targets
           (id, workspace_id, target_kind, display_name, contact_email)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(kind)
    .bind(display_name)
    .bind(format!("{display_name}@test.example"))
    .execute(&f.pool)
    .await
    .expect("insert target");
    id
}

/// Inserts an outbound interaction at the given time.
async fn insert_outbound(
    f: &Fixture,
    target_id: uuid::Uuid,
    occurred_at: OffsetDateTime,
    source_key: &str,
) {
    sqlx::query(
        r#"INSERT INTO viryaos_outreach_interactions
           (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
           VALUES ($1, $2, 'outbound', 'initial', 'none', $3, $4)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(target_id)
    .bind(source_key)
    .bind(occurred_at)
    .execute(&f.pool)
    .await
    .expect("insert outbound");
}

/// Inserts an inbound reply interaction.
async fn insert_inbound(
    f: &Fixture,
    target_id: uuid::Uuid,
    occurred_at: OffsetDateTime,
    disposition: &str,
    source_key: &str,
) {
    sqlx::query(
        r#"INSERT INTO viryaos_outreach_interactions
           (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
           VALUES ($1, $2, 'inbound', 'reply', $3, $4, $5)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(target_id)
    .bind(disposition)
    .bind(source_key)
    .bind(occurred_at)
    .execute(&f.pool)
    .await
    .expect("insert inbound");
}

/// A: Cold start — no outcomes, model returns the global prior for everything.
///
/// The global prior is Beta(1, 9) encoding a 10% base rate. Every target
/// gets the same prediction, and the confidence is 0. This is the fail-closed
/// behavior: the model never crashes, never blocks, and never overrides the
/// relevance ranking.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_cold_start_returns_global_prior() {
    let f = setup().await.expect("fixture");
    let target = insert_target(&f, "playlist", "chill-beats").await;

    let model = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("load model");

    assert!(
        model.is_cold_start(),
        "a workspace with no interactions must be cold start"
    );
    let pred = model.predict("playlist", &target.to_string());
    assert!(
        (pred.probability - 0.1).abs() < 1e-6,
        "cold-start probability must be the 10% prior, got {}",
        pred.probability
    );
    assert_eq!(pred.confidence, 0, "cold-start confidence must be 0");
}

/// B: A no-reply outcome (outbound 31 days ago, no inbound) loads as a
/// negative observation and moves the posterior below the prior.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn b_no_reply_outcome_moves_posterior_below_prior() {
    let f = setup().await.expect("fixture");
    let target = insert_target(&f, "playlist", "silent-curator").await;
    // Outbound 31 days ago — past the 30-day no-reply window.
    insert_outbound(&f, target, f.now - time::Duration::days(31), "wave-1").await;

    let model = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("load model");

    assert!(!model.is_cold_start(), "one outcome must end cold start");
    assert_eq!(
        model.global_confidence(),
        1,
        "one observation at global level"
    );
    let pred = model.predict("playlist", &target.to_string());
    assert!(
        pred.probability < 0.1,
        "a no-reply must pull the prediction below the 10% prior, got {}",
        pred.probability
    );
}

/// C: A positive reply moves the posterior above the prior, and above a
/// no-reply target.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn c_positive_reply_moves_posterior_above_prior() {
    let f = setup().await.expect("fixture");
    let silent = insert_target(&f, "playlist", "silent-curator").await;
    let responsive = insert_target(&f, "playlist", "responsive-curator").await;

    // Silent target: outbound 31 days ago, no reply.
    insert_outbound(&f, silent, f.now - time::Duration::days(31), "wave-silent").await;
    // Responsive target: outbound 35 days ago, positive reply 30 days ago.
    insert_outbound(
        &f,
        responsive,
        f.now - time::Duration::days(35),
        "wave-responsive",
    )
    .await;
    insert_inbound(
        &f,
        responsive,
        f.now - time::Duration::days(30),
        "positive",
        "wave-responsive-reply",
    )
    .await;

    let model = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("load model");

    let pred_silent = model.predict("playlist", &silent.to_string());
    let pred_responsive = model.predict("playlist", &responsive.to_string());

    assert!(
        pred_responsive.probability > pred_silent.probability,
        "a positive reply must rank above a no-reply: responsive={} vs silent={}",
        pred_responsive.probability,
        pred_silent.probability
    );
    assert!(
        pred_responsive.probability > 0.1,
        "a positive reply must pull the prediction above the 10% prior, got {}",
        pred_responsive.probability
    );
}

/// D: Delta replay — a no-reply whose 30-day window closed after the
/// checkpoint is included, even if the outbound preceded the checkpoint.
///
/// This is the bug the CTE in `load_outreach_reply_outcomes` prevents:
/// filtering by `outbound.occurred_at > checkpoint` would skip no-replies
/// whose outbound preceded the checkpoint but whose 30-day window closed
/// after it, systematically dropping negative outcomes and biasing the
/// model optimistic.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn d_delta_replay_includes_no_reply_window_closed_after_checkpoint() {
    let f = setup().await.expect("fixture");
    let target = insert_target(&f, "playlist", "late-silent").await;

    // Outbound 40 days ago. The 30-day no-reply window closed 10 days ago.
    let outbound_at = f.now - time::Duration::days(40);
    // The 30-day no-reply window closed 10 days after the checkpoint below.
    insert_outbound(&f, target, outbound_at, "wave-late").await;

    // Save a checkpoint dated 20 days ago — after the outbound but before
    // the 30-day window closed. The no-reply outcome became knowable at
    // `window_close`, which is after the checkpoint.
    let checkpoint_time = f.now - time::Duration::days(20);

    // First load: full replay (no checkpoint) to get the model state.
    let model_full = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("full replay");
    assert_eq!(
        model_full.global_confidence(),
        1,
        "full replay must see the no-reply outcome"
    );

    // Save the cold-start model as the checkpoint, backdated to 20 days ago.
    f.repository
        .save_reply_model(f.workspace_id, &ReplyProbabilityModel::new())
        .await
        .expect("save checkpoint");
    sqlx::query(
        "UPDATE viryaos_brain_state SET updated_at = $2 \
         WHERE workspace_id = $1 AND module = 'reply_probability'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(checkpoint_time)
    .execute(&f.pool)
    .await
    .expect("backdate checkpoint");

    // Delta replay: must include the no-reply because `observed_at` (= window_close)
    // is after the checkpoint.
    let model_delta = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("delta replay");

    assert_eq!(
        model_delta.global_confidence(),
        1,
        "delta replay must include the no-reply whose window closed after the checkpoint"
    );
    let pred_full = model_full.predict("playlist", &target.to_string());
    let pred_delta = model_delta.predict("playlist", &target.to_string());
    assert!(
        (pred_full.probability - pred_delta.probability).abs() < 1e-9,
        "delta replay must produce the same prediction as full replay: full={} delta={}",
        pred_full.probability,
        pred_delta.probability
    );
}

/// E: Checkpoint persists and reloads — the next load does not replay
/// outcomes the model has already learned from.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn e_checkpoint_persists_and_reloads() {
    let f = setup().await.expect("fixture");
    let target = insert_target(&f, "playlist", "persisted-curator").await;
    insert_outbound(&f, target, f.now - time::Duration::days(31), "wave-persist").await;

    // First load: full replay, learns from the outcome.
    let model1 = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("first load");
    assert_eq!(model1.global_confidence(), 1);

    // Save the checkpoint.
    f.repository
        .save_reply_model(f.workspace_id, &model1)
        .await
        .expect("save checkpoint");

    // Second load: delta replay. The outcome's `observed_at` is before the
    // checkpoint (which was just saved), so it must NOT be replayed.
    let model2 = f
        .repository
        .load_reply_model(f.workspace_id)
        .await
        .expect("second load");

    assert_eq!(
        model2.global_confidence(),
        model1.global_confidence(),
        "checkpoint reload must not replay already-learned outcomes"
    );
    let pred1 = model1.predict("playlist", &target.to_string());
    let pred2 = model2.predict("playlist", &target.to_string());
    assert!(
        (pred1.probability - pred2.probability).abs() < 1e-9,
        "checkpoint reload must produce the same prediction: first={} second={}",
        pred1.probability,
        pred2.probability
    );
}
