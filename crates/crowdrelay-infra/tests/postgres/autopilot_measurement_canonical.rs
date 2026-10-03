//! The measurement spine's canonical-person facts: a measurement counts a
//! merged person once, and only fans traced to its own action.
//!
//! Split out of `autopilot_measurement_spine.rs` (which shares its fixture and
//! helpers) when that file outgrew the source-size ratchet.

use super::autopilot_measurement_spine::*;
use crowdrelay_application::autopilot::*;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn signal_install_measurement_counts_only_canonical_fans_from_its_action()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let anchor = f.now - time::Duration::hours(18);
    let action_a = insert_dispatch(&f, "signal-proof-a", anchor).await;
    let action_b = insert_dispatch(&f, "signal-proof-b", anchor).await;

    async fn attributed_fan(
        f: &Fixture,
        action_id: uuid::Uuid,
        label: &str,
        endpoints: usize,
    ) -> Result<uuid::Uuid, Box<dyn std::error::Error>> {
        let fan_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
             VALUES($1,$2,$3,'active',$4)",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(format!("{label}-{}@signal-proof.test", fan_id.simple()))
        .bind(f.now - time::Duration::hours(14))
        .execute(&f.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_provenance_events(
                 workspace_id,fan_id,event_kind,channel,source_target,action_id,
                 attribution_method,attribution_confidence,occurred_at
             ) VALUES($1,$2,'conversion','reddit',$3,$4,
                      'last_tracked_click',1.0,$5)",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("proof-{label}"))
        .bind(action_id)
        .bind(f.now - time::Duration::hours(13))
        .execute(&f.pool)
        .await?;
        for n in 0..endpoints {
            let installation = format!("signal-proof-{label}-{n}-{}", fan_id.simple());
            sqlx::query(
                "INSERT INTO fan_push_endpoints(
                     workspace_id,fan_id,installation_id,transport,endpoint_address,
                     active,created_at,last_seen_at
                 ) VALUES($1,$2,$3,'android_fcm',$4,true,$5,$5)",
            )
            .bind(f.workspace_id.into_uuid())
            .bind(fan_id)
            .bind(installation)
            .bind(format!("fcm-endpoint-{label}-{n}-{}", fan_id.simple()))
            .bind(f.now - time::Duration::hours(12))
            .execute(&f.pool)
            .await?;
        }
        Ok(fan_id)
    }

    let _fan_a = attributed_fan(&f, action_a, "a", 2).await?;
    let _fan_b = attributed_fan(&f, action_b, "b", 1).await?;

    // A real active endpoint from an unrelated fan/action is workspace growth,
    // but it is not evidence for either measured dispatch.
    let unrelated = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
         VALUES($1,$2,$3,'active',$4)",
    )
    .bind(unrelated)
    .bind(f.workspace_id.into_uuid())
    .bind(format!(
        "unrelated-{}@signal-proof.test",
        unrelated.simple()
    ))
    .bind(f.now - time::Duration::hours(14))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_push_endpoints(
             workspace_id,fan_id,installation_id,transport,endpoint_address,
             active,created_at,last_seen_at
         ) VALUES($1,$2,$3,'android_fcm',$4,true,$5,$5)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(unrelated)
    .bind(format!("signal-proof-unrelated-{}", unrelated.simple()))
    .bind(format!("fcm-endpoint-unrelated-{}", unrelated.simple()))
    .bind(f.now - time::Duration::hours(12))
    .execute(&f.pool)
    .await?;

    for action_id in [action_a, action_b] {
        let measurement = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind: AutopilotMeasurementKind::AgentRunSignalInstalls7d,
            subject_id: action_id,
            baseline_value: 0.0,
            action_finished_at: anchor,
            due_at: f.now,
            attempt_number: 1,
        };
        let observed = f
            .repository
            .observe_measurement(f.workspace_id, &measurement, f.now)
            .await?;
        assert_eq!(
            observed, 1.0,
            "one canonical person belongs to this action; duplicate devices and parallel actions must not leak"
        );

        let fast = ClaimedAutopilotMeasurement {
            kind: AutopilotMeasurementKind::SignalInstalls1d,
            ..measurement
        };
        let observed_fast = f
            .repository
            .observe_measurement(f.workspace_id, &fast, f.now)
            .await?;
        assert_eq!(observed_fast, 1.0);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn attributed_fan_growth_counts_a_merged_person_once_even_when_durable()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let anchor = f.now - time::Duration::days(40);
    let action_id = insert_dispatch(&f, "canonical-attributed-fan", anchor).await;

    sqlx::query(
        "INSERT INTO smart_links(
             workspace_id,slug,destination_url,active,channel_source,action_id,created_at
         ) VALUES($1,$2,'https://example.test/join',true,'signal_invite',$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(format!("canonical-fan-{}", action_id.simple()))
    .bind(action_id)
    .bind(anchor)
    .execute(&f.pool)
    .await?;

    let root = uuid::Uuid::now_v7();
    let duplicate = uuid::Uuid::now_v7();
    for (fan_id, email) in [
        (
            root,
            format!("canonical-root-{}@example.test", root.simple()),
        ),
        (
            duplicate,
            format!("canonical-duplicate-{}@example.test", duplicate.simple()),
        ),
    ] {
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
             VALUES($1,$2,$3,'active',$4)",
        )
        .bind(fan_id)
        .bind(f.workspace_id.into_uuid())
        .bind(email)
        .bind(anchor)
        .execute(&f.pool)
        .await?;
    }

    let conversion_a = anchor + time::Duration::hours(1);
    let conversion_b = anchor + time::Duration::hours(2);
    for (fan_id, occurred_at) in [(root, conversion_a), (duplicate, conversion_b)] {
        sqlx::query(
            "INSERT INTO fan_provenance_events(
                 workspace_id,fan_id,event_kind,channel,source_target,action_id,
                 attribution_method,attribution_confidence,occurred_at
             ) VALUES($1,$2,'conversion','signal_invite',$3,$4,
                      'last_tracked_click',1.0,$5)",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(fan_id)
        .bind(format!("canonical-fan-{}", action_id.simple()))
        .bind(action_id)
        .bind(occurred_at)
        .execute(&f.pool)
        .await?;
    }

    sqlx::query(
        "UPDATE fans
         SET status='merged',merged_into_fan_id=$1
         WHERE workspace_id=$2 AND id=$3",
    )
    .bind(root)
    .bind(f.workspace_id.into_uuid())
    .bind(duplicate)
    .execute(&f.pool)
    .await?;

    sqlx::query(
        "INSERT INTO fan_consents(
             workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at
         ) VALUES($1,$2,'marketing',true,'v1','test',$3)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(root)
    .bind(anchor)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_sessions(
             workspace_id,fan_id,session_token_hash,last_seen_at,expires_at
         ) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(root)
    .bind(root.as_bytes().to_vec())
    .bind(f.now - time::Duration::days(1))
    .bind(f.now + time::Duration::days(30))
    .execute(&f.pool)
    .await?;

    for kind in [
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        AutopilotMeasurementKind::DurableFanGrowth30d,
    ] {
        let measurement = ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::now_v7()),
            action_id: AutopilotActionId::from(action_id),
            kind,
            subject_id: action_id,
            baseline_value: 0.0,
            action_finished_at: anchor,
            due_at: f.now,
            attempt_number: 1,
        };
        let observed = f
            .repository
            .observe_measurement(f.workspace_id, &measurement, f.now)
            .await?;
        assert_eq!(
            observed, 1.0,
            "{kind:?}: two historical rows resolved to one human must equal one fan"
        );
    }
    Ok(())
}
