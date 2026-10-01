//! The per-video scorecard's measured half.
//!
//! `crowdrelay_domain::video_scorecard` owns the labels and the arithmetic;
//! this module gathers the rows those pure functions grade. One video is a
//! `content_sources` row of kind `video`; everything else hangs off it:
//!
//! - `growth_metric_*` series keyed `content_source` + the source id carry the
//!   public `views` counter and the Analytics split (`traffic:{TYPE}`,
//!   `ext:{domain}`) the traffic sweep records.
//! - Posts about the video are found three ways: `community_posts` carry
//!   `relay_source_id`, every lane's action payload carries `source_id`, and
//!   the tracked link a post binds is itself evidence — a link whose
//!   destination is the video could only have been minted for it.
//! - The release plan joins on `source_key = 'youtube:{id}'` (the shape the
//!   video watcher writes) or on a `listen_url` naming the same video id.
//!
//! Chained links are the one counting trap: a link whose `destination_url` is
//! another of the video's `/l/` URLs makes the redirector record a click on
//! both hops. Only the first hop's row counts — the inner link's clicks are
//! the same people arriving twice, not a second audience.

use std::collections::HashMap;

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{
    VideoClickLedger, VideoCuratorLedger, VideoEmailLedger, VideoPressLedger, VideoPushLedger,
    VideoRedditStanding, VideoScorecardView, VideoSendsLedger,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::reddit_standing::{HaltReason, PostRecord, RedditStanding, RemovalCause};
use crowdrelay_domain::video_scorecard::{
    self, VIDEO_VIEW_TARGET, VIDEO_WINDOW_DAYS, VideoScorecardFacts,
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

mod posts;

use posts::{measurement_stages, post_ledgers};

/// Narrows a database failure to the repository's error vocabulary — the same
/// mapping every sibling module applies, so the read fails the way the rest
/// of the surface fails.
pub(super) fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}

/// The card feed covers the measurement window plus enough tail to say how
/// the recently closed videos actually ended.
const LIST_WINDOW_DAYS: i32 = 30;
/// The workspace never has many lanes' worth of recent videos; bound the join
/// fan-out anyway so a corrupted `occurred_at` cannot page the whole table.
const MAX_SCORECARDS: i64 = 30;
/// How far back the Reddit standing reads — mirrors the community executor's
/// history window so the card never disagrees with the send path.
const STANDING_HISTORY_DAYS: i32 = 180;

/// Surfaces CrowdRelay drives viewers through no matter which video is being
/// scored: the lanes it posts in and the `/l/` redirect origin every tracked
/// click passes through. Per-release additions — joined forums, the domains
/// of pitched targets — come from the database.
const BASE_TOUCHED_DOMAINS: &[&str] = &[
    "reddit.com",
    "virya.music",
    "t.me",
    "telegram.org",
    "discord.com",
    "discordapp.com",
];

#[derive(Debug, FromRow)]
struct VideoRow {
    id: Uuid,
    source_key: String,
    title: String,
    occurred_at: OffsetDateTime,
    url: Option<String>,
}

/// The latest point of each `views`/`traffic:*`/`ext:*` series, one row per
/// series per video.
#[derive(Debug, FromRow)]
struct MetricRow {
    subject_id: Uuid,
    metric_key: String,
    value: i64,
    captured_at: OffsetDateTime,
}

/// The release plan a video resolved to, when it has one.
struct PlanRow {
    plan_id: Uuid,
    active: bool,
}

/// The recent-video feed the list route serves.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn list_video_scorecards(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    limit: i64,
) -> Result<Vec<VideoScorecardView>, RepositoryError> {
    let videos = sqlx::query_as::<_, VideoRow>(
        r#"
        SELECT id, source_key, title, occurred_at, metadata ->> 'url' AS url
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'video'
          AND active
          AND occurred_at > now() - make_interval(days => $2)
        ORDER BY occurred_at DESC, id
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(LIST_WINDOW_DAYS)
    .bind(limit.clamp(1, MAX_SCORECARDS))
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    scorecards_for(pool, workspace_id, videos).await
}

/// One video's scorecard — `None` when `source_id` is not an active video
/// source of this workspace.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn video_scorecard(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
) -> Result<Option<VideoScorecardView>, RepositoryError> {
    let videos = sqlx::query_as::<_, VideoRow>(
        r#"
        SELECT id, source_key, title, occurred_at, metadata ->> 'url' AS url
        FROM content_sources
        WHERE workspace_id = $1 AND id = $2
          AND source_kind = 'video' AND active
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(scorecards_for(pool, workspace_id, videos)
        .await?
        .into_iter()
        .next())
}

/// Gathers every fact the `videos` rows need in set-shaped queries, then
/// hands each row to the domain for its labels.
async fn scorecards_for(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    videos: Vec<VideoRow>,
) -> Result<Vec<VideoScorecardView>, RepositoryError> {
    if videos.is_empty() {
        return Ok(Vec::new());
    }
    let ws = workspace_id.into_uuid();
    let source_ids: Vec<Uuid> = videos.iter().map(|video| video.id).collect();
    let now = OffsetDateTime::now_utc();

    let metrics = latest_metrics(pool, ws, &source_ids).await?;
    let clicks = click_ledgers(pool, ws, &source_ids).await?;
    let posts = post_ledgers(pool, ws, &source_ids).await?;
    let plans = release_plans(pool, ws, &source_ids).await?;
    let plan_keys: Vec<String> = plans
        .values()
        .map(|plan| format!("release:{}", plan.plan_id))
        .collect();
    let press = press_ledgers(pool, ws, &plan_keys).await?;
    let campaign_patterns: Vec<String> = plans
        .values()
        .map(|plan| format!("crowdrelay-release-{}-%", plan.plan_id))
        .collect();
    let emails = email_ledgers(pool, ws, &campaign_patterns).await?;
    let pushes = push_ledgers(pool, ws, &campaign_patterns).await?;
    let youtube_waiting = youtube_replies_waiting(pool, ws, &source_ids).await?;
    let curator = curator_queue(pool, ws, &source_ids).await?;
    let analytics_granted = analytics_grant(pool, ws).await?;
    let reddit = reddit_standing(pool, ws, now).await?;
    let common_touched = common_touched_domains(pool, ws).await?;
    let plan_domains = pitched_target_domains(pool, ws, &plan_keys).await?;
    let fans = fans_captured(pool, ws, &source_ids)
        .await?
        .into_iter()
        .collect::<HashMap<Uuid, i64>>();
    let mut stages = measurement_stages(pool, ws, &source_ids).await?;

    let mut cards = Vec::with_capacity(videos.len());
    for video in videos {
        let series: Vec<&MetricRow> = metrics
            .iter()
            .filter(|row| row.subject_id == video.id)
            .collect();
        let measured = series
            .iter()
            .any(|row| row.metric_key.starts_with("traffic:"));
        let total_views = series
            .iter()
            .find(|row| row.metric_key == "views")
            .map(|row| row.value.max(0) as u64);
        let ads_views = series
            .iter()
            .find(|row| row.metric_key == "traffic:ADVERTISING")
            .map(|row| row.value.max(0) as u64);
        let analytics_through = series
            .iter()
            .filter(|row| row.metric_key.starts_with("traffic:"))
            .map(|row| row.captured_at)
            .max();

        // Attribution counts only domains we actually sent viewers through:
        // the lanes' own hosts, the redirect origin, joined forums, and the
        // domains of the targets this video's release was pitched to. An
        // `embed:` host — the watch page first — counts the same way: a view
        // played on a touched domain is a view CrowdRelay drove.
        let plan_key = plans
            .get(&video.id)
            .map(|plan| format!("release:{}", plan.plan_id));
        let mut touched = common_touched.clone();
        if let Some(domains) = plan_key.as_ref().and_then(|key| plan_domains.get(key)) {
            touched.extend(domains.iter().cloned());
        }
        let attributed = measured.then(|| {
            series
                .iter()
                .filter(|row| {
                    let domain = row
                        .metric_key
                        .strip_prefix("ext:")
                        .or_else(|| row.metric_key.strip_prefix("embed:"));
                    domain.is_some_and(|domain| {
                        video_scorecard::attributable_domain(domain, &touched)
                    })
                })
                .map(|row| row.value.max(0) as u64)
                .sum()
        });

        let age = now - video.occurred_at;
        let lane_clicks = clicks.get(&video.id).cloned().unwrap_or_default();
        let lane_posts = posts.get(&video.id).cloned().unwrap_or_default();
        let plan = plans.get(&video.id);
        let press_ledger = plan_key.as_ref().and_then(|key| press.get(key)).cloned();
        let email_ledger = plan.and_then(|p| emails.get(&p.plan_id)).cloned();
        let push_ledger = plan.and_then(|p| pushes.get(&p.plan_id)).cloned();
        let facts = VideoScorecardFacts {
            reddit_halted_until: reddit.halted_until,
            manual_posts_waiting: lane_posts.community.awaiting_manual_post as u32,
            has_release_plan: plan.is_some_and(|p| p.active),
            press_seeded: press_ledger.as_ref().is_some_and(|p| p.seeded > 0),
            press_queued: press_ledger.as_ref().map_or(0, |p| p.remaining) as u32,
            press_replies_unanswered: press_ledger.as_ref().map_or(0, |p| p.replies_unanswered)
                as u32,
            has_analytics_grant: analytics_granted,
            fan_email_undelivered: email_ledger.as_ref().map_or(0, |e| e.failed) as u32,
            curator_queue_unsent: curator.get(&video.id).map_or(0, |c| c.unsent) as u32,
            approved_youtube_replies_blocked: youtube_waiting.get(&video.id).copied().unwrap_or(0)
                as u32,
        };

        cards.push(VideoScorecardView {
            source_id: video.id,
            source_key: video.source_key.clone(),
            video_id: video
                .source_key
                .strip_prefix("youtube:")
                .unwrap_or(&video.source_key)
                .to_owned(),
            title: video.title.clone(),
            url: video.url.clone(),
            published_at: video.occurred_at,
            age_days: age.whole_days(),
            view_target: VIDEO_VIEW_TARGET,
            window_days: VIDEO_WINDOW_DAYS,
            expected_by_now: video_scorecard::expected_by(age.whole_days()),
            attributed_views: attributed,
            total_views,
            ads_views,
            analytics_through,
            fans_captured: fans.get(&video.id).map(|count| (*count).max(0) as u64),
            pace: video_scorecard::pace(attributed, lane_clicks.total, age),
            tracked_clicks: lane_clicks,
            sends: VideoSendsLedger {
                community: lane_posts.community,
                telegram: lane_posts.telegram,
                discord: lane_posts.discord,
                social: lane_posts.social,
                fan_email: email_ledger,
                push: push_ledger,
                press: press_ledger,
                curator_queue: curator.get(&video.id).cloned().unwrap_or_default(),
                youtube_replies_approved_waiting: youtube_waiting
                    .get(&video.id)
                    .copied()
                    .unwrap_or(0),
            },
            missing: video_scorecard::missing_reasons(&facts),
            reddit: reddit.clone(),
            measurement_stages: stages.remove(&video.id).unwrap_or_default(),
        });
    }
    Ok(cards)
}

/// Latest point of every `views`/`traffic:*`/`ext:*` series the videos carry.
async fn latest_metrics(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<Vec<MetricRow>, RepositoryError> {
    sqlx::query_as::<_, MetricRow>(
        r#"
        SELECT s.subject_id, s.metric_key, p.value, p.captured_at
        FROM growth_metric_series s
        JOIN LATERAL (
            SELECT p.value, p.captured_at
            FROM growth_metric_points p
            WHERE p.workspace_id = s.workspace_id AND p.series_id = s.id
            ORDER BY p.captured_at DESC
            LIMIT 1
        ) p ON true
        WHERE s.workspace_id = $1
          AND s.subject_kind = 'content_source'
          AND s.subject_id = ANY($2)
          AND (s.metric_key = 'views'
               OR s.metric_key LIKE 'traffic:%'
               OR s.metric_key LIKE 'ext:%'
               OR s.metric_key LIKE 'embed:%')
        "#,
    )
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)
}

/// The link sets every lane read shares. `direct` links point at the video —
/// `substring(source_key from 9)` is the id inside a `youtube:{id}` source
/// key — `video_links` adds the links that chain onto a direct one, and
/// `inner_hops` marks the links that sit second in a chain so their clicks do
/// not count again.
pub(super) const VIDEO_LINKS_CTE: &str = r#"
    WITH videos AS (
        SELECT id, source_key, substring(source_key from 9) AS video_id
        FROM content_sources
        WHERE workspace_id = $1 AND id = ANY($2)
    ),
    direct AS (
        SELECT link.id, link.slug, videos.id AS source_id
        FROM smart_links link
        JOIN videos ON videos.source_key LIKE 'youtube:%'
          AND length(videos.video_id) >= 6
          AND strpos(link.destination_url, videos.video_id) > 0
        WHERE link.workspace_id = $1
    ),
    video_links AS (
        SELECT id, slug, source_id FROM direct
        UNION
        SELECT link.id, link.slug, direct.source_id
        FROM smart_links link
        JOIN direct ON right(link.destination_url, char_length(direct.slug) + 3) = '/l/' || direct.slug
        WHERE link.workspace_id = $1
    ),
    inner_hops AS (
        SELECT v.id, v.source_id
        FROM video_links v
        WHERE EXISTS (
            SELECT 1 FROM smart_links m
            WHERE m.workspace_id = $1 AND m.id <> v.id
              AND right(m.destination_url, char_length(v.slug) + 3) = '/l/' || v.slug
        )
    )
"#;

/// Distinct fans this video captured — counted from the canonical
/// `last_tracked_click` conversion rows, where `source_target` names the
/// clicked link's slug. The any-click join this replaced credited a person
/// to every video whose link they touched before signing up; the ledger's
/// assignment credits exactly one, and this card must agree with it.
/// Suppressed and deleted fans stop counting — consent withdrawal is a fact
/// about the person. A video with no tracked links gets no row, and the
/// caller renders that as `null`: links not yet minted is a different story
/// than links that captured no one.
async fn fans_captured(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<Vec<(Uuid, i64)>, RepositoryError> {
    sqlx::query_as::<_, (Uuid, i64)>(&format!(
        "{VIDEO_LINKS_CTE}
            SELECT v.source_id, count(DISTINCT conversion.fan_id) AS fans
            FROM video_links v
            JOIN fan_provenance_events AS conversion
              ON conversion.workspace_id = $1
             AND conversion.event_kind = 'conversion'
             AND conversion.attribution_method = 'last_tracked_click'
             AND conversion.source_target = v.slug
            JOIN fans AS fan
              ON fan.workspace_id = conversion.workspace_id
             AND fan.id = conversion.fan_id
             AND fan.status <> 'suppressed'
             AND fan.deleted_at IS NULL
            GROUP BY v.source_id"
    ))
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)
}

/// Clicks on each video's tracked links, split by the lane the post went out
/// on. Click membership is the link alone — a post carrying a video's link
/// promoted that video, whatever its action payload says.
async fn click_ledgers(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, VideoClickLedger>, RepositoryError> {
    let rows = sqlx::query_as::<_, (Uuid, String, i64)>(&format!(
        "{VIDEO_LINKS_CTE}
            SELECT v.source_id, 'community' AS lane, count(DISTINCT click.id) AS clicks
            FROM community_posts post
            JOIN smart_links link
              ON link.workspace_id = post.workspace_id
             AND post.smart_link = '/l/' || link.slug
            JOIN video_links v ON v.id = link.id
            JOIN click_events click
              ON click.workspace_id = post.workspace_id
             AND click.smart_link_id = link.id
            WHERE post.workspace_id = $1
              AND link.id NOT IN (SELECT id FROM inner_hops)
            GROUP BY v.source_id
            UNION ALL
            SELECT v.source_id, 'telegram', count(DISTINCT click.id)
            FROM telegram_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            JOIN click_events click
              ON click.workspace_id = post.workspace_id
             AND click.smart_link_id = v.id
            WHERE post.workspace_id = $1
              AND v.id NOT IN (SELECT id FROM inner_hops)
            GROUP BY v.source_id
            UNION ALL
            SELECT v.source_id, 'discord', count(DISTINCT click.id)
            FROM discord_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            JOIN click_events click
              ON click.workspace_id = post.workspace_id
             AND click.smart_link_id = v.id
            WHERE post.workspace_id = $1
              AND v.id NOT IN (SELECT id FROM inner_hops)
            GROUP BY v.source_id
            UNION ALL
            SELECT v.source_id, 'social', count(DISTINCT click.id)
            FROM social_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            JOIN click_events click
              ON click.workspace_id = post.workspace_id
             AND click.smart_link_id = v.id
            WHERE post.workspace_id = $1
              AND v.id NOT IN (SELECT id FROM inner_hops)
            GROUP BY v.source_id
            ",
    ))
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut ledgers: HashMap<Uuid, VideoClickLedger> = HashMap::new();
    for (source_id, lane, clicks) in rows {
        let ledger = ledgers.entry(source_id).or_default();
        let clicks = clicks.max(0) as u64;
        match lane.as_str() {
            "community" => ledger.community = clicks,
            "telegram" => ledger.telegram = clicks,
            "discord" => ledger.discord = clicks,
            _ => ledger.social = clicks,
        }
        ledger.total += clicks;
    }
    Ok(ledgers)
}

/// The plan a video resolves to — `source_key = 'youtube:{id}'` first (the
/// watcher's shape), a `listen_url` naming the video id for plans written
/// before the watcher existed. The newest active plan wins.
async fn release_plans(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, PlanRow>, RepositoryError> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, bool)>(
        r#"
        SELECT DISTINCT ON (cs.id) cs.id AS source_id, plan.id AS plan_id, plan.active
        FROM content_sources cs
        JOIN release_plans plan
          ON plan.workspace_id = cs.workspace_id
         AND (plan.source_key = cs.source_key
              OR (cs.source_key LIKE 'youtube:%'
                  AND strpos(plan.listen_url, substring(cs.source_key from 9)) > 0))
        WHERE cs.workspace_id = $1 AND cs.id = ANY($2)
        ORDER BY cs.id, plan.active DESC, plan.created_at DESC
        "#,
    )
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .map(|(source_id, plan_id, active)| (source_id, PlanRow { plan_id, active }))
        .collect())
}

