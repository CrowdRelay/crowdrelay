// Exact outcome readers for one named Beacon outreach action.
//
// These deliberately do not aggregate by event. Several local relationships
// may work the same show; one Beacon's reply/click/fan must not teach every
// other Beacon action that it succeeded.

use super::super::super::*;
use super::ClaimedAutopilotMeasurement;

pub(super) async fn reply_14d(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    // Reply ingress already writes an immutable operator_actions row carrying
    // the Beacon, event, disposition and actual occurred_at. Read that ledger
    // rather than beacon_campaigns.updated_at/last_reply_at: a conversation
    // can have more than one reply and the mutable campaign row only keeps
    // the latest one.
    //
    // The NOT EXISTS applies the same ownership rule booking uses: if another
    // successful Beacon outreach to the same person/show finished after this
    // action but before the reply, the reply belongs to that newer send.
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT CASE WHEN EXISTS (
            SELECT 1
            FROM operator_actions AS reply
            JOIN autopilot_actions AS original
              ON original.workspace_id=reply.workspace_id
             AND original.id=$2
            WHERE reply.workspace_id=$1
              AND reply.action='record_autopilot_beacon_reply'
              AND reply.target_type='beacon'
              AND reply.target_id=$3
              AND reply.details->>'disposition' <> 'none'
              AND (reply.details->>'event_id')::uuid
                    = (original.payload->>'event_id')::uuid
              AND (reply.details->>'occurred_at')::timestamptz >= $4
              AND (reply.details->>'occurred_at')::timestamptz
                    < $4 + INTERVAL '14 days'
              AND NOT EXISTS (
                  SELECT 1
                  FROM autopilot_actions AS newer
                  WHERE newer.workspace_id=original.workspace_id
                    AND newer.id <> original.id
                    AND newer.action_kind='beacon.outreach.request'
                    AND newer.status='succeeded'
                    AND newer.subject_kind='beacon'
                    AND newer.subject_id=reply.target_id
                    AND newer.payload->>'event_id'=original.payload->>'event_id'
                    AND newer.finished_at > $4
                    AND newer.finished_at
                        <= (reply.details->>'occurred_at')::timestamptz
              )
        ) THEN 1.0::double precision ELSE 0.0::double precision END
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.subject_id)
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

pub(super) async fn unique_visitors_14d(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(DISTINCT click.anonymous_visitor_id)::double precision
        FROM smart_links AS link
        JOIN click_events AS click
          ON click.workspace_id=link.workspace_id
         AND click.smart_link_id=link.id
        WHERE link.workspace_id=$1
          AND link.action_id=$2
          AND click.anonymous_visitor_id IS NOT NULL
          AND click.occurred_at >= $3
          AND click.occurred_at < $3 + INTERVAL '14 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

pub(super) async fn fan_acquisitions_14d(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(DISTINCT conversion.fan_id)::double precision
        FROM fan_provenance_events AS conversion
        WHERE conversion.workspace_id=$1
          AND conversion.action_id=$2
          AND conversion.event_kind='conversion'
          AND conversion.attribution_method='last_tracked_click'
          AND conversion.occurred_at >= $3
          AND conversion.occurred_at < $3 + INTERVAL '14 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}
