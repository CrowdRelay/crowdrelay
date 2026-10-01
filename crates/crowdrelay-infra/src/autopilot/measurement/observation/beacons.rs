// Exact outcome readers for one named Beacon outreach action.
//
// These deliberately do not aggregate by event. Several local relationships
// may work the same show; one Beacon's reply/click/fan must not teach every
// other Beacon action that it succeeded.

use super::*;

pub(super) async fn reply_quality_14d(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    // Reply ingress writes an immutable operator_actions row carrying the
    // Beacon, event, disposition and actual occurred_at. Read that ledger
    // rather than beacon_campaigns.updated_at/last_reply_at: a conversation
    // can have more than one reply and the mutable campaign row only keeps
    // the latest one.
    //
    // Quality is signed: a positive relationship answer earns +1, a refusal
    // earns -1, an acknowledgement with unknown intent earns 0, and silence
    // earns 0. "Somebody answered" is not automatically success.
    //
    // Latest-touch ownership matches booking semantics. If a newer
    // provider-confirmed Beacon outreach to the same person/show landed before
    // the reply, the reply belongs to that newer send. A merely dispatched
    // follow-up cannot steal credit from a delivered ask.
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COALESCE((
            SELECT CASE reply.details->>'disposition'
                WHEN 'interested' THEN 1.0::double precision
                WHEN 'partner' THEN 1.0::double precision
                WHEN 'declined' THEN -1.0::double precision
                WHEN 'do_not_contact' THEN -1.0::double precision
                ELSE 0.0::double precision
            END
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
                  JOIN autopilot_execution_reports AS receipt
                    ON receipt.workspace_id=newer.workspace_id
                   AND receipt.action_id=newer.id
                   AND receipt.status='succeeded'
                   AND receipt.provider_reference IS NOT NULL
                  WHERE newer.workspace_id=original.workspace_id
                    AND newer.id <> original.id
                    AND newer.action_kind='beacon.outreach.request'
                    AND newer.subject_kind='beacon'
                    AND newer.subject_id=reply.target_id
                    AND newer.payload->>'event_id'=original.payload->>'event_id'
                    AND receipt.occurred_at > $4
                    AND receipt.occurred_at
                        <= (reply.details->>'occurred_at')::timestamptz
              )
            ORDER BY (reply.details->>'occurred_at')::timestamptz DESC,
                     reply.created_at DESC,
                     reply.id DESC
            LIMIT 1
        ), 0.0::double precision)
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
