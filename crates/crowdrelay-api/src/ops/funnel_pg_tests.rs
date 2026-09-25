// Executes `load_funnel` against a real schema — the funnel is six queries
// over six tables, and the runtime-query surface means a wrong column name
// compiles clean. Seeding one row per stage is the cheapest way to prove
// every query in the read model answers what its shape claims.
//
// The file lives under `ops/` and ends in `_tests.rs` deliberately: the
// decision-trace contract gate scans `src/**/*.rs` for decision-table
// writers and exempts only `tests.rs`/`*_tests.rs` names — the fixture
// inserts below are test scaffolding, not a production write path.

#[cfg(test)]
mod funnel_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn funnel_counts_proposals_sends_replies_and_fans_per_channel() {
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
            .bind(format!("funnel-{}", workspace_id.into_uuid().simple()))
            .bind("Funnel Tests")
            .execute(&pool)
            .await
            .expect("workspace");

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

        async fn seed_action(
            pool: &PgPool,
            workspace_id: WorkspaceId,
            decision_id: Uuid,
            context: &str,
            action_kind: &str,
            status: &str,
            lapsed: bool,
        ) {
            let action_id = Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload,
                    status, action_class, trace_id, last_error_kind,
                    finished_at)
                   VALUES ($1,$2,$3,$4,$5,'target_community',$6,$7,'{}'::jsonb,
                           $8,'third_party',$9,
                           CASE WHEN $10 THEN 'approval_expired' END,
                           CASE WHEN $8 IN ('succeeded','failed','cancelled')
                                THEN now() END)"#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(context)
            .bind(action_kind)
            .bind(Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(status)
            .bind(Uuid::now_v7())
            .bind(lapsed)
            .execute(pool)
            .await
            .expect("action");
        }

        // One succeeded outward pitch, one parked beacon ask, one lapsed
        // outreach ask, one internal bookkeeping success.
        seed_action(
            &pool,
            workspace_id,
            decision_id,
            "outreach",
            "outreach.request",
            "succeeded",
            false,
        )
        .await;
        seed_action(
            &pool,
            workspace_id,
            decision_id,
            "beacon",
            "beacon.outreach.request",
            "awaiting_approval",
            false,
        )
        .await;
        seed_action(
            &pool,
            workspace_id,
            decision_id,
            "outreach",
            "outreach.request",
            "cancelled",
            true,
        )
        .await;
        seed_action(
            &pool,
            workspace_id,
            decision_id,
            "content_supply",
            "team.assignment.email",
            "succeeded",
            false,
        )
        .await;

        // Transport: one delivered outward send, one dead, one delivered
        // internal mailer.
        async fn seed_outbox(pool: &PgPool, workspace_id: WorkspaceId, event_type: &str, status: &str) {
            sqlx::query(
                r#"INSERT INTO outbox_events
                   (id, workspace_id, event_type, payload, status, delivered_at,
                    dead_at)
                   VALUES ($1,$2,$3,'{}'::jsonb,$4,
                           CASE WHEN $4 = 'delivered' THEN now() END,
                           CASE WHEN $4 = 'dead' THEN now() END)"#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id.into_uuid())
            .bind(event_type)
            .bind(status)
            .execute(pool)
            .await
            .expect("outbox");
        }
        seed_outbox(
            &pool,
            workspace_id,
            "crowdrelay.outreach.requested",
            "delivered",
        )
        .await;
        seed_outbox(&pool, workspace_id, "crowdrelay.outreach.requested", "dead").await;
        seed_outbox(
            &pool,
            workspace_id,
            "crowdrelay.team.assignment_email_requested",
            "delivered",
        )
        .await;

        // One target with a positive inbound reply and one sheet-logged
        // send — the two halves the funnel keeps apart.
        let target_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO outreach_targets
               (id, workspace_id, target_kind, display_name, contact_email)
               VALUES ($1,$2,'radio','Funnel Radio','funnel@radio.example')"#,
        )
        .bind(target_id)
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await
        .expect("target");
        sqlx::query(
            r#"INSERT INTO outreach_interactions
               (workspace_id, target_id, direction, phase, disposition,
                source_key, occurred_at)
               VALUES ($1,$2,'inbound','reply','positive','reply-1', now())"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(target_id)
        .execute(&pool)
        .await
        .expect("reply");
        sqlx::query(
            r#"INSERT INTO outreach_interactions
               (workspace_id, target_id, direction, phase, disposition,
                source_key, occurred_at)
               VALUES ($1,$2,'outbound','initial','none','master:ORC-1', now())"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(target_id)
        .execute(&pool)
        .await
        .expect("sheet send");

        // A beacon campaign that heard back.
        let beacon_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO beacons (id, workspace_id, beacon_kind, display_name)
               VALUES ($1,$2,'promoter','Funnel Promoter')"#,
        )
        .bind(beacon_id)
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await
        .expect("beacon");
        let event_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO events
               (id, workspace_id, slug, title, starts_at, status, published_at)
               VALUES ($1,$2,$3,'Funnel Show', now() + interval '20 days',
                       'published', now())"#,
        )
        .bind(event_id)
        .bind(workspace_id.into_uuid())
        .bind(format!("funnel-{}", event_id.simple()))
        .execute(&pool)
        .await
        .expect("event");
        sqlx::query(
            r#"INSERT INTO beacon_campaigns
               (workspace_id, beacon_id, event_id, status, last_reply_disposition)
               VALUES ($1,$2,$3,'interested','interested')"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(beacon_id)
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("campaign");

        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,'active')",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("{}@fan.example", Uuid::now_v7().simple()))
        .execute(&pool)
        .await
        .expect("fan");

        let funnel = load_funnel(&pool, workspace_id.into_uuid())
            .await
            .expect("funnel");

        assert_eq!(funnel["window_days"].as_i64(), Some(28));
        // Outward contexts keep their own rows.
        assert_eq!(funnel["channels"]["outreach"]["succeeded"].as_i64(), Some(1));
        assert_eq!(funnel["channels"]["outreach"]["lapsed"].as_i64(), Some(1));
        assert_eq!(funnel["channels"]["beacon"]["awaiting"].as_i64(), Some(1));
        // Transport folded into the channel: 1 delivered + 1 dead outreach
        // send, and the internal mailer delivered under `internal`.
        assert_eq!(
            funnel["channels"]["outreach"]["sent"]["delivered"].as_i64(),
            Some(1)
        );
        assert_eq!(
            funnel["channels"]["outreach"]["sent"]["dead"].as_i64(),
            Some(1)
        );
        assert_eq!(
            funnel["channels"]["internal"]["sent"]["delivered"].as_i64(),
            Some(1)
        );
        // The mailer kind counts as internal wherever its context filed it.
        assert_eq!(
            funnel["channels"]["internal"]["mailer_succeeded"].as_i64(),
            Some(1)
        );
        // Replies per ledger.
        assert_eq!(
            funnel["replies"]["outreach"]["by_disposition"]["positive"]["all_time"].as_i64(),
            Some(1)
        );
        assert_eq!(funnel["replies"]["beacon"]["positive"].as_i64(), Some(1));
        // The sheet send is named, not merged into "sent".
        assert_eq!(
            funnel["sheet_logged_outreach_sends"]["all_time"].as_i64(),
            Some(1)
        );
        assert_eq!(funnel["fans"]["active"].as_i64(), Some(1));
        assert_eq!(funnel["fans"]["new_28d"].as_i64(), Some(1));
    }
}
