//! Video scorecards against a real Postgres.
//!
//! A card is only honest if every join is the video's own: the Analytics
//! split counts domains CrowdRelay touched and nothing else, a link chained
//! onto another of the video's `/l/` links counts its clicks once, and a
//! video nobody measured must read `unmeasured` — never a fabricated zero.

use crate::common;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::video_scorecard::{MissingReason, Pace};
use crowdrelay_infra::content_scorecard::{list_video_scorecards, video_scorecard};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, _url) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("video-scorecard-{suffix}"))
        .bind("Video Scorecard Tests")
        .execute(&pool)
        .await?;
    Ok(Fixture {
        pool,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

impl Fixture {
    fn ws(&self) -> Uuid {
        self.workspace_id.into_uuid()
    }

    async fn video(&self, source_key: &str, title: &str, age_days: i64) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO content_sources
               (id, workspace_id, source_kind, source_key, title, occurred_at,
                expires_at, metadata, active)
               VALUES ($1,$2,'video',$3,$4,$5,$6,$7,true)"#,
        )
        .bind(id)
        .bind(self.ws())
        .bind(source_key)
        .bind(title)
        .bind(self.now - time::Duration::days(age_days))
        .bind(self.now + time::Duration::days(90))
        .bind(serde_json::json!({"url": format!("https://www.youtube.com/watch?v={}", &source_key[8..])}))
        .execute(&self.pool)
        .await
        .expect("video source");
        id
    }

    /// One `content_source` series with a single latest point — the shape the
    /// traffic sweep writes for `views`, `traffic:*` and `ext:*`.
    async fn series(&self, source_id: Uuid, metric_key: &str, value: i64) {
        let series_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO growth_metric_series
               (id, workspace_id, platform, metric_key, subject_kind, subject_id,
                display_name)
               VALUES ($1,$2,'youtube',$3,'content_source',$4,'test series')"#,
        )
        .bind(series_id)
        .bind(self.ws())
        .bind(metric_key)
        .bind(source_id)
        .execute(&self.pool)
        .await
        .expect("series");
        sqlx::query(
            r#"INSERT INTO growth_metric_points
               (workspace_id, series_id, captured_at, value, source)
               VALUES ($1,$2,$3,$4,'growth_metric_sync')"#,
        )
        .bind(self.ws())
        .bind(series_id)
        .bind(self.now - time::Duration::hours(3))
        .bind(value)
        .execute(&self.pool)
        .await
        .expect("point");
    }

    /// A succeeded action row — every post table keys its row to one.
    async fn action(&self, source_id: Option<Uuid>) -> Uuid {
        let decision_id = Uuid::now_v7();
        let action_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_decisions
               (id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
               VALUES ($1,$2,$3,'growth_metrics','content_source',$4,
                       'auto_execute',9000,'auto_execute','test',
                       '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
        )
        .bind(decision_id)
        .bind(self.ws())
        .bind(format!("key-{action_id}"))
        // The decision's subject only needs to be non-null — the scorecard
        // reads the action payload's `source_id`, not this column.
        .bind(source_id.unwrap_or_else(Uuid::now_v7))
        .execute(&self.pool)
        .await
        .expect("decision");
        sqlx::query(
            r#"INSERT INTO autopilot_actions
               (id, workspace_id, decision_id, context, action_kind, subject_kind,
                subject_id, idempotency_key, payload, status, action_class, finished_at)
               VALUES ($1,$2,$3,'growth_metrics','community.post','content_source',
                       $4,$5,$6,'succeeded','third_party', now())"#,
        )
        .bind(action_id)
        .bind(self.ws())
        .bind(decision_id)
        // The action's subject column is NOT NULL; an unrelated post gets a
        // throwaway id — only the payload's `source_id` keys a card anyway.
        .bind(source_id.unwrap_or_else(Uuid::now_v7))
        .bind(format!("idem-{action_id}"))
        .bind(match source_id {
            Some(id) => serde_json::json!({"source_id": id.to_string()}),
            None => serde_json::json!({}),
        })
        .execute(&self.pool)
        .await
        .expect("action");
        action_id
    }

    async fn smart_link(&self, slug: &str, destination: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO smart_links (id, workspace_id, slug, destination_url) VALUES ($1,$2,$3,$4)")
            .bind(id)
            .bind(self.ws())
            .bind(slug)
            .bind(destination)
            .execute(&self.pool)
            .await
            .expect("smart link");
        id
    }

    async fn click(&self, link_id: Uuid, days_ago: i64) {
        sqlx::query(
            "INSERT INTO click_events (workspace_id, smart_link_id, occurred_at) VALUES ($1,$2,$3)",
        )
        .bind(self.ws())
        .bind(link_id)
        .bind(self.now - time::Duration::days(days_ago))
        .execute(&self.pool)
        .await
        .expect("click");
    }
}