/// The press wave per release: opportunities seeded, the ones whose target
/// already took an outbound touch, and inbound replies still unanswered.
/// Rows are keyed by `subject_key` (`release:{plan_id}`).
async fn press_ledgers(
    pool: &PgPool,
    ws: Uuid,
    plan_keys: &[String],
) -> Result<HashMap<String, VideoPressLedger>, RepositoryError> {
    if plan_keys.is_empty() {
        return Ok(HashMap::new());
    }
    let counts = sqlx::query_as::<_, (String, i64, i64, i64)>(
        r#"
        SELECT o.subject_key,
               count(*) AS seeded,
               count(*) FILTER (WHERE pitched.pitched) AS pitched,
               -- Only an in-window row is still owed a letter; a pitch whose
               -- deadline passed reads as missed work, never as open work.
               count(*) FILTER (WHERE o.expires_at > now()
                                AND NOT pitched.pitched) AS remaining
        FROM outreach_opportunities o
        CROSS JOIN LATERAL (
            SELECT EXISTS (
                SELECT 1 FROM outreach_interactions i
                WHERE i.workspace_id = o.workspace_id
                  AND i.direction = 'outbound'
                  AND (i.opportunity_id = o.id
                       OR (i.target_id = o.target_id
                           AND i.occurred_at >= o.observed_at))
            ) AS pitched
        ) pitched
        WHERE o.workspace_id = $1
          AND o.subject_kind = 'release'
          AND o.subject_key = ANY($2)
          AND o.active
        GROUP BY o.subject_key
        "#,
    )
    .bind(ws)
    .bind(plan_keys)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let unanswered = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT o.subject_key, count(DISTINCT i.id)
        FROM outreach_interactions i
        -- One reply lands on one opportunity: the one it names, else the
        -- freshest this target was pitched at or before the reply — not every
        -- release that ever wrote them.
        JOIN LATERAL (
            SELECT o.subject_key
            FROM outreach_opportunities o
            WHERE o.workspace_id = i.workspace_id
              AND o.target_id = i.target_id
              AND o.subject_kind = 'release'
              AND o.subject_key = ANY($2)
              AND o.active
            ORDER BY (o.id = i.opportunity_id) DESC,
                     (o.observed_at <= i.occurred_at) DESC,
                     o.observed_at DESC
            LIMIT 1
        ) o ON true
        WHERE i.workspace_id = $1 AND i.direction = 'inbound'
          AND NOT EXISTS (
              SELECT 1 FROM outreach_interactions r
              WHERE r.workspace_id = i.workspace_id
                AND r.target_id = i.target_id
                AND r.direction = 'outbound'
                AND r.occurred_at > i.occurred_at)
        GROUP BY o.subject_key
        "#,
    )
    .bind(ws)
    .bind(plan_keys)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .collect::<HashMap<_, _>>();

    Ok(counts
        .into_iter()
        .map(|(key, seeded, pitched, remaining)| {
            let seeded = seeded.max(0) as u64;
            (
                key.clone(),
                VideoPressLedger {
                    seeded,
                    pitched: pitched.max(0) as u64,
                    remaining: remaining.max(0) as u64,
                    replies_unanswered: unanswered.get(&key).copied().unwrap_or(0).max(0) as u64,
                },
            )
        })
        .collect())
}

