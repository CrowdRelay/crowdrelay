use super::install_tests::{consented_fan, install_candidate, install_fixture};
use super::*;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotMeasurementKind, AutopilotMeasurementRepository,
    ClaimedAutopilotMeasurement,
};
use crowdrelay_domain::{AutopilotActionId, AutopilotMeasurementId, FanId};

async fn welcome_fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let f = install_fixture(label).await?;
    sqlx::query("UPDATE autopilot_policies SET autonomy_level='bounded_auto' WHERE workspace_id=$1 AND context='fan_lifecycle'")
        .bind(f.workspace_id.into_uuid()).execute(&f.pool).await?;
    Ok(f)
}

async fn register(f: &Fixture, upgraded: bool) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO executor_instances(workspace_id,executor_id,version,manifest_sha,observed_at,expires_at) VALUES($1,'welcome-test','2','test',now(),now()+interval '30 minutes') ON CONFLICT DO NOTHING")
        .bind(f.workspace_id.into_uuid()).execute(&f.pool).await?;
    for cap in if upgraded {
        vec!["fan.lifecycle.message", "fan.lifecycle.welcome.v2"]
    } else {
        vec!["fan.lifecycle.message"]
    } {
        sqlx::query("INSERT INTO executor_capabilities(workspace_id,executor_id,capability,capability_version,observed_at,expires_at) VALUES($1,'welcome-test',$2,'2',now(),now()+interval '30 minutes') ON CONFLICT DO NOTHING")
            .bind(f.workspace_id.into_uuid()).bind(cap).execute(&f.pool).await?;
    }
    Ok(())
}

fn candidate(fan: FanId) -> DecisionCandidate {
    let mut c = install_candidate(fan, "crowdrelay.fan.welcome.v2");
    c.disposition = PolicyDisposition::AutoExecute;
    c.action_idempotency_key =
        format!("action:lifecycle-episode:{fan}:crowdrelay.fan.welcome.v2:once");
    c.input_snapshot.as_object_mut().expect("snapshot").insert(
        "lifecycle_episode".to_owned(),
        serde_json::json!({"key":"once","since":null,"ticket_count":null,"event_slug":null}),
    );
    c
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_legacy_executor_cannot_consume_v2_and_registration_unblocks_the_same_episode()
-> Result<(), Box<dyn std::error::Error>> {
    let f = welcome_fixture("welcome-cap").await?;
    let fan = consented_fan(&f).await?;
    let c = candidate(fan);
    assert!(!persist(&f, f.workspace_id, &c).await?);
    register(&f, false).await?;
    assert!(!persist(&f, f.workspace_id, &c).await?);
    register(&f, true).await?;
    assert!(persist(&f, f.workspace_id, &c).await?);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_completed_v1_welcome_is_not_sent_again_as_v2() -> Result<(), Box<dyn std::error::Error>>
{
    let f = welcome_fixture("welcome-history").await?;
    let fan = consented_fan(&f).await?;
    let old = install_candidate(fan, "crowdrelay.fan.welcome.v1");
    assert!(persist(&f, f.workspace_id, &old).await?);
    sqlx::query("UPDATE autopilot_actions SET status='succeeded',finished_at=now() WHERE workspace_id=$1 AND idempotency_key=$2")
        .bind(f.workspace_id.into_uuid()).bind(&old.action_idempotency_key).execute(&f.pool).await?;
    register(&f, true).await?;
    assert!(!persist(&f, f.workspace_id, &candidate(fan)).await?);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn welcome_delivers_the_promised_action_owned_show_and_rechecks_consent()
-> Result<(), Box<dyn std::error::Error>> {
    let f = welcome_fixture("welcome-promise").await?;
    register(&f, true).await?;
    let event = Uuid::now_v7();
    sqlx::query("INSERT INTO events(id,workspace_id,slug,title,starts_at,status,published_at) VALUES($1,$2,'real-show','Real show',now()+interval '7 days','published',now())")
        .bind(event).bind(f.workspace_id.into_uuid()).execute(&f.pool).await?;
    let fan = consented_fan(&f).await?;
    sqlx::query("INSERT INTO fan_capture_contexts(workspace_id,fan_id,context) VALUES($1,$2,'{\"offer\":\"shows\",\"event_slug\":\"real-show\"}')")
        .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).execute(&f.pool).await?;
    assert!(persist(&f, f.workspace_id, &candidate(fan)).await?);
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    let action = claims.first().ok_or("claim")?;
    f.repository
        .execute_action(f.workspace_id, action, OffsetDateTime::now_utc())
        .await?;
    let (payload,owner,destination)=sqlx::query_as::<_,(serde_json::Value,Uuid,String)>("SELECT event.payload,link.action_id,link.destination_url FROM outbox_events event JOIN smart_links link ON link.workspace_id=event.workspace_id AND link.action_id=event.action_id WHERE event.workspace_id=$1 AND event.action_id=$2 AND event.event_type='crowdrelay.fan_lifecycle.message_requested'")
        .bind(f.workspace_id.into_uuid()).bind(action.id.into_uuid()).fetch_one(&f.pool).await?;
    assert_eq!(owner, action.id.into_uuid());
    assert!(destination.ends_with("/live/real-show"));
    assert_eq!(
        payload
            .pointer("/fan/activation/kind")
            .and_then(serde_json::Value::as_str),
        Some("event")
    );
    assert!(
        payload
            .pointer("/fan/activation/url")
            .and_then(serde_json::Value::as_str)
            .ok_or("activation link")?
            .contains("/l/welcome-")
    );
    // Confirmation/session creation and an email request are not activation.
    let now = OffsetDateTime::now_utc();
    let measurement = ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from_uuid(Uuid::now_v7()),
        action_id: AutopilotActionId::from_uuid(action.id.into_uuid()),
        kind: AutopilotMeasurementKind::FanLifecycleActivation7d,
        subject_id: fan.into_uuid(),
        baseline_value: 0.0,
        action_finished_at: now - time::Duration::hours(1),
        due_at: now + time::Duration::days(7),
        attempt_number: 1,
    };
    assert_eq!(
        f.repository
            .observe_measurement(f.workspace_id, &measurement, now)
            .await?,
        0.0
    );
    sqlx::query("INSERT INTO event_interests(workspace_id,event_id,fan_id) VALUES($1,$2,$3)")
        .bind(f.workspace_id.into_uuid())
        .bind(event)
        .bind(fan.into_uuid())
        .execute(&f.pool)
        .await?;
    assert_eq!(
        f.repository
            .observe_measurement(f.workspace_id, &measurement, OffsetDateTime::now_utc())
            .await?,
        1.0
    );
    let withdrawn = consented_fan(&f).await?;
    assert!(persist(&f, f.workspace_id, &candidate(withdrawn)).await?);
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    let action = claims.first().ok_or("withdrawn claim")?;
    sqlx::query("INSERT INTO fan_consents(workspace_id,fan_id,purpose,granted,policy_version,source) VALUES($1,$2,'marketing',false,'v1','welcome-test')")
        .bind(f.workspace_id.into_uuid()).bind(withdrawn.into_uuid()).execute(&f.pool).await?;
    assert!(
        f.repository
            .execute_action(f.workspace_id, action, OffsetDateTime::now_utc())
            .await
            .is_err()
    );
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM outbox_events WHERE workspace_id=$1 AND event_type='crowdrelay.fan_lifecycle.message_requested'")
        .bind(f.workspace_id.into_uuid()).fetch_one(&f.pool).await?;
    assert_eq!(count, 1);
    Ok(())
}
