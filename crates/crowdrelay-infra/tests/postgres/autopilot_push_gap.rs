//! The autopilot push gap at send time, against a real Postgres. Borrows
//! the dispatch-envelope fixtures.

use crowdrelay_application::autopilot::AutopilotActionRepository;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::autopilot_dispatch_envelope::{seed_outcome_action, setup};

/// The push gap holds where the push is sent. Relay pacing spaces pushes by
/// when they were raised; four raised apart, held, and released together
/// reached the same two phones in one second on 2026-09-26. A fan who got an
/// autopilot push inside the gap is skipped; everyone else still gets it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_fan_pushed_inside_the_gap_is_skipped_at_send_time()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    let suffix = f.workspace_id.into_uuid().simple().to_string();
    let mut fans = Vec::new();
    for index in 0..2 {
        let fan_id = Uuid::now_v7();
        let endpoint_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
             VALUES ($1, $2, $3, 'Fan', 'active')",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("gap-{suffix}-{index}@example.test"))
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
             VALUES ($1,$2,'marketing',true,'v1','test')",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_push_endpoints
               (id, workspace_id, fan_id, installation_id, transport, endpoint_address, active)
             VALUES ($1, $2, $3, $4, 'android_fcm', $5, true)",
        )
        .bind(endpoint_id)
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("gap-install-{suffix}-{index}"))
        .bind(format!("gap-token-{suffix}-{index}"))
        .execute(&f.pool)
        .await?;
        fans.push((fan_id, endpoint_id));
    }
    // The first fan got an autopilot push two hours ago.
    sqlx::query(
        "INSERT INTO fan_push_deliveries
           (workspace_id, fan_id, endpoint_id, source_kind, source_id, title, body, target_path,
            created_at)
         VALUES ($1, $2, $3, 'agent_signal_push', $4, 'earlier', 'earlier', '/my-signal/', $5)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fans[0].0)
    .bind(fans[0].1)
    .bind(Uuid::now_v7())
    .bind(now - time::Duration::hours(2))
    .execute(&f.pool)
    .await?;

    let action_id = seed_outcome_action(
        &f,
        "signal.push.request",
        json!({
            "kind": "request_signal_push",
            "task_id": Uuid::now_v7(),
            "title": "gap push",
            "body": "gap push body",
            "target_path": null,
            "event_id": null,
            "segment": null,
        }),
        now,
    )
    .await?;
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued push action must be claimable");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;

    let reached = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM fan_push_deliveries WHERE workspace_id = $1 AND source_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(
        reached,
        vec![fans[1].0],
        "only the fan outside the gap is pushed"
    );
    Ok(())
}