/// The release's fan-email campaigns and their delivery ledger. Campaign
/// slugs are `crowdrelay-release-{plan_id}-{phase}`; the plan id parses out
/// of the slug's fixed-width middle so one query serves every plan.
async fn email_ledgers(
    pool: &PgPool,
    ws: Uuid,
    campaign_patterns: &[String],
) -> Result<HashMap<Uuid, VideoEmailLedger>, RepositoryError> {
    if campaign_patterns.is_empty() {
        return Ok(HashMap::new());
    }
    let campaigns = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT c.id, c.slug, c.status
        FROM communication_campaigns c
        WHERE c.workspace_id = $1 AND c.slug LIKE ANY($2)
        "#,
    )
    .bind(ws)
    .bind(campaign_patterns)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    if campaigns.is_empty() {
        return Ok(HashMap::new());
    }
    let campaign_ids: Vec<Uuid> = campaigns.iter().map(|row| row.0).collect();
    let deliveries = sqlx::query_as::<_, (Uuid, String, i64)>(
        r#"
        SELECT d.campaign_id, d.status, count(*)
        FROM communication_campaign_deliveries d
        WHERE d.workspace_id = $1 AND d.campaign_id = ANY($2)
        GROUP BY d.campaign_id, d.status
        "#,
    )
    .bind(ws)
    .bind(&campaign_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut ledgers: HashMap<Uuid, VideoEmailLedger> = HashMap::new();
    let mut campaign_plan: HashMap<Uuid, Uuid> = HashMap::new();
    for (campaign_id, slug, status) in campaigns {
        let Some(plan_id) = campaign_plan_id(&slug) else {
            continue;
        };
        campaign_plan.insert(campaign_id, plan_id);
        let ledger = ledgers.entry(plan_id).or_default();
        match status.as_str() {
            "completed" => ledger.campaigns_completed += 1,
            "cancelled" => ledger.campaigns_cancelled += 1,
            _ => ledger.campaigns_scheduled += 1,
        }
    }
    for (campaign_id, status, count) in deliveries {
        let Some(plan_id) = campaign_plan.get(&campaign_id) else {
            continue;
        };
        let ledger = ledgers.entry(*plan_id).or_default();
        let count = count.max(0) as u64;
        match status.as_str() {
            "delivered" => ledger.delivered += count,
            "failed" => ledger.failed += count,
            _ => ledger.claimed += count,
        }
    }
    Ok(ledgers)
}

