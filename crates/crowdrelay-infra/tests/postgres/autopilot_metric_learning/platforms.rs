use super::*;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::performance::EffectAssessment;

async fn release_claim(f: &Fixture, anchor: OffsetDateTime) -> ClaimedAutopilotMeasurement {
    let action = insert_dispatch(f, "community-engager:target:post:ctx", anchor).await;
    let mut claim = queue_measurement(
        f,
        action,
        AutopilotMeasurementKind::ReleaseChannelLift14d,
        0.0,
        anchor,
    )
    .await;
    claim.subject_id = uuid::Uuid::now_v7();
    sqlx::query("UPDATE autopilot_measurements SET subject_id=$2 WHERE id=$1")
        .bind(claim.id.into_uuid())
        .bind(claim.subject_id)
        .execute(&f.pool)
        .await
        .unwrap();
    claim
}

async fn series(
    f: &Fixture,
    claim: &ClaimedAutopilotMeasurement,
    platform: &str,
    metric: &str,
    direction: &str,
    points: &[(i64, i64)],
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO growth_metric_series(id,workspace_id,platform,metric_key,subject_kind,subject_id,display_name,direction) VALUES($1,$2,$3,$4,'release_plan',$5,$4,$6)")
        .bind(id).bind(f.workspace_id.into_uuid()).bind(platform).bind(metric).bind(claim.subject_id).bind(direction).execute(&f.pool).await.unwrap();
    for &(day, value) in points {
        sqlx::query("INSERT INTO growth_metric_points(workspace_id,series_id,captured_at,value,source) VALUES($1,$2,$3,$4,'test')")
            .bind(f.workspace_id.into_uuid()).bind(id).bind(claim.action_finished_at + time::Duration::days(day)).bind(value).execute(&f.pool).await.unwrap();
    }
    id
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn platform_lifts_keep_units_and_survive_checkpoint_without_relearning() {
    let f = setup().await.unwrap();
    let claim = release_claim(&f, f.now - time::Duration::days(20)).await;
    let followers = series(
        &f,
        &claim,
        "spotify",
        "followers",
        "higher_is_better",
        &[(-20, 100), (-1, 110), (12, 110)],
    )
    .await;
    let views = series(
        &f,
        &claim,
        "youtube",
        "views",
        "higher_is_better",
        &[(-20, 100), (-1, 110), (12, 10120)],
    )
    .await;
    let observed = f
        .repository
        .observe_measurement_with_metrics(f.workspace_id, &claim, f.now)
        .await
        .unwrap();
    assert_eq!(observed.value, 9990.0);
    assert_eq!(observed.series_lifts.len(), 2);
    let effect = observed
        .assess_effect(&claim, &HarmObservation::default())
        .unwrap();
    assert_eq!(effect.assessment, EffectAssessment::Neutral);
    resolve_with_metrics(&f, &claim, observed.clone()).await;
    let metrics: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(claim.action_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(metrics["release_channel_lift:spotify:followers"], -10.0);
    assert_eq!(metrics["release_channel_lift:youtube:views"], 10000.0);
    assert!(metrics.get("release_channel_lift").is_none());
    let metadata: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM autopilot_outcomes WHERE workspace_id=$1 AND measurement_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(claim.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(metadata["scalar_aggregate_is_mixed"], true);
    let ids: Vec<_> = metadata["series_lifts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["series_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&followers.to_string()) && ids.contains(&views.to_string()));
    // The status/unique-outcome guard must reject a retry before any metric
    // can be added twice, even when the worker repeats completion.
    assert!(
        f.repository
            .complete_measurement_with_metrics(
                f.workspace_id,
                &claim,
                &observed,
                effect,
                None,
                f.now
            )
            .await
            .is_err()
    );
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT observed_metrics FROM growth_evidence WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(claim.action_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(after, metrics);
    let loaded = f
        .repository
        .load_causal_model(f.workspace_id)
        .await
        .unwrap();
    let follower = loaded
        .model
        .predict_metric_stats(
            "release_channel_lift:spotify:followers",
            "community-engager",
            None,
        )
        .unwrap();
    let view = loaded
        .model
        .predict_metric_stats(
            "release_channel_lift:youtube:views",
            "community-engager",
            None,
        )
        .unwrap();
    assert!(follower.0 < 0.0 && view.0 > 0.0);
    assert!(
        loaded
            .model
            .predict_metric_stats("release_channel_lift", "community-engager", None)
            .is_none()
    );
    f.repository
        .save_brain_state(
            f.workspace_id,
            "causal_model",
            &serde_json::to_value(loaded.model).unwrap(),
        )
        .await
        .unwrap();
    let (stored, _) = f
        .repository
        .load_brain_state(f.workspace_id, "causal_model")
        .await
        .unwrap()
        .unwrap();
    let expected: crowdrelay_brain::CausalModel = serde_json::from_value(stored).unwrap();
    let repeated = f
        .repository
        .load_causal_model(f.workspace_id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(repeated.model).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn platform_lifts_require_fresh_complete_series_and_preserve_direction() {
    let f = setup().await.unwrap();
    let mut claim = release_claim(&f, f.now - time::Duration::days(20)).await;
    series(
        &f,
        &claim,
        "spotify",
        "followers",
        "higher_is_better",
        &[(-20, 100), (12, 120)],
    )
    .await;
    // The old query borrowed the -20d point as pre_end. A missing second
    // baseline now makes this series unavailable, rather than a success.
    assert!(matches!(
        f.repository
            .observe_measurement_with_metrics(f.workspace_id, &claim, f.now)
            .await,
        Err(crowdrelay_application::RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_RELEASE_SERIES_DATA
        ))
    ));
    let loss = series(
        &f,
        &claim,
        "youtube",
        "unsubscribes",
        "lower_is_better",
        &[(-20, 100), (-1, 110), (12, 110)],
    )
    .await;
    let observed = f
        .repository
        .observe_measurement_with_metrics(f.workspace_id, &claim, f.now)
        .await
        .unwrap();
    assert_eq!(observed.series_lifts.len(), 1);
    assert_eq!(observed.series_lifts[0].series_id, loss);
    assert_eq!(observed.series_lifts[0].lift, -10.0);
    assert_eq!(
        observed
            .assess_effect(&claim, &HarmObservation::default())
            .unwrap()
            .assessment,
        EffectAssessment::Improved
    );
    // Future post points cannot become facts merely because a caller asks
    // to observe early. Readiness may reject before the series reader does.
    claim.action_finished_at = f.now - time::Duration::days(10);
    claim.subject_id = uuid::Uuid::now_v7();
    series(
        &f,
        &claim,
        "youtube",
        "views",
        "higher_is_better",
        &[(-20, 100), (-1, 110), (12, 10000)],
    )
    .await;
    assert!(
        f.repository
            .observe_measurement_with_metrics(f.workspace_id, &claim, f.now)
            .await
            .is_err()
    );
}
