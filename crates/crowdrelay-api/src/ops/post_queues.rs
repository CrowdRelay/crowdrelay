// The post queue, in its two lanes.
//
// One queue, two owners — that split is the whole model. The machine lane
// (`automatic_queue`) is rows the system is carrying: `pending`, `posting`,
// `rate_limited` mid-backoff, and `failed` it already gave up on. Nothing
// there waits on a person, so it renders as information, not attention.
// The human lane (`unpublished_drafts`) is `awaiting_manual_post` — drafts
// whose publish needs a person: Reddit by policy, X because its write API
// is paid-tier, and every other channel while its autopost switch is off.
//
// `social_posts` reports its real `platform` in both lanes so Meta entries
// are distinguishable from X instead of one opaque "social" count.

/// One channel's backlog of drafted-but-unpublished posts — the human half
/// of the post queue.
///
/// Reported per channel because the answer differs by channel: Reddit needs a
/// human by policy, X has no write API this stack holds, and Telegram,
/// Discord and the Meta pair are a setting away from publishing themselves.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct UnpublishedDraftChannel {
    /// A community platform (`reddit`, `lemmy`, `forum`, …), `telegram`,
    /// `discord`, or a social platform — `instagram`, `facebook`, `x`.
    channel: String,
    drafts: i64,
    /// When the oldest draft on this channel was created. The age is the
    /// point: one draft from this morning is a queue, twelve from last month
    /// is a channel nobody is running.
    #[serde(with = "time::serde::rfc3339::option")]
    oldest_drafted_at: Option<OffsetDateTime>,
    /// The drafts a person can publish right now, oldest first. A count says
    /// a channel is waiting; this names the post, where it goes and the
    /// tracked link it carries, so "post this one" is one glance. Policy
    /// holds (`held: …`) are not listed: posting by hand around a moderator
    /// gate is not what the queue is asking for. Community posts only — the
    /// other tables do not carry a target or link.
    #[sqlx(skip)]
    ready_to_post: Vec<ReadyPost>,
}

/// One drafted community post a person can publish now.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ReadyPost {
    #[serde(skip)]
    channel: String,
    post_id: Uuid,
    /// The community, server or forum it is drafted for.
    target: String,
    title: String,
    /// The tracked path the post carries. `None` means it would publish
    /// uninstrumented and could never count toward a fan.
    tracked_link: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    drafted_at: OffsetDateTime,
    /// Why a person, not the machine, publishes it.
    needs_person_because: Option<String>,
    /// The exact words to paste, when the system wrote them (a YouTube capture
    /// comment). Community drafts carry theirs in the post itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_text: Option<String>,
}

/// One channel's backlog inside the machine's own lane — the automatic
/// queue. `in_flight` rows are being worked (`pending`, `posting`,
/// `rate_limited` mid-backoff); `failed` rows resolved in the machine's
/// lane. Neither needs a person — this section exists so the operator can
/// tell "the system is posting" apart from "the queue is empty", not to
/// demand attention. The loud failed states still surface as alerts.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct AutomaticQueueChannel {
    /// A community platform (`reddit`, `lemmy`, `forum`, …), `telegram`,
    /// `discord`, or a social platform — `instagram`, `facebook`, `x`.
    /// Social and community rows report their platform so the lane matches
    /// the manual queue's vocabulary exactly.
    channel: String,
    in_flight: i64,
    failed: i64,
    /// When the oldest in-flight row was created — the difference between
    /// a queue that is moving and one silently wedged.
    #[serde(with = "time::serde::rfc3339::option")]
    oldest_queued_at: Option<OffsetDateTime>,
}

