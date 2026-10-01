use super::*;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::FanId;

fn install_candidate(fan: FanId, key: &str) -> DecisionCandidate {
    let mut candidate = outreach_candidate(OutreachTargetId::new(), Uuid::now_v7());
    candidate.context = AutopilotContext::FanLifecycle;
    candidate.subject = ActionSubject::Fan(fan);
    candidate.decision_kind = "request_lifecycle_message";
    candidate.action = AutopilotActionPayload::RequestFanLifecycleMessage {
        fan_id: fan,
        template_key: key.to_owned(),
        show: None,
    };
    candidate
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn install_template_grant_covers_multiple_fans_but_revocation_prevents_emission()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("install-grant").await?;
    let now = OffsetDateTime::now_utc();
    let target = "template:crowdrelay.fan.signal_install_ask.v1";
    grant(
        &f.pool,
        f.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "fan.lifecycle.message.request",
            target_key: target,
            class: ActionClass::OwnedAudience,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        now,
    )
    .await?;
    for _ in 0..2 {
        let fan = FanId::new();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status) VALUES($1,$2,$3,'active')",
        )
        .bind(fan.into_uuid())
        .bind(f.workspace_id.into_uuid())
        .bind(format!("{fan}@example.test"))
        .execute(&f.pool)
        .await?;
        let candidate = install_candidate(fan, "crowdrelay.fan.signal_install_ask.v1");
        assert!(persist(&f, f.workspace_id, &candidate).await?);
        let state = action_state(&f, f.workspace_id, &candidate.action_idempotency_key)
            .await?
            .expect("action");
        assert_eq!(state.0, "queued");
        assert_eq!(state.1.as_deref(), Some("operator:standing_grant"));
        let future_fan = FanId::new();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status) VALUES($1,$2,$3,'active')",
        )
        .bind(future_fan.into_uuid())
        .bind(f.workspace_id.into_uuid())
        .bind(format!("{future_fan}@example.test"))
        .execute(&f.pool)
        .await?;
        let future = install_candidate(future_fan, "crowdrelay.fan.signal_install_ask.v2");
        assert!(persist(&f, f.workspace_id, &future).await?);
        assert_eq!(
            action_state(&f, f.workspace_id, &future.action_idempotency_key)
                .await?
                .expect("future approval")
                .0,
            "awaiting_approval"
        );
    }
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(claims.len(), 2);
    revoke(
        &f.pool,
        f.workspace_id.into_uuid(),
        "fan.lifecycle.message.request",
        target,
        "operator:test",
        OffsetDateTime::now_utc(),
    )
    .await?;
    for action in claims {
        assert!(
            f.repository
                .execute_action(f.workspace_id, &action, OffsetDateTime::now_utc())
                .await
                .is_err()
        );
    }
    let emitted:i64=sqlx::query_scalar("SELECT COUNT(*) FROM outbox_events WHERE workspace_id=$1 AND event_type='crowdrelay.fan_lifecycle.message_requested'")
        .bind(f.workspace_id.into_uuid()).fetch_one(&f.pool).await?;
    assert_eq!(emitted, 0);
    Ok(())
}
