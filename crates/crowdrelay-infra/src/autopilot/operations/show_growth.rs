//! One set-oriented snapshot query for the attendance / merch demand loop.

use super::*;
use crowdrelay_domain::show_growth::{ShowGrowthHistory, ShowGrowthSnapshot};

#[derive(Debug, FromRow)]
struct ShowGrowthRow {
    event_id: Uuid,
    published: bool,
    communication_enabled: bool,
    starts_at: OffsetDateTime,
    capacity: i64,
    paid_tickets: i64,
    paid_buyers: i64,
    paid_tickets_last_7d: i64,
    interested_fans: i64,
    city_signal_fans: i64,
    qualified_referrers_in_city: i64,
    beacon_partners: i64,
    attendees: i64,
    free_listing_sweep_requested: bool,
    audience_capture_setup_requested: bool,
    partner_cross_promo_requested: bool,
    fan_ambassadors_requested: bool,
    social_proof_relay_requested: bool,
    free_fan_channel_push_requested: bool,
    merch_buyer_offer_requested: bool,
    high_intent_last_mile_requested: bool,
    post_show_merch_requested: bool,
}

pub(in crate::autopilot) async fn load_show_growth_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, ShowGrowthRow>(
        r#"
        SELECT
            event.id AS event_id,
            event.status IN ('published','completed') AS published,
            COALESCE(comm.enabled, false) AS communication_enabled,
            event.starts_at,
            COALESCE(ticket_sale.capacity, admission.capacity, 0)::bigint AS capacity,
            COALESCE(ticket.paid_tickets, 0)::bigint AS paid_tickets,
            COALESCE(ticket.paid_buyers, 0)::bigint AS paid_buyers,
            COALESCE(ticket.paid_tickets_last_7d, 0)::bigint AS paid_tickets_last_7d,
            COALESCE(interest.interested_fans, 0)::bigint AS interested_fans,
            COALESCE(city_signal.city_signal_fans, 0)::bigint AS city_signal_fans,
            COALESCE(referrers.qualified_referrers, 0)::bigint AS qualified_referrers_in_city,
            COALESCE(beacons.beacon_partners, 0)::bigint AS beacon_partners,
            COALESCE(attendance.attendees, 0)::bigint AS attendees,
            COALESCE(history.free_listing_sweep_requested, false) AS free_listing_sweep_requested,
            COALESCE(history.audience_capture_setup_requested, false) AS audience_capture_setup_requested,
            COALESCE(history.partner_cross_promo_requested, false) AS partner_cross_promo_requested,
            COALESCE(history.fan_ambassadors_requested, false) AS fan_ambassadors_requested,
            COALESCE(history.social_proof_relay_requested, false) AS social_proof_relay_requested,
            COALESCE(history.free_fan_channel_push_requested, false) AS free_fan_channel_push_requested,
            COALESCE(history.merch_buyer_offer_requested, false) AS merch_buyer_offer_requested,
            COALESCE(history.high_intent_last_mile_requested, false) AS high_intent_last_mile_requested,
            COALESCE(history.post_show_merch_requested, false) AS post_show_merch_requested
        FROM events AS event
        LEFT JOIN ecosystem_feature_flags AS comm
          ON comm.workspace_id = event.workspace_id
         AND comm.key = 'communication_campaigns_enabled'
        LEFT JOIN ticket_sales AS ticket_sale
          ON ticket_sale.workspace_id = event.workspace_id
         AND ticket_sale.event_id = event.id
         AND ticket_sale.active
        LEFT JOIN LATERAL (
            SELECT
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                ), 0)::bigint AS paid_tickets,
                COUNT(DISTINCT orders.buyer_email) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                )::bigint AS paid_buyers,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                      AND orders.paid_at >= $2 - INTERVAL '7 days'
                ), 0)::bigint AS paid_tickets_last_7d
            FROM ticket_orders AS orders
            JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE orders.workspace_id = event.workspace_id
              AND orders.ticket_sale_id = ticket_sale.id
        ) AS ticket ON true
        LEFT JOIN LATERAL (
            SELECT MAX(pool.capacity)::bigint AS capacity
            FROM admission_pools AS pool
            WHERE pool.workspace_id = event.workspace_id
              AND pool.event_id = event.id
        ) AS admission ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS interested_fans
            FROM event_interests AS ei
            WHERE ei.workspace_id = event.workspace_id
              AND ei.event_id = event.id
        ) AS interest ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(DISTINCT fci.fan_id)::bigint AS city_signal_fans
            FROM fan_city_interests AS fci
            JOIN fans AS fan
              ON fan.workspace_id = fci.workspace_id
             AND fan.id = fci.fan_id
             AND fan.status = 'active'
            WHERE fci.workspace_id = event.workspace_id
              AND event.city_id IS NOT NULL
              AND fci.city_id = event.city_id
        ) AS city_signal ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(DISTINCT referral.referrer_fan_id)::bigint AS qualified_referrers
            FROM referral_attributions AS referral
            JOIN fan_city_interests AS fci
              ON fci.workspace_id = referral.workspace_id
             AND fci.fan_id = referral.referrer_fan_id
            JOIN fans AS fan
              ON fan.workspace_id = referral.workspace_id
             AND fan.id = referral.referrer_fan_id
             AND fan.status = 'active'
            WHERE referral.workspace_id = event.workspace_id
              AND event.city_id IS NOT NULL
              AND fci.city_id = event.city_id
        ) AS referrers ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS beacon_partners
            FROM viryaos_beacon_campaigns AS campaign
            WHERE campaign.workspace_id = event.workspace_id
              AND campaign.event_id = event.id
              AND campaign.status = 'partner'
        ) AS beacons ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS attendees
            FROM admission_passes AS pass
            WHERE pass.workspace_id = event.workspace_id
              AND pass.event_id = event.id
              AND pass.status = 'redeemed'
        ) AS attendance ON true
        LEFT JOIN LATERAL (
            SELECT
                BOOL_OR(action.payload ->> 'lever' = 'free_listing_sweep') AS free_listing_sweep_requested,
                BOOL_OR(action.payload ->> 'lever' = 'audience_capture_setup') AS audience_capture_setup_requested,
                BOOL_OR(action.payload ->> 'lever' = 'partner_cross_promo') AS partner_cross_promo_requested,
                BOOL_OR(action.payload ->> 'lever' = 'fan_ambassadors') AS fan_ambassadors_requested,
                BOOL_OR(action.payload ->> 'lever' = 'social_proof_relay') AS social_proof_relay_requested,
                BOOL_OR(action.payload ->> 'lever' = 'free_fan_channel_push') AS free_fan_channel_push_requested,
                BOOL_OR(action.payload ->> 'lever' IN ('merch_buyer_offer','merch_preorder_pickup')) AS merch_buyer_offer_requested,
                BOOL_OR(action.payload ->> 'lever' = 'high_intent_last_mile') AS high_intent_last_mile_requested,
                BOOL_OR(action.payload ->> 'lever' = 'post_show_merch_follow_up') AS post_show_merch_requested
            FROM viryaos_autopilot_actions AS action
            WHERE action.workspace_id = event.workspace_id
              AND action.context = 'show_growth'
              AND action.subject_id = event.id
              AND action.status NOT IN ('cancelled','failed')
        ) AS history ON true
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          AND event.starts_at BETWEEN $2 - INTERVAL '2 days' AND $2 + INTERVAL '61 days'
        ORDER BY event.starts_at, event.id
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter().map(map_row).collect()
}

