//! Monthly activation baseline: two bounded history probes, not a cycle replay.

pub(super) const ACTIVATED_FAN_MONTH_BASELINE_SQL: &str = r#"
    SELECT COALESCE(
        (
            SELECT north_star_value::bigint
            FROM autopilot_cycle_runs
            WHERE workspace_id = $1
              AND north_star_metric = 'activated_fans_30d'
              AND north_star_value IS NOT NULL
              AND started_at < date_trunc('month', $2::timestamptz)
            ORDER BY started_at DESC
            LIMIT 1
        ),
        (
            SELECT north_star_value::bigint
            FROM autopilot_cycle_runs
            WHERE workspace_id = $1
              AND north_star_metric = 'activated_fans_30d'
              AND north_star_value IS NOT NULL
              AND started_at >= date_trunc('month', $2::timestamptz)
              AND started_at <= $2
            ORDER BY started_at ASC
            LIMIT 1
        ),
        $3::bigint
    )::bigint
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn monthly_baseline_uses_only_comparable_past_readings() {
        let Ok(url) = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("database");
        let mut tx = pool.begin().await.expect("transaction");
        // A transaction-local shadow keeps this test independent of migrations
        // and of other tests' production-shaped cycle fixtures.
        sqlx::query(
            "CREATE TEMP TABLE autopilot_cycle_runs (workspace_id uuid, \
             north_star_metric text, north_star_value bigint, started_at timestamptz) \
             ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await
        .expect("history table");
        let workspace = uuid::Uuid::now_v7();
        let now = time::OffsetDateTime::from_unix_timestamp(1_790_769_600).expect("date");
        let read = |workspace, now| {
            sqlx::query_scalar::<_, i64>(ACTIVATED_FAN_MONTH_BASELINE_SQL)
                .bind(workspace)
                .bind(now)
                .bind(20_i64)
        };
        assert_eq!(read(workspace, now).fetch_one(&mut *tx).await.unwrap(), 20);
        sqlx::query(
            "INSERT INTO autopilot_cycle_runs VALUES \
             ($1,'activated_fans_30d',11,date_trunc('month',$2::timestamptz)), \
             ($1,'activated_fans_30d',99,$2 + interval '1 day'), \
             ($3,'activated_fans_30d',999,$2 - interval '40 days'), \
             ($1,'spotify_followers',999,$2 - interval '40 days')",
        )
        .bind(workspace)
        .bind(now)
        .bind(uuid::Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(read(workspace, now).fetch_one(&mut *tx).await.unwrap(), 11);
        sqlx::query(
            "INSERT INTO autopilot_cycle_runs VALUES \
             ($1,'activated_fans_30d',3,$2 - interval '60 days'), \
             ($1,'activated_fans_30d',7,$2 - interval '40 days'), \
             ($1,'activated_fans_30d',NULL,$2 - interval '35 days')",
        )
        .bind(workspace)
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(read(workspace, now).fetch_one(&mut *tx).await.unwrap(), 7);
        tx.rollback().await.expect("rollback");
    }
}