/// The drafted posts waiting on a person, per channel.
///
/// Counts the draft states rather than excluding the published one, so a
/// status added later is not silently reported as a backlog. `rate_limited`
/// and `failed` are deliberately absent: those are the system's problem and
/// already surface as alerts, while `awaiting_manual_post` is the operator's.
async fn load_unpublished_drafts(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<UnpublishedDraftChannel>, OpsError> {
    let counts = format!(
        r#"
        SELECT channel, count(*)::bigint AS drafts, min(created_at) AS oldest_drafted_at
        FROM (
            SELECT platform AS channel, created_at FROM community_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'telegram', created_at FROM telegram_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'discord', created_at FROM discord_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT platform, created_at FROM social_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'youtube', (cs.metadata->>'fan_capture_draft_at')::timestamptz
            FROM content_sources cs
            WHERE cs.workspace_id = $1 AND {OPEN_CAPTURE_DRAFT}
        ) AS drafts
        GROUP BY channel
        ORDER BY min(created_at)
        "#
    );
    let mut channels = sqlx::query_as::<_, UnpublishedDraftChannel>(&counts)
        .bind(workspace_id)
        .fetch_all(pool)
        .await
        .map_err(OpsError::sqlx)?;
    let ready_sql = format!(
        r#"
        SELECT * FROM (
            SELECT platform AS channel, id AS post_id, subreddit AS target, title,
                   smart_link AS tracked_link, created_at AS drafted_at,
                   NULLIF(error_message, '') AS needs_person_because,
                   NULL::text AS draft_text
            FROM community_posts
            WHERE workspace_id = $1
              AND status = 'awaiting_manual_post'
              AND COALESCE(error_message, '') NOT LIKE 'held:%'
            UNION ALL
            SELECT 'youtube', cs.id, cs.title, 'Pinned comment with the tracked join link',
                   '/l/' || (cs.metadata->>'fan_capture_link_slug'),
                   (cs.metadata->>'fan_capture_draft_at')::timestamptz,
                   'a person posts it: nothing is allowed to comment on YouTube unattended',
                   cs.metadata->>'fan_capture_draft_text'
            FROM content_sources cs
            WHERE cs.workspace_id = $1 AND {OPEN_CAPTURE_DRAFT}
        ) AS ready
        ORDER BY drafted_at
        LIMIT 50
        "#
    );
    let ready = sqlx::query_as::<_, ReadyPost>(&ready_sql)
        .bind(workspace_id)
        .fetch_all(pool)
        .await
        .map_err(OpsError::sqlx)?;
    for post in ready {
        if let Some(channel) = channels.iter_mut().find(|c| c.channel == post.channel) {
            channel.ready_to_post.push(post);
        }
    }
    Ok(channels)
}

/// A YouTube capture comment that was prepared and has not visibly gone up: no
/// click has landed on its tracked link yet (a click means someone saw it), the
/// video is still inside the capture window, and the machine did not post it.
const OPEN_CAPTURE_DRAFT: &str = r#"
    cs.source_kind = 'video' AND cs.active
    AND cs.metadata ? 'fan_capture_draft_at'
    AND NOT (cs.metadata ? 'fan_capture_comment_posted_unix')
    AND cs.occurred_at > now() - interval '30 days'
    AND NOT EXISTS (
        SELECT 1 FROM smart_links l
        JOIN click_events c ON c.smart_link_id = l.id
        WHERE l.workspace_id = cs.workspace_id
          AND l.slug = cs.metadata->>'fan_capture_link_slug')
"#;

/// One channel's publication-measurement pipeline, counted by stage.
///
/// The stage is `publication_stage::PUBLICATION_STAGE_SQL`'s per-post
/// reading — `awaiting_publication` while the post can still go live,
/// `no_tracked_link` when it published uninstrumented, `maturing` inside
/// the seven-day window, `mature_zero`/`conversions` once the canonical
/// ledger has answered. `accepted` counts the posts whose outcome row the
/// learner already holds — a mature stage without it is a gap to alarm on,
/// not a quieter kind of zero.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct PublicationStageChannel {
    /// A community platform (`reddit`, `lemmy`, `forum`, …), `telegram`,
    /// `discord`, or a social platform — `instagram`, `facebook`, `x`.
    channel: String,
    stage: String,
    posts: i64,
    /// Of these, how many already carry an `autopilot_outcomes` row — the
    /// learner's input. Always all of them under `mature_zero`/`conversions`
    /// (the outcome commits with the success), so a shortfall anywhere else
    /// means the pipeline lost a result.
    accepted: i64,
}

