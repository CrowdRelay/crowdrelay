use super::install_tests::{consented_fan, install_candidate, install_fixture};
use super::*;
use crowdrelay_domain::lifecycle_episode::LifecycleEpisode;

fn episode_candidate(
    fan: crowdrelay_domain::FanId,
    template: &str,
    episode: &LifecycleEpisode,
) -> DecisionCandidate {
    let mut candidate = install_candidate(fan, template);
    candidate.action_idempotency_key =
        format!("action:lifecycle-episode:{fan}:{template}:{}", episode.key);
    candidate
        .input_snapshot
        .as_object_mut()
        .expect("snapshot object")
        .insert(
            "lifecycle_episode".to_owned(),
            serde_json::to_value(episode).expect("episode"),
        );
    candidate
}

fn once() -> LifecycleEpisode {
    LifecycleEpisode {
        key: "once".to_owned(),
        since: None,
        ticket_count: None,
        event_slug: None,
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn historical_requests_are_not_replayed_under_new_episode_keys()
-> Result<(), Box<dyn std::error::Error>> {
    for state in [
        "awaiting_approval",
        "succeeded",
        "failed",
        "cancelled",
        "unknown",
    ] {
        let f = install_fixture(state).await?;
        let fan = consented_fan(&f).await?;
        let old = install_candidate(fan, "crowdrelay.fan.welcome.v1");
        assert!(persist(&f, f.workspace_id, &old).await?);
        sqlx::query("UPDATE autopilot_actions SET status=$3, finished_at=CASE WHEN $3 IN ('succeeded','failed','cancelled') THEN now() ELSE NULL END WHERE workspace_id=$1 AND idempotency_key=$2")
            .bind(f.workspace_id.into_uuid()).bind(&old.action_idempotency_key).bind(state).execute(&f.pool).await?;
        let new = episode_candidate(fan, "crowdrelay.fan.welcome.v1", &once());
        assert!(
            !persist(&f, f.workspace_id, &new).await?,
            "historical {state} request must not resend"
        );
        assert_eq!(
            action_state(&f, f.workspace_id, &old.action_idempotency_key)
                .await?
                .expect("old action")
                .0,
            state
        );
        let actions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM autopilot_actions WHERE workspace_id=$1 AND subject_id=$2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan.into_uuid())
        .fetch_one(&f.pool)
        .await?;
        assert_eq!(actions, 1);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn concurrent_variants_of_the_same_episode_create_only_one_action()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("episode-race").await?;
    let fan = consented_fan(&f).await?;
    let first = episode_candidate(fan, "crowdrelay.fan.welcome.v1", &once());
    let mut second = episode_candidate(fan, "crowdrelay.fan.welcome.v1", &once());
    // Simulate two evaluator versions with different action keys but the same
    // frozen episode. The policy lock must serialize check + insert.
    second.action_idempotency_key.push_str(":other-version");
    let (first, second) = tokio::join!(
        persist(&f, f.workspace_id, &first),
        persist(&f, f.workspace_id, &second)
    );
    assert_eq!(usize::from(first?) + usize::from(second?), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_new_qualified_referral_episode_is_not_swallowed_by_history()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("episode-referral").await?;
    let fan = consented_fan(&f).await?;
    let now = OffsetDateTime::now_utc();
    let mut episode = LifecycleEpisode {
        key: "referral-one".to_owned(),
        since: Some(now - time::Duration::hours(2)),
        ticket_count: None,
        event_slug: None,
    };
    let first = episode_candidate(fan, "crowdrelay.fan.referral_thanks.v1", &episode);
    assert!(persist(&f, f.workspace_id, &first).await?);
    sqlx::query("UPDATE autopilot_actions SET status='succeeded', finished_at=now() WHERE workspace_id=$1 AND idempotency_key=$2")
        .bind(f.workspace_id.into_uuid()).bind(&first.action_idempotency_key).execute(&f.pool).await?;
    episode.key = "referral-two".to_owned();
    episode.since = Some(now - time::Duration::hours(1));
    let second = episode_candidate(fan, "crowdrelay.fan.referral_thanks.v1", &episode);
    assert!(persist(&f, f.workspace_id, &second).await?);
    Ok(())
}
