//! Harm observation — what the action cost, per source.
//!
//! Split out of `observation.rs` so both stay inside the source-size
//! ratchet. The primary metric answers "what did the action earn"; this
//! collector answers "what did it cost" — withdrawals, complaints, refunds,
//! suppressions, cancellations — each attributed to the action that caused
//! it, each counted inside the measurement's own window.
//!
//! Fan-facing attribution is last-touch across every contact channel we can
//! see: a withdrawal belongs to the action whose delivery the fan most
//! recently received before leaving, not to whichever measurement happened
//! to be due. Contacts we cannot attribute (a `nearby_concert` push, an
//! operator's manual send whose outbox row names no action) simply do not
//! appear — the last touch is the last *visible* touch, which is the honest
//! limit of the ledger.
//!
//! Campaign deliveries bind to their action through the dispatch chain —
//! `campaign.dispatch_event_id → outbox.action_id` — which every autopilot
//! campaign path writes (audience lifecycle, release waves, show growth).
//! The lifecycle emissions ledger is deliberately not consulted: it only
//! covers the event-phase vocabulary and would orphan the other senders.

use super::*;

/// Counts every harm source attributable to the measurement's action in
/// `[action_finished_at, due_at)`. Returns zeros for a measurement whose
/// action could not have caused harm — a clean reading is evidence the
/// posterior needs, not a missing instrument.
pub async fn observe_harm(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<HarmObservation, RepositoryError> {
    let row = sqlx::query_as::<_, HarmRow>(
        r#"
        WITH all_contacts AS (
            -- Every delivered fan contact with its owning action resolved.
            -- Campaign deliveries and campaign pushes reach the action
            -- through the dispatch outbox row; signal pushes name it
            -- directly in `source_id`. Contacts at or after the window end
            -- cannot precede an in-window harm event, so the scan bounds
            -- there instead of reading every delivery the tenant ever sent.
            SELECT delivery.fan_id, delivery.completed_at AS contacted_at,
                   outbox.action_id
            FROM communication_campaign_deliveries AS delivery
            JOIN communication_campaigns AS campaign
              ON campaign.workspace_id = delivery.workspace_id
             AND campaign.id = delivery.campaign_id
            JOIN outbox_events AS outbox
              ON outbox.workspace_id = campaign.workspace_id
             AND outbox.id = campaign.dispatch_event_id
            WHERE delivery.workspace_id = $1
              AND delivery.status = 'delivered'
              AND delivery.completed_at < $4
              AND outbox.action_id IS NOT NULL
            UNION ALL
            SELECT push.fan_id, push.delivered_at, push.source_id
            FROM fan_push_deliveries AS push
            WHERE push.workspace_id = $1
              AND push.delivered_at IS NOT NULL
              AND push.delivered_at < $4
              AND push.source_kind = 'agent_signal_push'
            UNION ALL
            SELECT push.fan_id, push.delivered_at, outbox.action_id
            FROM fan_push_deliveries AS push
            JOIN communication_campaigns AS campaign
              ON campaign.workspace_id = push.workspace_id
             AND campaign.id = push.source_id
            JOIN outbox_events AS outbox
              ON outbox.workspace_id = campaign.workspace_id
             AND outbox.id = campaign.dispatch_event_id
            WHERE push.workspace_id = $1
              AND push.delivered_at IS NOT NULL
              AND push.delivered_at < $4
              AND push.source_kind = 'communication_campaign'
              AND outbox.action_id IS NOT NULL
        ),
        promoted_event AS (
            -- Refunds and cancellations live on the event, but a send's
            -- measurement subject is the campaign. The action payload names
            -- the event it promoted; the regex guard keeps a non-uuid
            -- `event_id` field on some other payload shape from aborting
            -- the cast.
            SELECT (action.payload->>'event_id')::uuid AS event_id
            FROM autopilot_actions AS action
            WHERE action.workspace_id = $1
              AND action.id = $2
              AND action.payload->>'event_id'
                  ~ '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
        )
        SELECT
            (SELECT COUNT(DISTINCT consent.fan_id)::double precision
             FROM fan_consents AS consent
             JOIN LATERAL (
                 SELECT contact.action_id
                 FROM all_contacts AS contact
                 WHERE contact.fan_id = consent.fan_id
                   AND contact.contacted_at <= consent.recorded_at
                 ORDER BY contact.contacted_at DESC
                 LIMIT 1
             ) AS last_touch ON last_touch.action_id = $2
             WHERE consent.workspace_id = $1
               AND consent.purpose = 'marketing'
               AND NOT consent.granted
               AND consent.recorded_at >= $3
               AND consent.recorded_at < $4) AS unsubscribes,
            (SELECT COUNT(*)::double precision
             FROM outreach_delivery_faults AS fault
             -- Same last-touch rule the fan arms use: the complaint belongs
             -- to the action whose reach reached the target most recently
             -- before the fault, so two actions writing to one target can
             -- never both count it. When the ledger is silent about the
             -- target entirely, a subject match is the only tie left.
             LEFT JOIN LATERAL (
                 SELECT reach.action_id
                 FROM reach_events AS reach
                 WHERE reach.workspace_id = $1
                   AND reach.recipient_kind = 'outreach_target'
                   AND reach.recipient_id = fault.target_id::text
                   AND reach.action_id IS NOT NULL
                   AND reach.sent_at <= fault.occurred_at
                 ORDER BY reach.sent_at DESC
                 LIMIT 1
             ) AS last_reach ON true
             WHERE fault.workspace_id = $1
               AND fault.fault = 'complaint'
               AND fault.occurred_at >= $3
               AND fault.occurred_at < $4
               AND (last_reach.action_id = $2
                    OR (last_reach.action_id IS NULL AND fault.target_id = $5)))
               AS complaints,
            (SELECT COUNT(*)::double precision
             FROM ticket_accounting_entries AS entry
             WHERE entry.workspace_id = $1
               AND entry.entry_kind = 'refund'
               AND entry.occurred_at >= $3
               AND entry.occurred_at < $4
               AND entry.event_id IN ($5, (SELECT event_id FROM promoted_event)))
               AS refunds,
            (SELECT COUNT(*)::double precision
             FROM fans AS fan
             JOIN LATERAL (
                 SELECT contact.action_id
                 FROM all_contacts AS contact
                 WHERE contact.fan_id = fan.id
                   AND contact.contacted_at <= fan.deleted_at
                 ORDER BY contact.contacted_at DESC
                 LIMIT 1
             ) AS last_touch ON last_touch.action_id = $2
             WHERE fan.workspace_id = $1
               AND fan.deleted_at >= $3
               AND fan.deleted_at < $4) AS fan_suppressions,
            (SELECT COUNT(*)::double precision
             FROM events AS event
             WHERE event.workspace_id = $1
               AND event.id IN ($5, (SELECT event_id FROM promoted_event))
               AND event.status = 'cancelled'
               -- `events` carries no cancelled_at: `updated_at` is the
               -- only marker, so the bound reads "the row reached the
               -- cancelled state no earlier than the action's window" —
               -- a show cancelled before the action ran is not harm the
               -- action caused, and one cancelled inside the window stays
               -- counted even if a later edit re-stamps the row.
               AND event.updated_at >= $3) AS show_cancellations,
            -- The unsubscribe denominator: how many distinct fans this
            -- action's sends reached. The assessment floor asks "what
            -- share left" — a count without its audience answers nothing.
            (SELECT COUNT(DISTINCT contact.fan_id)::double precision
             FROM all_contacts AS contact
             WHERE contact.action_id = $2) AS contacted
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.action_finished_at)
    .bind(measurement.due_at)
    .bind(measurement.subject_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(HarmObservation {
        unsubscribes: row.unsubscribes,
        complaints: row.complaints,
        refunds: row.refunds,
        fan_suppressions: row.fan_suppressions,
        show_cancellations: row.show_cancellations,
        contacted: row.contacted,
    })
}

#[derive(sqlx::FromRow)]
struct HarmRow {
    unsubscribes: f64,
    complaints: f64,
    refunds: f64,
    fan_suppressions: f64,
    show_cancellations: f64,
    contacted: f64,
}
