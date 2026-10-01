#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn separate_horizons_survive_checkpoint_restart_and_teach_durability_once()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repo, workspace) = continuity_fixture().await?;
    let early = OffsetDateTime::now_utc().replace_nanosecond(0)? - time::Duration::days(31);
    let late = early + time::Duration::days(30);
    let evidence_id = Uuid::now_v7();
    sqlx::query("INSERT INTO growth_evidence(id,workspace_id,recipient_id,channel,observed_incremental_fans,replayed_14d_at,last_partial_resolution_at,partial_resolution_count,outcome_basis) VALUES($1,$2,'bridge-lifecycle','reddit_post',8,$3,$3,1,'attributed')")
        .bind(evidence_id).bind(workspace.into_uuid()).bind(early).execute(&pool).await?;
    let first = repo.load_causal_model(workspace).await?;
    assert_eq!(first.model.bridge.confidence(), 0);
    repo.save_brain_state(
        workspace,
        "causal_model",
        &serde_json::to_value(&first.model)?,
    )
    .await?;
    sqlx::query("UPDATE growth_evidence SET durable_fans_30d=2,replayed_30d_at=$2,resolved_at=$2 WHERE id=$1")
        .bind(evidence_id).bind(late).execute(&pool).await?;
    let complete = repo.load_causal_model(workspace).await?;
    assert_eq!(complete.model.bridge.confidence(), 1);
    let once = serde_json::to_value(&complete.model)?;
    repo.save_brain_state(workspace, "causal_model", &once)
        .await?;
    assert_checkpoint_is_unchanged(&repo, workspace).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_malformed_only_batch_advances_without_legacy_fallback_or_repeated_work()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repo, workspace) = continuity_fixture().await?;
    let at = OffsetDateTime::now_utc().replace_nanosecond(0)? - time::Duration::days(1);
    sqlx::query(r#"INSERT INTO growth_evidence(workspace_id,recipient_id,channel,context,observed_incremental_fans,replayed_14d_at,resolved_at,outcome_basis) VALUES($1,'malformed-only','reddit_post','{"fan_growth_trend": []}',10000,$2,$2,'attributed')"#)
        .bind(workspace.into_uuid()).bind(at).execute(&pool).await?;
    let learned = repo.load_causal_model(workspace).await?;
    assert_eq!(learned.model.evidence_cursor, Some(at));
    assert_eq!(learned.model.bridge.confidence(), 0);
    let once = serde_json::to_value(learned.model)?;
    repo.save_brain_state(workspace, "causal_model", &once)
        .await?;
    assert_checkpoint_is_unchanged(&repo, workspace).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn malformed_context_is_preserved_but_cannot_teach_a_fabricated_cell()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_brain::StateConditionedStrategyPosterior;
    let (pool, repo, workspace) = continuity_fixture().await?;
    let at = OffsetDateTime::now_utc().replace_nanosecond(0)? - time::Duration::days(1);
    sqlx::query(r#"INSERT INTO growth_evidence(workspace_id,recipient_id,channel,context,strategy,observed_incremental_fans,replayed_14d_at,resolved_at,outcome_basis) VALUES($1,'valid','reddit_post','{}','content_first',4,$2,$2,'attributed'),($1,'malformed','reddit_post','{"fan_growth_trend": []}','content_first',10000,$2,$2,'attributed')"#)
        .bind(workspace.into_uuid()).bind(at).execute(&pool).await?;
    let loaded = repo.load_growth_evidence(workspace, None).await?;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].recipient_id, "valid");
    let learned = repo.load_causal_model(workspace).await?;
    assert_eq!(learned.model.evidence_cursor, Some(at));
    let (state, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("strategy")?;
    let posterior: StateConditionedStrategyPosterior = serde_json::from_value(state)?;
    assert_eq!(posterior.cells().map(|(_, _, _, n)| n).sum::<u32>(), 1);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM growth_evidence WHERE workspace_id=$1")
            .bind(workspace.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        count, 2,
        "bad evidence must remain available for inspection/repair"
    );
    repo.save_brain_state(
        workspace,
        "causal_model",
        &serde_json::to_value(&learned.model)?,
    )
    .await?;
    assert_checkpoint_is_unchanged(&repo, workspace).await?;
    Ok(())
}

// Compare against the state decoded from the actual PostgreSQL JSONB row.
// A decimal round trip can change the in-memory f64 by one ULP; that is not
// another learning update. No tolerance is allowed between decoded models.
async fn assert_checkpoint_is_unchanged(
    repo: &PostgresAutopilotRepository,
    workspace: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    let (stored, _) = repo
        .load_brain_state(workspace, "causal_model")
        .await?
        .ok_or("checkpoint")?;
    let expected: crowdrelay_brain::CausalModel = serde_json::from_value(stored)?;
    let repeated = repo.load_causal_model(workspace).await?;
    assert_eq!(
        serde_json::to_value(repeated.model)?,
        serde_json::to_value(expected)?
    );
    Ok(())
}