/// One card carries the whole read: touched-domain attribution, first-hop
/// click counting, the sends ledger and the ordered missing reasons.
#[tokio::test]
#[ignore = "postgres"]
async fn scorecard_reads_the_release_and_its_ledgers() {
    let f = setup().await.expect("fixture");
    let ws = f.ws();

    let video = f.video("youtube:technopho01", "Technophobia", 5).await;
    for (key, value) in [
        ("views", 1200),
        ("traffic:EXT_URL", 125),
        ("traffic:ADVERTISING", 50),
        ("ext:reddit.com", 80),
        ("ext:virya.music", 30),
        // The pitched target's own domain — a review on it belongs to us.
        ("ext:press.example", 5),
        // A subdomain of the joined forum's host.
        ("ext:forums.metal-board.example", 10),
        // Traffic from a domain CrowdRelay never touched stays out.
        ("ext:nottouched.example", 10),
    ] {
        f.series(video, key, value).await;
    }

    // The release-landing link wraps the video link: `/l/vid-wrap` redirects
    // to `/l/vid-inner`, which redirects to the watch page. The redirector
    // records a click on both hops; only the first is a person we sent.
    let wrap = f
        .smart_link("vid-wrap", "https://virya.music/l/vid-inner")
        .await;
    let inner = f
        .smart_link("vid-inner", "https://www.youtube.com/watch?v=technopho01")
        .await;
    for _ in 0..3 {
        f.click(wrap, 1).await;
    }
    for _ in 0..5 {
        f.click(inner, 1).await;
    }

    // One posted community post carrying the wrap link, one waiting on the
    // operator, and one Reddit-removed post that halts the account.
    let posted_action = f.action(Some(video)).await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, smart_link,
            status, posted_at)
           VALUES ($1,$2,'Metal','technophobia','watch it','/l/vid-wrap',
                   'posted',$3)"#,
    )
    .bind(ws)
    .bind(posted_action)
    .bind(f.now - time::Duration::days(4))
    .execute(&f.pool)
    .await
    .expect("posted community post");
    let manual_action = f.action(Some(video)).await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, relay_source_id, subreddit, title, body,
            status)
           VALUES ($1,$2,$3,'Metal','technophobia','watch it',
                   'awaiting_manual_post')"#,
    )
    .bind(ws)
    .bind(manual_action)
    .bind(video)
    .execute(&f.pool)
    .await
    .expect("manual community post");
    let removed_action = f.action(None).await;
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status,
            posted_at, removed_by_category, removal_seen_at)
           VALUES ($1,$2,'Metal','old post','body','posted',$3,'reddit',$3)"#,
    )
    .bind(ws)
    .bind(removed_action)
    .bind(f.now - time::Duration::days(2))
    .execute(&f.pool)
    .await
    .expect("removed post");

    // The release plan the watcher wrote, its press wave, and the fan-email
    // wave: one of two seeded targets pitched, one inbound reply unanswered,
    // two deliveries home and one failed.
    let plan_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO release_plans
           (id, workspace_id, source_key, title, release_at, listen_url, active)
           VALUES ($1,$2,'youtube:technopho01','Technophobia',$3,
                   'https://www.youtube.com/watch?v=technopho01',true)"#,
    )
    .bind(plan_id)
    .bind(ws)
    .bind(f.now - time::Duration::days(5))
    .execute(&f.pool)
    .await
    .expect("release plan");
    let subject_key = format!("release:{plan_id}");
    for (email, outbound, inbound) in [
        ("writer@press.example", true, false),
        ("editor@press.example", false, true),
    ] {
        let target = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO outreach_targets
               (id, workspace_id, target_kind, display_name, contact_email,
                active, verified, accepts_outreach)
               VALUES ($1,$2,'press','Writer',$3,true,true,true)"#,
        )
        .bind(target)
        .bind(ws)
        .bind(email)
        .execute(&f.pool)
        .await
        .expect("target");
        let opportunity = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO outreach_opportunities
               (id, workspace_id, target_id, source, subject_kind, subject_key,
                template_key, relevance_basis_points, confidence_basis_points,
                active, observed_at, expires_at)
               VALUES ($1,$2,$3,'test','release',$4,'press.v1',7000,8000,true,
                       $5,$6)"#,
        )
        .bind(opportunity)
        .bind(ws)
        .bind(target)
        .bind(&subject_key)
        .bind(f.now - time::Duration::days(4))
        .bind(f.now + time::Duration::days(10))
        .execute(&f.pool)
        .await
        .expect("opportunity");
        if outbound {
            sqlx::query(
                r#"INSERT INTO outreach_interactions
                   (workspace_id, target_id, opportunity_id, direction, phase,
                    source_key, occurred_at)
                   VALUES ($1,$2,$3,'outbound','initial','pitch',$4)"#,
            )
            .bind(ws)
            .bind(target)
            .bind(opportunity)
            .bind(f.now - time::Duration::days(3))
            .execute(&f.pool)
            .await
            .expect("pitch");
        }
        if inbound {
            sqlx::query(
                r#"INSERT INTO outreach_interactions
                   (workspace_id, target_id, direction, phase, source_key,
                    occurred_at)
                   VALUES ($1,$2,'inbound','reply','reply',$3)"#,
            )
            .bind(ws)
            .bind(target)
            .bind(f.now - time::Duration::days(1))
            .execute(&f.pool)
            .await
            .expect("reply");
        }
    }

    let segment = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO audience_segments
           (id, workspace_id, slug, name, description, filter, active)
           VALUES ($1,$2,$3,'release audience','test','{}'::jsonb,true)"#,
    )
    .bind(segment)
    .bind(ws)
    .bind(format!("seg-{}", segment.simple()))
    .execute(&f.pool)
    .await
    .expect("segment");
    let campaign = Uuid::now_v7();
    let dispatch_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, payload) \
         VALUES ($1,$2,'release.release_day.v1','{}'::jsonb)",
    )
    .bind(dispatch_event)
    .bind(ws)
    .execute(&f.pool)
    .await
    .expect("dispatch event");
    sqlx::query(
        r#"INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key,
            status, scheduled_at, dispatch_event_id, recipient_count,
            delivered_count, failed_count, completed_at)
           VALUES ($1,$2,$3,$4,'release day','email','release.release_day.v1',
                   'completed', now() - interval '1 hour', $5, 3, 2, 1,
                   now())"#,
    )
    .bind(campaign)
    .bind(ws)
    .bind(segment)
    .bind(format!("crowdrelay-release-{plan_id}-release_day"))
    .bind(dispatch_event)
    .execute(&f.pool)
    .await
    .expect("campaign");
    for (tag, status) in [("a", "delivered"), ("b", "delivered"), ("c", "failed")] {
        let fan = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,'active')",
        )
        .bind(fan)
        .bind(ws)
        .bind(format!("fan-{tag}-{}@x.test", fan.simple()))
        .execute(&f.pool)
        .await
        .expect("fan");
        sqlx::query(
            "INSERT INTO communication_campaign_recipients (workspace_id, campaign_id, fan_id) VALUES ($1,$2,$3)",
        )
        .bind(ws)
        .bind(campaign)
        .bind(fan)
        .execute(&f.pool)
        .await
        .expect("recipient");
        sqlx::query(
            r#"INSERT INTO communication_campaign_deliveries
               (workspace_id, campaign_id, fan_id, attempt_key, status,
                completed_at)
               VALUES ($1,$2,$3,$4,$5, now())"#,
        )
        .bind(ws)
        .bind(campaign)
        .bind(fan)
        .bind(format!("attempt-{fan}"))
        .bind(status)
        .execute(&f.pool)
        .await
        .expect("delivery");
    }

    // One approved YouTube reply draft still unposted, one admitted curator
    // handle nobody sent, the joined forum, and a connected YouTube grant
    // that carries the Analytics scope.
    sqlx::query(
        r#"INSERT INTO community_comments
           (workspace_id, platform, content_source_id, platform_comment_id,
            parent_id, author, body, status, draft)
           VALUES ($1,'youtube',$2,'ug-abc','video-top-comment','fan','nice',
                   'approved','thank you!')"#,
    )
    .bind(ws)
    .bind(video)
    .execute(&f.pool)
    .await
    .expect("youtube comment");
    sqlx::query(
        r#"INSERT INTO outreach_candidates
           (workspace_id, target_kind, display_name, source, source_reference,
            route_kind, route_value, route_is_published, fit_basis_points,
            status, pitch_class)
           VALUES ($1,'creator','DJ','curator_site','https://x.test',
                   'handle','@djhandle',true,8000,'admitted','third_party')"#,
    )
    .bind(ws)
    .execute(&f.pool)
    .await
    .expect("curator candidate");
    sqlx::query(
        r#"INSERT INTO discovery_places
           (workspace_id, place_kind, platform, name, url, status)
           VALUES ($1,'forum','forum','Metal Board',
                   'https://metal-board.example/forum','active')"#,
    )
    .bind(ws)
    .execute(&f.pool)
    .await
    .expect("forum");
    sqlx::query(
        r#"INSERT INTO fanbase_connections
           (workspace_id, platform, external_account_ref, credential_ref,
            status, token_scope, label)
           VALUES ($1,'youtube_account','UC-channel','cred','connected',
                   'youtube.force-ssl yt-analytics.readonly','YouTube')"#,
    )
    .bind(ws)
    .execute(&f.pool)
    .await
    .expect("youtube grant");

    // A second video nobody measured: no series at all.
    let blind = f.video("youtube:rise0000000", "Rise", 3).await;
    let stale = f.video("youtube:oldtrack000", "Old Track", 45).await;

    let cards = list_video_scorecards(&f.pool, f.workspace_id, 10)
        .await
        .expect("list");
    assert_eq!(cards.len(), 2, "the 45-day-old video is outside the feed");
    assert_eq!(cards[0].source_id, blind, "newest first");

    let card = cards
        .iter()
        .find(|card| card.source_id == video)
        .expect("card");
    assert_eq!(card.video_id, "technopho01");
    assert_eq!(card.attributed_views, Some(125));
    assert_eq!(card.total_views, Some(1200));
    assert_eq!(card.ads_views, Some(50));
    assert!(card.analytics_through.is_some());
    assert_eq!(card.pace, Pace::Behind, "125 at day 5 is under half of 357");
    assert_eq!(card.tracked_clicks.community, 3);
    assert_eq!(
        card.tracked_clicks.total, 3,
        "the inner hop's five clicks are the same people arriving twice"
    );
    assert_eq!(card.sends.community.posted, 1);
    assert_eq!(card.sends.community.awaiting_manual_post, 1);
    let press = card.sends.press.as_ref().expect("press ledger");
    assert_eq!((press.seeded, press.pitched, press.remaining), (2, 1, 1));
    assert_eq!(press.replies_unanswered, 1);
    let email = card.sends.fan_email.as_ref().expect("email ledger");
    assert_eq!(
        (email.campaigns_completed, email.delivered, email.failed),
        (1, 2, 1)
    );
    assert_eq!(card.sends.curator_queue.unsent, 1);
    assert_eq!(card.sends.youtube_replies_approved_waiting, 1);
    assert!(!card.reddit.open);
    assert!(card.reddit.halted_until.is_some());

    let reasons: Vec<String> = card
        .missing
        .iter()
        .map(|reason| serde_json::to_value(reason).unwrap()["reason"].to_string())
        .collect();
    for expected in [
        "\"reddit_halted\"",
        "\"manual_posts_waiting\"",
        "\"press_queued\"",
        "\"press_replies_unanswered\"",
        "\"fan_email_undelivered\"",
        "\"curator_queue_unsent\"",
        "\"approved_youtube_replies_blocked\"",
    ] {
        assert!(reasons.iter().any(|r| r == expected), "missing {expected}");
    }
    assert!(card.missing.iter().all(|reason| !matches!(
        reason,
        MissingReason::NoReleasePlan
            | MissingReason::PressNotSeeded
            | MissingReason::NoAnalyticsGrant
    )));

    let unmeasured = cards
        .iter()
        .find(|card| card.source_id == blind)
        .expect("blind card");
    assert_eq!(
        unmeasured.attributed_views, None,
        "no traffic series, not zero"
    );
    assert_eq!(unmeasured.pace, Pace::Unmeasured);
    assert!(
        unmeasured
            .missing
            .iter()
            .any(|reason| matches!(reason, MissingReason::NoReleasePlan))
    );
    assert!(unmeasured.sends.fan_email.is_none());
    assert!(unmeasured.sends.press.is_none());

    // The single-card route reads the same card and says None for a non-video.
    let single = video_scorecard(&f.pool, f.workspace_id, video)
        .await
        .expect("single")
        .expect("present");
    assert_eq!(single.attributed_views, Some(125));
    let none = video_scorecard(&f.pool, f.workspace_id, stale)
        .await
        .expect("stale read");
    assert!(
        none.is_some(),
        "an old video still scores on the card route"
    );
    let unknown = video_scorecard(&f.pool, f.workspace_id, Uuid::now_v7())
        .await
        .expect("unknown read");
    assert!(unknown.is_none());
}

