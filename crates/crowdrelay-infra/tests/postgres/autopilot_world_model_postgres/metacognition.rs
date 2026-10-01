use crowdrelay_brain::self_assessment::{
    BrainState,
    checkpoint::{MetacognitionCheckpoint, MetacognitionObservation},
};

async fn continuity_fixture()
-> Result<(sqlx::PgPool, PostgresAutopilotRepository, WorkspaceId), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'Continuity')")
        .bind(workspace.into_uuid())
        .bind(format!("continuity-{}", workspace.into_uuid().simple()))
        .execute(&pool)
        .await?;
    let config = DatabaseConfig {
        url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &config);
    Ok((pool, repository, workspace))
}

fn continuity_observation(at: i64, metric: &str, state: BrainState) -> serde_json::Value {
    serde_json::to_value(MetacognitionObservation {
        metric: metric.into(),
        state,
        observed_at_micros: at,
    })
    .expect("observation")
}

async fn read_continuity(
    repo: &PostgresAutopilotRepository,
    workspace: WorkspaceId,
) -> Result<MetacognitionCheckpoint, Box<dyn std::error::Error>> {
    let (value, _) = repo
        .load_brain_state(workspace, "metacognition")
        .await?
        .ok_or("checkpoint missing")?;
    Ok(serde_json::from_value(value)?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn metacognition_continuity_is_idempotent_and_previews_are_read_only()
-> Result<(), Box<dyn std::error::Error>> {
    let (_pool, repo, workspace) = continuity_fixture().await?;
    let first = continuity_observation(10, "activated_fans_30d", BrainState::Learning);
    let (a, b) = tokio::join!(
        repo.save_brain_state(workspace, "metacognition", &first),
        repo.save_brain_state(workspace, "metacognition", &first),
    );
    a?;
    b?;
    assert_eq!(
        read_continuity(&repo, workspace)
            .await?
            .monitor
            .learning_cycles,
        1
    );
    let next = continuity_observation(11, "activated_fans_30d", BrainState::Learning);
    repo.save_brain_state(workspace, "metacognition", &next)
        .await?;
    repo.save_brain_state(workspace, "metacognition", &first)
        .await?;
    assert_eq!(
        read_continuity(&repo, workspace)
            .await?
            .monitor
            .learning_cycles,
        2
    );
    let before = repo.load_brain_state(workspace, "metacognition").await?;
    for _ in 0..2 {
        repo.load_growth_intelligence_snapshots(workspace, OffsetDateTime::now_utc())
            .await?;
    }
    assert_eq!(
        repo.load_brain_state(workspace, "metacognition").await?,
        before
    );
    let changed = continuity_observation(12, "spotify_followers", BrainState::Improving);
    repo.save_brain_state(workspace, "metacognition", &changed)
        .await?;
    let checkpoint = read_continuity(&repo, workspace).await?;
    assert_eq!(checkpoint.monitor.learning_cycles, 0);
    assert_eq!(checkpoint.monitor.improving_cycles_total, 1);
    assert_eq!(checkpoint.metric, "spotify_followers");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unreadable_and_contended_continuity_preserves_history()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repo, workspace) = continuity_fixture().await?;
    let first = continuity_observation(10, "spotify_followers", BrainState::Learning);
    repo.save_brain_state(workspace, "metacognition", &first)
        .await?;
    let mut lock = pool.begin().await?;
    sqlx::query(
        "SELECT state FROM brain_state WHERE workspace_id=$1 AND module='metacognition' FOR UPDATE",
    )
    .bind(workspace.into_uuid())
    .fetch_one(&mut *lock)
    .await?;
    let next = continuity_observation(11, "spotify_followers", BrainState::Learning);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(4),
            repo.save_brain_state(workspace, "metacognition", &next)
        )
        .await?
        .is_err()
    );
    lock.rollback().await?;
    assert_eq!(
        read_continuity(&repo, workspace)
            .await?
            .monitor
            .learning_cycles,
        1
    );
    sqlx::query(
        "UPDATE brain_state SET state='{}'::jsonb WHERE workspace_id=$1 AND module='metacognition'",
    )
    .bind(workspace.into_uuid())
    .execute(&pool)
    .await?;
    // Advisory continuity cannot prevent the preview/decision snapshot, but
    // a completed evaluator must not silently overwrite its unreadable state.
    assert!(
        !repo
            .load_growth_intelligence_snapshots(workspace, OffsetDateTime::now_utc())
            .await?
            .is_empty()
    );
    assert!(
        repo.save_brain_state(workspace, "metacognition", &next)
            .await
            .is_err()
    );
    let (value, _) = repo
        .load_brain_state(workspace, "metacognition")
        .await?
        .ok_or("checkpoint")?;
    assert_eq!(value, serde_json::json!({}));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn spotify_growth_reaches_assessment_while_signal_is_flat()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repo, workspace) = continuity_fixture().await?;
    sqlx::query("INSERT INTO tenant_settings(workspace_id,key,value) VALUES($1,'north_star_metric','spotify_followers')")
        .bind(workspace.into_uuid()).execute(&pool).await?;
    let now = OffsetDateTime::now_utc();
    let month: OffsetDateTime = sqlx::query_scalar("SELECT date_trunc('month',$1::timestamptz)")
        .bind(now)
        .fetch_one(&pool)
        .await?;
    seed_series(
        &pool,
        workspace,
        "spotify",
        "followers",
        &[(month - time::Duration::days(1), 185), (now, 300)],
    )
    .await?;
    for (day, value) in [185_i32, 185, 185, 300, 300, 300].into_iter().enumerate() {
        let at = now - time::Duration::days(6 - day as i64);
        sqlx::query("INSERT INTO autopilot_cycle_runs(id,workspace_id,trigger,started_at,finished_at,outcome,north_star_metric,north_star_value) VALUES($1,$2,'scheduled',$3,$3,'succeeded','spotify_followers',$4)")
            .bind(Uuid::now_v7()).bind(workspace.into_uuid()).bind(at).bind(value)
            .execute(&pool).await?;
    }
    let snapshots = repo
        .load_growth_intelligence_snapshots(workspace, now)
        .await?;
    let first = snapshots.first().ok_or("snapshot")?;
    assert_eq!(first.world_model.north_star_current, 300);
    assert_eq!(first.world_model.north_star_this_month, 115);
    assert_eq!(first.world_model.total_fans, 0);
    assert_eq!(first.metacognition.state, BrainState::Improving);
    assert_eq!(first.metacognition.sizing_multiplier(), 1.0);
    assert!(
        first
            .world_model
            .platform_growth
            .iter()
            .any(|platform| platform.platform == "spotify" && platform.gained_this_month == 115)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn strategy_learning_retries_independently_of_a_newer_causal_checkpoint()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_brain::{CausalModel, StateConditionedStrategyPosterior};
    let (pool, repo, workspace) = continuity_fixture().await?;
    let start = OffsetDateTime::now_utc() - time::Duration::days(2);
    let measured = start + time::Duration::hours(1);
    let mut state = serde_json::to_value(StateConditionedStrategyPosterior::default())?;
    state.as_object_mut().ok_or("posterior")?.insert(
        "_strategy_observation_cursor_micros".into(),
        serde_json::json!(start.unix_timestamp() * 1_000_000 + i64::from(start.microsecond())),
    );
    repo.save_brain_state(workspace, "strategy_posterior", &state)
        .await?;
    // This newer model checkpoint used to make the strategy's missed reading
    // permanently invisible. No strategy input may depend on its write time.
    repo.save_brain_state(
        workspace,
        "causal_model",
        &serde_json::to_value(CausalModel::default())?,
    )
    .await?;
    sqlx::query("INSERT INTO growth_evidence(workspace_id,recipient_id,channel,strategy,observed_incremental_fans,replayed_14d_at,resolved_at,outcome_basis) VALUES($1,'strategy-retry','reddit_post','content_first',4,$2,$2,'attributed')")
        .bind(workspace.into_uuid()).bind(measured).execute(&pool).await?;

    let constraint = format!("strategy_retry_{}", workspace.into_uuid().simple());
    let micros = start.unix_timestamp() * 1_000_000 + i64::from(start.microsecond());
    // Disposable-database fault injection, limited to this fixture's workspace.
    sqlx::query(&format!("ALTER TABLE brain_state ADD CONSTRAINT {constraint} CHECK (workspace_id <> '{}'::uuid OR module <> 'strategy_posterior' OR (state->>'_strategy_observation_cursor_micros')::bigint = {micros})",workspace.into_uuid()))
        .execute(&pool).await?;
    repo.load_causal_model(workspace).await?;
    let (failed, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    assert_eq!(
        failed, state,
        "failed persistence must preserve both beliefs and cursor"
    );
    sqlx::query(&format!(
        "ALTER TABLE brain_state DROP CONSTRAINT {constraint}"
    ))
    .execute(&pool)
    .await?;
    repo.load_causal_model(workspace).await?;
    let (learned, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    let posterior: StateConditionedStrategyPosterior = serde_json::from_value(learned.clone())?;
    assert_eq!(posterior.cells().map(|(_, _, _, n)| n).sum::<u32>(), 1);
    repo.load_causal_model(workspace).await?;
    let (repeated, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    assert_eq!(
        repeated, learned,
        "a repeated model load must not invent confidence"
    );
    sqlx::query("UPDATE growth_evidence SET replayed_30d_at=now() WHERE workspace_id=$1")
        .bind(workspace.into_uuid())
        .execute(&pool)
        .await?;
    repo.load_causal_model(workspace).await?;
    let (settled, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    let settled_posterior: StateConditionedStrategyPosterior =
        serde_json::from_value(settled.clone())?;
    assert_eq!(
        settled_posterior.cells().map(|(_, _, _, n)| n).sum::<u32>(),
        1
    );
    assert!(
        settled["_strategy_observation_cursor_micros"].as_i64()
            > learned["_strategy_observation_cursor_micros"].as_i64()
    );
    repo.load_causal_model(workspace).await?;
    assert_eq!(
        repo.load_brain_state(workspace, "strategy_posterior")
            .await?
            .ok_or("posterior")?
            .0,
        settled
    );
    sqlx::query("UPDATE growth_evidence SET resolved_at=NULL,replayed_14d_at=NULL,replayed_30d_at=NULL,last_partial_resolution_at=now(),partial_resolution_count=1 WHERE workspace_id=$1")
        .bind(workspace.into_uuid()).execute(&pool).await?;
    repo.load_causal_model(workspace).await?;
    let (partial, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    let partial_posterior: StateConditionedStrategyPosterior =
        serde_json::from_value(partial.clone())?;
    assert_eq!(
        partial_posterior.cells().map(|(_, _, _, n)| n).sum::<u32>(),
        1
    );
    assert!(
        partial["_strategy_observation_cursor_micros"].as_i64()
            > settled["_strategy_observation_cursor_micros"].as_i64()
    );
    repo.load_causal_model(workspace).await?;
    assert_eq!(
        repo.load_brain_state(workspace, "strategy_posterior")
            .await?
            .ok_or("posterior")?
            .0,
        partial
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn causal_checkpoint_does_not_consume_an_unread_measurement()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_brain::CausalModel;
    let (pool, repo, workspace) = continuity_fixture().await?;
    let start = OffsetDateTime::now_utc().replace_nanosecond(0)? - time::Duration::days(2);
    let measured = start + time::Duration::hours(1);
    let mut model = CausalModel::default();
    model.evidence_cursor = Some(start);
    // Saving after measurement must not claim the measurement was read.
    repo.save_brain_state(workspace, "causal_model", &serde_json::to_value(&model)?)
        .await?;
    sqlx::query("INSERT INTO growth_evidence(workspace_id,recipient_id,channel,observed_fans,replayed_14d_at,resolved_at,outcome_basis) VALUES($1,'causal-race','reddit_post',4,$2,$2,'attributed')")
        .bind(workspace.into_uuid()).bind(measured).execute(&pool).await?;
    let learned = repo.load_causal_model(workspace).await?;
    assert_eq!(learned.model.evidence_cursor, Some(measured));
    let once = serde_json::to_value(&learned.model)?;
    repo.save_brain_state(workspace, "causal_model", &once)
        .await?;
    let repeated = repo.load_causal_model(workspace).await?;
    assert_eq!(
        serde_json::to_value(&repeated.model)?,
        once,
        "repeated load must not manufacture confidence"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_busy_strategy_writer_does_not_block_the_growth_learner()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, repo, workspace) = continuity_fixture().await?;
    let state =
        serde_json::to_value(crowdrelay_brain::StateConditionedStrategyPosterior::default())?;
    repo.save_brain_state(workspace, "strategy_posterior", &state)
        .await?;
    repo.save_brain_state(
        workspace,
        "causal_model",
        &serde_json::to_value(crowdrelay_brain::CausalModel::default())?,
    )
    .await?;
    let mut owner = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text,hashtextextended('crowdrelay:strategy-checkpoint',0)))")
        .bind(workspace.into_uuid()).execute(&mut *owner).await?;
    tokio::time::timeout(Duration::from_secs(3), repo.load_causal_model(workspace)).await??;
    let (after, _) = repo
        .load_brain_state(workspace, "strategy_posterior")
        .await?
        .ok_or("posterior")?;
    assert_eq!(
        after, state,
        "a skipped writer cannot overwrite another writer's checkpoint"
    );
    owner.rollback().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn legacy_cursor_upgrade_preserves_existing_learning()
-> Result<(), Box<dyn std::error::Error>> {
    let (_pool, repo, workspace) = continuity_fixture().await?;
    let mut model = crowdrelay_brain::CausalModel::default();
    let old_day = (OffsetDateTime::now_utc() - time::Duration::days(366)).date();
    model.value_exchange.observe(i64::from(old_day.to_julian_day()), 12345.0, 100.0);
    let mut legacy = serde_json::to_value(&model)?;
    legacy.as_object_mut().ok_or("model")?.remove("evidence_cursor");
    repo.save_brain_state(workspace, "causal_model", &legacy).await?;
    let (_, written_at) = repo
        .load_brain_state(workspace, "causal_model")
        .await?
        .ok_or("checkpoint")?;
    let loaded = repo.load_causal_model(workspace).await?;
    assert!(matches!(
        loaded.belief,
        crowdrelay_application::autopilot::BeliefStateOrigin::Checkpoint { .. }
    ));
    assert_eq!(loaded.model.evidence_cursor, Some(written_at));
    assert_eq!(loaded.model.value_exchange.days_observed, 1);
    let mut retained = serde_json::to_value(&loaded.model)?;
    retained.as_object_mut().ok_or("model")?.remove("evidence_cursor");
    assert_eq!(retained, legacy, "a metadata upgrade must retain existing learning");
    Ok(())
}
