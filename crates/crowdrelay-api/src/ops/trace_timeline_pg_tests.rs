// Executes `load_trace_timeline` against a real schema. This endpoint
// 500'd on every call in production — an arm named a column
// (`created_at`) that `viryaos_autopilot_decisions` never had — because
// the query sat in the sql-result-types *unprepared* baseline and
// nothing else ever ran it. Preparing is not executing: this test walks
// a seeded decision→action→measurement→growth-evidence chain through
// the UNION so a broken arm fails here, not in an operator's console.
//
// The file lives under `ops/` and ends in `_tests.rs` deliberately: the
// decision-trace contract gate scans `src/**/*.rs` for decision-table
// writers and exempts only `tests.rs`/`*_tests.rs` names — the fixture
// inserts below are test scaffolding, not a production write path.

#[cfg(test)]
mod trace_timeline_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn trace_walks_decision_action_measurement_and_growth_evidence() {
        let Ok(database_url) = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect");
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .expect("migrate");

        let workspace_id = WorkspaceId::new();
        let trace_id = Uuid::now_v7();
        let decision_id = Uuid::now_v7();
        let action_id = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
            .bind(workspace_id.into_uuid())
            .bind(format!("trace-{}", workspace_id.into_uuid().simple()))
            .bind("Trace Tests")
            .execute(&pool)
            .await
            .expect("workspace");
        sqlx::query(
            r#"INSERT INTO viryaos_autopilot_decisions
               (id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
               VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                       'auto_execute',9000,'auto_execute','test',
                       '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
        )
        .bind(decision_id)
        .bind(workspace_id.into_uuid())
        .bind(format!("key-{decision_id}"))
        .bind(Uuid::now_v7())
        .bind(trace_id)
        .execute(&pool)
        .await
        .expect("decision");
        sqlx::query(
            r#"INSERT INTO viryaos_autopilot_actions
               (id, workspace_id, decision_id, context, action_kind, subject_kind,
                subject_id, idempotency_key, payload, status, action_class,
                trace_id, finished_at)
               VALUES ($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                       $4,$5,'{}'::jsonb,'succeeded','third_party',$6,now())"#,
        )
        .bind(action_id)
        .bind(workspace_id.into_uuid())
        .bind(decision_id)
        .bind(Uuid::now_v7())
        .bind(format!("idem-{action_id}"))
        .bind(trace_id)
        .execute(&pool)
        .await
        .expect("action");
        sqlx::query(
            r#"INSERT INTO viryaos_autopilot_measurements
               (id, workspace_id, action_id, measurement_kind, subject_id,
                action_finished_at, baseline_value, due_at, available_at, trace_id)
               VALUES ($1,$2,$3,'incremental_fan_growth_3d',$3,now(),1.0,
                       now() + interval '3 days', now(),$4)"#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(trace_id)
        .execute(&pool)
        .await
        .expect("measurement");
        sqlx::query(
            r#"INSERT INTO viryaos_growth_evidence
               (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
                channel, estimated_reach, treatment, propensity, converted,
                predicted_fans, predicted_signal_installs, context, evidence_quality,
                observed_incremental_fans, resolved_at)
               VALUES ($1,$2,'opp',now(),'recipient','reddit_post',100,'treatment',
                       0.9,false,2.0,1.0,'{}'::jsonb,'observational',3.0,now())"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .execute(&pool)
        .await
        .expect("evidence");

        let ops = OpsState::new(workspace_id, pool, Duration::from_secs(10));
        let events = load_trace_timeline(&ops, &trace_id)
            .await
            .expect("trace query must execute against a real schema");

        let sources: Vec<&str> = events.iter().map(|e| e.source.as_str()).collect();
        for expected in ["decision", "action", "measurement", "growth_evidence"] {
            assert!(
                sources.contains(&expected),
                "trace is missing the {expected} arm: {sources:?}"
            );
        }
        let evidence = events
            .iter()
            .find(|e| e.source == "growth_evidence")
            .expect("growth evidence arm");
        assert_eq!(evidence.state.as_deref(), Some("resolved"));
    }
}
