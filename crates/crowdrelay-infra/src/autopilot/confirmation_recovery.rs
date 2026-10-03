//! Transactional double-opt-in recovery for the organic funnel.
//!
//! This path is deliberately narrower than ordinary fan lifecycle messaging.
//! It sends no marketing copy and never retries a confirmation merely because
//! the person did not click. Decision-time eligibility requires terminal
//! delivery failure; execution re-checks the exact failed event under the same
//! per-fan token lock the public access route uses.

use crowdrelay_application::{
    RepositoryError,
    autopilot::{CONFIRMATION_RECOVERY_TEMPLATE, ConfirmationRecoverySnapshot},
};
use crowdrelay_domain::{AutopilotActionId, FanId, WorkspaceId};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::fan_lifecycle::{issue_confirmation_token, lock_fan_access};

pub(super) async fn execute(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    fan_id: FanId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let emission_key = format!("autopilot-action:{action_id}");
    let already_emitted = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM autopilot_action_emissions
            WHERE workspace_id=$1 AND action_id=$2 AND emission_key=$3
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(&emission_key)
    .fetch_one(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?;
    if already_emitted {
        return Ok(());
    }

    lock_fan_access(transaction, workspace_id, fan_id)
        .await
        .map_err(super::map_sqlx)?;

    let snapshot = sqlx::query_scalar::<_, serde_json::Value>(
        r#"
        SELECT decision.input_snapshot->'confirmation_recovery'
        FROM autopilot_actions AS action
        JOIN autopilot_decisions AS decision
          ON decision.workspace_id=action.workspace_id
         AND decision.id=action.decision_id
        WHERE action.workspace_id=$1
          AND action.id=$2
          AND action.subject_id=$3
          AND action.action_kind='fan.lifecycle.message.request'
          AND action.payload->>'template_key'=$4
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(CONFIRMATION_RECOVERY_TEMPLATE)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?
    .ok_or(RepositoryError::ConflictBecause(
        "confirmation recovery refused: decision evidence is missing",
    ))?;
    let snapshot: ConfirmationRecoverySnapshot =
        serde_json::from_value(snapshot).map_err(|_| RepositoryError::Unexpected)?;
    if snapshot.fan_id != fan_id {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery refused: fan identity changed",
        ));
    }

    let fan = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        r#"
        SELECT normalized_email, display_name, locale
        FROM fans
        WHERE workspace_id=$1 AND id=$2
          AND status='pending' AND deleted_at IS NULL
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?
    .ok_or(RepositoryError::ConflictBecause(
        "confirmation recovery refused: fan is no longer pending",
    ))?;

    let latest_consent = sqlx::query_as::<_, (bool, String)>(
        r#"
        SELECT granted,policy_version
        FROM fan_consents
        WHERE workspace_id=$1 AND fan_id=$2
          AND purpose='marketing'
          AND recorded_at <= $3
        ORDER BY recorded_at DESC,id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?
    .ok_or(RepositoryError::ConflictBecause(
        "confirmation recovery refused: current consent is absent",
    ))?;
    if !latest_consent.0 {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery refused: consent was withdrawn",
        ));
    }
    let policy_version = latest_consent.1;

    #[allow(clippy::type_complexity)]
    let latest: Option<(Uuid, String, i64, i64, i64, i64)> = sqlx::query_as(
        r#"
        SELECT event.id,
               event.status,
               COALESCE(delivery.delivered,0)::bigint,
               COALESCE(delivery.in_flight,0)::bigint,
               COALESCE(delivery.dead,0)::bigint,
               COALESCE(delivery.cancelled,0)::bigint
        FROM outbox_events AS event
        LEFT JOIN LATERAL (
            SELECT
                count(*) FILTER (WHERE d.status='delivered') AS delivered,
                count(*) FILTER (WHERE d.status IN ('pending','processing')) AS in_flight,
                count(*) FILTER (WHERE d.status='dead') AS dead,
                count(*) FILTER (WHERE d.status='cancelled') AS cancelled
            FROM webhook_deliveries AS d
            WHERE d.workspace_id=event.workspace_id
              AND d.outbox_event_id=event.id
        ) AS delivery ON true
        WHERE event.workspace_id=$1
          AND event.event_type='fan.confirmation_requested'
          AND event.payload->>'fan_id'=$2
          AND event.created_at >= $3
          AND event.created_at <= $4
        ORDER BY event.created_at DESC,event.id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid().to_string())
    .bind(snapshot.acquired_at)
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?;

    let Some((latest_event_id, event_status, delivered, in_flight, dead, cancelled)) = latest
    else {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery refused: confirmation history disappeared",
        ));
    };
    if latest_event_id != snapshot.failed_outbox_event_id {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery refused: a newer confirmation request exists",
        ));
    }
    let terminal_failure =
        event_status == "dead" || (delivered == 0 && in_flight == 0 && dead + cancelled > 0);
    if delivered > 0 || in_flight > 0 || !terminal_failure {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery refused: latest confirmation is not a terminal delivery failure",
        ));
    }

    let token = issue_confirmation_token(transaction, workspace_id, fan_id).await?;
    let payload = json!({
        "workspace_id": workspace_id,
        "fan_id": fan_id,
        "email": fan.0,
        "display_name": fan.1,
        "locale": fan.2,
        "confirmation_token": token.as_str(),
        "policy_version": policy_version,
        "recovery": {
            "kind": "autopilot_delivery_failure",
            "failed_outbox_event_id": snapshot.failed_outbox_event_id,
            "source_action_id": snapshot.source_action_id,
            "source_target": snapshot.source_target,
        }
    });

    let outbox_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH action_trace AS (
            SELECT trace_id,causation_id
            FROM autopilot_actions
            WHERE workspace_id=$1 AND id=$2
        ), emission AS (
            INSERT INTO autopilot_action_emissions(
                workspace_id,action_id,emission_key,outbox_event_id
            )
            VALUES($1,$2,$3,$4)
            ON CONFLICT(workspace_id,emission_key) DO NOTHING
            RETURNING outbox_event_id
        ), outbound AS (
            INSERT INTO outbox_events(
                id,workspace_id,event_type,event_version,payload,request_id,
                max_attempts,trace_id,causation_id,action_id
            )
            SELECT $4,$1,'fan.confirmation_requested',1,$5,$3,12,
                   trace.trace_id,trace.causation_id,$2
            FROM emission CROSS JOIN action_trace AS trace
            RETURNING id
        )
        SELECT id FROM outbound
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(&emission_key)
    .bind(outbox_id)
    .bind(payload)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(super::map_sqlx)?;

    if inserted.is_none() {
        return Err(RepositoryError::ConflictBecause(
            "confirmation recovery emission identity was already consumed",
        ));
    }
    Ok(())
}
