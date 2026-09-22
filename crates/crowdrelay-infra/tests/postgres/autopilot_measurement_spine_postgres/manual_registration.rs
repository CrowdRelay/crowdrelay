/// I: a post the operator published by hand carries the same measurement
/// facts as one the executor published — the reach row the credit allocator
/// divides by, and the experiment assignment moving `dispatched` →
/// `executed`. The reddit path filed both from the start; the social and
/// chat registrations used to write neither, leaving a treated unit the
/// model could never count as treated.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn i_manual_social_and_chat_posts_file_reach_and_execute_the_assignment() {
    let f = setup().await.expect("fixture");

    let dispatched_assignment = |f: &Fixture, action_id: uuid::Uuid| {
        let pool = f.pool.clone();
        let workspace_id = f.workspace_id.into_uuid();
        async move {
            // Assignments FK to a persisted design, so the unit's experiment
            // identity must exist before the arm does.
            let design_id = uuid::Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO experiment_designs
                   (experiment_uuid, workspace_id, intervention_key,
                    logical_cycle_key, unit_kind, holdout_probability,
                    interference_policy)
                   VALUES ($1,$2,$3,'cycle-i','audience',0.0,'none')"#,
            )
            .bind(design_id)
            .bind(workspace_id)
            .bind(format!("intervention-{action_id}"))
            .execute(&pool)
            .await
            .expect("design");
            sqlx::query(
                r#"INSERT INTO experiment_assignments
                   (id, workspace_id, unit_id, unit_kind, arm, propensity,
                    intended_template_id, action_id, execution_status,
                    experiment_uuid)
                   VALUES ($1,$2,$3,'audience','treatment',0.9,
                           'test-template',$4,'dispatched',$5)"#,
            )
            .bind(format!("assign-{action_id}"))
            .bind(workspace_id)
            .bind(format!("unit-{action_id}"))
            .bind(action_id)
            .bind(design_id)
            .execute(&pool)
            .await
            .expect("assignment");
        }
    };

    // — social, facebook arm —
    let action_id = insert_dispatch(&f, "manual-social:i", f.now).await;
    dispatched_assignment(&f, action_id).await;
    let post_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO social_posts
           (id, workspace_id, action_id, platform, content, status)
           VALUES ($1,$2,$3,'facebook','{"text":"hi"}'::jsonb,'awaiting_manual_post')"#,
    )
    .bind(post_id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("social draft");
    crowdrelay_infra::fanbase::register_manual_social_post(
        &f.pool,
        f.workspace_id.into_uuid(),
        post_id,
        "https://www.facebook.com/123",
        Some("fb-123"),
    )
    .await
    .expect("social registration");

    // — telegram arm —
    let tg_action = insert_dispatch(&f, "manual-telegram:i", f.now).await;
    dispatched_assignment(&f, tg_action).await;
    let tg_post = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO telegram_posts
           (id, workspace_id, action_id, channel, status)
           VALUES ($1,$2,$3,'-100123','awaiting_manual_post')"#,
    )
    .bind(tg_post)
    .bind(f.workspace_id.into_uuid())
    .bind(tg_action)
    .execute(&f.pool)
    .await
    .expect("telegram draft");
    crowdrelay_infra::fanbase::register_manual_telegram_post(
        &f.pool,
        f.workspace_id.into_uuid(),
        tg_post,
        42,
    )
    .await
    .expect("telegram registration");

    // — discord arm —
    let dc_action = insert_dispatch(&f, "manual-discord:i", f.now).await;
    dispatched_assignment(&f, dc_action).await;
    let dc_post = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO discord_posts
           (id, workspace_id, action_id, channel_id, status)
           VALUES ($1,$2,$3,'1234567890','awaiting_manual_post')"#,
    )
    .bind(dc_post)
    .bind(f.workspace_id.into_uuid())
    .bind(dc_action)
    .execute(&f.pool)
    .await
    .expect("discord draft");
    crowdrelay_infra::fanbase::register_manual_discord_post(
        &f.pool,
        f.workspace_id.into_uuid(),
        dc_post,
        "msg-1",
    )
    .await
    .expect("discord registration");

    for (action_id, channel, kind) in [
        (action_id, "social_post", "platform_audience"),
        (tg_action, "telegram_post", "telegram_channel"),
        (dc_action, "discord_post", "discord_channel"),
    ] {
        let (reach_rows, recipient_kind) = sqlx::query_as::<_, (i64, String)>(
            "SELECT count(*), max(recipient_kind) FROM reach_events \
                 WHERE workspace_id = $1 AND action_id = $2 AND channel = $3",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(action_id)
        .bind(channel)
        .fetch_one(&f.pool)
        .await
        .expect("reach rows");
        assert_eq!(
            reach_rows, 1,
            "a manual {channel} publication files exactly one reach row"
        );
        assert_eq!(recipient_kind, kind, "{channel} names its own audience");

        let status: String = sqlx::query_scalar(
            "SELECT execution_status FROM experiment_assignments \
                 WHERE workspace_id = $1 AND action_id = $2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("assignment status");
        assert_eq!(
            status, "executed",
            "a manual {channel} publication proves the intervention ran"
        );
    }

    // Re-registering the same post refuses with its status and files no
    // second reach row — the idempotency the reddit path already guarantees.
    let repeat = crowdrelay_infra::fanbase::register_manual_social_post(
        &f.pool,
        f.workspace_id.into_uuid(),
        post_id,
        "https://www.facebook.com/123",
        Some("fb-123"),
    )
    .await;
    assert!(matches!(
        repeat,
        Err(crowdrelay_infra::fanbase::ManualContentPostError::NotAwaitingPublication { .. })
    ));
    let reach_rows_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reach_events \
             WHERE workspace_id = $1 AND action_id = $2 AND channel = 'social_post'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("reach rows after a repeat registration");
    assert_eq!(reach_rows_after, 1, "a refused re-registration adds no reach");
}