/// The plan id inside a `crowdrelay-release-{plan}-{phase}` campaign slug —
/// the uuid sits between the constant head and the phase dash.
fn campaign_plan_id(slug: &str) -> Option<Uuid> {
    let rest = slug.strip_prefix("crowdrelay-release-")?;
    Uuid::parse_str(rest.get(..36)?).ok()
}

/// Push deliveries the release's campaigns produced.
async fn push_ledgers(
    pool: &PgPool,
    ws: Uuid,
    campaign_patterns: &[String],
) -> Result<HashMap<Uuid, VideoPushLedger>, RepositoryError> {
    if campaign_patterns.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        r#"
        SELECT c.slug, p.status, count(*)
        FROM fan_push_deliveries p
        JOIN communication_campaigns c
          ON c.workspace_id = p.workspace_id
         AND c.id = p.source_id
        WHERE p.workspace_id = $1
          AND p.source_kind = 'communication_campaign'
          AND c.slug LIKE ANY($2)
        GROUP BY c.slug, p.status
        "#,
    )
    .bind(ws)
    .bind(campaign_patterns)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut ledgers: HashMap<Uuid, VideoPushLedger> = HashMap::new();
    for (slug, status, count) in rows {
        let Some(plan_id) = campaign_plan_id(&slug) else {
            continue;
        };
        let ledger = ledgers.entry(plan_id).or_default();
        let count = count.max(0) as u64;
        match status.as_str() {
            "delivered" => ledger.delivered += count,
            "failed" => ledger.failed += count,
            _ => ledger.in_flight += count,
        }
    }
    Ok(ledgers)
}

