// Executes `load_goal_scoreboard` against a real schema: dispatch
// predictions, growth evidence and actions go through the same tables the
// worker writes, so the test proves the window, the resolved/pending split and
// that standing grants are not counted as a person's approval.
//
// The `_tests.rs` name exempts the fixture inserts from the decision-trace
// contract gate (test scaffolding, not a production write path).

#[cfg(test)]
mod goal_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn the_scoreboard_counts_plan_learning_and_people_since_declaration() {
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
            .bind(format!("goal-{}", workspace_id.into_uuid().simple()))
            .bind("Goal Scoreboard Tests")
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

        let now = OffsetDateTime::now_utc();
        let declared = now - time::Duration::days(7);

        // `created_hours_ago` / `approved_hours_ago` place the action in time;
        // `approved_by` None leaves it awaiting approval.
        async fn seed_action(
            pool: &PgPool,
            workspace_id: WorkspaceId,
            decision_id: Uuid,
            now: OffsetDateTime,
            created_hours_ago: i64,
            approval: Option<(i64, &str)>,
        ) -> Uuid {
            let action_id = Uuid::now_v7();
            let status = if approval.is_some() { "queued" } else { "awaiting_approval" };
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload,
                    status, action_class, trace_id, created_at, approved_at, approved_by)
                   VALUES ($1,$2,$3,'outreach','outreach.request',
                           'outreach_target',$4,$5,'{}'::jsonb,$6,
                           'third_party',$7,$8,$9,$10)"#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(status)
            .bind(Uuid::now_v7())
            .bind(now - time::Duration::hours(created_hours_ago))
            .bind(approval.map(|(hours_ago, _)| now - time::Duration::hours(hours_ago)))
            .bind(approval.map(|(_, by)| by.to_owned()))
            .execute(pool)
            .await
            .expect("action");
            action_id
        }

        // A person approved after 2h; another after 6h.
        let human_fast =
            seed_action(&pool, workspace_id, decision_id, now, 50, Some((48, "admin_api_key")))
                .await;
        let human_slow =
            seed_action(&pool, workspace_id, decision_id, now, 30, Some((24, "admin_api_key")))
                .await;
        // A standing grant approves instantly and is not a person.
        let granted = seed_action(
            &pool,
            workspace_id,
            decision_id,
            now,
            10,
            Some((10, "operator:standing_grant")),
        )
        .await;
        // Approved before the objective existed: outside the window.
        let _before = seed_action(
            &pool,
            workspace_id,
            decision_id,
            now,
            24 * 10,
            Some((24 * 9, "admin_api_key")),
        )
        .await;
        // Still waiting, 5 hours old.
        let _waiting = seed_action(&pool, workspace_id, decision_id, now, 5, None).await;

        for (action_id, expected, predicted_days_ago) in [
            (human_fast, 1.5_f64, 2_i64),
            (human_slow, 2.5, 1),
            (granted, 4.0, 20),
        ] {
            sqlx::query(
                r#"INSERT INTO dispatch_predictions
                   (workspace_id, action_id, template_id, expected_new_fans, predicted_at)
                   VALUES ($1,$2,'community-engager',$3,$4)"#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id)
            .bind(expected)
            .bind(now - time::Duration::days(predicted_days_ago))
            .execute(&pool)
            .await
            .expect("prediction");
        }

        // One resolved since declaration, one resolved before, one pending.
        for (action_id, resolved_days_ago) in
            [(human_fast, Some(1_i64)), (granted, Some(12)), (human_slow, None)]
        {
            sqlx::query(
                r#"INSERT INTO growth_evidence
                   (workspace_id, action_id, recipient_id, channel, resolved_at)
                   VALUES ($1,$2,'r/test','reddit_post',$3)"#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id)
            .bind(resolved_days_ago.map(|days| now - time::Duration::days(days)))
            .execute(&pool)
            .await
            .expect("evidence");
        }

        let objective = crowdrelay_application::ActiveObjective {
            objective_id: Uuid::now_v7(),
            platform: "signal".to_owned(),
            metric_key: "activated_fans_30d".to_owned(),
            direction: crowdrelay_domain::growth_metrics::MetricDirection::HigherIsBetter,
            baseline_value: 20,
            target_value: 120,
            observed_value: Some(35),
            declared_at: declared,
            deadline: declared + time::Duration::days(21),
            state: crowdrelay_domain::objectives::ObjectiveState::Behind {
                progress_basis_points: 1_500,
                projected_value: 65,
                shortfall: 85,
            },
        };
        let board = load_goal_scoreboard(
            &pool,
            workspace_id.into_uuid(),
            Some(objective),
            now,
        )
        .await
        .expect("scoreboard");

        assert_eq!(board["planned"]["dispatches"], 2, "the 20-day-old one predates the goal");
        assert_eq!(board["planned"]["expected_new_fans"], 4.0);
        assert_eq!(board["actual"]["travelled"], 15);
        assert_eq!(board["pace"]["posture"], "behind");
        assert_eq!(board["learning"]["resolved_since"], 1);
        assert_eq!(board["learning"]["resolved_total"], 2);
        assert_eq!(board["learning"]["pending"], 1);
        // One lane, two of three resolved, no fans — too few resolved to
        // call it a cut yet.
        let lane = &board["lanes_60d"][0];
        assert_eq!(lane["context"], "outreach");
        assert_eq!(lane["action_kind"], "outreach.request");
        assert_eq!(lane["dispatched"], 3);
        assert_eq!(lane["resolved"], 2);
        assert_eq!(lane["fans"], 0.0);
        assert_eq!(lane["cut_candidate"], false);
        // No posts yet: open at the floor, nothing removed.
        assert_eq!(board["reddit"]["state"], "open");
        assert_eq!(board["reddit"]["daily_cap"], 1);
        assert_eq!(board["reddit"]["removed_by"], json!([]));
        assert_eq!(
            board["approvals"]["approved_by_people"], 2,
            "a standing grant is not a person, and a pre-goal approval is outside the window"
        );
        let median = board["approvals"]["median_hours"].as_f64().expect("median");
        assert!((median - 4.0).abs() < 1e-6, "median of 2h and 6h, got {median}");
        assert_eq!(board["approvals"]["awaiting"], 1);
        let oldest = board["approvals"]["oldest_awaiting_hours"]
            .as_f64()
            .expect("oldest");
        assert!((oldest - 5.0).abs() < 0.01, "got {oldest}");

        // No objective: the trailing window still answers.
        let unanchored = load_goal_scoreboard(&pool, workspace_id.into_uuid(), None, now)
            .await
            .expect("scoreboard without objective");
        assert!(unanchored["objective"].is_null());
        assert!(unanchored["actual"]["travelled"].is_null());
        assert_eq!(unanchored["planned"]["dispatches"], 3);
    }

    #[tokio::test]
    async fn proof_spine_counts_only_complete_causal_people() {
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

        let ws = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'proof spine')")
            .bind(ws)
            .bind(format!("proof-{}", ws.simple()))
            .execute(&pool)
            .await
            .expect("workspace");
        let now = OffsetDateTime::now_utc();
        let since = now - time::Duration::days(7);

        async fn prospect(
            pool: &PgPool,
            ws: Uuid,
            handle: &str,
            now: OffsetDateTime,
        ) -> Uuid {
            let person = Uuid::now_v7();
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO persons(id,workspace_id) VALUES($1,$2)")
                .bind(person)
                .bind(ws)
                .execute(pool)
                .await
                .expect("person");
            sqlx::query(
                "INSERT INTO person_identities(
                     workspace_id,person_id,kind,platform,value,source
                 ) VALUES($1,$2,'platform_handle','instagram',$3,'test')",
            )
            .bind(ws)
            .bind(person)
            .bind(handle)
            .execute(pool)
            .await
            .expect("identity");
            sqlx::query(
                "INSERT INTO fan_prospects(
                     id,workspace_id,person_id,platform,external_identity,
                     expires_at,first_seen_at,last_seen_at,created_at
                 ) VALUES($1,$2,$3,'instagram',$4,$5,$6,$6,$6)",
            )
            .bind(id)
            .bind(ws)
            .bind(person)
            .bind(handle)
            .bind(now + time::Duration::days(30))
            .bind(now - time::Duration::days(1))
            .execute(pool)
            .await
            .expect("prospect");
            id
        }

        let converted_prospect = prospect(&pool, ws, "proof_person", now).await;
        let click_only_prospect = prospect(&pool, ws, "click_only", now).await;
        let link_good = Uuid::now_v7();
        let link_click_only = Uuid::now_v7();
        for (id, slug) in [
            (link_good, "proof-good"),
            (link_click_only, "proof-click-only"),
        ] {
            sqlx::query(
                "INSERT INTO smart_links(id,workspace_id,slug,destination_url,active)
                 VALUES($1,$2,$3,'https://example.test/signal',true)",
            )
            .bind(id)
            .bind(ws)
            .bind(slug)
            .execute(&pool)
            .await
            .expect("link");
        }
        let touched_at = now - time::Duration::hours(20);
        for (prospect_id, link_id, source_ref) in [
            (converted_prospect, link_good, "reply-good"),
            (click_only_prospect, link_click_only, "reply-click-only"),
        ] {
            sqlx::query(
                "INSERT INTO fan_prospect_touches(
                     workspace_id,prospect_id,kind,source,source_ref,smart_link_id,touched_at
                 ) VALUES($1,$2,'invite','owned_reply',$3,$4,$5)",
            )
            .bind(ws)
            .bind(prospect_id)
            .bind(source_ref)
            .bind(link_id)
            .bind(touched_at)
            .execute(&pool)
            .await
            .expect("touch");
        }

        let visitor = Uuid::now_v7();
        let click_only_visitor = Uuid::now_v7();
        for (link_id, visitor_id) in [
            (link_good, visitor),
            (link_click_only, click_only_visitor),
        ] {
            sqlx::query(
                "INSERT INTO click_events(
                     workspace_id,smart_link_id,anonymous_visitor_id,occurred_at
                 ) VALUES($1,$2,$3,$4)",
            )
            .bind(ws)
            .bind(link_id)
            .bind(visitor_id)
            .bind(now - time::Duration::hours(19))
            .execute(&pool)
            .await
            .expect("click");
        }

        let fan = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
             VALUES($1,$2,'proof-person@example.test','active',$3)",
        )
        .bind(fan)
        .bind(ws)
        .bind(now - time::Duration::hours(18))
        .execute(&pool)
        .await
        .expect("fan");
        sqlx::query(
            "INSERT INTO fan_identifiers(
                 workspace_id,fan_id,kind,value,source,verified_at
             ) VALUES($1,$2,'email','proof-person@example.test','test',$3)",
        )
        .bind(ws)
        .bind(fan)
        .bind(now - time::Duration::hours(18))
        .execute(&pool)
        .await
        .expect("fan id");
        let converted_at = now - time::Duration::hours(18);
        sqlx::query(
            "INSERT INTO fan_acquisition_events(
                 workspace_id,fan_id,anonymous_visitor_id,source,request_id,occurred_at
             ) VALUES($1,$2,$3,'public_signup','proof-request',$4)",
        )
        .bind(ws)
        .bind(fan)
        .bind(visitor)
        .bind(converted_at)
        .execute(&pool)
        .await
        .expect("arrival");
        sqlx::query(
            "UPDATE fan_prospects
             SET status='converted',linked_fan_id=$3,updated_at=$4
             WHERE workspace_id=$1 AND id=$2",
        )
        .bind(ws)
        .bind(converted_prospect)
        .bind(fan)
        .bind(converted_at)
        .execute(&pool)
        .await
        .expect("link prospect");

        // Two devices are still one Signal fan.
        for n in 0..2 {
            sqlx::query(
                "INSERT INTO fan_push_endpoints(
                     workspace_id,fan_id,installation_id,transport,endpoint_address,
                     active,created_at,last_seen_at
                 ) VALUES($1,$2,$3,'android_fcm',$4,true,$5,$5)",
            )
            .bind(ws)
            .bind(fan)
            .bind(format!("proof-install-{n}"))
            .bind(format!("proof-endpoint-{n}"))
            .bind(now - time::Duration::hours(17))
            .execute(&pool)
            .await
            .expect("endpoint");
        }

        // Separate action-owned acquisition rail.
        let decision = Uuid::now_v7();
        let action = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO autopilot_decisions(
                 id,workspace_id,decision_key,context,subject_kind,subject_id,
                 decision_kind,confidence_basis_points,disposition,reason,
                 input_snapshot,policy_snapshot,recommendation,trace_id
             ) VALUES($1,$2,$3,'growth_metrics','target_community',$4,
                      'auto_execute',9000,'auto_execute','proof',
                      '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
        )
        .bind(decision)
        .bind(ws)
        .bind(format!("proof-{decision}"))
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .expect("decision");
        sqlx::query(
            "INSERT INTO autopilot_actions(
                 id,workspace_id,decision_id,context,action_kind,subject_kind,subject_id,
                 idempotency_key,payload,status,action_class,finished_at
             ) VALUES($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                      $4,$5,'{}'::jsonb,'succeeded','third_party',$6)",
        )
        .bind(action)
        .bind(ws)
        .bind(decision)
        .bind(Uuid::now_v7())
        .bind(format!("proof-action-{action}"))
        .bind(now - time::Duration::hours(16))
        .execute(&pool)
        .await
        .expect("action");

        let action_fan = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
             VALUES($1,$2,'action-fan@example.test','active',$3)",
        )
        .bind(action_fan)
        .bind(ws)
        .bind(now - time::Duration::hours(15))
        .execute(&pool)
        .await
        .expect("action fan");
        let action_converted_at = now - time::Duration::hours(15);
        sqlx::query(
            "INSERT INTO fan_provenance_events(
                 workspace_id,fan_id,event_kind,channel,source_target,action_id,
                 attribution_method,attribution_confidence,occurred_at
             ) VALUES($1,$2,'conversion','instagram','proof-action',$3,
                      'last_tracked_click',1.0,$4)",
        )
        .bind(ws)
        .bind(action_fan)
        .bind(action)
        .bind(action_converted_at)
        .execute(&pool)
        .await
        .expect("provenance");
        sqlx::query(
            "INSERT INTO fan_push_endpoints(
                 workspace_id,fan_id,installation_id,transport,endpoint_address,
                 active,created_at,last_seen_at
             ) VALUES($1,$2,'action-install','android_fcm','action-endpoint',true,$3,$3)",
        )
        .bind(ws)
        .bind(action_fan)
        .bind(now - time::Duration::hours(14))
        .execute(&pool)
        .await
        .expect("action endpoint");

        // A completely unrelated Signal fan must not leak into either proof rail.
        let unrelated = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status)
             VALUES($1,$2,'unrelated@example.test','active')",
        )
        .bind(unrelated)
        .bind(ws)
        .execute(&pool)
        .await
        .expect("unrelated fan");
        sqlx::query(
            "INSERT INTO fan_push_endpoints(
                 workspace_id,fan_id,installation_id,transport,endpoint_address,active
             ) VALUES($1,$2,'unrelated-install','android_fcm','unrelated-endpoint',true)",
        )
        .bind(ws)
        .bind(unrelated)
        .execute(&pool)
        .await
        .expect("unrelated endpoint");

        let proof = load_fan_acquisition_proof(&pool, ws, since, now)
            .await
            .expect("proof");
        assert_eq!(proof["fan_scout"]["prospects_discovered"], 2);
        assert_eq!(proof["fan_scout"]["provider_confirmed_invites"], 2);
        assert_eq!(proof["fan_scout"]["exact_converted_fans"], 1);
        assert_eq!(proof["fan_scout"]["exact_signal_fans"], 1);
        assert_eq!(proof["actions"]["exact_canonical_fans"], 1);
        assert_eq!(proof["actions"]["exact_signal_fans"], 1);
    }
}
