// The community executor's tests.
//
// `include!`d into `community_executor.rs` so they share that module's scope
// and can reach `claim_pending_actions`, which is private and is where the
// claim predicate lives — the predicate is the defect these tests exist for.
//
// Split out because the parent crossed the source-size ratchet, and because
// the fixture here has to insert a decision and an action (`decision_id` is
// NOT NULL on actions). That insert made the parent file read as a writer to
// the decision ledger in `test_decision_trace_contract_v1.py`, which is a
// true statement about a test fixture and a false one about the executor.

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker pointed at a real database, in the mode the test needs.
    ///
    /// `claim_pending_actions` is private and the claim predicate is the whole
    /// defect, so this lives in the crate's own test module rather than in
    /// `tests/` — the same arrangement the outbox, reminder and retention suites
    /// use for the same reason.
    async fn live_worker(manual_mode: bool) -> Option<(CommunityExecutorWorker, uuid::Uuid)> {
        let url = std::env::var("CROWDRELAY_COMMUNITY_TEST_DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url).await.expect("connect");
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .expect("migrate");
        let workspace_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Community executor test')",
        )
        .bind(workspace_id)
        .bind(format!("commex-{}", workspace_id.simple()))
        .execute(&pool)
        .await
        .expect("workspace");
        Some((
            CommunityExecutorWorker {
                pool,
                workspace_id: WorkspaceId::from_uuid(workspace_id),
                http_client: Arc::new(RwLock::new(reqwest::Client::new())),
                poll_interval: POLL_INTERVAL,
                operation_timeout: Duration::from_secs(5),
                public_origin: "https://example.test".to_owned(),
                manual_mode,
                agent_service_url: "http://agent-service:8095".to_owned(),
                agent_service_auth_key: Some("key".to_owned()),
                env_proxy_url: None,
            },
            workspace_id,
        ))
    }

    async fn seed_draft(
        worker: &CommunityExecutorWorker,
        workspace_id: uuid::Uuid,
        subreddit: &str,
        status: &str,
    ) -> uuid::Uuid {
        let post_id = uuid::Uuid::now_v7();
        // `community_posts.action_id` has a foreign key, and the action needs a
        // decision, so the whole chain is seeded rather than faked.
        let decision_id = uuid::Uuid::now_v7();
        let action_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO viryaos_autopilot_decisions \
               (id, workspace_id, decision_key, context, subject_kind, subject_id, \
                decision_kind, confidence_basis_points, disposition, reason, \
                input_snapshot, policy_snapshot, recommendation, trace_id) \
             VALUES ($1,$2,$3,'growth_metrics','target_community',$4, \
                     'auto_execute',9000,'auto_execute','test', \
                     '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
        )
        .bind(decision_id)
        .bind(workspace_id)
        .bind(format!("key-{action_id}"))
        .bind(uuid::Uuid::now_v7())
        .execute(&worker.pool)
        .await
        .expect("decision");
        sqlx::query(
            "INSERT INTO viryaos_autopilot_actions \
               (id, workspace_id, decision_id, context, action_kind, subject_kind, \
                subject_id, idempotency_key, payload, status, action_class, \
                finished_at) \
             VALUES ($1,$2,$3,'growth_metrics','community.engage.request', \
                     'target_community',$4,$5,'{}'::jsonb,'succeeded','third_party', \
                     now())",
        )
        .bind(action_id)
        .bind(workspace_id)
        .bind(decision_id)
        .bind(uuid::Uuid::now_v7())
        .bind(format!("idem-{action_id}"))
        .execute(&worker.pool)
        .await
        .expect("action");
        sqlx::query(
            "INSERT INTO community_posts \
               (id, workspace_id, action_id, target_id, subreddit, title, body, status) \
             VALUES ($1, $2, $3, $4, $5, 'title', 'body', $6)",
        )
        .bind(post_id)
        .bind(workspace_id)
        .bind(action_id)
        .bind(uuid::Uuid::now_v7())
        .bind(subreddit)
        .bind(status)
        .execute(&worker.pool)
        .await
        .expect("draft");
        post_id
    }

    async fn status_of(worker: &CommunityExecutorWorker, post_id: uuid::Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM community_posts WHERE id = $1")
            .bind(post_id)
            .fetch_one(&worker.pool)
            .await
            .expect("status")
    }

    /// Turning autopilot on has to move the drafts manual mode wrote.
    ///
    /// `awaiting_manual_post` was in no claim predicate, the parent action was
    /// already consumed, and the seven-day cooldown stopped the brain from
    /// drafting the same community again. Five ready drafts for communities of
    /// 2.6M and 1M members were stranded permanently by a missing status in one
    /// WHERE clause — the operator enabled every switch and the count of
    /// published posts stayed at zero.
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_COMMUNITY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn publishing_adopts_the_drafts_manual_mode_wrote() {
        let Some((worker, workspace_id)) = live_worker(false).await else {
            return;
        };
        let waiting = seed_draft(&worker, workspace_id, "r/adopted", "awaiting_manual_post").await;
        let claimed = worker.claim_pending_actions().await.expect("claim");
        assert!(
            claimed.iter().any(|action| action.id == waiting),
            "an automatic executor must adopt a draft that was waiting for a person"
        );
        assert_eq!(
            claimed
                .iter()
                .find(|action| action.id == waiting)
                .map(|action| action.claimed_from.as_str()),
            Some("awaiting_manual_post"),
            "the prior status must travel, so an adoption can be announced before \
             it posts"
        );
    }

    /// And manual mode must not adopt them, or there is no manual mode.
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_COMMUNITY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn manual_mode_leaves_its_own_drafts_alone() {
        let Some((worker, workspace_id)) = live_worker(true).await else {
            return;
        };
        let waiting =
            seed_draft(&worker, workspace_id, "r/untouched", "awaiting_manual_post").await;
        let claimed = worker.claim_pending_actions().await.expect("claim");
        assert!(
            !claimed.iter().any(|action| action.id == waiting),
            "a draft written for a person must stay theirs while manual mode is on"
        );
        assert_eq!(status_of(&worker, waiting).await, "awaiting_manual_post");
    }

    /// Our own rate limit must defer a draft, never discard it.
    ///
    /// `claim_pending_actions` checks the 24h cap before it claims anything, so
    /// the common case is already safe: a draft waits as `pending` rather than
    /// being claimed and then refused. This covers the race the claim guard
    /// cannot — the cap becoming reached between the claim and the attempt,
    /// which is reachable because the two are separate transactions.
    ///
    /// It mattered because the refusal called `mark_failed`, which set the draft
    /// to `failed` AND propagated failure to the parent autopilot action. A
    /// draft nobody attempted would be discarded, and the brain would learn that
    /// a post it wrote had failed. `rate_limited` with a retry window already
    /// existed and was used only for Reddit's own rate limit.
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_COMMUNITY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn the_rate_limit_defers_a_draft_instead_of_failing_it() {
        let Some((worker, workspace_id)) = live_worker(false).await else {
            return;
        };
        // Claim first, while the cap is clear.
        let held = seed_draft(&worker, workspace_id, "r/held", "pending").await;
        let action = worker
            .claim_pending_actions()
            .await
            .expect("claim")
            .into_iter()
            .find(|action| action.id == held)
            .expect("the held draft must be claimable while the cap is clear");

        // Now the cap is reached — the race the claim guard cannot cover.
        let posted = seed_draft(&worker, workspace_id, "r/already", "posted").await;
        sqlx::query("UPDATE community_posts SET posted_at = now() WHERE id = $1")
            .bind(posted)
            .execute(&worker.pool)
            .await
            .expect("mark posted");
        assert!(worker.rate_limit_reached().await.expect("limit"));

        worker.process_action(&action).await.expect("process");
        assert_eq!(
            status_of(&worker, held).await,
            "rate_limited",
            "our own cap saying 'not yet' must not discard a publishable draft"
        );
        let retry_at: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT rate_limited_until FROM community_posts WHERE id = $1")
                .bind(held)
                .fetch_one(&worker.pool)
                .await
                .expect("retry window");
        assert!(
            retry_at.is_some(),
            "a deferred draft must carry the time to reconsider it"
        );
        // And the parent action must not be marked failed: nothing failed.
        let action_status: String = sqlx::query_scalar(
            "SELECT a.status FROM viryaos_autopilot_actions a \
             JOIN community_posts c ON c.action_id = a.id WHERE c.id = $1",
        )
        .bind(held)
        .fetch_one(&worker.pool)
        .await
        .expect("action status");
        assert_eq!(
            action_status, "succeeded",
            "a deferred post must not teach the brain that its post failed"
        );
    }

    fn worker(origin: &str) -> CommunityExecutorWorker {
        CommunityExecutorWorker {
            pool: PgPool::connect_lazy("postgres://invalid/invalid").expect("lazy pool"),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::nil()),
            http_client: Arc::new(RwLock::new(reqwest::Client::new())),
            poll_interval: POLL_INTERVAL,
            operation_timeout: Duration::from_secs(5),
            public_origin: origin.to_owned(),
            manual_mode: true,
            agent_service_url: "http://agent-service:8095".to_owned(),
            agent_service_auth_key: None,
            env_proxy_url: None,
        }
    }

    #[tokio::test]
    async fn asking_for_auto_post_alone_still_drafts() {
        // The community auto-post flag is one of two switches, and on its own
        // it does not publish to Reddit. A caller asking for automatic posting
        // without `CROWDRELAY_REDDIT_WRITE_ENABLED` still gets a drafting
        // executor, because the login session this would post through is the
        // only Reddit access the growth loop has for reading.
        //
        // Renamed from `reddit_stays_read_only_however_the_worker_is_
        // constructed`: that name asserted an invariant that no longer holds,
        // and a test whose name overstates what it proves is worse than no
        // test. What it actually pins — and pinned before — is that this
        // argument alone is not enough.
        let worker = CommunityExecutorWorker::new(
            PgPool::connect_lazy("postgres://invalid/invalid").expect("lazy pool"),
            WorkspaceId::from_uuid(uuid::Uuid::nil()),
            Duration::from_secs(5),
            false, // caller asks for automatic posting
            None,
            "http://agent-service:8095".to_owned(),
            Some("key".to_owned()),
        )
        .expect("worker");
        assert!(
            worker.manual_mode,
            "one switch is not enough: CROWDRELAY_REDDIT_WRITE_ENABLED is also required"
        );
        assert!(CommunityExecutorWorker::reddit_is_read_only());
    }

    #[tokio::test]
    async fn a_rooted_path_resolves_against_our_own_origin() {
        let worker = worker("https://virya.music");
        let body = worker.build_post_body("hello", Some("/l/spring-tour"));
        assert_eq!(body.as_ref(), "hello\n\nhttps://virya.music/l/spring-tour");
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_origin_does_not_double_up() {
        let worker = worker("https://virya.music/");
        let body = worker.build_post_body("hello", Some("/l/x"));
        assert_eq!(body.as_ref(), "hello\n\nhttps://virya.music/l/x");
    }

    #[tokio::test]
    async fn our_own_absolute_url_is_kept() {
        let worker = worker("https://virya.music");
        let body = worker.build_post_body("hello", Some("https://virya.music/l/x"));
        assert_eq!(body.as_ref(), "hello\n\nhttps://virya.music/l/x");
    }

    #[tokio::test]
    async fn a_hallucinated_domain_never_reaches_the_post() {
        // Both drafts production has produced carry a domain the model made
        // up. Sending fans to a stranger's site is the smaller half of the
        // problem; an unrelated outbound link in a promo post is what gets
        // the one account this channel has banned.
        let worker = worker("https://virya.music");
        for link in [
            "https://virya.com",
            "https://virya.com/smartlink",
            "http://example.com/anything",
        ] {
            let body = worker.build_post_body("hello", Some(link));
            assert_eq!(body.as_ref(), "hello", "{link} must not be appended");
        }
    }

    #[tokio::test]
    async fn a_lookalike_host_is_not_our_origin() {
        // `starts_with` would admit this, which is why the check compares
        // hosts rather than prefixes.
        let worker = worker("https://virya.music");
        let body = worker.build_post_body("hello", Some("https://virya.music.evil.example/l/x"));
        assert_eq!(body.as_ref(), "hello");
    }

    #[tokio::test]
    async fn a_link_that_is_neither_absolute_nor_rooted_is_dropped() {
        let worker = worker("https://virya.music");
        assert_eq!(
            worker.build_post_body("hello", Some("l/x")).as_ref(),
            "hello"
        );
        assert_eq!(
            worker.build_post_body("hello", Some("   ")).as_ref(),
            "hello"
        );
    }

    #[tokio::test]
    async fn no_link_leaves_the_body_untouched() {
        let worker = worker("https://virya.music");
        assert_eq!(worker.build_post_body("hello", None).as_ref(), "hello");
    }

    #[tokio::test]
    async fn host_comparison_ignores_case_and_a_trailing_dot() {
        let worker = worker("https://virya.music");
        let body = worker.build_post_body("hello", Some("https://VIRYA.MUSIC./l/x"));
        assert_eq!(body.as_ref(), "hello\n\nhttps://VIRYA.MUSIC./l/x");
    }

    /// Publishing needs both switches, and the Reddit-specific one is the
    /// second. `manual_mode` carries `CROWDRELAY_COMMUNITY_AUTO_POST` being
    /// off; this asserts the other half cannot be skipped.
    ///
    /// Exercised through the same expression the constructor uses rather than
    /// through the constructor, which would need a live pool. The point being
    /// pinned is the boolean rule, not the wiring.
    #[test]
    fn publishing_needs_both_switches() {
        for (auto_post_off, reddit_write, expect_manual) in [
            (true, false, true),  // neither
            (false, false, true), // community only
            (true, true, true),   // reddit only
            (false, true, false), // both — the only publishing case
        ] {
            let manual = auto_post_off || !reddit_write;
            assert_eq!(
                manual, expect_manual,
                "auto_post_off={auto_post_off} reddit_write={reddit_write}"
            );
        }
    }

    /// Unset, misspelled, or a fresh deployment all mean draft.
    #[test]
    fn reddit_write_is_off_unless_explicitly_enabled() {
        // The variable is absent in the test environment, which is the
        // default every deployment starts from.
        assert!(!reddit_write_enabled());
        assert!(CommunityExecutorWorker::reddit_is_read_only());
    }

    /// One post a day while autonomous posting is unproven. This is the
    /// number a moderator sees, so it is worth a test rather than a comment.
    #[test]
    fn the_daily_post_ceiling_stays_conservative() {
        assert_eq!(
            MAX_POSTS_PER_24H, 1,
            "raise this only after posts have survived a week"
        );
        assert_eq!(SUBREDDIT_COOLDOWN_DAYS, 7);
    }

    /// A transient failure must keep the draft and say nothing to the brain.
    ///
    /// Every failure used to call `mark_failed`, which set the draft to `failed`
    /// AND marked the parent autopilot action failed. The action is terminal once
    /// told and the brain will not draft the same community for seven days, so
    /// one invalid credential could consume every queued draft in a single cycle
    /// — and teach the brain that each of those posts had failed.
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_COMMUNITY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn a_transient_failure_defers_the_draft_and_spares_the_action() {
        let Some((worker, workspace_id)) = live_worker(false).await else {
            return;
        };
        let draft = seed_draft(&worker, workspace_id, "r/transient", "pending").await;
        worker
            .mark_transient_failure(draft, "error sending request")
            .await
            .expect("defer");
        assert_eq!(status_of(&worker, draft).await, "rate_limited");
        let action_status: String = sqlx::query_scalar(
            "SELECT a.status FROM viryaos_autopilot_actions a \
             JOIN community_posts c ON c.action_id = a.id WHERE c.id = $1",
        )
        .bind(draft)
        .fetch_one(&worker.pool)
        .await
        .expect("action status");
        assert_eq!(
            action_status, "succeeded",
            "a deferred post must not tell the brain that its post failed"
        );
    }

    /// And it must stop deferring eventually.
    ///
    /// A condition that has not resolved in `MAX_TRANSIENT_ATTEMPTS` attempts is
    /// not transient, so the draft is given up on and the ledger is corrected.
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_COMMUNITY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn an_exhausted_draft_fails_and_corrects_the_ledger() {
        let Some((worker, workspace_id)) = live_worker(false).await else {
            return;
        };
        let draft = seed_draft(&worker, workspace_id, "r/exhausted", "pending").await;
        sqlx::query("UPDATE community_posts SET attempts = $2 WHERE id = $1")
            .bind(draft)
            .bind(MAX_TRANSIENT_ATTEMPTS)
            .execute(&worker.pool)
            .await
            .expect("exhaust");
        worker
            .mark_transient_failure(draft, "error sending request")
            .await
            .expect("give up");
        assert_eq!(status_of(&worker, draft).await, "failed");
        let retry_at: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT rate_limited_until FROM community_posts WHERE id = $1")
                .bind(draft)
                .fetch_one(&worker.pool)
                .await
                .expect("retry window");
        assert_eq!(
            retry_at, None,
            "a draft given up on must not also carry a retry time"
        );
        let action_status: String = sqlx::query_scalar(
            "SELECT a.status FROM viryaos_autopilot_actions a \
             JOIN community_posts c ON c.action_id = a.id WHERE c.id = $1",
        )
        .bind(draft)
        .fetch_one(&worker.pool)
        .await
        .expect("action status");
        assert_eq!(
            action_status, "failed",
            "once the draft is given up on, the ledger must say so"
        );
    }
}
