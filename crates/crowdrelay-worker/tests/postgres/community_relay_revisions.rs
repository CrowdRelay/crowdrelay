//! Operator edits at the community relay batch's approval gate, against a
//! real Postgres. Split from `community_relay_batch.rs`, which owns the
//! helpers reused here (`workspace`, `worker`, `repository`, `batch_row`).

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use uuid::Uuid;

use super::community_relay_batch::{
    action_payload, action_rows, batch_row, community_target, content_source, engage_outcome,
    repository, worker, workspace,
};

// ── Operator edits at the approval gate ────────────────────────────
//
// The batch card's edit boxes land as `revisions` on the approval — each
// delivery's words reviewed through the same gate a single-action edit
// passes. A refused edit refuses the whole approval: the batch never
// approves around a draft the operator meant to fix.

async fn batch_action(pool: &PgPool, workspace_id: WorkspaceId, source_id: Uuid) -> Result<Uuid> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM autopilot_actions \
         WHERE workspace_id = $1 AND action_kind = 'community.engage.request' \
           AND payload->>'source_id' = $2::text",
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id)
    .fetch_one(pool)
    .await?)
}

async fn seed_post_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    source_id: Uuid,
    status: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, subreddit, title, body, relay_source_id, status) \
         VALUES ($1,$2,'metalpolska','seeded title','seeded body',$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(source_id)
    .bind(status)
    .execute(pool)
    .await
    .context("seed post row")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approving_with_revisions_rewrites_the_draft_and_audits_the_edit() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;

    let actions = action_rows(&pool, ws).await?;
    let edited = actions[0].0;
    let untouched = actions[1].0;
    let revisions = std::collections::BTreeMap::from([(
        edited,
        std::collections::BTreeMap::from([
            ("title".to_owned(), "The fixed title".to_owned()),
            ("body".to_owned(), "The fixed body".to_owned()),
        ]),
    )]);
    let mutation = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-edit-1")?,
            None,
        )
        .await?;
    ensure!(
        mutation.status == "approved:2",
        "the spread still releases as one, got {}",
        mutation.status
    );

    let revised = action_payload(&pool, edited).await?;
    ensure!(
        revised["title"] == "The fixed title" && revised["body"] == "The fixed body",
        "the edited delivery carries the operator's words: {revised}"
    );
    let original = action_payload(&pool, untouched).await?;
    ensure!(
        original["title"] == "New video is out",
        "the delivery nobody edited keeps its draft: {original}"
    );
    // The released action queued carrying the revised text — the queue never
    // held an approved action with the unapproved words.
    let statuses = action_rows(&pool, ws).await?;
    ensure!(
        statuses.iter().all(|(_, status, _, _)| status == "queued"),
        "both deliveries released"
    );

    // The voice ledger: one row per edited field, the machine's words and
    // the operator's, keyed to this approval's operation.
    let rows = sqlx::query_as::<_, (String, String, String, Uuid)>(
        "SELECT field, before_text, after_text, operation_id \
         FROM draft_revisions WHERE workspace_id = $1 AND action_id = $2 \
         ORDER BY field",
    )
    .bind(ws.into_uuid())
    .bind(edited)
    .fetch_all(&pool)
    .await?;
    ensure!(
        rows.len() == 2,
        "one audit row per edited field, got {rows:?}"
    );
    ensure!(
        rows[0]
            == (
                "body".to_owned(),
                "The band just dropped it — what do you think?".to_owned(),
                "The fixed body".to_owned(),
                mutation.operation_id
            ),
        "the body edit audits before → after under this approval, got {:?}",
        rows[0]
    );
    ensure!(
        rows[1].0 == "title"
            && rows[1].1 == "New video is out"
            && rows[1].2 == "The fixed title"
            && rows[1].3 == mutation.operation_id,
        "the title edit audits the same way, got {:?}",
        rows[1]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_revision_for_an_action_outside_the_batch_refuses_the_whole_approval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_a = content_source(&pool, ws).await?;
    let source_b = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_a, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_b, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;

    // The revision names source_b's delivery while approving source_a's
    // batch — an id the card cannot have shown for this batch. The map also
    // carries a *valid* edit for source_a's own delivery: it was drafted
    // first, so its v7 id sorts first in the map and its revision applies
    // before the refusal — the assertions below then prove the applied edit
    // rolled back, not merely that nothing ran.
    let own = batch_action(&pool, ws, source_a).await?;
    let foreign = batch_action(&pool, ws, source_b).await?;
    ensure!(
        own < foreign,
        "the own-delivery edit must apply before the refusal"
    );
    let revisions = std::collections::BTreeMap::from([
        (
            own,
            std::collections::BTreeMap::from([(
                "title".to_owned(),
                "applied then rolled back".to_owned(),
            )]),
        ),
        (
            foreign,
            std::collections::BTreeMap::from([("title".to_owned(), "edited".to_owned())]),
        ),
    ]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_a,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-foreign-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a foreign action id must refuse");

    // Nothing half-happened: batch_a still awaits, both drafts untouched,
    // and the applied edit's audit rows rolled back with it.
    let (status, _) = batch_row(&pool, ws, source_a).await?.expect("batch a");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    for (id, status, _, _) in action_rows(&pool, ws).await? {
        let payload = action_payload(&pool, id).await?;
        ensure!(
            status == "awaiting_approval",
            "no delivery released, got {status}"
        );
        ensure!(
            payload["title"] == "New video is out",
            "no draft edited: {payload}"
        );
    }
    let audit_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM draft_revisions WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        audit_rows == 0,
        "the applied edit's ledger rows rolled back too"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_revision_to_a_fact_field_refuses_the_whole_approval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    worker(&pool, ws).run_once().await?;

    let action_id = action_rows(&pool, ws).await?[0].0;
    // `subreddit` is where the post goes — the batch's fact, not its words.
    let revisions = std::collections::BTreeMap::from([(
        action_id,
        std::collections::BTreeMap::from([("subreddit".to_owned(), "othercommunity".to_owned())]),
    )]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-badfield-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a non-words field must refuse");
    let (status, _) = batch_row(&pool, ws, source_id).await?.expect("batch");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    let payload = action_payload(&pool, action_id).await?;
    ensure!(
        payload["subreddit"] == "metalpolska",
        "the fact stayed put: {payload}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_seeded_post_row_takes_the_edit_but_a_post_out_the_door_refuses() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let source_b = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_b, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;
    let action_a = batch_action(&pool, ws, source_id).await?;
    let action_b = batch_action(&pool, ws, source_b).await?;

    // A grant-covered delivery is the real shape this guards: its action ran
    // (queued → processing → succeeded, the ledger's own ladder) and its
    // post row seeded ahead of the card's answer — so the seeded row is
    // what will send, and the edit must reach it too.
    for status in ["queued", "processing", "succeeded"] {
        // Terminal states require finished_at — the same invariant the
        // executor's own writes satisfy.
        sqlx::query(
            "UPDATE autopilot_actions \
             SET status = $2, finished_at = CASE WHEN $2 IN ('succeeded','failed','cancelled') \
                 THEN now() ELSE finished_at END \
             WHERE id = $1",
        )
        .bind(action_a)
        .bind(status)
        .execute(&pool)
        .await
        .with_context(|| format!("step action to {status}"))?;
    }
    seed_post_row(&pool, ws, action_a, source_id, "pending").await?;
    let revisions = std::collections::BTreeMap::from([(
        action_a,
        std::collections::BTreeMap::from([("title".to_owned(), "the operator's title".to_owned())]),
    )]);
    repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-seeded-1")?,
            None,
        )
        .await?;
    let post_title = sqlx::query_scalar::<_, String>(
        "SELECT title FROM community_posts WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(action_a)
    .fetch_one(&pool)
    .await?;
    ensure!(
        post_title == "the operator's title",
        "the seeded row carries the edit, got {post_title}"
    );
    ensure!(
        action_payload(&pool, action_a).await?["title"] == "the operator's title",
        "the payload tells the same story"
    );

    // Once a post has left there is nothing to edit — the row on Reddit
    // keeps the words it left with, and the approval refuses loudly.
    seed_post_row(&pool, ws, action_b, source_b, "posted").await?;
    let revisions = std::collections::BTreeMap::from([(
        action_b,
        std::collections::BTreeMap::from([("title".to_owned(), "too late".to_owned())]),
    )]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_b,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-posted-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a post already out must refuse");
    let post_title = sqlx::query_scalar::<_, String>(
        "SELECT title FROM community_posts WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(action_b)
    .fetch_one(&pool)
    .await?;
    ensure!(
        post_title == "seeded title",
        "the posted row kept its words"
    );
    let (status, _) = batch_row(&pool, ws, source_b).await?.expect("batch b");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    Ok(())
}
