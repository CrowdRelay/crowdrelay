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
    /// `reddit`, `telegram`, `discord`, or a social platform —
    /// `instagram`, `facebook`, `x`.
    channel: String,
    drafts: i64,
    /// When the oldest draft on this channel was created. The age is the
    /// point: one draft from this morning is a queue, twelve from last month
    /// is a channel nobody is running.
    #[serde(with = "time::serde::rfc3339::option")]
    oldest_drafted_at: Option<OffsetDateTime>,
}

/// One channel's backlog inside the machine's own lane — the automatic
/// queue. `in_flight` rows are being worked (`pending`, `posting`,
/// `rate_limited` mid-backoff); `failed` rows resolved in the machine's
/// lane. Neither needs a person — this section exists so the operator can
/// tell "the system is posting" apart from "the queue is empty", not to
/// demand attention. The loud failed states still surface as alerts.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct AutomaticQueueChannel {
    /// `reddit`, `telegram`, `discord`, or a social platform —
    /// `instagram`, `facebook`, `x`. Social reports its platform so the
    /// lane matches the manual queue's vocabulary exactly.
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
    sqlx::query_as::<_, UnpublishedDraftChannel>(
        r#"
        SELECT channel, count(*)::bigint AS drafts, min(created_at) AS oldest_drafted_at
        FROM (
            SELECT 'reddit' AS channel, created_at FROM community_posts
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
        ) AS drafts
        GROUP BY channel
        ORDER BY min(created_at)
        "#,
    )
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
            SELECT 'reddit' AS channel, created_at, status FROM community_posts
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
