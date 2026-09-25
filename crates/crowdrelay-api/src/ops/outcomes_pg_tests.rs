// Executes `load_outcomes` against a real schema. The read joins
// `autopilot_actions` to `autopilot_outcomes` and `autopilot_measurements`
// — the fixture goes through all three so the test proves the join keys
// and the terminal-status filter, not just the JSON shape.
//
// The `_tests.rs` name exempts the fixture inserts from the decision-trace
// contract gate (test scaffolding, not a production write path).

#[cfg(test)]
mod outcomes_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn outcomes_reports_approved_actions_with_their_verdicts() {
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
            .bind(format!("outcomes-{}", workspace_id.into_uuid().simple()))
            .bind("Outcomes Tests")
            .execute(&pool)
            .await
            .expect("workspace");

        let decision_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_decisions
               (id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
               VALUES ($1,$2,$3,'outreach','outreach_target',$4,
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

        async fn seed_action(
            pool: &PgPool,
            workspace_id: WorkspaceId,
            decision_id: Uuid,
            status: &str,
            approved: bool,
        ) -> Uuid {
            let action_id = Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload,
                    status, action_class, trace_id, approved_at, approved_by,
                    finished_at)
                   VALUES ($1,$2,$3,'outreach','outreach.request',
                           'outreach_target',$4,$5,
                           '{"target_name":"Radio Z"}'::jsonb,$6,
                           'third_party',$7,
                           CASE WHEN $8 THEN now() ELSE NULL END,
                           CASE WHEN $8 THEN 'operator:test' ELSE NULL END,
                           CASE WHEN $6 IN ('succeeded','failed') THEN now() ELSE NULL END)"#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(status)
            .bind(Uuid::now_v7())
            .bind(approved)
            .execute(pool)
            .await
            .expect("action");
            action_id
        }

        // An approved send with a measured verdict.
        let measured = seed_action(&pool, workspace_id, decision_id, "succeeded", true).await;
        let measurement_id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO autopilot_measurements
               (workspace_id, action_id, subject_id,
                measurement_kind, action_finished_at, baseline_value,
                available_at, due_at, status, finished_at)
               VALUES ($1,$2,$3,'outreach_reply_7d',now(),0.0,
                       now(), now(), 'succeeded', now())
               RETURNING id"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(measured)
        .bind(Uuid::now_v7())
        .fetch_one(&pool)
        .await
        .expect("measurement");
        sqlx::query(
            r#"INSERT INTO autopilot_outcomes
               (workspace_id, decision_id, action_id, measurement_id, metric_key,
                observed_value, baseline_value, effect_assessment, delta_basis_points)
               VALUES ($1,$2,$3,$4,'replies_received',1.0,0.0,'improved',10000)"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(decision_id)
        .bind(measured)
        .bind(measurement_id)
        .execute(&pool)
        .await
        .expect("outcome");

        // An approved send still waiting on its measurement horizon.
        let pending = seed_action(&pool, workspace_id, decision_id, "succeeded", true).await;
        sqlx::query(
            r#"INSERT INTO autopilot_measurements
               (workspace_id, action_id, subject_id,
                measurement_kind, action_finished_at, baseline_value,
                available_at, due_at, status)
               VALUES ($1,$2,$3,'outreach_reply_7d',now(),0.0,
                       now(), now() + interval '7 days', 'pending')"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pending)
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .expect("pending measurement");

        // A never-approved row and a queued row must not appear.
        let _unapproved = seed_action(&pool, workspace_id, decision_id, "succeeded", false).await;
        let _queued = seed_action(&pool, workspace_id, decision_id, "queued", true).await;

        let result = load_outcomes(&pool, workspace_id.into_uuid())
            .await
            .expect("load_outcomes");
        let actions = result["actions"].as_array().expect("actions array");
        assert_eq!(actions.len(), 2, "only approved + terminal actions");

        let measured_row = actions
            .iter()
            .find(|a| a["id"] == measured.to_string())
            .expect("measured action");
        assert_eq!(measured_row["outcome_state"], "measured");
        assert_eq!(measured_row["label"], "Radio Z");
        assert_eq!(measured_row["outcomes"][0]["verdict"], "improved");

        let pending_row = actions
            .iter()
            .find(|a| a["id"] == pending.to_string())
            .expect("pending action");
        assert_eq!(pending_row["outcome_state"], "pending");
        assert!(!pending_row["next_measurement_due"].is_null());
    }
}
