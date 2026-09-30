#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn source_refresh_preserves_promotion_policy_campaign_and_request() -> Result<()> {
    let (pool, worker, workspace_id) = fixture().await?;
    let mut upload = entry("policy-refresh", OffsetDateTime::now_utc());
    worker
        .upsert_video("UCchan", &upload)
        .await
        .map_err(|error| anyhow!(error))?;
    let owned = serde_json::json!({
        "promotion_excluded_platforms":["reddit","facebook","instagram"],
        "promotion_campaign_id":Uuid::now_v7().to_string(),
        "surge_requested_at":"2026-09-30T00:00:00Z",
    });
    sqlx::query("UPDATE content_sources SET metadata=metadata || $2 WHERE workspace_id=$1 AND source_kind IN ('video','release')")
        .bind(workspace_id).bind(&owned).execute(&pool).await?;
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM content_sources WHERE workspace_id=$1 AND source_kind='video'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    worker
        .upsert_video("UCchan", &upload)
        .await
        .map_err(|error| anyhow!(error))?;
    let unchanged: i64 = sqlx::query_scalar(
        "SELECT version FROM content_sources WHERE workspace_id=$1 AND source_kind='video'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        version, unchanged,
        "policy metadata must not make an unchanged feed look edited"
    );
    upload.title = "Corrected source facts".to_owned();
    worker
        .upsert_video("UCchan", &upload)
        .await
        .map_err(|error| anyhow!(error))?;
    // This old producer still replaces release metadata. The additive trigger
    // preserves ownership during mixed-version rollout too.
    sqlx::query("UPDATE release_plans SET title='Corrected release' WHERE workspace_id=$1")
        .bind(workspace_id)
        .execute(&pool)
        .await?;
    let metadata: Vec<serde_json::Value>=sqlx::query_scalar("SELECT metadata FROM content_sources WHERE workspace_id=$1 AND source_kind IN ('video','release')")
        .bind(workspace_id).fetch_all(&pool).await?;
    assert_eq!(metadata.len(), 2);
    for metadata in metadata {
        for key in [
            "promotion_excluded_platforms",
            "promotion_campaign_id",
            "surge_requested_at",
        ] {
            assert_eq!(metadata[key], owned[key], "source refresh erased {key}");
        }
    }
    // Exercise rollback only inside a disposable transaction; restore both
    // DDL and source data before releasing the lock.
    let mut tx = pool.begin().await?;
    sqlx::query("SAVEPOINT promotion_rollback_proof")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP TRIGGER content_sources_preserve_promotion_metadata ON content_sources")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP FUNCTION crowdrelay_preserve_source_promotion_metadata()")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP FUNCTION crowdrelay_promotion_video_key(jsonb)")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE content_sources SET metadata='{}' WHERE workspace_id=$1 AND source_kind='release'",
    )
    .bind(workspace_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("ROLLBACK TO SAVEPOINT promotion_rollback_proof")
        .execute(&mut *tx)
        .await?;
    let restored: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM content_sources WHERE workspace_id=$1 AND source_kind='release'",
    )
    .bind(workspace_id)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(
        restored["promotion_campaign_id"],
        owned["promotion_campaign_id"]
    );
    tx.rollback().await?;
    Ok(())
}
