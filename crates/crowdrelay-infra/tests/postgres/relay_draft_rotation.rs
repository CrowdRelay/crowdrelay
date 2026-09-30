// Source-bound drafting requests rotate the audience pool before a post exists.

async fn seed_draft_request(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    source: Uuid,
    target: Uuid,
    suffix: &str,
    status: &str,
) -> Result<Uuid, sqlx::Error> {
    let decision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions
         (id, workspace_id, decision_key, context, subject_kind, subject_id,
          decision_kind, confidence_basis_points, disposition, reason,
          input_snapshot, policy_snapshot, recommendation, trace_id)
         VALUES ($1,$2,$3,'content_supply','target_community',$4,
                 'drop_surge_fanout',9500,'auto_execute','source-bound draft',
                 '{}','{}','{}',$5)",
    )
    .bind(decision)
    .bind(workspace_id.into_uuid())
    .bind(decision.to_string())
    .bind(target)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    let action = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions
         (id, workspace_id, decision_id, context, action_kind, subject_kind,
          subject_id, idempotency_key, payload, status, created_at, finished_at)
         VALUES ($1,$2,$3,'content_supply','agent.run.request','target_community',
                 $4,$5,$6,$7,now()-interval '10 minutes',
                 CASE WHEN $7 IN ('succeeded','failed') THEN now() END)",
    )
    .bind(action).bind(workspace_id.into_uuid()).bind(decision).bind(target)
    .bind(format!("action:drop_surge:{source}:community:{target}{suffix}"))
    .bind(serde_json::json!({"kind":"request_agent_run", "template_id":"community-engager",
        "prompt":format!("source_id: {source}\ntarget_id: {target}"), "priority":1, "tier":"basic"}))
    .bind(status).execute(pool).await?;
    Ok(action)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn dispatched_drafts_rotate_before_any_community_post_exists()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws).await?;
    let source = Uuid::now_v7();
    for name in [
        "rotationa",
        "rotationb",
        "rotationc",
        "rotationd",
        "rotatione",
    ] {
        seed_community(&pool, ws, name, None).await?;
    }
    let first = repo.load_relay_community_targets(ws).await?;
    assert_eq!(first.len(), 3);
    for target in &first {
        seed_draft_request(
            &pool,
            ws,
            source,
            target.target_id.into_uuid(),
            "",
            "succeeded",
        )
        .await?;
    }
    let next = repo.load_relay_community_targets(ws).await?;
    assert_eq!(next.len(), 3);
    assert_eq!(
        next.iter()
            .filter(|target| !first.iter().any(|old| old.target_id == target.target_id))
            .count(),
        2,
        "both untouched communities get a turn while the first drafts have no post rows"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM community_posts WHERE workspace_id=$1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(count, 0, "rotation must not depend on post materialization");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn inflight_drafts_wait_but_failed_dispatches_do_not_block_targets()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws).await?;
    let held = seed_community(&pool, ws, "helddraft", None).await?;
    let failed = seed_community(&pool, ws, "faileddraft", None).await?;
    let source = Uuid::now_v7();
    seed_draft_request(&pool, ws, source, held, "", "queued").await?;
    seed_draft_request(&pool, ws, source, failed, "", "failed").await?;
    let targets = repo.load_relay_community_targets(ws).await?;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].target_id.into_uuid(), failed);
    Ok(())
}