/// YouTube reply drafts the operator approved that never posted, per video.
async fn youtube_replies_waiting(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, u64>, RepositoryError> {
    sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT comment.content_source_id, count(*)
        FROM community_comments comment
        WHERE comment.workspace_id = $1
          AND comment.platform = 'youtube'
          AND comment.status = 'approved'
          AND comment.content_source_id = ANY($2)
        GROUP BY comment.content_source_id
        "#,
    )
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)
    .map(|rows| {
        rows.into_iter()
            .map(|(id, count)| (id, count.max(0) as u64))
            .collect()
    })
}

/// The manual curator-DM lane: the workspace's admitted handle candidates,
/// scored per video. A send is keyed `manual:curator:{source_id}` — sent for
/// one video must not mark the candidate done for every video on the board.
async fn curator_queue(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, VideoCuratorLedger>, RepositoryError> {
    sqlx::query_as::<_, (Uuid, i64, i64)>(
        r#"
        SELECT video.source_id,
               count(*) FILTER (WHERE sent.id IS NULL) AS unsent,
               count(*) FILTER (WHERE sent.id IS NOT NULL) AS sent
        FROM outreach_candidates c
        CROSS JOIN unnest($2::uuid[]) AS video(source_id)
        LEFT JOIN outreach_interactions sent
          ON sent.workspace_id = c.workspace_id
         AND sent.direction = 'outbound'
         AND sent.candidate_id = c.id
         AND sent.source_key = 'manual:curator:' || video.source_id::text
        WHERE c.workspace_id = $1
          AND c.route_kind = 'handle'
          AND c.status = 'admitted'
        GROUP BY video.source_id
        "#,
    )
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)
    .map(|rows| {
        rows.into_iter()
            .map(|(source_id, unsent, sent)| {
                (
                    source_id,
                    VideoCuratorLedger {
                        unsent: unsent.max(0) as u64,
                        sent: sent.max(0) as u64,
                    },
                )
            })
            .collect()
    })
}