/// A curator DM the operator marked sent must count in the ledger — the
/// interaction links the candidate through `candidate_id`, not metadata, so
/// a join that reads metadata would report every sent handle as unsent.
#[tokio::test]
#[ignore = "postgres"]
async fn a_marked_curator_send_counts_in_the_card() {
    let f = setup().await.expect("fixture");
    let ws = f.ws();
    let video = f.video("youtube:technopho02", "Technophobia", 2).await;

    let candidate = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outreach_candidates
           (workspace_id, id, target_kind, display_name, source,
            source_reference, route_kind, route_value, route_is_published,
            fit_basis_points, status, pitch_class)
           VALUES ($1,$2,'creator','DJ','curator_site','https://x.test',
                   'handle','@djhandle',true,8000,'admitted','third_party')"#,
    )
    .bind(ws)
    .bind(candidate)
    .execute(&f.pool)
    .await
    .expect("candidate");
    sqlx::query(
        r#"INSERT INTO outreach_interactions
           (workspace_id, target_id, candidate_id, direction, phase,
            source_key, occurred_at)
           VALUES ($1, NULL, $2, 'outbound', 'initial', $3, now())"#,
    )
    .bind(ws)
    .bind(candidate)
    .bind(format!("manual:curator:{video}"))
    .execute(&f.pool)
    .await
    .expect("sent mark");

    let card = video_scorecard(&f.pool, f.workspace_id, video)
        .await
        .expect("card")
        .expect("present");
    assert_eq!(card.sends.curator_queue.sent, 1);
    assert_eq!(card.sends.curator_queue.unsent, 0);
    assert!(
        !card
            .missing
            .iter()
            .any(|reason| matches!(reason, MissingReason::CuratorQueueUnsent { .. })),
        "a sent handle is not a missing reason"
    );
}
