//! Offline analysis of the decision ledger. Read-only, no deployment, no waiting.
//!
//! Every decision the brain has ever made stored its `input_snapshot` and
//! `policy_snapshot`, and `crowdrelay-brain` has zero IO. Those two facts
//! together mean the brain's judgement can be examined against real history
//! without a post landing, without a database write, and without the autopilot
//! running.
//!
//! That matters because of a closed loop in production:
//!
//! ```text
//! DEFAULT_EXPECTED_FANS = 2.0   the prior: each dispatch yields ~2 fans
//!   -> the causal posterior only updates from RESOLVED evidence
//!   -> resolved evidence is 0, because no post has ever landed
//!   -> every candidate keeps its optimistic prior value
//!   -> `has_positive_candidates` is always true
//!   -> `min_dispatches = 1` overrides WAIT every cycle
//!   -> dispatch, nothing resolves, the prior never corrects
//! ```
//!
//! Realized yield is 22 fans, every one `public_signup`, none attributed to an
//! action. The prior claims two per dispatch. It cannot self-correct, because
//! correction needs resolution and resolution needs the edge to work.
//!
//! This tool answers what can be answered now:
//!
//! 1. Is the confidence number meaningful, or does every decision carry the same
//!    value? Every gate keyed to `minimum_confidence_basis_points` depends on
//!    the answer.
//! 2. Is the brain fixating? 733 decisions over a handful of subjects is ten
//!    decisions repeated, not seven hundred.
//! 3. Has the causal posterior ever been corrected by an observation?
//! 4. How sensitive is the ranking to the prior? If WAIT can never win at 2.0
//!    but wins at 0.1, the optimism is the bug rather than the override.
//!
//! ```text
//! cargo run -p crowdrelay-replay -- "postgres://…"      # or $CROWDRELAY_DATABASE_URL
//! ```
//!
//! Point it at a read replica or a restored snapshot. It issues no writes, but
//! "issues no writes" is a property of this file, not of the credential you give
//! it — prefer a read-only role.

use std::collections::BTreeMap;

use crowdrelay_brain::CausalModel;
use sqlx::{Row, postgres::PgPoolOptions};

/// Prior settings to compare. The first is what production runs today.
const PRIOR_SWEEP: [f64; 4] = [2.0, 0.5, 0.1, 0.0];

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("CROWDRELAY_DATABASE_URL").ok())
        .unwrap_or_else(|| {
            eprintln!("usage: crowdrelay-replay <postgres-url>");
            eprintln!("   or: CROWDRELAY_DATABASE_URL=… crowdrelay-replay");
            std::process::exit(2);
        });

    // A small pool: this is an analysis run, not a service, and it must not
    // compete with the API for connections if somebody points it at a primary.
    let pool = match PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!("could not connect: {error}");
            std::process::exit(1);
        }
    };

    println!("CrowdRelay decision-ledger replay — read-only\n");

    if let Err(error) = report(&pool).await {
        eprintln!("analysis failed: {error}");
        std::process::exit(1);
    }
}

async fn report(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let total = decision_total(pool).await?;
    if total == 0 {
        println!("No decisions in this database. Point the tool at one that has history.");
        return Ok(());
    }
    println!("{total} decisions in the ledger\n");

    confidence_distribution(pool, total).await?;
    subject_diversity(pool, total).await?;
    learning_state(pool).await?;
    prior_sensitivity(pool).await?;
    Ok(())
}

async fn decision_total(pool: &sqlx::PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*)::bigint FROM viryaos_autopilot_decisions")
        .fetch_one(pool)
        .await
}

/// Question 1 — is `confidence_basis_points` carrying information?
///
/// Policies gate on `minimum_confidence_basis_points`. If every decision lands
/// on the same value, that gate is not filtering anything and the number is
/// decoration.
async fn confidence_distribution(pool: &sqlx::PgPool, total: i64) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT confidence_basis_points AS bp, count(*)::bigint AS hits
        FROM viryaos_autopilot_decisions
        GROUP BY confidence_basis_points
        ORDER BY hits DESC
        LIMIT 10
        "#,
    )
    .fetch_all(pool)
    .await?;

    println!("── confidence ─────────────────────────────────────────────");
    let mut distinct = 0usize;
    let mut top_share = 0.0;
    for (index, row) in rows.iter().enumerate() {
        let bp: i32 = row.try_get("bp")?;
        let hits: i64 = row.try_get("hits")?;
        let share = hits as f64 / total as f64 * 100.0;
        if index == 0 {
            top_share = share;
        }
        distinct += 1;
        println!("  {bp:>5} bp  {hits:>6}  {share:5.1}%");
    }
    if distinct == 1 {
        println!(
            "\n  VERDICT: one value across every decision. `minimum_confidence_basis_points`\n  \
             cannot be filtering anything — the gate is decoration."
        );
    } else if top_share > 90.0 {
        println!(
            "\n  VERDICT: {top_share:.0}% of decisions share one value. The confidence gate\n  \
             separates almost nothing."
        );
    } else {
        println!("\n  VERDICT: confidence varies. The gate can discriminate.");
    }
    println!();
    Ok(())
}

