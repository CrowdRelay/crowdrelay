// Executes the two post-queue loaders against a real schema — both fan out
// over four post tables, and the runtime-query surface means a wrong column
// name compiles clean. Seeding one row per lane per table is the cheapest
// way to prove every arm answers what its shape claims.
//
// The file lives under `ops/` and ends in `_tests.rs` deliberately: the
// decision-trace contract gate scans `src/**/*.rs` for decision-table
// writers and exempts only `tests.rs`/`*_tests.rs` names — the fixture
// inserts below are test scaffolding, not a production write path.

#[cfg(test)]
mod post_queues_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn the_queue_splits_machine_and_human_lanes_per_platform() {
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
            .bind(format!("queues-{}", workspace_id.into_uuid().simple()))
            .bind("Queue Tests")
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

        async fn seed_action(pool: &PgPool, workspace_id: WorkspaceId, decision_id: Uuid) -> Uuid {
            let action_id = Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO autopilot_actions
                   (id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload,
                    status, action_class, trace_id, finished_at)
                   VALUES ($1,$2,$3,'content_supply','agent.content.request',
                           'target_community',$4,$5,'{}'::jsonb,
                           'succeeded','third_party',$6, now())"#,
            )
            .bind(action_id)
            .bind(workspace_id.into_uuid())
            .bind(decision_id)
            .bind(Uuid::now_v7())
            .bind(format!("idem-{action_id}"))
            .bind(Uuid::now_v7())
            .execute(pool)
            .await
            .expect("action");
            action_id
        }

        // Every post-table row needs a parent action — one each.
        let mut actions = Vec::new();
        for _ in 0..9 {
            actions.push(seed_action(&pool, workspace_id, decision_id).await);
        }

        // Human lane: a Meta draft and an X draft, both awaiting a person.
        for (action, platform) in [(actions[0], "instagram"), (actions[1], "x")] {
            sqlx::query(
                "INSERT INTO social_posts (workspace_id, action_id, platform, content, status) VALUES ($1,$2,$3,'{}'::jsonb,'awaiting_manual_post')",
            )
            .bind(workspace_id.into_uuid())
            .bind(action)
            .bind(platform)
            .execute(&pool)
            .await
            .expect("social draft");
        }
        // Machine lane: a claimed Facebook send and one the machine failed.
        for (action, status) in [(actions[2], "posting"), (actions[3], "failed")] {
            sqlx::query(
                "INSERT INTO social_posts (workspace_id, action_id, platform, content, status) VALUES ($1,$2,'facebook','{}'::jsonb,$3)",
            )
            .bind(workspace_id.into_uuid())
            .bind(action)
            .bind(status)
            .execute(&pool)
            .await
            .expect("social send");
        }
        // A posted row belongs to neither queue — seed one to prove it is
        // not counted anywhere.
        sqlx::query(
            "INSERT INTO social_posts (workspace_id, action_id, platform, content, status, posted_at) VALUES ($1,$2,'instagram','{}'::jsonb,'posted', now())",
        )
        .bind(workspace_id.into_uuid())
        .bind(actions[4])
        .execute(&pool)
        .await
        .expect("social posted");

        // The non-social tables keep their channel names in both lanes:
        // a reddit draft (human) and a rate-limited telegram send (machine).
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, subreddit, title, body, status) VALUES ($1,$2,'metal','t','b','awaiting_manual_post')",
        )
        .bind(workspace_id.into_uuid())
        .bind(actions[5])
        .execute(&pool)
        .await
        .expect("community draft");
        sqlx::query(
            "INSERT INTO telegram_posts (workspace_id, action_id, channel, status) VALUES ($1,$2,'@virya','rate_limited')",
        )
        .bind(workspace_id.into_uuid())
        .bind(actions[6])
        .execute(&pool)
        .await
        .expect("telegram send");
        sqlx::query(
            "INSERT INTO discord_posts (workspace_id, action_id, channel_id, status) VALUES ($1,$2,'chan-1','awaiting_manual_post')",
        )
        .bind(workspace_id.into_uuid())
        .bind(actions[7])
        .execute(&pool)
        .await
        .expect("discord draft");
        // Community posts went beyond Reddit (0377): a forum draft must
        // report channel 'forum', not read as Reddit.
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status) VALUES ($1,$2,'forum','metal','t','b','awaiting_manual_post')",
        )
        .bind(workspace_id.into_uuid())
        .bind(actions[8])
        .execute(&pool)
        .await
        .expect("forum draft");

        let ws = workspace_id.into_uuid();
        let drafts = load_unpublished_drafts(&pool, ws).await.expect("manual lane");
        let lanes: Vec<(&str, i64)> = drafts
            .iter()
            .map(|row| (row.channel.as_str(), row.drafts))
            .collect();
        // Social drafts report their platform — Meta and X are separate
        // rows, not one "social" count.
        assert!(lanes.contains(&("instagram", 1)), "{lanes:?}");
        assert!(lanes.contains(&("x", 1)), "{lanes:?}");
        assert!(lanes.contains(&("reddit", 1)), "{lanes:?}");
        assert!(lanes.contains(&("forum", 1)), "{lanes:?}");
        assert!(lanes.contains(&("discord", 1)), "{lanes:?}");
        assert!(!lanes.iter().any(|(channel, _)| *channel == "facebook"), "{lanes:?}");

        // The forum draft is named, with where it goes; a policy-held Reddit
        // draft is counted but never offered as "post this now".
        let forum = drafts.iter().find(|row| row.channel == "forum").expect("forum");
        assert_eq!(forum.ready_to_post.len(), 1, "{forum:?}");
        assert_eq!(forum.ready_to_post[0].target, "metal");
        sqlx::query(
            "UPDATE community_posts SET error_message = 'held: moderators removed two or more of our posts' WHERE workspace_id = $1 AND platform = 'reddit'",
        )
        .bind(ws)
        .execute(&pool)
        .await
        .expect("hold reddit");
        let held = load_unpublished_drafts(&pool, ws).await.expect("manual lane");
        let reddit = held.iter().find(|row| row.channel == "reddit").expect("reddit");
        assert_eq!(reddit.drafts, 1);
        assert!(reddit.ready_to_post.is_empty(), "{reddit:?}");

        // A prepared YouTube capture comment is listed with the words to paste
        // and its tracked link, until a click proves it is up.
        let source_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO content_sources (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
             VALUES ($1,'video','youtube:aaaaaaaaaaa','Live at FLSS', now() - interval '1 day', now() + interval '30 days',
                     jsonb_build_object('fan_capture_draft_at', now(), 'fan_capture_draft_text', 'Join us\n\nhttps://b.example/l/cap',
                                        'fan_capture_link_slug', 'cap'))
             RETURNING id",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .expect("capture draft source");
        let listed = load_unpublished_drafts(&pool, ws).await.expect("manual lane");
        let youtube = listed.iter().find(|row| row.channel == "youtube").expect("youtube");
        assert_eq!(youtube.drafts, 1);
        assert_eq!(youtube.ready_to_post.len(), 1, "{youtube:?}");
        assert_eq!(youtube.ready_to_post[0].tracked_link.as_deref(), Some("/l/cap"));
        assert!(
            youtube.ready_to_post[0].draft_text.as_deref().is_some_and(|t| t.contains("Join us")),
            "{youtube:?}"
        );
        sqlx::query(
            "INSERT INTO smart_links (workspace_id, slug, destination_url, active) VALUES ($1,'cap','https://b.example/signal',true)",
        )
        .bind(ws)
        .execute(&pool)
        .await
        .expect("capture link");
        sqlx::query(
            "INSERT INTO click_events (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
             SELECT $1, id, gen_random_uuid(), now() FROM smart_links WHERE workspace_id = $1 AND slug = 'cap'",
        )
        .bind(ws)
        .execute(&pool)
        .await
        .expect("a click");
        let after = load_unpublished_drafts(&pool, ws).await.expect("manual lane");
        assert!(
            !after.iter().any(|row| row.channel == "youtube"),
            "a click means the comment is up; it is no longer waiting: {after:?}"
        );
        let _ = source_id;

        let automatic = load_automatic_queue(&pool, ws).await.expect("machine lane");
        let facebook = automatic
            .iter()
            .find(|row| row.channel == "facebook")
            .expect("facebook lane");
        assert_eq!(facebook.in_flight, 1, "{automatic:?}");
        assert_eq!(facebook.failed, 1, "{automatic:?}");
        let telegram = automatic
            .iter()
            .find(|row| row.channel == "telegram")
            .expect("telegram lane");
        assert_eq!(telegram.in_flight, 1, "{automatic:?}");
        assert_eq!(telegram.failed, 0, "{automatic:?}");
        // The posted row is in neither queue.
        assert!(!automatic.iter().any(|row| row.channel == "instagram"), "{automatic:?}");
        assert!(!automatic.iter().any(|row| row.channel == "reddit"), "{automatic:?}");
    }
}