fn map_row(row: ShowGrowthRow) -> Result<ShowGrowthSnapshot, RepositoryError> {
    Ok(ShowGrowthSnapshot {
        event_id: EventId::from_uuid(row.event_id),
        published: row.published,
        communication_enabled: row.communication_enabled,
        starts_at: row.starts_at,
        capacity: bounded_u32(row.capacity)?,
        paid_tickets: bounded_u32(row.paid_tickets)?,
        paid_buyers: bounded_u32(row.paid_buyers)?,
        paid_tickets_last_7d: bounded_u32(row.paid_tickets_last_7d)?,
        interested_fans: bounded_u32(row.interested_fans)?,
        city_signal_fans: bounded_u32(row.city_signal_fans)?,
        qualified_referrers_in_city: bounded_u32(row.qualified_referrers_in_city)?,
        beacon_partners: u16::try_from(row.beacon_partners)
            .map_err(|_| RepositoryError::Unexpected)?,
        attendees: bounded_u32(row.attendees)?,
        history: ShowGrowthHistory {
            free_listing_sweep_requested: row.free_listing_sweep_requested,
            audience_capture_setup_requested: row.audience_capture_setup_requested,
            partner_cross_promo_requested: row.partner_cross_promo_requested,
            fan_ambassadors_requested: row.fan_ambassadors_requested,
            social_proof_relay_requested: row.social_proof_relay_requested,
            free_fan_channel_push_requested: row.free_fan_channel_push_requested,
            merch_buyer_offer_requested: row.merch_buyer_offer_requested,
            high_intent_last_mile_requested: row.high_intent_last_mile_requested,
            post_show_merch_requested: row.post_show_merch_requested,
        },
    })
}

fn bounded_u32(value: i64) -> Result<u32, RepositoryError> {
    u32::try_from(value).map_err(|_| RepositoryError::Unexpected)
}
