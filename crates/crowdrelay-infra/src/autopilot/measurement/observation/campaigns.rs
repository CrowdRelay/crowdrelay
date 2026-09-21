//! Communication-campaign observation arms.
//!
//! Split out of `observation.rs` so both stay inside the source-size
//! ratchet. These kinds all read the delivery ledger for the send under
//! measurement: the ledger is the system of record for who the campaign
//! actually reached, and every query bounds on each fan's own receipt rather
//! than the action's finish — a slow send still gets its full window.

use super::*;

/// Where a communication campaign's send stands, for the measurement guard.
/// `delivered_count` on the campaign row is the same answer denormalized,
/// but it is only stamped when the campaign completes — the ledger is what
/// the sends themselves write, so a stalled send must not read as delivered
/// through the count.
pub(super) enum CampaignDeliveryState {
    /// At least one `delivered` receipt exists.
    Reached,
    /// Dispatched (`status='scheduled'`) but no delivered receipt yet — the
    /// ledger results are still landing, so the measurement retries.
    InFlight,
    /// Draft, cancelled, completed-empty, or the campaign row is gone — no
    /// delivered receipt can still arrive.
    NeverReached,
}

pub(super) async fn campaign_delivery_state(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    campaign_id: uuid::Uuid,
) -> Result<CampaignDeliveryState, RepositoryError> {
    let row = sqlx::query_as::<_, (Option<String>, bool)>(
        r#"
        SELECT
            (SELECT campaign.status FROM communication_campaigns AS campaign
             WHERE campaign.workspace_id = $1 AND campaign.id = $2),
            EXISTS(
                SELECT 1 FROM communication_campaign_deliveries
                WHERE workspace_id = $1 AND campaign_id = $2
                  AND status = 'delivered'
            )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(campaign_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(if row.1 {
        CampaignDeliveryState::Reached
    } else if row.0.as_deref() == Some("scheduled") {
        CampaignDeliveryState::InFlight
    } else {
        CampaignDeliveryState::NeverReached
    })
}

/// Delivered fans who paid for a ticket inside fourteen days of their own
/// receipt — the conversion a send to existing fans can actually produce.
/// The window bounds on the fan's delivery, not the action's finish: a slow
/// send would otherwise hand late receipts a truncated or empty window.
/// Last-touch like the booking reply kinds — a newer delivered send to the
/// same fan before the order owns the conversion.
pub(super) async fn ticket_conversions(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    campaign_id: uuid::Uuid,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(DISTINCT delivery.fan_id)::double precision
        FROM communication_campaign_deliveries AS delivery
        JOIN fans AS fan
          ON fan.workspace_id=delivery.workspace_id
         AND fan.id=delivery.fan_id
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id=fan.workspace_id
         AND ticket_order.buyer_email=fan.normalized_email
        WHERE delivery.workspace_id=$1 AND delivery.campaign_id=$2
          AND delivery.status='delivered'
          AND ticket_order.status IN ('paid','partially_refunded')
          AND ticket_order.paid_at >= delivery.completed_at
          AND ticket_order.paid_at < delivery.completed_at + INTERVAL '14 days'
          AND NOT EXISTS (
              SELECT 1 FROM communication_campaign_deliveries AS newer
              WHERE newer.workspace_id=delivery.workspace_id
                AND newer.fan_id=delivery.fan_id
                AND newer.status='delivered'
                AND newer.campaign_id <> delivery.campaign_id
                AND newer.completed_at > delivery.completed_at
                AND newer.completed_at <= ticket_order.paid_at
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(campaign_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

/// The send's cost: consent withdrawals a delivered fan recorded after their
/// own receipt, over everyone the send reached. A rate, not a count — a
/// hundred sends should not read a hundred times worse than one for the same
/// per-fan harm. Per-delivery window and last-touch like the conversion arm:
/// a fan who got a newer email before withdrawing attributes the harm to it.
pub(super) async fn unsubscribe_rate(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    campaign_id: uuid::Uuid,
) -> Result<f64, RepositoryError> {
    let row = sqlx::query_as::<_, (f64, i64)>(
        r#"
        SELECT
            COUNT(DISTINCT delivery.fan_id) FILTER (
                WHERE EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id=delivery.workspace_id
                      AND consent.fan_id=delivery.fan_id
                      AND consent.purpose='marketing'
                      AND NOT consent.granted
                      AND consent.recorded_at >= delivery.completed_at
                      AND consent.recorded_at < delivery.completed_at + INTERVAL '7 days'
                      AND NOT EXISTS (
                          SELECT 1 FROM communication_campaign_deliveries AS newer
                          WHERE newer.workspace_id=delivery.workspace_id
                            AND newer.fan_id=delivery.fan_id
                            AND newer.status='delivered'
                            AND newer.campaign_id <> delivery.campaign_id
                            AND newer.completed_at > delivery.completed_at
                            AND newer.completed_at <= consent.recorded_at
                      )
                )
            )::double precision,
            COUNT(DISTINCT delivery.fan_id)
        FROM communication_campaign_deliveries AS delivery
        WHERE delivery.workspace_id=$1 AND delivery.campaign_id=$2
          AND delivery.status='delivered'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(campaign_id)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    // The reach guard already refused delivered=0; the division is still
    // guarded because the two statements can read different worlds under
    // concurrency.
    if row.1 <= 0 {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NEVER_PUBLISHED,
        ));
    }
    Ok(row.0 / row.1 as f64)
}
