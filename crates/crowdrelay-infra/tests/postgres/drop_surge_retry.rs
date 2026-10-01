// Dispatch, task, and destination failures are distinct durable receipts.

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn drop_surge_task_failures_are_target_scoped_and_never_counted_twice()
-> Result<(), Box<dyn std::error::Error>> {
    let isolated = common::isolated_database("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let pool = &isolated.pool;
    let repo =
        PostgresAutopilotRepository::new_with_timeouts(pool.clone(), Duration::from_secs(10));
    let result = async {
        let ws = WorkspaceId::new();
        seed_workspace(pool, ws).await?;
        let source: Uuid = sqlx::query_scalar(
            "INSERT INTO content_sources (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
             VALUES ($1,'video','receipt-video','Real video',now()-interval '1 hour',now()+interval '30 days',$2)
             RETURNING id",
        ).bind(ws.into_uuid()).bind(serde_json::json!({"url":"https://youtu.be/realvideo"}))
            .fetch_one(pool).await?;
        let first = seed_community(pool, ws, "firstreceipt", None).await?;
        let second = seed_community(pool, ws, "secondreceipt", None).await?;
        let dispatch = seed_draft_request(pool, ws, source, first, "", "succeeded").await?;
        let other = seed_draft_request(pool, ws, source, second, "", "succeeded").await?;

        // A CrowdRelay-only database must still load the source. This is a
        // fresh isolated database, not a table drop in the shared suite.
        let absent: bool = sqlx::query_scalar("SELECT to_regclass('agent_service_tasks') IS NULL")
            .fetch_one(pool).await?;
        assert!(absent, "this proof requires the agents table to be absent");
        let loaded = repo.load_content_supply_snapshots(ws, time::OffsetDateTime::now_utc()).await?;
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].drop_surge_failures.is_empty());

        sqlx::query(
            "CREATE TABLE agent_service_tasks (
                id UUID PRIMARY KEY, workspace_id UUID NOT NULL, status TEXT NOT NULL,
                metadata JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        ).execute(pool).await?;
        // The foreign table can exist without completed_at. Both readers
        // must retain genuine failure receipts and tenant/latest-task gates.
        let legacy_created = time::OffsetDateTime::now_utc().replace_nanosecond(0)?
            - time::Duration::minutes(40);
        for workspace in [ws, WorkspaceId::new()] {
            sqlx::query("INSERT INTO agent_service_tasks(id,workspace_id,status,metadata,created_at)
                         VALUES($1,$2,'failed',$3,$4)")
                .bind(Uuid::now_v7()).bind(workspace.into_uuid())
                .bind(serde_json::json!({"action_id":dispatch})).bind(legacy_created)
                .execute(pool).await?;
        }
        let legacy = repo.load_content_supply_snapshots(ws, time::OffsetDateTime::now_utc()).await?;
        assert_eq!(legacy[0].drop_surge_failures.len(), 1);
        assert_eq!(legacy[0].drop_surge_failures[0].failures, 1);
        assert_eq!(legacy[0].drop_surge_failures[0].last_failed_at, legacy_created);
        sqlx::query("UPDATE autopilot_actions SET idempotency_key=$2 WHERE id=$1")
            .bind(dispatch).bind(format!("action:relay:{source}:community:{first}"))
            .execute(pool).await?;
        let targets = repo.load_relay_community_targets(ws).await?;
        let failed = &targets.iter().find(|t| t.target_id.into_uuid()==first)
            .ok_or("legacy relay target")?.relay_failures;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].failures, 1);
        assert_eq!(failed[0].last_failed_at, legacy_created);
        sqlx::query("INSERT INTO agent_service_tasks(id,workspace_id,status,metadata,created_at)
                     VALUES($1,$2,'completed',$3,$4)")
            .bind(Uuid::now_v7()).bind(ws.into_uuid())
            .bind(serde_json::json!({"action_id":dispatch}))
            .bind(legacy_created + time::Duration::minutes(1)).execute(pool).await?;
        let targets = repo.load_relay_community_targets(ws).await?;
        assert!(targets.iter().find(|t| t.target_id.into_uuid()==first)
            .ok_or("legacy relay target")?.relay_failures.is_empty());
        sqlx::query("UPDATE autopilot_actions SET idempotency_key=$2 WHERE id=$1")
            .bind(dispatch).bind(format!("action:drop_surge:{source}:community:{first}"))
            .execute(pool).await?;
        assert!(repo.load_content_supply_snapshots(ws, time::OffsetDateTime::now_utc())
            .await?[0].drop_surge_failures.is_empty());
        sqlx::query("DELETE FROM agent_service_tasks").execute(pool).await?;
        sqlx::query("ALTER TABLE agent_service_tasks ADD COLUMN completed_at timestamptz")
            .execute(pool).await?;
        let task = |workspace: WorkspaceId, action: Uuid, status: &'static str, minutes: i32| async move {
            sqlx::query(
                "INSERT INTO agent_service_tasks (id,workspace_id,status,metadata,created_at,completed_at)
                 VALUES ($1,$2,$3,$4,now()-make_interval(mins=>$5),now()-make_interval(mins=>$5))",
            ).bind(Uuid::now_v7()).bind(workspace.into_uuid()).bind(status)
                .bind(serde_json::json!({"action_id":action})).bind(minutes).execute(pool).await
        };
        task(ws, dispatch, "failed", 40).await?;
        task(ws, other, "completed", 30).await?;
        task(WorkspaceId::new(), other, "failed", 0).await?;
        let now = time::OffsetDateTime::now_utc();
        let loaded = repo.load_content_supply_snapshots(ws, now).await?;
        let failures = &loaded[0].drop_surge_failures;
        assert_eq!(failures.len(), 1, "another target and another tenant must not contribute");
        assert_eq!(failures[0].lane, format!("community:{first}"));
        assert_eq!(failures[0].failures, 1, "successful dispatch does not hide its failed task");
        assert!(failures[0].retry_due() <= now);

        // Persist the repaired key through the real action repository. The
        // second evaluation must dedupe, not create a second retry.
        let candidate = crowdrelay_application::autopilot::DecisionCandidate {
            context: crowdrelay_application::autopilot::AutopilotContext::ContentSupply,
            subject: crowdrelay_application::autopilot::ActionSubject::TargetCommunity(first),
            decision_kind: "drop_surge_fanout",
            confidence: crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(9_500),
            disposition: crowdrelay_domain::autonomy::PolicyDisposition::RequireApproval,
            reason: "retry a failed source-bound draft",
            input_snapshot: serde_json::json!({}), policy_snapshot: serde_json::json!({}),
            action: crowdrelay_application::autopilot::AutopilotActionPayload::RequestAgentRun {
                template_id: "community-engager".to_owned(), prompt: "draft only".to_owned(),
                priority: 1, tier: crowdrelay_brain::AgentTier::Basic,
            },
            decision_key: format!("decision:drop_surge:v1:{source}:community:{first}:attempt1"),
            action_idempotency_key: format!("action:drop_surge:{source}:community:{first}:attempt1"),
        };
        let trace = crowdrelay_domain::TraceContext::root(ws);
        let persisted = repo.persist_candidate(ws, &candidate, &trace).await?;
        assert!(persisted.action_created, "a dead dispatch must not swallow the retry");
        assert!(!repo.persist_candidate(ws, &candidate, &trace).await?.action_created);
        let retry = persisted.action_id.expect("retry action");
        for status in ["queued", "processing", "failed"] {
            sqlx::query("UPDATE autopilot_actions SET status=$3, finished_at=CASE WHEN $3='failed' THEN now() END WHERE workspace_id=$1 AND id=$2")
                .bind(ws.into_uuid()).bind(retry).bind(status).execute(pool).await?;
        }
        // A failed dispatch and its failed task are one attempt, not two.
        task(ws, retry, "failed", 0).await?;
        let loaded = repo.load_content_supply_snapshots(ws, now).await?;
        assert_eq!(loaded[0].drop_surge_failures.len(), 1);
        assert_eq!(loaded[0].drop_surge_failures[0].failures, 2);
        assert!(loaded[0].drop_surge_failures[0].retry_due() > now);

        // The latest task is the receipt: a superseded failure must not
        // re-open a dispatch whose replacement completed successfully.
        task(ws, dispatch, "completed", 0).await?;
        let loaded = repo.load_content_supply_snapshots(ws, now).await?;
        assert_eq!(loaded[0].drop_surge_failures[0].failures, 1);
        Ok::<(), Box<dyn std::error::Error>>(())
    }.await;
    isolated.drop().await?;
    result
}
