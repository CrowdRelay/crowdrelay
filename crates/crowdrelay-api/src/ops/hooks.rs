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
    Ok(json!({ "window_days": HOOK_WINDOW_DAYS, "posts": posts }))
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
    }
}