/// Whether the workspace's `youtube_account` grant carries the Analytics
/// scope — the difference between "no one came" and "we cannot tell". A grant
/// whose stored scope does not name `yt-analytics` cannot measure, whether
/// the scope column predates it or the consent did.
async fn analytics_grant(pool: &PgPool, ws: Uuid) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'youtube_account'
              AND status = 'connected'
              AND token_scope ILIKE '%yt-analytics%')
        "#,
    )
    .bind(ws)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

/// The workspace-wide touched domains: the lane constants plus the hosts of
/// every joined forum place.
async fn common_touched_domains(pool: &PgPool, ws: Uuid) -> Result<Vec<String>, RepositoryError> {
    let forum_urls = sqlx::query_scalar::<_, String>(
        r#"
        SELECT url FROM discovery_places
        WHERE workspace_id = $1 AND place_kind = 'forum' AND status = 'active'
        "#,
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let mut domains: Vec<String> = BASE_TOUCHED_DOMAINS
        .iter()
        .map(|d| (*d).to_owned())
        .collect();
    for url in forum_urls {
        if let Some(host) = url_host(&url) {
            domains.push(host);
        }
    }
    Ok(domains)
}

/// The domains of the targets each release was pitched to — a review that
/// lands on the target's own site attributes back to this video. Targets
/// carry no domain column; the email's domain is the site's, since the
/// pitch went to a person behind it.
async fn pitched_target_domains(
    pool: &PgPool,
    ws: Uuid,
    plan_keys: &[String],
) -> Result<HashMap<String, Vec<String>>, RepositoryError> {
    if plan_keys.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT DISTINCT o.subject_key,
               split_part(lower(t.contact_email), '@', 2) AS domain
        FROM outreach_opportunities o
        JOIN outreach_targets t
          ON t.workspace_id = o.workspace_id AND t.id = o.target_id
        WHERE o.workspace_id = $1
          AND o.subject_kind = 'release'
          AND o.subject_key = ANY($2)
          AND position('.' in split_part(lower(t.contact_email), '@', 2)) > 0
        "#,
    )
    .bind(ws)
    .bind(plan_keys)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (key, domain) in rows {
        map.entry(key).or_default().push(domain);
    }
    Ok(map)
}

