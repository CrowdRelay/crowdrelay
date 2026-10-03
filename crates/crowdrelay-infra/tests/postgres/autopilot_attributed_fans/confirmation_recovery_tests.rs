use super::*;
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, AutopilotActionRepository, CONFIRMATION_RECOVERY_TEMPLATE,
};
use crowdrelay_domain::FanId;

async fn seed_pending_attributed_fan(
    f: &Fixture,
    label: &str,
    acquired_at: OffsetDateTime,
) -> (uuid::Uuid, uuid::Uuid) {
    let action = insert_dispatch(f, label, acquired_at - time::Duration::hours(1)).await;
    let fan = converted_fan(f, action, acquired_at, acquired_at, "pending").await;
    sqlx::query(
        "UPDATE fan_provenance_events
         SET source_target=$3
         WHERE workspace_id=$1 AND fan_id=$2 AND action_id=$4",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan)
    .bind(label)
    .bind(action)
    .execute(&f.pool)
    .await
    .expect("source target");
    marketing_consent(f, fan, true, acquired_at).await;
    (fan, action)
}

async fn endpoint(f: &Fixture, suffix: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhook_endpoints
           (id,workspace_id,name,url,signing_secret_ref,max_attempts)
         VALUES($1,$2,$3,$4,'test-secret',3)",
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("confirmation-{suffix}"))
    .bind(format!("https://example.test/{suffix}"))
    .execute(&f.pool)
    .await
    .expect("endpoint");
    id
}

async fn confirmation_event(
    f: &Fixture,
    fan: uuid::Uuid,
    created_at: OffsetDateTime,
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events
           (id,workspace_id,event_type,event_version,payload,status,
            request_id,created_at,updated_at,delivered_at)
         VALUES($1,$2,'fan.confirmation_requested',1,$3,'delivered',
                $4,$5,$5,$5)",
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(serde_json::json!({"fan_id": fan, "confirmation_token": "test"}))
    .bind(format!("confirmation-{id}"))
    .bind(created_at)
    .execute(&f.pool)
    .await
    .expect("confirmation event");
    id
}

