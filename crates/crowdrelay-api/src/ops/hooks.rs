// The hook scorecard: which of the band's own posts held attention over the
// last 60 days, judged against the band's own medians
// (`crowdrelay_domain::hook_scorecard`). The Content page reads it so the
// people making the next video see which openings worked; the drafters read
// the same verdicts through the agents service.

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
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

pub async fn content_hooks(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match load_content_hooks(&state.ops.pool, workspace_id, OffsetDateTime::now_utc()).await {
        Ok(body) => (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response(),
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
    use crowdrelay_domain::hook_scorecard::{HookPost, score_hooks};

    // Metadata numbers are written by the social-post sync and its insights
    // refresh; a key that was never written reads as NULL, never as zero.
    let rows = sqlx::query_as::<_, HookRow>(
        r#"
        SELECT id,
               metadata->>'platform',
               metadata->>'media_type',
               metadata->>'body',
               metadata->>'url',
               occurred_at,
               (metadata->>'reach')::bigint,
               (metadata->>'avg_watch_ms')::bigint,
               (metadata->>'saves')::bigint,
               (metadata->>'shares')::bigint,
               (metadata->>'views')::bigint
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'social_post'
          AND occurred_at >= $2 - make_interval(days => $3::int)
        ORDER BY occurred_at DESC
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
            is_video: row.2.as_deref().is_some_and(|kind| kind.eq_ignore_ascii_case("video")),
            reach: row.6,
            avg_watch_ms: row.7,
            saves: row.8,
            shares: row.9,
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
            json!({
                "id": row.0,
                "platform": row.1,
                "media_type": row.2,
                "opening": opening,
                "url": row.4,
                "posted_at": row.5,
                "reach": row.6,
                "avg_watch_ms": row.7,
                "saves": row.8,
                "shares": row.9,
                "views": row.10,
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
        LEFT JOIN smart_links AS link
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
        for (index, watch_ms) in [4_000_i64, 4_000, 4_000, 4_000, 6_000].into_iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO content_sources
                   (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
                   VALUES ($1,'social_post',$2,'reel',$3,$3 + interval '45 days',
                           jsonb_build_object('platform','instagram','media_type','VIDEO',
                                              'body', E'\nOpening line ' || $4::text || E'\nrest',
                                              'reach',1000,'avg_watch_ms',$5::bigint,
                                              'saves',5,'shares',5))"#,
            )
            .bind(workspace_id)
            .bind(format!("instagram:{index}"))
            .bind(now - time::Duration::days(i64::try_from(index).unwrap_or(0) + 1))
            .bind(index.to_string())
            .bind(watch_ms)
            .execute(&pool)
            .await
            .expect("post");
        }

        let body = load_content_hooks(&pool, workspace_id, now).await.expect("hooks");
        let posts = body["posts"].as_array().expect("posts");
        assert_eq!(posts.len(), 5);
        let held = posts
            .iter()
            .find(|post| post["verdict"] == "held_attention")
            .expect("the long-watched reel is named");
        assert_eq!(held["avg_watch_ms"], 6_000);
        assert_eq!(held["watch_index_bps"], 15_000);
        assert_eq!(held["opening"], "Opening line 4", "the first non-empty line");
        assert_eq!(body["links"], json!([]), "no conversions yet, no links");
    }
}