/// The host part of a stored URL: scheme, credentials, port and path folded
/// away, lowercased, `www.` stripped — the same normalization the Analytics
/// detail parser applies so both sides agree what a domain is.
fn url_host(url: &str) -> Option<String> {
    let mut rest = url.trim();
    if let Some((_scheme, after)) = rest.split_once("://") {
        rest = after;
    } else if let Some(stripped) = rest.strip_prefix("//") {
        rest = stripped;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port
        .split(':')
        .next()?
        .trim()
        .trim_end_matches('.')
        .to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(host.as_str());
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    Some(host.to_string())
}

/// The Reddit standing the way the community executor reads it, plus when a
/// halt lifts: the verdict keeping it closed aging out of the 30-day window.
async fn reddit_standing(
    pool: &PgPool,
    ws: Uuid,
    now: OffsetDateTime,
) -> Result<VideoRedditStanding, RepositoryError> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            OffsetDateTime,
            Option<String>,
            Option<OffsetDateTime>,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT normalize_subreddit(subreddit), posted_at, removed_by_category,
               removal_seen_at, last_seen_live_at
        FROM community_posts
        WHERE workspace_id = $1
          AND status = 'posted'
          AND posted_at IS NOT NULL
          AND posted_at > now() - make_interval(days => $2)
        "#,
    )
    .bind(ws)
    .bind(STANDING_HISTORY_DAYS)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let history: Vec<PostRecord> = rows
        .into_iter()
        .map(
            |(subreddit, posted_at, category, removal_seen_at, last_seen_live_at)| PostRecord {
                subreddit,
                posted_at,
                removal: category.as_deref().and_then(RemovalCause::from_category),
                removal_seen_at,
                last_seen_live_at,
            },
        )
        .collect();

    match crowdrelay_domain::reddit_standing::reddit_standing(&history, now) {
        RedditStanding::Open { daily_cap } => Ok(VideoRedditStanding {
            open: true,
            daily_cap,
            hold_reason: None,
            halted_until: None,
        }),
        RedditStanding::Halted(reason) => {
            let mut verdict_times: Vec<OffsetDateTime> = history
                .iter()
                .filter_map(|post| {
                    let cause = post.removal.filter(|cause| cause.is_verdict())?;
                    if matches!(reason, HaltReason::SiteFilterRemoval)
                        && cause != RemovalCause::SiteFilter
                    {
                        return None;
                    }
                    Some(post.removal_seen_at.unwrap_or(post.posted_at))
                })
                .collect();
            verdict_times.sort_unstable();
            // A repeated-removals halt needs two in-window verdicts and clears
            // when the second-newest ages out; a site-filter halt clears when
            // that verdict ages out.
            let index = match reason {
                HaltReason::SiteFilterRemoval => verdict_times.len().saturating_sub(1),
                HaltReason::RepeatedRemovals => verdict_times.len().saturating_sub(2),
            };
            let halted_until = verdict_times
                .get(index)
                .map(|seen| *seen + crowdrelay_domain::reddit_standing::REMOVAL_WINDOW);
            Ok(VideoRedditStanding {
                open: false,
                daily_cap: crowdrelay_domain::reddit_standing::BASE_DAILY_CAP,
                hold_reason: Some(reason.as_str().to_owned()),
                halted_until,
            })
        }
    }
}

/// The one fact a public capture page may render: an owned video's title and
/// publish time. Everything else about the source stays server-side.
#[derive(Debug, FromRow)]
pub struct PublicVideoSummary {
    /// The video's title as the watcher last saw it.
    pub title: String,
    /// The publish timestamp the watcher recorded.
    pub published_at: OffsetDateTime,
}

/// `None` when `youtube_id` names no active video source of this workspace —
/// the same retirement flag the scorecard read applies.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn public_owned_video(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    youtube_id: &str,
) -> Result<Option<PublicVideoSummary>, RepositoryError> {
    sqlx::query_as::<_, PublicVideoSummary>(
        r#"
        SELECT title, occurred_at AS published_at
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'video'
          AND source_key = 'youtube:' || $2
          AND active
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(youtube_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)
}
