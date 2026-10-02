// Executes `load_trace_timeline` against a real schema. This endpoint
// 500'd on every call in production — an arm named a column
// (`created_at`) that `autopilot_decisions` never had — because
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
        .bind(trace_id)
        .execute(&pool)
        .await
        .expect("decision");
        sqlx::query(
            r#"INSERT INTO autopilot_actions
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
            r#"INSERT INTO autopilot_measurements
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
            r#"INSERT INTO growth_evidence
               (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
                channel, estimated_reach, treatment, propensity, converted,
                predicted_fans, predicted_signal_installs, context, evidence_quality,
                observed_incremental_fans, resolved_at, observed_metrics)
               VALUES ($1,$2,'opp',now(),'recipient','reddit_post',100,'treatment',
                       0.9,false,2.0,1.0,'{}'::jsonb,'observational',3.0,now(),
                       '{"incremental_fan_growth":3.0,"harm:complaints":0}'::jsonb)"#,
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

        // Each observed_metrics key lands as its own metric_observation event —
        // what the loop measured, beside the row that resolved.
        let observations: Vec<&TraceTimelineEvent> = events
            .iter()
            .filter(|e| e.source == "metric_observation")
            .collect();
        assert_eq!(
            observations.len(),
            2,
            "expected one event per observed_metrics key: {observations:?}"
        );
        let fan_metric = observations
            .iter()
            .find(|e| e.kind == "incremental_fan_growth")
            .expect("fan-growth observation");
        assert_eq!(fan_metric.state.as_deref(), Some("3.0"));
        assert_eq!(fan_metric.certainty, "FACT");
        assert_eq!(fan_metric.action_id.as_deref(), Some(action_id.to_string().as_str()));
    }

    /// The plan's one-trace proof: one real person walked from what the band read
    /// about them, through the thing it said and the tracked link it carried, to
    /// the fan they became — every step under the single `trace_id` of the touch,
    /// the arrival labelled as the inference it is, and a touch that was NOT the
    /// one whose link they clicked carrying none of it.
    #[tokio::test]
    async fn trace_walks_prospect_to_touch_to_link_to_fan() {
        use crowdrelay_domain::fan_prospect::{ObservationKind, ProspectSource};
        use crowdrelay_infra::fan_prospects::{
            ObserveOutcome, ObservedPerson, TouchKind, TouchReceipt, attribute_conversions,
            observe, record_touch,
        };
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
        let w = workspace_id.into_uuid();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
            .bind(w)
            .bind(format!("scout-trace-{}", w.simple()))
            .bind("Scout Trace")
            .execute(&pool)
            .await
            .expect("workspace");
        let now = OffsetDateTime::now_utc();
        let ObserveOutcome::Created { prospect_id } = observe(
            &pool,
            w,
            &ObservedPerson {
                source: ProspectSource::OwnComments,
                platform: "instagram",
                platform_user_id: None,
                handle: Some("kuba_metal"),
                display_identity: "kuba_metal",
                display_name: None,
                profile_url: None,
                kind: ObservationKind::AskedAboutShow,
                source_ref: "comment-1",
                source_url: None,
                observed_at: now - time::Duration::days(3),
                evidence: "Kiedy gracie Wrocław?",
                confidence_basis_points: 8_000,
            },
        )
        .await
        .expect("observe") else {
            panic!("created");
        };
        let link: Uuid = sqlx::query_scalar(
            "INSERT INTO smart_links (workspace_id, slug, destination_url, active)
             VALUES ($1,$2,'https://band.example/signal',true) RETURNING id",
        )
        .bind(w)
        .bind(format!("t-{}", Uuid::now_v7().simple()))
        .fetch_one(&pool)
        .await
        .expect("link");
        // Two touches: an earlier plain answer, then the invitation that carried the link.
        record_touch(
            &pool,
            w,
            &TouchReceipt {
                prospect_id,
                kind: TouchKind::Engage,
                source: "owned_reply",
                source_ref: "reply-1",
                smart_link_id: None,
                touched_at: now - time::Duration::days(2),
            },
        )
        .await
        .expect("engage");
        record_touch(
            &pool,
            w,
            &TouchReceipt {
                prospect_id,
                kind: TouchKind::Invite,
                source: "owned_reply",
                source_ref: "reply-2",
                smart_link_id: Some(link),
                touched_at: now - time::Duration::days(1),
            },
        )
        .await
        .expect("invite");
        let visitor = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO click_events (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(w)
        .bind(link)
        .bind(visitor)
        .bind(now - time::Duration::hours(20))
        .execute(&pool)
        .await
        .expect("click");
        let fan = Uuid::now_v7();
        sqlx::query("INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,'kuba@fan.test','active')")
            .bind(fan)
            .bind(w)
            .execute(&pool)
            .await
            .expect("fan");
        sqlx::query(
            "INSERT INTO fan_acquisition_events (workspace_id, fan_id, anonymous_visitor_id, source, request_id, occurred_at)
             VALUES ($1,$2,$3,'public_signup','req-trace',$4)",
        )
        .bind(w)
        .bind(fan)
        .bind(visitor)
        .bind(now - time::Duration::hours(19))
        .execute(&pool)
        .await
        .expect("arrival");
        assert_eq!(
            attribute_conversions(&pool, w, now, 10).await.expect("attribute"),
            1
        );

        let trace_of = |source_ref: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT trace_id FROM fan_prospect_touches WHERE workspace_id=$1 AND source_ref=$2",
                )
                .bind(w)
                .bind(source_ref)
                .fetch_one(&pool)
                .await
                .expect("trace")
            }
        };
        let ops = OpsState::new(workspace_id, pool.clone(), Duration::from_secs(10));

        // The invitation's trace carries the whole path, in order, with honest labels.
        let events = load_trace_timeline(&ops, &trace_of("reply-2").await)
            .await
            .expect("trace query must execute against a real schema");
        let path: Vec<(&str, &str, &str)> = events
            .iter()
            .map(|e| (e.source.as_str(), e.kind.as_str(), e.certainty.as_str()))
            .collect();
        assert_eq!(
            path,
            [
                ("scout_observation", "asked_about_show", "FACT"),
                ("scout_touch", "invite", "FACT"),
                ("scout_arrival", "public_signup", "INFERENCE"),
                ("scout_conversion", "prospect_converted", "FACT"),
            ],
            "{events:?}"
        );
        assert_eq!(
            events.iter().find(|e| e.source == "scout_conversion").and_then(|e| e.state.as_deref()),
            Some("active")
        );
        // No stranger's words ride the trace: kinds only.
        assert!(!format!("{events:?}").contains("Wrocław"));

        // The earlier plain answer did not carry the link: it has its own trace
        // with the observation and the touch, and neither the arrival nor the
        // conversion is claimed by it.
        let earlier = load_trace_timeline(&ops, &trace_of("reply-1").await)
            .await
            .expect("trace");
        let sources: Vec<&str> = earlier.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, ["scout_observation", "scout_touch"], "{earlier:?}");
    }

    /// The attention board's band notices read the escalation lane's durable
    /// record — deduplicated per subject, prefix stripped, delivery state
    /// reported rather than trusted.
    #[tokio::test]
    async fn band_notices_dedupe_per_subject_and_report_delivery() {
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
            .bind(format!("notices-{}", workspace_id.into_uuid().simple()))
            .bind("Notice Tests")
            .execute(&pool)
            .await
            .expect("workspace");

        let event_id = Uuid::now_v7();
        // The same task escalation raised twice for one show is one notice —
        // the newer raise wins, and a second row would only ever say "still
        // owed" about the same thing.
        for _ in 0..2 {
            sqlx::query(
                "INSERT INTO outbox_events (workspace_id, event_type, payload, status, delivered_at)
                 VALUES ($1,'crowdrelay.show.task_attention_required',$2,'delivered',now())",
            )
            .bind(workspace_id.into_uuid())
            .bind(serde_json::json!({
                "event_id": event_id,
                "task": "post_show_report",
                "action_id": Uuid::now_v7(),
            }))
            .execute(&pool)
            .await
            .expect("task notice");
        }
        sqlx::query(
            "INSERT INTO outbox_events (workspace_id, event_type, payload, status)
             VALUES ($1,'crowdrelay.release.r3_report_due',$2,'pending')",
        )
        .bind(workspace_id.into_uuid())
        .bind(serde_json::json!({
            "release_id": Uuid::now_v7(),
            "title": "Demo EP",
            "action_id": Uuid::now_v7(),
        }))
        .execute(&pool)
        .await
        .expect("release notice");
        // A lane that is not the band-facing one must not surface here.
        sqlx::query(
            "INSERT INTO outbox_events (workspace_id, event_type, payload, status)
             VALUES ($1,'crowdrelay.play.step_requested',$2,'pending')",
        )
        .bind(workspace_id.into_uuid())
        .bind(serde_json::json!({"action_id": Uuid::now_v7()}))
        .execute(&pool)
        .await
        .expect("non-escalation event");

        let ops = OpsState::new(workspace_id, pool, Duration::from_secs(10));
        let notices = load_band_notices(&ops)
            .await
            .expect("notices query must execute against a real schema");

        assert_eq!(
            notices.len(),
            2,
            "two subjects, not three rows: {notices:?}"
        );
        let task = notices
            .iter()
            .find(|n| n.kind == "show.task_attention_required")
            .expect("task notice");
        assert!(task.delivered, "a delivered emit reports delivered");
        assert_eq!(
            task.detail["task"].as_str(),
            Some("post_show_report"),
            "the notice carries what the escalation was about"
        );
        let release = notices
            .iter()
            .find(|n| n.kind == "release.r3_report_due")
            .expect("release notice");
        assert!(
            !release.delivered,
            "an undelivered escalation is the one the board exists for"
        );
    }
}