/// The publication-measurement pipeline per channel — the last 30 days of
/// posts, one stage each.
///
/// The representative measurement is `content_fan_acquisition_7d` when it
/// exists, else `content_link_clicks_7d`: both share the publication clock,
/// and the acquisition arm is the one that feeds variant learning. A post
/// with neither reads `no_measurement`.
async fn load_publication_stages(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<PublicationStageChannel>, OpsError> {
    let stage = crowdrelay_infra::publication_stage::PUBLICATION_STAGE_SQL;
    let sql = format!(
        r#"
        SELECT channel, stage, count(*)::bigint AS posts,
               count(*) FILTER (WHERE accepted)::bigint AS accepted
        FROM (
            SELECT DISTINCT ON (post.id)
                   post.channel,
                   ({stage}) AS stage,
                   outcome.id IS NOT NULL AS accepted
            FROM (
                SELECT id, workspace_id, action_id, platform AS channel,
                       status, posted_at, created_at FROM community_posts
                WHERE workspace_id = $1
                  AND created_at > now() - INTERVAL '30 days'
                UNION ALL
                SELECT id, workspace_id, action_id, platform, status, posted_at, created_at
                FROM social_posts
                WHERE workspace_id = $1
                  AND created_at > now() - INTERVAL '30 days'
                UNION ALL
                SELECT id, workspace_id, action_id, 'telegram', status, posted_at, created_at
                FROM telegram_posts
                WHERE workspace_id = $1
                  AND created_at > now() - INTERVAL '30 days'
                UNION ALL
                SELECT id, workspace_id, action_id, 'discord', status, posted_at, created_at
                FROM discord_posts
                WHERE workspace_id = $1
                  AND created_at > now() - INTERVAL '30 days'
            ) AS post
            LEFT JOIN autopilot_measurements AS measurement
              ON measurement.workspace_id = post.workspace_id
             AND measurement.action_id = post.action_id
             AND measurement.measurement_kind IN
                 ('content_fan_acquisition_7d', 'content_link_clicks_7d')
            LEFT JOIN autopilot_outcomes AS outcome
              ON outcome.workspace_id = measurement.workspace_id
             AND outcome.measurement_id = measurement.id
            ORDER BY post.id,
                     CASE measurement.measurement_kind
                         WHEN 'content_fan_acquisition_7d' THEN 0 ELSE 1
                     END
        ) AS staged
        GROUP BY channel, stage
        ORDER BY channel, stage
        "#,
    );
    sqlx::query_as::<_, PublicationStageChannel>(&sql)
        .bind(workspace_id)
        .fetch_all(pool)
        .await
        .map_err(OpsError::sqlx)
}

/// The post queue's machine half, per channel.
///
/// `pending`, `posting` and `rate_limited` are in flight — claimed, sending,
/// or waiting out a backoff. `failed` is a terminal state in the same lane:
/// counted so the board is honest that the system gave up on something, but
/// deliberately not a needs-you — alerts already carry those. `posted` and
/// `awaiting_manual_post` are absent by construction: done is not a queue,
/// and waiting-for-a-person is the other queue.
async fn load_automatic_queue(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<AutomaticQueueChannel>, OpsError> {
    sqlx::query_as::<_, AutomaticQueueChannel>(
        r#"
        SELECT channel,
               count(*) FILTER (WHERE status IN ('pending', 'posting', 'rate_limited')) AS in_flight,
               count(*) FILTER (WHERE status = 'failed') AS failed,
               min(created_at) FILTER (WHERE status IN ('pending', 'posting', 'rate_limited')) AS oldest_queued_at
        FROM (
            SELECT platform AS channel, created_at, status FROM community_posts
            WHERE workspace_id = $1 AND status IN ('pending', 'posting', 'rate_limited', 'failed')
            UNION ALL
            SELECT 'telegram', created_at, status FROM telegram_posts
            WHERE workspace_id = $1 AND status IN ('pending', 'posting', 'rate_limited', 'failed')
            UNION ALL
            SELECT 'discord', created_at, status FROM discord_posts
            WHERE workspace_id = $1 AND status IN ('pending', 'posting', 'rate_limited', 'failed')
            UNION ALL
            SELECT platform, created_at, status FROM social_posts
            WHERE workspace_id = $1 AND status IN ('pending', 'posting', 'rate_limited', 'failed')
        ) AS lanes
        GROUP BY channel
        ORDER BY channel
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
    .map_err(OpsError::sqlx)
}
