// Process-run read models — kept out of `ops.rs` to preserve the
// control-plane source-size ratchet. This file is included into the `ops`
// module, so it deliberately shares its private database state and helpers.
//
// A "process run" is one pass of a pipeline over one subject, rendered for
// the operator as steps: observed → decided → awaiting a person → sent →
// proof → measured. The entity tables already carry every step; these reads
// join them into the run shape the process pages render, one query per call.
//
// The community relay is the first kind: one synced band post fans out to
// one Signal push plus one drafted Reddit post per admitted community. The
// run's grouping key is the content source — decisions carry it in
// `input_snapshot.source_id`, the engagement actions in `payload.source_id`,
// and the drafting dispatches in `idempotency_key` ("action:relay:{source}:…").

const RELAY_RUNS_WINDOW: &str = "30 days";
const RELAY_RUNS_LIMIT: i64 = 20;
const RELAY_TARGETS_LIMIT: i64 = 250;

#[derive(Debug, Serialize)]
pub struct ProcessRelays {
    runs: Vec<ProcessRelayRun>,
}

#[derive(Debug, Serialize)]
pub struct ProcessRelayRun {
    source_id: String,
    title: Option<String>,
    platform: Option<String>,
    source_url: Option<String>,
    thumbnail_url: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    occurred_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    decided_at: OffsetDateTime,
    confidence_bp: i32,
    communities_decided: i64,
    push_decided: bool,
    /// Decided communities with no drafting outcome yet — the draft is
    /// queued, running, or failed before an approval existed.
    deciding: i64,
    /// Awaiting a person and still inside the approval window.
    awaiting: i64,
    /// Awaiting a person past the approval window — the ask lapsed.
    expired: i64,
    queued: i64,
    posting: i64,
    posted: i64,
    manual: i64,
    failed: i64,
    skipped: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    last_posted_at: Option<OffsetDateTime>,
    total_score: Option<i64>,
    total_comments: Option<i64>,
    replies: i64,
    conversions: i64,
    push_status: Option<String>,
}

