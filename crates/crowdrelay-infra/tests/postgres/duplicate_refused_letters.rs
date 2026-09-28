//! Letters refused as duplicates may be written again (migration 0376).
//!
//! Nameless Polish letters were identical, so the dispatch guard sent one
//! and refused the rest; the refused rows kept their idempotency keys and
//! the evaluator could never offer those contacts a letter again. The
//! migration re-keys exactly those rows.

use crate::common;
use sqlx::PgPool;
use uuid::Uuid;

fn migration_sql() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../migrations/0376_duplicate_refused_letters_may_be_written_again.sql");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

async fn letter(
    pool: &PgPool,
    ws: Uuid,
    key: &str,
    status: &str,
    body: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions (
               id, workspace_id, decision_key, context, subject_kind, subject_id,
               decision_kind, confidence_basis_points, disposition, reason,
               input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'outreach','outreach_opportunity',gen_random_uuid(),
                   'request_relationship_outreach',8800,'require_approval','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(ws)
    .bind(format!("decision-{key}"))
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions (
               id, workspace_id, decision_id, context, action_kind, subject_kind,
               subject_id, idempotency_key, payload, status, last_error_kind, finished_at)
           VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',
                   gen_random_uuid(),$4,
                   jsonb_build_object('kind','request_outreach',
                       'draft', jsonb_build_object('subject','Virya — koncert','body',$5::text)),
                   $6, CASE WHEN $6 = 'failed' THEN 'state_changed' END, now())"#,
    )
    .bind(action_id)
    .bind(ws)
    .bind(decision_id)
    .bind(format!("action:outreach:{key}"))
    .bind(body)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(action_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn only_letters_refused_as_duplicates_are_rekeyed() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(ws)
        .bind(format!("dup-letters-{}", ws.simple()))
        .execute(&pool)
        .await?;

    let same = "Dzień dobry,\n\nPiszemy w imieniu Virya.";
    let sent = letter(&pool, ws, "sent", "succeeded", same).await?;
    let refused = letter(&pool, ws, "refused", "failed", same).await?;
    let other_failure = letter(
        &pool,
        ws,
        "other",
        "failed",
        "Dzień dobry, Radio Gorzów,\n\nInny list.",
    )
    .await?;

    let outbox_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events (id, workspace_id, event_type, event_version, payload)
           VALUES ($1,$2,'crowdrelay.autopilot.outreach_requested',1,
                   jsonb_build_object('draft', jsonb_build_object('subject','Virya — koncert','body',$3::text)))"#,
    )
    .bind(outbox_id)
    .bind(ws)
    .bind(same)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_action_emissions (workspace_id, action_id, emission_key, outbox_event_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(ws)
    .bind(sent)
    .bind(format!("emission-{sent}"))
    .bind(outbox_id)
    .execute(&pool)
    .await?;

    sqlx::raw_sql(&migration_sql()).execute(&pool).await?;

    let key = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT idempotency_key FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
            )
            .bind(ws)
            .bind(id)
            .fetch_one(&pool)
            .await
        }
    };
    assert_eq!(
        key(sent).await?,
        "action:outreach:sent",
        "the sent letter keeps its key"
    );
    assert!(
        key(refused).await?.contains(":duplicate:"),
        "the refused duplicate is freed"
    );
    assert_eq!(
        key(other_failure).await?,
        "action:outreach:other",
        "an unrelated failure keeps its key"
    );
    Ok(())
}
