// The hook scorecard: which of the band's own posts held attention over the
// last 60 days, judged against the band's own medians
// (`crowdrelay_domain::hook_scorecard`), and which of those exact sources
// created first-party fans. Attention remains its own verdict; acquisition is
// shown beside it so the next creator can distinguish a hook people watched
// from a hook that actually moved somebody into the fan graph. The Content
// page reads it and the drafters consume the same evidence.

/// The window the medians are taken over — about two months of posting, so
/// a median has enough posts behind it without reaching back to a different
/// era of the band.
const HOOK_WINDOW_DAYS: i64 = 60;
/// Posts the read returns at most.
const HOOK_POST_LIMIT: i64 = 60;

type HookRow = (
    Uuid,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    OffsetDateTime,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
);

pub async fn content_hooks(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match load_content_hooks(&state.ops.pool, workspace_id, OffsetDateTime::now_utc()).await {
        Ok(body) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(body),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "content hooks read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

async fn load_content_hooks(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Value, sqlx::Error> {
    use crowdrelay_domain::hook_scorecard::{HookPost, parse_social_count, score_hooks};

    // Metadata numbers are written by the social-post sync and its insights
    // refresh; a key that was never written reads as NULL, never as zero.
    let rows = sqlx::query_as::<_, HookRow>(
        r#"
        SELECT source.id,
               source.metadata->>'platform',
               source.metadata->>'media_type',
               source.metadata->>'body',
               source.metadata->>'url',
               source.occurred_at,
               source.metadata->>'reach',
               source.metadata->>'avg_watch_ms',
               source.metadata->>'saves',
               source.metadata->>'shares',
               source.metadata->>'views',
               COALESCE(outcome.fans_acquired, 0)::bigint AS fans_acquired,
               COALESCE(outcome.fans_activated_within_30d, 0)::bigint AS fans_activated_within_30d
        FROM content_sources AS source
        LEFT JOIN LATERAL (
            SELECT
                count(DISTINCT provenance.fan_id)::bigint AS fans_acquired,
                count(DISTINCT provenance.fan_id) FILTER (
                    WHERE fan.status = 'active'
                      AND consent.granted
                      AND fan_has_meaningful_action_between(
                          fan.workspace_id, fan.id, fan.normalized_email,
                          fan.created_at,
                          LEAST($2, fan.created_at + interval '30 days')
                      )
                )::bigint AS fans_activated_within_30d
            FROM autopilot_actions AS action
            JOIN fan_provenance_events AS provenance
              ON provenance.workspace_id = action.workspace_id
             AND provenance.action_id = action.id
             AND provenance.event_kind = 'conversion'
             AND provenance.attribution_method = 'last_tracked_click'
            JOIN fans AS fan
              ON fan.workspace_id = provenance.workspace_id
             AND fan.id = provenance.fan_id
            LEFT JOIN LATERAL (
                SELECT latest.granted
                FROM fan_consents AS latest
                WHERE latest.workspace_id = fan.workspace_id
                  AND latest.fan_id = fan.id
                  AND latest.purpose = 'marketing'
                  AND latest.recorded_at <= $2
                ORDER BY latest.recorded_at DESC, latest.id DESC
                LIMIT 1
            ) AS consent ON true
            WHERE action.workspace_id = source.workspace_id
              AND (
                  lower(action.payload->>'source_id') = source.id::text
                  OR lower(action.payload->'draft'->>'source_id') = source.id::text
              )
              AND provenance.occurred_at >= source.occurred_at
              AND provenance.occurred_at >= $2 - interval '60 days'
              AND provenance.occurred_at <= $2
              AND provenance.occurred_at <
                  source.occurred_at + interval '14 days'
        ) AS outcome ON true
        WHERE source.workspace_id = $1
          AND source.source_kind = 'social_post'
          AND source.occurred_at >= $2 - make_interval(days => $3::int)
          AND source.occurred_at <= $2
        ORDER BY source.occurred_at DESC
        LIMIT $4
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(i32::try_from(HOOK_WINDOW_DAYS).unwrap_or(60))
    .bind(HOOK_POST_LIMIT)
    .fetch_all(pool)
    .await?;

    let inputs: Vec<HookPost> = rows
        .iter()
        .map(|row| HookPost {
            is_video: row
                .2
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("video")),
            reach: parse_social_count(row.6.as_deref()),
            avg_watch_ms: parse_social_count(row.7.as_deref()),
            saves: parse_social_count(row.8.as_deref()),
            shares: parse_social_count(row.9.as_deref()),
        })
        .collect();
    let scores = score_hooks(&inputs);
    let posts: Vec<Value> = rows
        .into_iter()
        .zip(scores)
        .map(|(row, score)| {
            // The opening line is what a hook is made of in text; the video's
            // first seconds are the band's to look at through the link.
            let opening = row
                .3
                .as_deref()
                .and_then(|body| body.lines().find(|line| !line.trim().is_empty()))
                .map(|line| line.chars().take(160).collect::<String>());
            let reach = parse_social_count(row.6.as_deref());
            let fan_conversion_per_1000_reach = reach
                .filter(|reach| *reach > 0)
                .map(|reach| row.11.saturating_mul(1_000) / reach);
            let fan_activation_bps = (row.11 > 0).then(|| row.12.saturating_mul(10_000) / row.11);
            json!({
                "id": row.0,
                "platform": row.1,
                "media_type": row.2,
                "opening": opening,
                "url": row.4,
                "posted_at": row.5,
                "reach": reach,
                "avg_watch_ms": parse_social_count(row.7.as_deref()),
                "saves": parse_social_count(row.8.as_deref()),
                "shares": parse_social_count(row.9.as_deref()),
                "views": parse_social_count(row.10.as_deref()),
                "fans_acquired": row.11,
                "fans_activated_within_30d": row.12,
                "fan_conversion_per_1000_reach": fan_conversion_per_1000_reach,
                "fan_activation_bps": fan_activation_bps,
                "verdict": score.verdict,
                "watch_index_bps": score.watch_index_bps,
                "keep_index_bps": score.keep_index_bps,
            })
        })
        .collect();
    let links = load_fan_links(pool, workspace_id, now).await?;
    Ok(json!({ "window_days": HOOK_WINDOW_DAYS, "posts": posts, "links": links }))
}

/// The tracked links that brought fans over the last 90 days, and how many of
/// those fans stayed — still active, consented to marketing (latest record)
/// and meaningfully active in the last 30 days, the definition the channel
/// and community rankings use. A post's own link (a story sticker, a bio
/// link per drop) is how an owned-channel post earns credit; the view names
/// the link's channel and creative label so the band can tell which one it
/// was.
async fn load_fan_links(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64, i64)>(
        r#"
        WITH converted AS (
            SELECT DISTINCT pe.channel, pe.source_target AS slug, pe.fan_id
            FROM fan_provenance_events AS pe
            WHERE pe.workspace_id = $1
              AND pe.event_kind = 'conversion'
              AND pe.fan_id IS NOT NULL
              AND pe.source_target IS NOT NULL
              AND pe.occurred_at >= $2 - interval '90 days'
        )
        SELECT converted.channel,
               converted.slug,
               link.channel_creative,
               link.destination_url,
               COUNT(*)::bigint AS fans,
               COUNT(*) FILTER (
                   WHERE fan.status = 'active'
                     AND EXISTS (
                         SELECT 1 FROM fan_consents AS consent
                         WHERE consent.workspace_id = fan.workspace_id
                           AND consent.fan_id = fan.id
                           AND consent.purpose = 'marketing'
                           AND consent.granted
                           AND consent.recorded_at = (
                               SELECT max(latest.recorded_at) FROM fan_consents AS latest
                               WHERE latest.workspace_id = fan.workspace_id
                                 AND latest.fan_id = fan.id
                                 AND latest.purpose = 'marketing'
                           )
                     )
                     AND fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                         >= $2 - interval '30 days'
               )::bigint AS stayed
        FROM converted
        JOIN fans AS fan
          ON fan.workspace_id = $1
         AND fan.id = converted.fan_id
        JOIN smart_links AS link
          ON link.workspace_id = $1
         AND link.slug = converted.slug
        GROUP BY 1, 2, 3, 4
        ORDER BY stayed DESC, fans DESC, 1, 2
        LIMIT 20
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(channel, slug, creative, destination, fans, stayed)| {
            json!({
                "channel": channel,
                "slug": slug,
                "creative": creative,
                "destination_url": destination,
                "fans": fans,
                "stayed": stayed,
            })
        })
        .collect())
}

#[cfg(test)]
mod hooks_postgres_tests {
    use super::*;

    #[tokio::test]
    async fn the_hook_read_judges_each_post_against_the_bands_own_median() {
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

        let workspace_id = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
            .bind(workspace_id)
            .bind(format!("hooks-{}", workspace_id.simple()))
            .bind("Hook Scorecard Tests")
            .execute(&pool)
            .await
            .expect("workspace");
        let now = OffsetDateTime::now_utc();
        // Four ordinary reels and one watched half again as long.
        let mut source_ids = Vec::new();
        for (index, watch_ms) in [4_000_i64, 4_000, 4_000, 4_000, 6_000]
            .into_iter()
            .enumerate()
        {
            let source_id = Uuid::now_v7();
            sqlx::query(
                r#"INSERT INTO content_sources
                   (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
                   VALUES ($1,$2,'social_post',$3,'reel',$4,$4 + interval '45 days',
                           jsonb_build_object('platform','instagram','media_type','VIDEO',
                                              'body', E'\nOpening line ' || $5::text || E'\nrest',
                                              'reach',1000,'avg_watch_ms',$6::bigint,
                                              'saves',5,'shares',5))"#,
            )
            .bind(source_id)
            .bind(workspace_id)
            .bind(format!("instagram:{index}"))
            .bind(now - time::Duration::days(i64::try_from(index).unwrap_or(0) + 1))
            .bind(index.to_string())
            .bind(watch_ms)
            .execute(&pool)
            .await
            .expect("post");
            source_ids.push(source_id);
        }

        // The long-watched reel also acquired two fans through actions that
        // explicitly carry this source. One of them opened a real first-party
        // session inside 30 days of signup; the other only signed up. The
        // scorecard must keep attention and fan outcome as separate facts.
        let held_source_id = source_ids[4];
        sqlx::query("UPDATE content_sources SET occurred_at=$2 WHERE id=$1 AND workspace_id=$3")
            .bind(held_source_id)
            .bind(now - time::Duration::days(35))
            .bind(workspace_id)
            .execute(&pool)
            .await
            .expect("older cohort");
        let decision_id = Uuid::now_v7();
        let action_id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO autopilot_decisions
               (id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id)
               VALUES ($1,$2,$3,'content_supply','content_source',$4,
                       'seed.hook_outcome',9000,'auto_execute','hook test',
                       '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
        )
        .bind(decision_id)
        .bind(workspace_id)
        .bind(format!("hook-outcome-{}", workspace_id.simple()))
        .bind(held_source_id)
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .expect("decision");
        sqlx::query(
            r#"INSERT INTO autopilot_actions
               (id, workspace_id, decision_id, context, action_kind, subject_kind,
                subject_id, idempotency_key, payload, status, finished_at)
               VALUES ($1,$2,$3,'content_supply','community.engage.request',
                       'content_source',$4,$5,$6,'succeeded',$7)"#,
        )
        .bind(action_id)
        .bind(workspace_id)
        .bind(decision_id)
        .bind(held_source_id)
        .bind(format!("hook-outcome-action-{}", workspace_id.simple()))
        .bind(json!({
            "kind": "request_community_engagement",
            "source_id": held_source_id.to_string(),
            "platform": "reddit",
        }))
        .bind(now - time::Duration::days(4))
        .execute(&pool)
        .await
        .expect("action");

        for index in 0..2 {
            let fan_id = Uuid::now_v7();
            let created_at = now - time::Duration::days(34);
            sqlx::query(
                r#"INSERT INTO fans
                   (id, workspace_id, normalized_email, status, created_at, updated_at)
                   VALUES ($1,$2,$3,'active',$4,$4)"#,
            )
            .bind(fan_id)
            .bind(workspace_id)
            .bind(format!(
                "hook-fan-{index}-{}@example.test",
                workspace_id.simple()
            ))
            .bind(created_at)
            .execute(&pool)
            .await
            .expect("fan");
            sqlx::query(
                r#"INSERT INTO fan_consents
                   (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
                   VALUES ($1,$2,'marketing',true,'privacy-v1','hook-test',$3)"#,
            )
            .bind(workspace_id)
            .bind(fan_id)
            .bind(created_at)
            .execute(&pool)
            .await
            .expect("consent");
            sqlx::query(
                r#"INSERT INTO fan_provenance_events
                   (workspace_id, fan_id, event_kind, channel, source_target,
                    action_id, attribution_method, attribution_confidence, occurred_at)
                   VALUES ($1,$2,'conversion','instagram','held-hook',$3,
                           'last_tracked_click',1.0,$4)"#,
            )
            .bind(workspace_id)
            .bind(fan_id)
            .bind(action_id)
            .bind(created_at + time::Duration::hours(1))
            .execute(&pool)
            .await
            .expect("provenance");

            if index == 0 {
                let mut session_hash = fan_id.as_bytes().to_vec();
                session_hash.extend_from_slice(fan_id.as_bytes());
                sqlx::query(
                    r#"INSERT INTO fan_sessions
                       (workspace_id, fan_id, session_token_hash, created_at,
                        last_seen_at, expires_at)
                       VALUES ($1,$2,$3,$4,$5,$6)"#,
                )
                .bind(workspace_id)
                .bind(fan_id)
                .bind(session_hash)
                .bind(created_at)
                .bind(now - time::Duration::days(32))
                .bind(now + time::Duration::days(30))
                .execute(&pool)
                .await
                .expect("session");
                // A return after day 30 must not erase the earlier activation.
                let mut later_hash = fan_id.as_bytes().to_vec();
                later_hash.extend_from_slice(fan_id.as_bytes());
                later_hash[0] ^= 1;
                sqlx::query(
                    r#"INSERT INTO fan_sessions
                       (workspace_id, fan_id, session_token_hash, created_at,
                        last_seen_at, expires_at)
                       VALUES ($1,$2,$3,$4,$5,$6)"#,
                )
                .bind(workspace_id)
                .bind(fan_id)
                .bind(later_hash)
                .bind(created_at)
                .bind(now - time::Duration::days(2))
                .bind(now + time::Duration::days(30))
                .execute(&pool)
                .await
                .expect("later return");
            }
        }

        let body = load_content_hooks(&pool, workspace_id, now)
            .await
            .expect("hooks");
        let posts = body["posts"].as_array().expect("posts");
        assert_eq!(posts.len(), 5);
        let held = posts
            .iter()
            .find(|post| post["verdict"] == "held_attention")
            .expect("the long-watched reel is named");
        assert_eq!(held["avg_watch_ms"], 6_000);
        assert_eq!(held["watch_index_bps"], 15_000);
        assert_eq!(
            held["opening"], "Opening line 4",
            "the first non-empty line"
        );
        assert_eq!(held["fans_acquired"], 2);
        assert_eq!(held["fans_activated_within_30d"], 1);
        assert_eq!(held["fan_conversion_per_1000_reach"], 2);
        assert_eq!(held["fan_activation_bps"], 5_000);

        sqlx::query(
            "UPDATE content_sources SET metadata = metadata || $2 WHERE id=$1 AND workspace_id=$3",
        )
        .bind(held_source_id)
        .bind(json!({"reach": "unknown", "views": "9223372036854775808"}))
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("invalid optional metrics");
        let malformed = load_content_hooks(&pool, workspace_id, now)
            .await
            .expect("bad metrics do not break hooks");
        let malformed_post = malformed["posts"]
            .as_array()
            .expect("posts")
            .iter()
            .find(|post| post["id"] == held["id"])
            .expect("same source");
        assert!(malformed_post["reach"].is_null());
        assert!(malformed_post["views"].is_null());
        assert!(malformed_post["fan_conversion_per_1000_reach"].is_null());
        assert_eq!(malformed_post["fans_activated_within_30d"], 1);
        assert!(
            posts
                .iter()
                .filter(|post| post["id"] != held["id"])
                .all(|post| post["fans_acquired"] == 0),
            "fan outcome belongs to the exact source, not every post in the window"
        );
        assert_eq!(
            body["links"],
            json!([]),
            "source-level provenance does not fabricate smart-link rows"
        );
    }
}
