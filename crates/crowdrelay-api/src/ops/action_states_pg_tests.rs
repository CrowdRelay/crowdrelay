// Executes `load_action_states` against a real schema. The aggregate reads
// `action_ledger`, which is trigger-maintained from `autopilot_actions` —
// the seed below goes through the action table so the test also proves the
// trigger still projects the statuses the report counts.
//
// The file lives under `ops/` and ends in `_tests.rs` deliberately: the
// decision-trace contract gate scans `src/**/*.rs` for decision-table
// writers and exempts only `tests.rs`/`*_tests.rs` names — the fixture
// inserts below are test scaffolding, not a production write path.

#[cfg(test)]
mod action_states_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn action_states_reports_every_in_flight_state_in_pipeline_order() {
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
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
            .bind(workspace_id.into_uuid())
            .bind(format!("states-{}", workspace_id.into_uuid().simple()))
            .bind("Action States Tests")
            .execute(&pool)
            .await
            .expect("workspace");

        // One decision owns every seeded action — the ledger only needs the
        // FK chain to resolve, not distinct parents.
        let decision_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_decisions
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
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .expect("decision");

        // Status → ledger state via action_ledger_sync:
        //   queued → QUEUED, processing → RUNNING, unknown → UNKNOWN.
        async fn seed_action(
            pool: &PgPool,
            workspace_id: WorkspaceId,
            decision_id: Uuid,
            status: &str,
        ) -> Uuid {
            let action_id = Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload,
                    status, action_class, trace_id)
                   VALUES ($1,$2,$3,'growth_metrics','agent.run.request',
                           'target_community',$4,$5,'{}'::jsonb,$6,
                           'third_party',$7)"#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(status)
            .bind(Uuid::now_v7())
            .execute(pool)
            .await
            .expect("action");
            action_id
        }
        let older_queued = seed_action(&pool, workspace_id, decision_id, "queued").await;
        let _newer_queued = seed_action(&pool, workspace_id, decision_id, "queued").await;
        let _running = seed_action(&pool, workspace_id, decision_id, "processing").await;
        let _unknown = seed_action(&pool, workspace_id, decision_id, "unknown").await;

        // The append-only trigger blocks DELETE only — backdating
        // state_entered_at is how the test measures "oldest" against an age
        // it chose rather than whatever now() happened to be.
        sqlx::query(
            "UPDATE action_ledger
             SET state_entered_at = now() - interval '3 days'
             WHERE action_id = $1",
        )
        .bind(older_queued)
        .execute(&pool)
        .await
        .expect("backdate queued");

        // A second workspace's in-flight work must not leak into the count —
        // workspace_id is the whole of the tenant isolation.
        let other_workspace_id = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
            .bind(other_workspace_id.into_uuid())
            .bind(format!("other-{}", other_workspace_id.into_uuid().simple()))
            .bind("Other Workspace")
            .execute(&pool)
            .await
            .expect("other workspace");
        let other_decision_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_decisions
               (id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
               VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                       'auto_execute',9000,'auto_execute','test',
                       '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
        )
        .bind(other_decision_id)
        .bind(other_workspace_id.into_uuid())
        .bind(format!("key-{other_decision_id}"))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .expect("other decision");
        let other_action_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_actions
               (id, workspace_id, decision_id, context, action_kind,
                subject_kind, subject_id, idempotency_key, payload,
                status, action_class)
               VALUES ($1,$2,$3,'growth_metrics','agent.run.request',
                       'target_community',$4,$5,'{}'::jsonb,'queued',
                       'third_party')"#,
        )
        .bind(other_action_id)
        .bind(other_workspace_id.into_uuid())
        .bind(other_decision_id)
        .bind(Uuid::now_v7())
        .bind(format!("idem-{other_action_id}"))
        .execute(&pool)
        .await
        .expect("other workspace action");

        let ops = OpsState::new(workspace_id, pool.clone(), Duration::from_secs(10));
        let report = load_action_states(&ops)
            .await
            .expect("aggregate query must execute against a real schema");

        // Always all six in-flight states, in pipeline order — an empty
        // state is count 0 + oldest null, a measured zero.
        let states: Vec<&str> = report.in_flight.iter().map(|row| row.state).collect();
        assert_eq!(
            states,
            ["PLANNED", "AUTHORIZED", "QUEUED", "RUNNING", "UNKNOWN", "RECONCILING"],
            "in_flight must list every in-flight state in order: {states:?}"
        );

        let by_state = |name: &str| {
            report
                .in_flight
                .iter()
                .find(|row| row.state == name)
                .unwrap_or_else(|| panic!("{name} missing from report"))
        };
        assert_eq!(by_state("PLANNED").count, 0);
        assert_eq!(by_state("PLANNED").oldest_entered_at, None);
        assert_eq!(by_state("AUTHORIZED").count, 0);
        assert_eq!(by_state("QUEUED").count, 2, "two queued, not three — the other workspace's row must not count");
        assert_eq!(by_state("RUNNING").count, 1);
        assert_eq!(by_state("UNKNOWN").count, 1);
        assert_eq!(by_state("RECONCILING").count, 0);

        // The QUEUED oldest is the backdated row's, not the newer one's.
        let (ledger_oldest,): (OffsetDateTime,) = sqlx::query_as(
            "SELECT state_entered_at FROM action_ledger WHERE action_id = $1",
        )
        .bind(older_queued)
        .fetch_one(&pool)
        .await
        .expect("ledger row");
        assert_eq!(
            by_state("QUEUED").oldest_entered_at,
            Some(ledger_oldest),
            "oldest_entered_at must be the older QUEUED row's entry time"
        );
    }
}