async fn delivery(
    f: &Fixture,
    event: uuid::Uuid,
    endpoint_id: uuid::Uuid,
    status: &str,
    occurred_at: OffsetDateTime,
) {
    let id = uuid::Uuid::now_v7();
    match status {
        "delivered" => {
            sqlx::query(
                "INSERT INTO webhook_deliveries
                   (id,workspace_id,outbox_event_id,endpoint_id,status,max_attempts,
                    attempt_count,created_at,updated_at,delivered_at,last_response_status)
                 VALUES($1,$2,$3,$4,'delivered',3,1,$5,$5,$5,200)",
            )
            .bind(id)
            .bind(f.workspace_id.into_uuid())
            .bind(event)
            .bind(endpoint_id)
            .bind(occurred_at)
            .execute(&f.pool)
            .await
            .expect("delivered route");
        }
        "dead" | "ambiguous_dead" => {
            let error_kind = if status == "dead" {
                "http_permanent_status"
            } else {
                "transport_timeout"
            };
            sqlx::query(
                "INSERT INTO webhook_deliveries
                   (id,workspace_id,outbox_event_id,endpoint_id,status,max_attempts,
                    attempt_count,created_at,updated_at,dead_at,last_error_kind)
                 VALUES($1,$2,$3,$4,'dead',3,3,$5,$5,$5,$6)",
            )
            .bind(id)
            .bind(f.workspace_id.into_uuid())
            .bind(event)
            .bind(endpoint_id)
            .bind(occurred_at)
            .bind(error_kind)
            .execute(&f.pool)
            .await
            .expect("dead route");
        }
        "pending" => {
            sqlx::query(
                "INSERT INTO webhook_deliveries
                   (id,workspace_id,outbox_event_id,endpoint_id,status,max_attempts,
                    created_at,updated_at)
                 VALUES($1,$2,$3,$4,'pending',3,$5,$5)",
            )
            .bind(id)
            .bind(f.workspace_id.into_uuid())
            .bind(event)
            .bind(endpoint_id)
            .bind(occurred_at)
            .execute(&f.pool)
            .await
            .expect("pending route");
        }
        other => panic!("unsupported fixture delivery {other}"),
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn only_terminal_failed_confirmation_delivery_is_auto_recoverable() {
    let f = setup().await.expect("fixture");
    let acquired = f.now - time::Duration::days(3);
    let ep = endpoint(&f, "eligibility").await;

    let (dead_fan, dead_action) =
        seed_pending_attributed_fan(&f, "dead-confirmation", acquired).await;
    let dead_event = confirmation_event(&f, dead_fan, acquired + time::Duration::hours(1)).await;
    delivery(
        &f,
        dead_event,
        ep,
        "dead",
        acquired + time::Duration::hours(2),
    )
    .await;

    let (delivered_fan, _) =
        seed_pending_attributed_fan(&f, "delivered-confirmation", acquired).await;
    let delivered_event =
        confirmation_event(&f, delivered_fan, acquired + time::Duration::hours(1)).await;
    delivery(
        &f,
        delivered_event,
        ep,
        "delivered",
        acquired + time::Duration::hours(2),
    )
    .await;

    let (pending_fan, _) = seed_pending_attributed_fan(&f, "pending-confirmation", acquired).await;
    let pending_event =
        confirmation_event(&f, pending_fan, acquired + time::Duration::hours(1)).await;
    delivery(
        &f,
        pending_event,
        ep,
        "pending",
        acquired + time::Duration::hours(2),
    )
    .await;

    // A transport timeout is not proof the provider rejected the message.
    // It may have accepted the mail before the acknowledgement was lost, so
    // rotating the confirmation token here could invalidate the real email
    // sitting in the person's inbox.
    let (ambiguous_event_fan, _) =
        seed_pending_attributed_fan(&f, "ambiguous-event-confirmation", acquired).await;
    let ambiguous_event = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events
           (id,workspace_id,event_type,event_version,payload,status,
            request_id,created_at,updated_at,dead_at,last_error_kind)
         VALUES($1,$2,'fan.confirmation_requested',1,$3,'dead',
                $4,$5,$5,$5,'transport_timeout')",
    )
    .bind(ambiguous_event)
    .bind(f.workspace_id.into_uuid())
    .bind(serde_json::json!({
        "fan_id": ambiguous_event_fan,
        "confirmation_token": "possibly-delivered"
    }))
    .bind(format!("confirmation-{ambiguous_event}"))
    .bind(acquired + time::Duration::hours(1))
    .execute(&f.pool)
    .await
    .expect("ambiguous dead event");

    let (ambiguous_delivery_fan, _) =
        seed_pending_attributed_fan(&f, "ambiguous-delivery-confirmation", acquired).await;
    let ambiguous_delivery_event = confirmation_event(
        &f,
        ambiguous_delivery_fan,
        acquired + time::Duration::hours(1),
    )
    .await;
    delivery(
        &f,
        ambiguous_delivery_event,
        ep,
        "ambiguous_dead",
        acquired + time::Duration::hours(2),
    )
    .await;

    let (withdrawn_fan, _) =
        seed_pending_attributed_fan(&f, "withdrawn-confirmation", acquired).await;
    let withdrawn_event =
        confirmation_event(&f, withdrawn_fan, acquired + time::Duration::hours(1)).await;
    delivery(
        &f,
        withdrawn_event,
        ep,
        "dead",
        acquired + time::Duration::hours(2),
    )
    .await;
    marketing_consent(&f, withdrawn_fan, false, f.now - time::Duration::hours(2)).await;

    let recoverable = crowdrelay_infra::organic_funnel::confirmation_recovery_snapshots(
        &f.pool,
        f.workspace_id.into_uuid(),
        f.now,
    )
    .await
    .expect("recovery snapshots");

    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].fan_id.into_uuid(), dead_fan);
    assert_eq!(recoverable[0].source_action_id, dead_action);
    assert_eq!(recoverable[0].failed_outbox_event_id, dead_event);
    assert_eq!(recoverable[0].failure_kind, "http_permanent_status");
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn autonomous_confirmation_recovery_is_bounded_to_once_per_acquisition() {
    let f = setup().await.expect("fixture");
    let acquired = f.now - time::Duration::days(3);
    let ep = endpoint(&f, "bounded").await;
    let (fan, _) = seed_pending_attributed_fan(&f, "bounded-confirmation", acquired).await;
    let failed = confirmation_event(&f, fan, acquired + time::Duration::hours(1)).await;
    delivery(&f, failed, ep, "dead", acquired + time::Duration::hours(2)).await;

    assert_eq!(
        crowdrelay_infra::organic_funnel::confirmation_recovery_snapshots(
            &f.pool,
            f.workspace_id.into_uuid(),
            f.now,
        )
        .await
        .expect("before")
        .len(),
        1
    );

    let action = insert_dispatch(&f, "confirmation-recovery-action", f.now).await;
    sqlx::query(
        "UPDATE autopilot_actions
         SET subject_id=$2,
             action_kind='fan.lifecycle.message.request',
             action_class='owned_audience',
             payload=$3,
             created_at=$4
         WHERE workspace_id=$1 AND id=$5",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan)
    .bind(serde_json::json!({
        "kind":"request_fan_lifecycle_message",
        "fan_id":fan,
        "template_key":"crowdrelay.fan.confirmation_recovery.v1",
        "show":null
    }))
    .bind(f.now - time::Duration::minutes(30))
    .bind(action)
    .execute(&f.pool)
    .await
    .expect("recovery action");

    assert!(
        crowdrelay_infra::organic_funnel::confirmation_recovery_snapshots(
            &f.pool,
            f.workspace_id.into_uuid(),
            f.now,
        )
        .await
        .expect("after")
        .is_empty(),
        "one autonomous retry owns the acquisition episode even if its delivery later fails"
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn recovery_execution_mints_one_action_owned_confirmation_without_fake_growth_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let acquired = f.now - time::Duration::days(3);
    let ep = endpoint(&f, "execute").await;
    let (fan, source_action) =
        seed_pending_attributed_fan(&f, "execute-confirmation", acquired).await;
    let failed = confirmation_event(&f, fan, acquired + time::Duration::hours(1)).await;
    delivery(&f, failed, ep, "dead", acquired + time::Duration::hours(2)).await;

    let snapshot = crowdrelay_infra::organic_funnel::confirmation_recovery_snapshots(
        &f.pool,
        f.workspace_id.into_uuid(),
        f.now,
    )
    .await?
    .into_iter()
    .next()
    .ok_or("recovery snapshot")?;
    assert_eq!(snapshot.source_action_id, source_action);

    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    let payload = AutopilotActionPayload::RequestFanLifecycleMessage {
        fan_id: FanId::from_uuid(fan),
        template_key: CONFIRMATION_RECOVERY_TEMPLATE.to_owned(),
        show: None,
    };
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id,workspace_id,decision_key,context,subject_kind,subject_id,
            decision_kind,confidence_basis_points,disposition,reason,
            input_snapshot,policy_snapshot,recommendation,evaluated_at,trace_id)
           VALUES($1,$2,$3,'fan_lifecycle','fan',$4,
                  'recover_failed_fan_confirmation',9900,'auto_execute',
                  'terminal confirmation route failure',$5,'{}','{}',$6,$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-recovery-{decision_id}"))
    .bind(fan)
    .bind(serde_json::json!({
        "confirmation_recovery": snapshot,
        "organic_funnel_control": {
            "directive": "repair_confirmation",
            "mature_links": 1,
            "unique_visitors": 2,
            "signups": 1,
            "confirmed": 0,
            "activation_mature": 0,
            "activated_mature": 0,
            "retention_mature": 0,
            "retained": 0
        }
    }))
    .bind(f.now)
    .execute(&f.pool)
    .await?;

    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id,workspace_id,decision_id,context,action_kind,subject_kind,
            subject_id,idempotency_key,payload,status,action_class,
            approved_at,approved_by,available_at)
           VALUES($1,$2,$3,'fan_lifecycle','fan.lifecycle.message.request','fan',
                  $4,$5,$6,'queued','owned_audience',$7,'policy:bounded_auto',$7)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(fan)
    .bind(format!("action:confirmation-recovery:{fan}:{failed}"))
    .bind(serde_json::to_value(&payload)?)
    .bind(f.now)
    .execute(&f.pool)
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, f.now)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .ok_or("recovery action was not claimable")?;
    f.repository
        .execute_action(f.workspace_id, action, f.now)
        .await?;

    let status: String = sqlx::query_scalar("SELECT status FROM autopilot_actions WHERE id=$1")
        .bind(action_id)
        .fetch_one(&f.pool)
        .await?;
    assert_eq!(status, "succeeded");

    let emitted: Vec<(uuid::Uuid, serde_json::Value)> = sqlx::query_as(
        r#"SELECT id,payload
           FROM outbox_events
           WHERE workspace_id=$1
             AND action_id=$2
             AND event_type='fan.confirmation_requested'"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        emitted[0].1.pointer("/recovery/failed_outbox_event_id"),
        Some(&serde_json::Value::String(failed.to_string()))
    );

    let token_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM fan_action_tokens
         WHERE workspace_id=$1 AND fan_id=$2 AND purpose='confirm'
           AND consumed_at IS NULL",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(token_count, 1);

    let fake_learning: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM dispatch_predictions WHERE action_id=$1)::bigint,
           (SELECT count(*) FROM growth_evidence
             WHERE workspace_id=$2 AND action_id=$1)::bigint,
           (SELECT count(*) FROM autopilot_measurements
             WHERE workspace_id=$2 AND action_id=$1)::bigint",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        fake_learning,
        (0, 0, 0),
        "transactional auth recovery must not masquerade as a growth experiment"
    );
    Ok(())
}
