//! The language gate on community drafts, against a real Postgres.
//!
//! Lives beside `community_relay_batch` and borrows its fixtures: the gate
//! matters most under an approved batch, where a draft queues with nobody
//! reading it.

use anyhow::{Result, ensure};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::AutopilotControlRepository;
use uuid::Uuid;

use crate::common;
use crate::community_relay_batch::{
    action_rows, community_target, content_source, engage_outcome, engage_outcome_text, repository,
    worker, workspace,
};

/// On 2026-09-26 a batch approved on an English sample carried a later
/// draft into r/melodicdeathmetal in Polish, word for word, and a moderator
/// removed it. A draft in a language other than its community's — English
/// when none is recorded — is rejected even under an approved batch; the
/// same words for a community recorded as Polish queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_draft_in_the_wrong_language_is_rejected_under_an_approved_batch() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let sample = community_target(&pool, ws, "metalcore").await?;
    let english = community_target(&pool, ws, "melodicdeathmetal").await?;
    let polish = community_target(&pool, ws, "metalpolska").await?;
    sqlx::query("UPDATE agent_outreach_targets SET language = 'pl' WHERE id = $1")
        .bind(polish)
        .execute(&pool)
        .await?;

    engage_outcome(&pool, ws, sample, source_id, "metalcore").await?;
    worker(&pool, ws).run_once().await?;
    repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            None,
            &IdempotencyKey::parse("batch-approve-language")?,
            None,
        )
        .await?;

    let title = "Terapia grupowa, spowiedź szaleńca, mental metal.";
    let body = "Łapcie mordeczki i do zobaczenia na terapii.";
    let wrong = engage_outcome_text(
        &pool,
        ws,
        english,
        source_id,
        "melodicdeathmetal",
        title,
        body,
    )
    .await?;
    let right =
        engage_outcome_text(&pool, ws, polish, source_id, "metalpolska", title, body).await?;
    worker(&pool, ws).run_once().await?;

    let outcome = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT status, rejection_reason FROM agent_outcomes WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
        }
    };
    let (status, reason) = outcome(wrong).await?;
    ensure!(
        status == "rejected"
            && reason
                .as_deref()
                .is_some_and(|r| r.starts_with("COMMUNITY_LANGUAGE_MISMATCH")),
        "a Polish draft for an English community is rejected, got {status} {reason:?}"
    );
    let (status, reason) = outcome(right).await?;
    ensure!(
        status == "processed",
        "the Polish community takes it, got {status} {reason:?}"
    );
    let actions = action_rows(&pool, ws).await?;
    ensure!(
        actions.len() == 2,
        "the sample and the Polish delivery, not the rejected draft: {actions:?}"
    );
    Ok(())
}