/// `GET /v1/control-plane/processes/relays` — recent relay runs, one row of
/// step state per run. The per-target detail is the sibling endpoint; this
/// stays thin so the list renders in a single indexed read.
pub async fn process_relays(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match run_limited(
        &state.read_budget,
        state.ops.operation_timeout,
        load_relay_runs(&state.ops),
    )
    .await
    {
        Ok(runs) => private_json(StatusCode::OK, ProcessRelays { runs }),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

async fn load_relay_runs(state: &OpsState) -> Result<Vec<ProcessRelayRun>, OpsError> {
    let rows = sqlx::query_as::<_, RelayRunRow>(
        r#"
        WITH runs AS (
            SELECT
                (d.input_snapshot->>'source_id')::uuid AS source_id,
                min(d.evaluated_at) AS decided_at,
                max(d.confidence_basis_points) AS confidence_bp,
                -- Only executable dispositions count — an observe/recommend/
                -- deny decision can never produce an action, so counting it
                -- would pin the run at "drafting" forever. DISTINCT because
                -- decision_key carries the policy version: a version bump
                -- re-persists a decision row per community while the
                -- versionless action key dedups — row counting would
                -- inflate the community count permanently.
                count(DISTINCT d.subject_id) FILTER (
                    WHERE d.subject_kind = 'target_community'
                      AND d.disposition IN ('require_approval', 'auto_execute')
                ) AS communities_decided,
                bool_or(
                    d.decision_key LIKE '%:signal_push'
                    AND d.disposition IN ('require_approval', 'auto_execute')
                ) AS push_decided
            FROM viryaos_autopilot_decisions d
            WHERE d.workspace_id = $1
              AND d.decision_kind = 'relay_owned_post'
              AND d.input_snapshot->>'source_id' ~
                  '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
              AND d.evaluated_at >= now() - CAST($3 AS interval)
            GROUP BY 1
            ORDER BY decided_at DESC
            LIMIT $2
        ),
        -- One community can earn a second engage action when a draft is
        -- retried — latest wins, so every aggregate reads this same set and
        -- the counts stay per-community.
        latest_engage AS (
            SELECT DISTINCT ON (r.source_id, (a.payload->>'target_id')::uuid)
                r.source_id,
                (a.payload->>'target_id')::uuid AS target_id,
                a.id AS action_id,
                a.status,
                a.approval_expires_at
            FROM runs r
            JOIN viryaos_autopilot_actions a
              ON a.workspace_id = $1
             AND a.action_kind = 'community.engage.request'
             AND a.payload->>'source_id' ~
                 '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
             AND (a.payload->>'source_id')::uuid = r.source_id
             AND a.payload->>'target_id' ~
                 '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
            ORDER BY r.source_id, (a.payload->>'target_id')::uuid, a.created_at DESC
        )
        SELECT
            r.source_id::text AS source_id,
            r.decided_at,
            r.confidence_bp,
            r.communities_decided,
            r.push_decided,
            cs.title,
            cs.occurred_at,
            cs.metadata->>'platform' AS platform,
            cs.metadata->>'url' AS source_url,
            cs.metadata->>'thumbnail_url' AS thumbnail_url,
            COALESCE(act.awaiting, 0) AS awaiting,
            COALESCE(act.expired, 0) AS expired,
            COALESCE(act.queued, 0) AS queued,
            COALESCE(act.posting, 0) AS posting,
            COALESCE(act.posted, 0) AS posted,
            COALESCE(act.manual, 0) AS manual,
            COALESCE(act.failed, 0) + COALESCE(dfail.n, 0) AS failed,
            COALESCE(act.skipped, 0) AS skipped,
            COALESCE(act.acted, 0) AS actions_total,
            COALESCE(dfail.n, 0) AS drafts_failed,
            act.last_posted_at,
            met.total_score,
            met.total_comments,
            COALESCE(conv.replies, 0) AS replies,
            COALESCE(conv2.conversions, 0) AS conversions,
            push.status AS push_status
        FROM runs r
        LEFT JOIN viryaos_content_sources cs
          ON cs.workspace_id = $1 AND cs.id = r.source_id
        LEFT JOIN LATERAL (
            SELECT
                count(*) FILTER (
                    WHERE le.status = 'awaiting_approval'
                      AND (le.approval_expires_at IS NULL OR le.approval_expires_at > now())
                ) AS awaiting,
                count(*) FILTER (
                    WHERE le.status = 'awaiting_approval'
                      AND le.approval_expires_at <= now()
                ) AS expired,
                count(*) FILTER (WHERE le.status IN ('queued', 'processing')) AS queued,
                count(*) FILTER (WHERE p.status IN ('pending', 'posting', 'rate_limited')
                                       OR (le.status = 'succeeded' AND p.id IS NULL)) AS posting,
                count(*) FILTER (WHERE p.status = 'posted') AS posted,
                count(*) FILTER (WHERE p.status = 'awaiting_manual_post') AS manual,
                count(*) FILTER (WHERE le.status = 'failed' OR p.status = 'failed') AS failed,
                count(*) FILTER (WHERE le.status = 'cancelled') AS skipped,
                count(*) AS acted,
                max(p.posted_at) AS last_posted_at
            FROM latest_engage le
            -- The receipt belongs to the community, not the action attempt —
            -- a retried target's earlier posted row is still the proof.
            -- Scoped through the action's source_id so another run's post on
            -- the same community never counts here.
            LEFT JOIN LATERAL (
                SELECT p.id, p.status, p.posted_at
                FROM community_posts p
                JOIN viryaos_autopilot_actions pa
                  ON pa.workspace_id = p.workspace_id AND pa.id = p.action_id
                 AND pa.action_kind = 'community.engage.request'
                 AND pa.payload->>'source_id' ~
                     '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
                 AND (pa.payload->>'source_id')::uuid = r.source_id
                WHERE p.workspace_id = $1 AND p.target_id = le.target_id
                ORDER BY p.created_at DESC
                LIMIT 1
            ) p ON true
            WHERE le.source_id = r.source_id
        ) act ON true
        -- A community whose drafting run failed never reaches the approval
        -- queue — without this count it would read "deciding" forever.
        -- DISTINCT: re-decided rows after a policy version bump would
        -- otherwise count the same community once per version.
        LEFT JOIN LATERAL (
            SELECT count(DISTINCT d.subject_id) AS n
            FROM viryaos_autopilot_decisions d
            WHERE d.workspace_id = $1
              AND d.decision_kind = 'relay_owned_post'
              AND d.subject_kind = 'target_community'
              AND d.disposition IN ('require_approval', 'auto_execute')
              AND d.input_snapshot->>'source_id' ~
                  '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
              AND (d.input_snapshot->>'source_id')::uuid = r.source_id
              AND NOT EXISTS (
                  SELECT 1 FROM latest_engage le
                  WHERE le.source_id = r.source_id AND le.target_id = d.subject_id
              )
              AND (
                  SELECT dr.status
                  FROM viryaos_autopilot_actions dr
                  WHERE dr.workspace_id = $1
                    AND dr.action_kind = 'agent.run.request'
                    AND dr.idempotency_key =
                        'action:relay:' || r.source_id::text || ':community:' || d.subject_id::text
                  ORDER BY dr.created_at DESC
                  LIMIT 1
              ) = 'failed'
        ) dfail ON true
        LEFT JOIN LATERAL (
            SELECT sum(latest.score)::bigint AS total_score,
                   sum(latest.num_comments)::bigint AS total_comments
            FROM (
                -- Latest engage action → latest post → latest metric, per
                -- community — a superseded attempt's numbers never inflate
                -- the run totals.
                SELECT DISTINCT ON (le.target_id) le.target_id, m.score, m.num_comments
                FROM latest_engage le
                JOIN LATERAL (
                    SELECT p.id
                    FROM community_posts p
                    JOIN viryaos_autopilot_actions pa
                      ON pa.workspace_id = p.workspace_id AND pa.id = p.action_id
                     AND pa.action_kind = 'community.engage.request'
                     AND pa.payload->>'source_id' ~
                         '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
                     AND (pa.payload->>'source_id')::uuid = r.source_id
                    WHERE p.workspace_id = $1 AND p.target_id = le.target_id
                    ORDER BY p.created_at DESC
                    LIMIT 1
                ) p ON true
                JOIN community_post_metrics m
                  ON m.workspace_id = $1 AND m.community_post_id = p.id
                WHERE le.source_id = r.source_id
                ORDER BY le.target_id, m.measured_at DESC
            ) latest
        ) met ON true
        -- Replies and conversions aggregate separately — joining both tables
        -- to the action set in one scan would multiply each by the other's
        -- row count.
        LEFT JOIN LATERAL (
            SELECT count(*) FILTER (WHERE re.status IN ('replied', 'positive_reply')) AS replies
            FROM latest_engage le
            JOIN viryaos_reach_events re
              ON re.workspace_id = $1 AND re.action_id = le.action_id
             AND re.channel = 'reddit_post'
            WHERE le.source_id = r.source_id
        ) conv ON true
        LEFT JOIN LATERAL (
            SELECT count(*) FILTER (WHERE ge.converted) AS conversions
            FROM latest_engage le
            JOIN viryaos_growth_episodes ge
              ON ge.workspace_id = $1 AND ge.action_id = le.action_id
            WHERE le.source_id = r.source_id
        ) conv2 ON true
        LEFT JOIN LATERAL (
            SELECT a.status
            FROM viryaos_autopilot_actions a
            WHERE a.workspace_id = $1
              AND a.action_kind = 'signal.push.request'
              AND a.payload->>'task_id' = r.source_id::text
            ORDER BY a.created_at DESC
            LIMIT 1
        ) push ON true
        ORDER BY r.decided_at DESC
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(RELAY_RUNS_LIMIT)
    .bind(RELAY_RUNS_WINDOW)
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    Ok(rows
        .into_iter()
        .map(|row| ProcessRelayRun {
            deciding: (row.communities_decided - row.actions_total - row.drafts_failed).max(0),
            source_id: row.source_id,
            title: row.title,
            platform: row.platform,
            source_url: row.source_url,
            thumbnail_url: row.thumbnail_url,
            occurred_at: row.occurred_at,
            decided_at: row.decided_at,
            confidence_bp: row.confidence_bp,
            communities_decided: row.communities_decided,
            push_decided: row.push_decided,
            awaiting: row.awaiting,
            expired: row.expired,
            queued: row.queued,
            posting: row.posting,
            posted: row.posted,
            manual: row.manual,
            failed: row.failed,
            skipped: row.skipped,
            last_posted_at: row.last_posted_at,
            total_score: row.total_score,
            total_comments: row.total_comments,
            replies: row.replies,
            conversions: row.conversions,
            push_status: row.push_status,
        })
        .collect())
}

#[derive(Debug, FromRow)]
struct RelayRunRow {
    source_id: String,
    decided_at: OffsetDateTime,
    confidence_bp: i32,
    communities_decided: i64,
    push_decided: bool,
    title: Option<String>,
    occurred_at: Option<OffsetDateTime>,
    platform: Option<String>,
    source_url: Option<String>,
    thumbnail_url: Option<String>,
    awaiting: i64,
    expired: i64,
    queued: i64,
    posting: i64,
    posted: i64,
    manual: i64,
    failed: i64,
    skipped: i64,
    actions_total: i64,
    drafts_failed: i64,
    last_posted_at: Option<OffsetDateTime>,
    total_score: Option<i64>,
    total_comments: Option<i64>,
    replies: i64,
    conversions: i64,
    push_status: Option<String>,
}

// ── Run detail: the per-forum checklist ──

#[derive(Debug, Serialize)]
pub struct ProcessRelayRunDetail {
    source_id: String,
    title: Option<String>,
    platform: Option<String>,
    source_url: Option<String>,
    thumbnail_url: Option<String>,
    body: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    occurred_at: Option<OffsetDateTime>,
    decided_at: OffsetDateTime,
    confidence_bp: i32,
    push: Option<RelayPushLeg>,
    /// True when the community list hit the bounded cap — the view says so
    /// rather than silently showing a partial fan-out. `targets_total` is
    /// the untruncated count so the note can say "250 of 412".
    targets_truncated: bool,
    targets_total: i64,
    targets: Vec<RelayTarget>,
}

#[derive(Debug, Serialize)]
pub struct RelayPushLeg {
    action_id: String,
    status: String,
    audience_size: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    approval_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct RelayTarget {
    target_id: String,
    subreddit: Option<String>,
    display_name: Option<String>,
    confidence_bp: Option<i32>,
    /// `deciding` (draft pending/failed before an approval existed),
    /// `awaiting_you`, `expired`, `queued`, `posting`, `posted`, `manual`,
    /// `failed`, `skipped`.
    state: String,
    action_id: Option<String>,
    community_post_id: Option<String>,
    draft_title: Option<String>,
    draft_body: Option<String>,
    image_url: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    approval_expires_at: Option<OffsetDateTime>,
    post_status: Option<String>,
    reddit_post_url: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    posted_at: Option<OffsetDateTime>,
    score: Option<i32>,
    upvotes: Option<i32>,
    num_comments: Option<i32>,
    upvote_ratio: Option<f64>,
    #[serde(with = "time::serde::rfc3339::option")]
    measured_at: Option<OffsetDateTime>,
    reach_status: Option<String>,
    observed_fans: Option<f64>,
    converted: Option<bool>,
    /// Why the action or the post failed, when it did — "failed" without the
    /// reason is a status, not an answer.
    error_kind: Option<String>,
}

/// `GET /v1/control-plane/processes/relays/{source_id}` — one run's full
/// checklist: every community the decision named, its draft, its approval
/// state, its receipt and its latest measurement.
pub async fn process_relay_run(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(raw_source_id): Path<String>,
) -> Response {
    let source_id = match parse_trace_id(&raw_source_id) {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    match run_limited(
        &state.read_budget,
        state.ops.operation_timeout,
        load_relay_run(&state.ops, source_id),
    )
    .await
    {
        Ok(Some(run)) => private_json(StatusCode::OK, run),
        Ok(None) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

async fn load_relay_run(
    state: &OpsState,
    source_id: Uuid,
) -> Result<Option<ProcessRelayRunDetail>, OpsError> {
    let header = sqlx::query_as::<_, RelayRunHeaderRow>(
        r#"
        SELECT
            $2::text AS source_id,
            min(d.evaluated_at) AS decided_at,
            max(d.confidence_basis_points) AS confidence_bp,
            cs.title,
            cs.occurred_at,
            cs.metadata->>'platform' AS platform,
            cs.metadata->>'url' AS source_url,
            cs.metadata->>'thumbnail_url' AS thumbnail_url,
            cs.metadata->>'body' AS body
        FROM viryaos_autopilot_decisions d
        LEFT JOIN viryaos_content_sources cs
          ON cs.workspace_id = d.workspace_id AND cs.id = $2
        WHERE d.workspace_id = $1
          AND d.decision_kind = 'relay_owned_post'
          AND d.subject_kind = 'target_community'
          AND d.disposition IN ('require_approval', 'auto_execute')
          AND d.input_snapshot->>'source_id' ~
              '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
          AND (d.input_snapshot->>'source_id')::uuid = $2
        GROUP BY cs.id, cs.title, cs.occurred_at, cs.metadata
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(source_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    let Some(header) = header else {
        return Ok(None);
    };

    let targets = sqlx::query_as::<_, RelayTargetRow>(
        r#"
        WITH decided AS (
            SELECT
                d.subject_id AS target_id,
                max(d.confidence_basis_points) AS confidence_bp
            FROM viryaos_autopilot_decisions d
            WHERE d.workspace_id = $1
              AND d.decision_kind = 'relay_owned_post'
              AND d.subject_kind = 'target_community'
              -- Only executable dispositions are candidates — an observe/
              -- recommend/deny verdict never reaches the checklist.
              AND d.disposition IN ('require_approval', 'auto_execute')
              AND d.input_snapshot->>'source_id' ~
                  '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
              AND (d.input_snapshot->>'source_id')::uuid = $2
            GROUP BY 1
        ),
        engaged AS (
            -- A retried draft can write a second engage action for the same
            -- community — latest wins so the checklist stays per-forum.
            SELECT DISTINCT ON ((a.payload->>'target_id')::uuid)
                (a.payload->>'target_id')::uuid AS target_id,
                a.id AS action_id,
                a.status AS action_status,
                a.approval_expires_at,
                a.last_error_kind,
                a.payload->>'subreddit' AS subreddit,
                a.payload->>'title' AS draft_title,
                a.payload->>'body' AS draft_body,
                a.payload->>'image_url' AS image_url
            FROM viryaos_autopilot_actions a
            WHERE a.workspace_id = $1
              AND a.action_kind = 'community.engage.request'
              AND a.payload->>'source_id' ~
                  '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
              AND (a.payload->>'source_id')::uuid = $2
              AND a.payload->>'target_id' ~
                  '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
            ORDER BY (a.payload->>'target_id')::uuid, a.created_at DESC
        ),
        targets AS (
            SELECT target_id FROM decided
            UNION
            SELECT target_id FROM engaged
        )
        SELECT
            t.target_id::text AS target_id,
            COALESCE(e.subreddit, ot.subreddit) AS subreddit,
            ot.display_name,
            dc.confidence_bp,
            e.action_id::text AS action_id,
            e.action_status,
            e.approval_expires_at,
            dr.status AS draft_status,
            -- The most recent failure wins the reason — a post that failed
            -- after a succeeded action carries it on the post row, a dead
            -- drafting run carries it on the dispatch.
            COALESCE(p.error_message, e.last_error_kind, dr.last_error_kind) AS error_kind,
            e.draft_title,
            e.draft_body,
            e.image_url,
            p.id::text AS community_post_id,
            p.status AS post_status,
            p.reddit_post_url,
            p.posted_at,
            latest.score,
            latest.upvotes,
            latest.num_comments,
            latest.upvote_ratio,
            latest.measured_at,
            re.status AS reach_status,
            ge.observed_fans,
            ge.converted,
            count(*) OVER () AS total_targets
        FROM targets t
        LEFT JOIN decided dc ON dc.target_id = t.target_id
        LEFT JOIN engaged e ON e.target_id = t.target_id
        LEFT JOIN agent_outreach_targets ot
          ON ot.workspace_id = $1 AND ot.id = t.target_id
        -- The drafting dispatch — its failure is why a target can sit with
        -- no approval ask at all.
        LEFT JOIN LATERAL (
            SELECT dr.status, dr.last_error_kind
            FROM viryaos_autopilot_actions dr
            WHERE dr.workspace_id = $1
              AND dr.action_kind = 'agent.run.request'
              AND dr.idempotency_key =
                  'action:relay:' || $2::text || ':community:' || t.target_id::text
            ORDER BY dr.created_at DESC
            LIMIT 1
        ) dr ON true
        -- The receipt belongs to the community — the latest post for the
        -- target is the proof regardless of which action attempt made it.
        -- Scoped through the action's source_id so a post from an earlier
        -- run on the same community never shows as this run's receipt.
        LEFT JOIN LATERAL (
            SELECT p.id, p.status, p.reddit_post_url, p.posted_at, p.error_message
            FROM community_posts p
            JOIN viryaos_autopilot_actions pa
              ON pa.workspace_id = p.workspace_id AND pa.id = p.action_id
             AND pa.action_kind = 'community.engage.request'
             AND pa.payload->>'source_id' ~
                 '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
             AND (pa.payload->>'source_id')::uuid = $2
            WHERE p.workspace_id = $1 AND p.target_id = t.target_id
            ORDER BY p.created_at DESC
            LIMIT 1
        ) p ON true
        LEFT JOIN LATERAL (
            SELECT m.score, m.upvotes, m.num_comments, m.upvote_ratio, m.measured_at
            FROM community_post_metrics m
            WHERE m.workspace_id = $1 AND m.community_post_id = p.id
            ORDER BY m.measured_at DESC
            LIMIT 1
        ) latest ON true
        -- Reach is one row per recipient with the status updated in place,
        -- but nothing enforces one recipient per action — pick the furthest-
        -- advanced row so a second recipient can never fork the target.
        LEFT JOIN LATERAL (
            SELECT re.status
            FROM viryaos_reach_events re
            WHERE re.workspace_id = $1 AND re.action_id = e.action_id
              AND re.channel = 'reddit_post'
            ORDER BY CASE re.status
                         WHEN 'converted' THEN 8
                         WHEN 'positive_reply' THEN 7
                         WHEN 'replied' THEN 6
                         WHEN 'declined' THEN 5
                         WHEN 'complained' THEN 4
                         WHEN 'clicked' THEN 3
                         WHEN 'opened' THEN 2
                         WHEN 'delivered' THEN 1
                         ELSE 0
                     END DESC,
                     re.status_updated_at DESC
            LIMIT 1
        ) re ON true
        LEFT JOIN viryaos_growth_episodes ge
          ON ge.workspace_id = $1 AND ge.action_id = e.action_id
        ORDER BY
            CASE WHEN e.action_status = 'awaiting_approval'
                       AND (e.approval_expires_at IS NULL OR e.approval_expires_at > now())
                 THEN 0 ELSE 1 END,
            COALESCE(e.subreddit, ot.subreddit, '') ASC
        LIMIT $3
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(source_id)
    .bind(RELAY_TARGETS_LIMIT)
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    let push = sqlx::query_as::<_, RelayPushRow>(
        r#"
        SELECT
            a.id::text AS action_id,
            a.status,
            a.approval_expires_at,
            CASE WHEN a.payload->>'audience_size' ~ '^[0-9]+$'
                 THEN (a.payload->>'audience_size')::bigint END AS audience_size
        FROM viryaos_autopilot_actions a
        WHERE a.workspace_id = $1
          AND a.action_kind = 'signal.push.request'
          AND a.payload->>'task_id' = $2::text
        ORDER BY a.created_at DESC
        LIMIT 1
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(source_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    // count(*) OVER () rides on every returned row — if the LIMIT cut the
    // fan-out short, the first row's total is larger than what came back.
    let targets_total = targets
        .first()
        .map_or(0, |row| row.total_targets);
    let targets_truncated =
        targets_total > i64::try_from(targets.len()).unwrap_or(i64::MAX);

    Ok(Some(ProcessRelayRunDetail {
        source_id: header.source_id,
        title: header.title,
        platform: header.platform,
        source_url: header.source_url,
        thumbnail_url: header.thumbnail_url,
        body: header.body,
        occurred_at: header.occurred_at,
        decided_at: header.decided_at,
        confidence_bp: header.confidence_bp,
        push: push.map(|p| RelayPushLeg {
            action_id: p.action_id,
            status: p.status,
            audience_size: p.audience_size,
            approval_expires_at: p.approval_expires_at,
        }),
        targets_truncated,
        targets_total,
        targets: targets.into_iter().map(RelayTarget::from).collect(),
    }))
}

#[derive(Debug, FromRow)]
struct RelayRunHeaderRow {
    source_id: String,
    decided_at: OffsetDateTime,
    confidence_bp: i32,
    title: Option<String>,
    occurred_at: Option<OffsetDateTime>,
    platform: Option<String>,
    source_url: Option<String>,
    thumbnail_url: Option<String>,
    body: Option<String>,
}

#[derive(Debug, FromRow)]
struct RelayPushRow {
    action_id: String,
    status: String,
    approval_expires_at: Option<OffsetDateTime>,
    audience_size: Option<i64>,
}

#[derive(Debug, FromRow)]
struct RelayTargetRow {
    target_id: String,
    subreddit: Option<String>,
    display_name: Option<String>,
    confidence_bp: Option<i32>,
    action_id: Option<String>,
    action_status: Option<String>,
    approval_expires_at: Option<OffsetDateTime>,
    draft_status: Option<String>,
    error_kind: Option<String>,
    draft_title: Option<String>,
    draft_body: Option<String>,
    image_url: Option<String>,
    community_post_id: Option<String>,
    post_status: Option<String>,
    reddit_post_url: Option<String>,
    posted_at: Option<OffsetDateTime>,
    score: Option<i32>,
    upvotes: Option<i32>,
    num_comments: Option<i32>,
    upvote_ratio: Option<f64>,
    measured_at: Option<OffsetDateTime>,
    reach_status: Option<String>,
    observed_fans: Option<f64>,
    converted: Option<bool>,
    total_targets: i64,
}

impl From<RelayTargetRow> for RelayTarget {
    fn from(row: RelayTargetRow) -> Self {
        // One word per step position, derived here so every consumer reads
        // the same state — the page never re-interprets three timestamps
        // into its own vocabulary.
        let state = match row.action_status.as_deref() {
            // No approval ask yet — the drafting dispatch decides whether
            // this reads as still-moving or dead.
            None => match row.draft_status.as_deref() {
                Some("failed") => "failed",
                Some("cancelled") => "skipped",
                _ => "deciding",
            },
            Some("awaiting_approval") => match row.approval_expires_at {
                Some(expires) if expires <= OffsetDateTime::now_utc() => "expired",
                _ => "awaiting_you",
            },
            Some("queued") | Some("processing") => "queued",
            Some("cancelled") => "skipped",
            Some("failed") => "failed",
            Some("succeeded") => match row.post_status.as_deref() {
                None => "posting",
                Some("posted") => "posted",
                Some("awaiting_manual_post") => "manual",
                Some("failed") => "failed",
                Some("pending") | Some("posting") | Some("rate_limited") => "posting",
                Some(_) => "posting",
            },
            Some(_) => "deciding",
        };
        Self {
            target_id: row.target_id,
            subreddit: row.subreddit,
            display_name: row.display_name,
            confidence_bp: row.confidence_bp,
            state: state.to_owned(),
            action_id: row.action_id,
            community_post_id: row.community_post_id,
            draft_title: row.draft_title,
            draft_body: row.draft_body,
            image_url: row.image_url,
            approval_expires_at: row.approval_expires_at,
            post_status: row.post_status,
            reddit_post_url: row.reddit_post_url,
            posted_at: row.posted_at,
            score: row.score,
            upvotes: row.upvotes,
            num_comments: row.num_comments,
            upvote_ratio: row.upvote_ratio,
            measured_at: row.measured_at,
            reach_status: row.reach_status,
            observed_fans: row.observed_fans,
            converted: row.converted,
            error_kind: row.error_kind,
        }
    }
}

#[cfg(test)]
mod target_state_tests {
    use super::*;
    use time::Duration;

    /// A decided-but-not-yet-acted-on community row — the `None` action state
    /// every assertion below mutates one field away from.
    fn row() -> RelayTargetRow {
        RelayTargetRow {
            target_id: "t".to_owned(),
            subreddit: Some("indieheads".to_owned()),
            display_name: None,
            confidence_bp: Some(8400),
            action_id: None,
            action_status: None,
            approval_expires_at: None,
            draft_status: None,
            error_kind: None,
            draft_title: None,
            draft_body: None,
            image_url: None,
            community_post_id: None,
            post_status: None,
            reddit_post_url: None,
            posted_at: None,
            score: None,
            upvotes: None,
            num_comments: None,
            upvote_ratio: None,
            measured_at: None,
            reach_status: None,
            observed_fans: None,
            converted: None,
            total_targets: 0,
        }
    }

    fn state(row: RelayTargetRow) -> String {
        RelayTarget::from(row).state
    }

    /// No action row yet: the community was decided but the draft is still
    /// queued, running, or failed before an approval existed.
    #[test]
    fn no_action_is_deciding() {
        assert_eq!(state(row()), "deciding");
    }

    /// The approval ask is live while its window is; a lapsed window is its
    /// own word — the operator sees "expired", not a still-pending ask.
    #[test]
    fn awaiting_splits_on_the_window() {
        let mut open = row();
        open.action_status = Some("awaiting_approval".to_owned());
        assert_eq!(state(open), "awaiting_you");

        let mut within = row();
        within.action_status = Some("awaiting_approval".to_owned());
        within.approval_expires_at = Some(OffsetDateTime::now_utc() + Duration::hours(2));
        assert_eq!(state(within), "awaiting_you");

        let mut lapsed = row();
        lapsed.action_status = Some("awaiting_approval".to_owned());
        lapsed.approval_expires_at = Some(OffsetDateTime::now_utc() - Duration::minutes(1));
        assert_eq!(state(lapsed), "expired");
    }

    /// Approved and handed off, but no post row yet — the executor is on it.
    #[test]
    fn executor_states_read_as_moving() {
        for status in ["queued", "processing"] {
            let mut r = row();
            r.action_status = Some(status.to_owned());
            assert_eq!(state(r), "queued");
        }
        let mut posted_no_row = row();
        posted_no_row.action_status = Some("succeeded".to_owned());
        assert_eq!(state(posted_no_row), "posting");
    }

    /// The community post row is the receipt: its status wins over the
    /// action's own "succeeded".
    #[test]
    fn the_post_row_is_the_proof() {
        for (post_status, expected) in [
            ("posted", "posted"),
            ("awaiting_manual_post", "manual"),
            ("failed", "failed"),
            ("pending", "posting"),
            ("posting", "posting"),
            ("rate_limited", "posting"),
        ] {
            let mut r = row();
            r.action_status = Some("succeeded".to_owned());
            r.post_status = Some(post_status.to_owned());
            assert_eq!(state(r), expected, "post_status={post_status}");
        }
    }

    /// Cancelled reads as the operator's "skip"; failed carries its reason
    /// through to the response so the row can say why.
    #[test]
    fn terminal_states_keep_their_reason() {
        let mut skipped = row();
        skipped.action_status = Some("cancelled".to_owned());
        assert_eq!(state(skipped), "skipped");

        let mut failed = row();
        failed.action_status = Some("failed".to_owned());
        failed.error_kind = Some("rate_limited".to_owned());
        let target = RelayTarget::from(failed);
        assert_eq!(target.state, "failed");
        assert_eq!(target.error_kind.as_deref(), Some("rate_limited"));
    }

    /// A status the vocabulary does not know yet falls back to the earliest
    /// step rather than masquerading as a known one.
    #[test]
    fn unknown_statuses_read_as_deciding() {
        let mut r = row();
        r.action_status = Some("reconciling".to_owned());
        assert_eq!(state(r), "deciding");
    }

    /// No approval ask ever materialized because the drafting dispatch died —
    /// the community reads as failed (with the dispatch's reason) instead of
    /// sitting in "deciding" forever.
    #[test]
    fn a_dead_drafting_leg_is_failed() {
        let mut r = row();
        r.draft_status = Some("failed".to_owned());
        r.error_kind = Some("provider_error".to_owned());
        let target = RelayTarget::from(r);
        assert_eq!(target.state, "failed");
        assert_eq!(target.error_kind.as_deref(), Some("provider_error"));

        let mut cancelled = row();
        cancelled.draft_status = Some("cancelled".to_owned());
        assert_eq!(state(cancelled), "skipped");

        // Still moving — queued or running drafts stay in "deciding".
        for status in ["queued", "processing", "awaiting_approval"] {
            let mut r = row();
            r.draft_status = Some(status.to_owned());
            assert_eq!(state(r), "deciding", "draft_status={status}");
        }
    }
}
