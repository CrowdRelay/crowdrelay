//! Public leaderboard read/publish transport kept separate from run lifecycle.

use super::*;
use axum::extract::{Query, rejection::QueryRejection};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaderboardQuery {
    limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct LeaderboardEntryResponse {
    rank: u16,
    display_name: String,
    elapsed_ms: i64,
}

#[derive(Debug, Serialize)]
struct LeaderboardResponse {
    items: Vec<LeaderboardEntryResponse>,
}

#[derive(Debug, Serialize)]
struct LeaderboardPublishResponse {
    published: bool,
    display_name: String,
    rank: i64,
    best_elapsed_ms: i64,
}

pub async fn list_leaderboard(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Result<Query<LeaderboardQuery>, QueryRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let limit = query.limit.unwrap_or(LEADERBOARD_DEFAULT_LIMIT);
    if limit == 0 || limit > LEADERBOARD_MAX_LIMIT {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let rows = sqlx::query_as::<_, (String, i64)>(
        r#"
        WITH best AS (
            SELECT DISTINCT ON (run.fan_id)
                run.leaderboard_name AS display_name,
                run.client_total_elapsed_ms AS elapsed_ms,
                run.completed_at,
                run.id
            FROM synesthesia_runs AS run
            WHERE run.workspace_id = $1
              AND run.campaign_slug = $2
              AND run.fan_id IS NOT NULL
              AND run.completed_at IS NOT NULL
              AND run.client_total_elapsed_ms IS NOT NULL
              AND run.leaderboard_name IS NOT NULL
            ORDER BY run.fan_id, run.client_total_elapsed_ms, run.completed_at, run.id
        )
        SELECT display_name, elapsed_ms
        FROM best
        ORDER BY elapsed_ms, completed_at, id
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(i64::from(limit))
    .fetch_all(state.ticketing.pool())
    .await;

    match rows {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .enumerate()
                .map(
                    |(index, (display_name, elapsed_ms))| LeaderboardEntryResponse {
                        rank: u16::try_from(index + 1).unwrap_or(u16::MAX),
                        display_name,
                        elapsed_ms,
                    },
                )
                .collect();
            (
                StatusCode::OK,
                [(
                    CACHE_CONTROL,
                    HeaderValue::from_static(PUBLIC_LEADERBOARD_CACHE),
                )],
                Json(LeaderboardResponse { items }),
            )
                .into_response()
        }
        Err(error) => SynesthesiaError::sqlx(error).response(request_id_value),
    }
}

pub async fn publish_leaderboard(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let authorized = sqlx::query_as::<_, (String, Uuid, String)>(
        r#"
        SELECT run.campaign_slug, run.fan_id, fan.normalized_email
        FROM synesthesia_runs AS run
        INNER JOIN fans AS fan
          ON fan.workspace_id = run.workspace_id AND fan.id = run.fan_id
        WHERE run.workspace_id = $1 AND run.id = $2 AND run.run_token_hash = $3
          AND run.completed_at IS NOT NULL
        FOR SHARE OF run, fan
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(&token_hash)
    .fetch_optional(&mut *transaction)
    .await;
    let (campaign_slug, fan_id, normalized_email) = match authorized {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if campaign_slug != CAMPAIGN_SLUG {
        return SynesthesiaError::Conflict.response(request_id_value);
    }
    let display_name = match masked_email_alias(&normalized_email) {
        Some(alias) => alias,
        None => return SynesthesiaError::Conflict.response(request_id_value),
    };

    if let Err(error) = sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET leaderboard_name = $4,
            leaderboard_published_at = COALESCE(leaderboard_published_at, now()),
            updated_at = now()
        WHERE workspace_id = $1 AND campaign_slug = $2 AND fan_id = $3
          AND completed_at IS NOT NULL AND client_total_elapsed_ms IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(fan_id)
    .bind(&display_name)
    .execute(&mut *transaction)
    .await
    {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    let ranked = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH best AS (
            SELECT DISTINCT ON (run.fan_id)
                run.fan_id,
                run.client_total_elapsed_ms AS elapsed_ms,
                run.completed_at,
                run.id
            FROM synesthesia_runs AS run
            WHERE run.workspace_id = $1
              AND run.campaign_slug = $2
              AND run.fan_id IS NOT NULL
              AND run.completed_at IS NOT NULL
              AND run.client_total_elapsed_ms IS NOT NULL
              AND run.leaderboard_name IS NOT NULL
            ORDER BY run.fan_id, run.client_total_elapsed_ms, run.completed_at, run.id
        ), ranked AS (
            SELECT fan_id, elapsed_ms,
                   ROW_NUMBER() OVER (ORDER BY elapsed_ms, completed_at, id)::bigint AS rank
            FROM best
        )
        SELECT rank, elapsed_ms FROM ranked WHERE fan_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(fan_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (rank, best_elapsed_ms) = match ranked {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(LeaderboardPublishResponse {
            published: true,
            display_name,
            rank,
            best_elapsed_ms,
        }),
    )
        .into_response()
}

pub(super) fn masked_email_alias(value: &str) -> Option<String> {
    let (local, domain) = value.rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    let local_prefix: String = local.chars().take(3).collect();
    (!local_prefix.is_empty()).then(|| format!("{local_prefix}••••"))
}