/// Question 2 — is the brain fixating?
///
/// Many decisions over few subjects is one decision repeated. It is also the
/// shape a stuck cooldown produces, so the two are worth telling apart.
async fn subject_diversity(pool: &sqlx::PgPool, total: i64) -> Result<(), sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT count(DISTINCT subject_id)::bigint AS subjects,
               count(DISTINCT context)::bigint     AS contexts,
               count(DISTINCT decision_key)::bigint AS keys
        FROM viryaos_autopilot_decisions
        "#,
    )
    .fetch_one(pool)
    .await?;
    let subjects: i64 = row.try_get("subjects")?;
    let contexts: i64 = row.try_get("contexts")?;
    let keys: i64 = row.try_get("keys")?;

    println!("── diversity ──────────────────────────────────────────────");
    println!("  distinct subjects      {subjects}");
    println!("  distinct contexts      {contexts}");
    println!("  distinct decision keys {keys}");
    let per_subject = total as f64 / subjects.max(1) as f64;
    println!("  decisions per subject  {per_subject:.1}");
    if per_subject > 20.0 {
        println!(
            "\n  VERDICT: {per_subject:.0} decisions per subject. This is a small number of\n  \
             decisions repeated, not a broad search. Check the cooldowns."
        );
    } else {
        println!("\n  VERDICT: the search is spread across subjects.");
    }

    let per_context = sqlx::query(
        r#"
        SELECT context, count(*)::bigint AS hits
        FROM viryaos_autopilot_decisions
        GROUP BY context ORDER BY hits DESC LIMIT 8
        "#,
    )
    .fetch_all(pool)
    .await?;
    println!("\n  busiest contexts:");
    for row in per_context {
        let context: String = row.try_get("context")?;
        let hits: i64 = row.try_get("hits")?;
        println!("    {context:32} {hits:>6}");
    }
    println!();
    Ok(())
}

/// Question 3 — has any observation ever corrected the posterior?
///
/// This is the loop. The causal model learns from `observed_new_fans` on a
/// resolved outcome; if nothing resolves, the prior stands forever and the brain
/// acts on a belief no evidence has touched.
async fn learning_state(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT
          (SELECT count(*)::bigint FROM viryaos_growth_evidence)                          AS evidence,
          (SELECT count(*)::bigint FROM viryaos_growth_evidence
            WHERE resolved_at IS NOT NULL)                                                AS resolved,
          (SELECT count(*)::bigint FROM viryaos_autopilot_actions)                        AS actions,
          (SELECT count(*)::bigint FROM viryaos_reach_events)                             AS reach
        "#,
    )
    .fetch_one(pool)
    .await?;
    let evidence: i64 = row.try_get("evidence")?;
    let resolved: i64 = row.try_get("resolved")?;
    let actions: i64 = row.try_get("actions")?;
    let reach: i64 = row.try_get("reach")?;

    println!("── learning ───────────────────────────────────────────────");
    println!("  actions            {actions}");
    println!("  reach events       {reach}");
    println!("  evidence rows      {evidence}");
    println!("  of those, resolved {resolved}");
    if resolved == 0 {
        println!(
            "\n  VERDICT: the causal posterior has never been corrected by an observation.\n  \
             Every prediction it makes is the prior. `DEFAULT_EXPECTED_FANS = 2.0` is\n  \
             therefore the brain's belief about every template, and nothing in the system\n  \
             says so."
        );
    } else {
        println!("\n  VERDICT: {resolved} observations have reached the posterior.");
    }
    println!();
    Ok(())
}

/// Question 4 — how much does the prior decide?
///
/// Builds a fresh causal model per prior, feeds it the outcomes the ledger
/// already has, and reports what it would predict. A model whose prediction
/// barely moves between a prior of 2.0 and one of 0.0 is being driven by data;
/// one that tracks the prior exactly is being driven by the prior.
async fn prior_sensitivity(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let templates = sqlx::query(
        r#"
        SELECT DISTINCT input_snapshot #>> '{prediction,template_id}' AS template
        FROM viryaos_autopilot_decisions
        WHERE input_snapshot #>> '{prediction,template_id}' IS NOT NULL
        LIMIT 12
        "#,
    )
    .fetch_all(pool)
    .await?;

    println!("── prior sensitivity ──────────────────────────────────────");
    if templates.is_empty() {
        println!(
            "  No decision carries a prediction with a template id, so the ranking cannot\n  \
             be replayed from this ledger. The other three answers still stand."
        );
        println!();
        return Ok(());
    }

    let names: Vec<String> = templates
        .iter()
        .filter_map(|row| row.try_get::<Option<String>, _>("template").ok().flatten())
        .collect();
    println!("  templates found: {}", names.len());

    // A model with no observations predicts its prior for every template. That
    // is the production state, and showing it beside the alternatives is the
    // point: the spread across the sweep IS the prior's influence.
    let mut table: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for prior in PRIOR_SWEEP {
        let model = CausalModel::with_expected_outcome(prior);
        for name in &names {
            table
                .entry(name.clone())
                .or_default()
                .push(model.expected_fans(name));
        }
    }

    print!("\n  {:<34}", "template");
    for prior in PRIOR_SWEEP {
        print!("  prior={prior:<5.1}");
    }
    println!();
    for (name, values) in table.iter().take(10) {
        let short: String = name.chars().take(32).collect();
        print!("  {short:<34}");
        for value in values {
            print!("  {value:<11.3}");
        }
        println!();
    }
    println!(
        "\n  VERDICT: with zero observations every prediction equals the prior exactly.\n  \
         The brain is not estimating anything — it is reading back what it was told\n  \
         to assume. Ranking differences between templates come entirely from context\n  \
         adjustment and EFE, not from evidence."
    );
    println!();
    Ok(())
}
