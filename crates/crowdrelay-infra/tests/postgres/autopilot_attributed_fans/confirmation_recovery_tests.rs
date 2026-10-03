use super::*;

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
        "dead" => {
            sqlx::query(
                "INSERT INTO webhook_deliveries
                   (id,workspace_id,outbox_event_id,endpoint_id,status,max_attempts,
                    attempt_count,created_at,updated_at,dead_at,last_error_kind)
                 VALUES($1,$2,$3,$4,'dead',3,3,$5,$5,$5,'http_permanent_status')",
            )
            .bind(id)
            .bind(f.workspace_id.into_uuid())
            .bind(event)
            .bind(endpoint_id)
            .bind(occurred_at)
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

    let (pending_fan, _) =
        seed_pending_attributed_fan(&f, "pending-confirmation", acquired).await;
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
    marketing_consent(
        &f,
        withdrawn_fan,
        false,
        f.now - time::Duration::hours(2),
    )
    .await;

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
    delivery(
        &f,
        failed,
        ep,
        "dead",
        acquired + time::Duration::hours(2),
    )
    .await;

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
