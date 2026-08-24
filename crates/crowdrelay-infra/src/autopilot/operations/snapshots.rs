//! Set-oriented bounded-context snapshot loaders.

use super::*;
use crate::autopilot::bounded_u32;
use crowdrelay_domain::booking_discovery::BookingSupplySnapshot;

#[derive(Debug, FromRow)]
struct EventCampaignRow {
    event_id: Uuid,
    published: bool,
    communication_enabled: bool,
    starts_at: OffsetDateTime,
    interested_fans: i64,
    paid_buyers: i64,
    attendees: i64,
    announcement_sent: bool,
    interest_reminder_sent: bool,
    last_call_sent: bool,
    day_of_sent: bool,
    thank_you_sent: bool,
}

pub(in crate::autopilot) async fn load_event_campaign_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<EventCampaignSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, EventCampaignRow>(
        r#"
        SELECT
            event.id AS event_id,
            event.status IN ('published','completed') AS published,
            COALESCE(flag.enabled, false) AS communication_enabled,
            event.starts_at,
            (SELECT count(*)::bigint
             FROM event_interests AS interest
             WHERE interest.workspace_id = event.workspace_id
               AND interest.event_id = event.id) AS interested_fans,
            (SELECT count(DISTINCT orders.buyer_email)::bigint
             FROM ticket_orders AS orders
             JOIN ticket_sales AS sale
               ON sale.workspace_id = orders.workspace_id
              AND sale.id = orders.ticket_sale_id
             WHERE sale.workspace_id = event.workspace_id
               AND sale.event_id = event.id
               AND orders.status IN ('paid','partially_refunded')) AS paid_buyers,
            (SELECT count(DISTINCT pass.fan_id)::bigint
             FROM admission_passes AS pass
             WHERE pass.workspace_id = event.workspace_id
               AND pass.event_id = event.id
               AND pass.status = 'redeemed') AS attendees,
            COALESCE(bool_or(emission.phase = 'announcement'), false) AS announcement_sent,
            COALESCE(bool_or(emission.phase = 'interest_reminder'), false) AS interest_reminder_sent,
            COALESCE(bool_or(emission.phase = 'last_call'), false) AS last_call_sent,
            COALESCE(bool_or(emission.phase = 'day_of'), false) AS day_of_sent,
            COALESCE(bool_or(emission.phase = 'thank_you'), false) AS thank_you_sent
        FROM events AS event
        LEFT JOIN ecosystem_feature_flags AS flag
          ON flag.workspace_id = event.workspace_id
         AND flag.key = 'communication_campaigns_enabled'
        LEFT JOIN viryaos_campaign_lifecycle_emissions AS emission
          ON emission.workspace_id = event.workspace_id
         AND emission.event_id = event.id
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          AND event.starts_at BETWEEN $2 - INTERVAL '14 days' AND $2 + INTERVAL '121 days'
        GROUP BY event.id, flag.enabled
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

    rows.into_iter()
        .map(|row| {
            Ok(EventCampaignSnapshot {
                event_id: EventId::from_uuid(row.event_id),
                published: row.published,
                communication_enabled: row.communication_enabled,
                starts_at: row.starts_at,
                interested_fans: u32::try_from(row.interested_fans)
                    .map_err(|_| RepositoryError::Unexpected)?,
                paid_buyers: u32::try_from(row.paid_buyers)
                    .map_err(|_| RepositoryError::Unexpected)?,
                attendees: u32::try_from(row.attendees).map_err(|_| RepositoryError::Unexpected)?,
                history: EventCampaignHistory {
                    announcement_sent: row.announcement_sent,
                    interest_reminder_sent: row.interest_reminder_sent,
                    last_call_sent: row.last_call_sent,
                    day_of_sent: row.day_of_sent,
                    thank_you_sent: row.thank_you_sent,
                },
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct BundleRow {
    product_a: Uuid,
    product_b: Uuid,
    price_a_minor: i64,
    price_b_minor: i64,
    unit_cost_a_minor: Option<i64>,
    unit_cost_b_minor: Option<i64>,
    orders_a: i64,
    orders_b: i64,
    joint_orders: i64,
    in_flight: bool,
}

pub(in crate::autopilot) async fn load_merch_bundle_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<MerchBundleSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, BundleRow>(
        r#"
        WITH committed AS (
            SELECT reservation.id, variant.product_id
            FROM inventory_reservations AS reservation
            JOIN inventory_reservation_items AS item
              ON item.workspace_id = reservation.workspace_id
             AND item.reservation_id = reservation.id
            JOIN merch_variants AS variant
              ON variant.workspace_id = item.workspace_id
             AND variant.id = item.variant_id
            WHERE reservation.workspace_id = $1
              AND reservation.status = 'committed'
              AND reservation.reservation_kind = 'order'
              AND reservation.committed_at >= $2 - INTERVAL '90 days'
            GROUP BY reservation.id, variant.product_id
        ),
        product_orders AS (
            SELECT product_id, count(*)::bigint AS orders
            FROM committed
            GROUP BY product_id
        ),
        pairs AS (
            SELECT a.product_id AS product_a,
                   b.product_id AS product_b,
                   count(*)::bigint AS joint_orders
            FROM committed AS a
            JOIN committed AS b ON b.id = a.id AND b.product_id > a.product_id
            GROUP BY a.product_id, b.product_id
            HAVING count(*) >= 2
        )
        SELECT
            pairs.product_a,
            pairs.product_b,
            pa.price_gross_minor AS price_a_minor,
            pb.price_gross_minor AS price_b_minor,
            ea.unit_cost_minor AS unit_cost_a_minor,
            eb.unit_cost_minor AS unit_cost_b_minor,
            oa.orders AS orders_a,
            ob.orders AS orders_b,
            pairs.joint_orders,
            EXISTS (
                SELECT 1
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = $1
                  AND action.context = 'merch_bundle'
                  AND action.status IN ('awaiting_approval','queued','processing')
                  AND (
                      (action.payload->>'product_a')::uuid IN (pairs.product_a, pairs.product_b)
                      OR (action.payload->>'product_b')::uuid IN (pairs.product_a, pairs.product_b)
                  )
            ) AS in_flight
        FROM pairs
        JOIN product_orders AS oa ON oa.product_id = pairs.product_a
        JOIN product_orders AS ob ON ob.product_id = pairs.product_b
        JOIN merch_products AS pa
          ON pa.workspace_id = $1 AND pa.id = pairs.product_a AND pa.active
        JOIN merch_products AS pb
          ON pb.workspace_id = $1 AND pb.id = pairs.product_b AND pb.active
        LEFT JOIN viryaos_merch_product_economics AS ea
          ON ea.workspace_id = $1 AND ea.product_id = pairs.product_a
        LEFT JOIN viryaos_merch_product_economics AS eb
          ON eb.workspace_id = $1 AND eb.product_id = pairs.product_b
        ORDER BY pairs.joint_orders DESC, pairs.product_a, pairs.product_b
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(MerchBundleSnapshot {
                product_a: MerchProductId::from_uuid(row.product_a),
                product_b: MerchProductId::from_uuid(row.product_b),
                price_a_minor: row.price_a_minor,
                price_b_minor: row.price_b_minor,
                unit_cost_a_minor: row.unit_cost_a_minor,
                unit_cost_b_minor: row.unit_cost_b_minor,
                orders_a: u32::try_from(row.orders_a).map_err(|_| RepositoryError::Unexpected)?,
                orders_b: u32::try_from(row.orders_b).map_err(|_| RepositoryError::Unexpected)?,
                joint_orders: u32::try_from(row.joint_orders)
                    .map_err(|_| RepositoryError::Unexpected)?,
                in_flight: row.in_flight,
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct OutreachRow {
    opportunity_id: Uuid,
    target_id: Uuid,
    target_kind: String,
    target_version: i64,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    relevance_basis_points: i32,
    confidence_basis_points: i32,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    last_outreach_at: Option<OffsetDateTime>,
    target_last_outreach_at: Option<OffsetDateTime>,
    followup_count: i32,
    last_reply_disposition: String,
    in_flight: bool,
}

pub(in crate::autopilot) async fn load_outreach_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, OutreachRow>(
        r#"
        SELECT
            opportunity.id AS opportunity_id,
            target.id AS target_id,
            target.target_kind,
            target.version,
            opportunity.active AND target.active AND NOT target.do_not_contact AS active,
            target.verified,
            target.accepts_outreach,
            opportunity.relevance_basis_points,
            opportunity.confidence_basis_points,
            opportunity.observed_at,
            opportunity.expires_at,
            (SELECT max(interaction.occurred_at)
             FROM viryaos_outreach_interactions AS interaction
             WHERE interaction.workspace_id = opportunity.workspace_id
               AND interaction.opportunity_id = opportunity.id
               AND interaction.direction = 'outbound') AS last_outreach_at,
            target.last_outreach_at AS target_last_outreach_at,
            (SELECT count(*)::integer
             FROM viryaos_outreach_interactions AS interaction
             WHERE interaction.workspace_id = opportunity.workspace_id
               AND interaction.opportunity_id = opportunity.id
               AND interaction.direction = 'outbound'
               AND interaction.phase = 'followup') AS followup_count,
            COALESCE((
                SELECT interaction.disposition
                FROM viryaos_outreach_interactions AS interaction
                WHERE interaction.workspace_id = opportunity.workspace_id
                  AND interaction.opportunity_id = opportunity.id
                  AND interaction.direction = 'inbound'
                  AND interaction.phase = 'reply'
                ORDER BY interaction.occurred_at DESC, interaction.id DESC
                LIMIT 1
            ), 'none') AS last_reply_disposition,
            EXISTS (
                SELECT 1
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = $1
                  AND action.context = 'outreach'
                  AND action.subject_id = opportunity.id
                  AND action.status IN ('awaiting_approval','queued','processing')
            ) AS in_flight
        FROM viryaos_outreach_opportunities AS opportunity
        JOIN viryaos_outreach_targets AS target
          ON target.workspace_id = opportunity.workspace_id
         AND target.id = opportunity.target_id
        WHERE opportunity.workspace_id = $1
          AND opportunity.active
        ORDER BY opportunity.relevance_basis_points DESC, opportunity.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(OutreachSnapshot {
                opportunity_id: OutreachOpportunityId::from_uuid(row.opportunity_id),
                target_id: OutreachTargetId::from_uuid(row.target_id),
                target_kind: parse_outreach_kind(&row.target_kind)?,
                target_version: row.target_version,
                active: row.active,
                verified: row.verified,
                accepts_outreach: row.accepts_outreach,
                relevance_basis_points: u16::try_from(row.relevance_basis_points)
                    .map_err(|_| RepositoryError::Unexpected)?,
                evidence_confidence: parse_confidence(row.confidence_basis_points)?,
                observed_at: row.observed_at,
                expires_at: row.expires_at,
                last_outreach_at: row.last_outreach_at,
                target_last_outreach_at: row.target_last_outreach_at,
                followup_count: u16::try_from(row.followup_count)
                    .map_err(|_| RepositoryError::Unexpected)?,
                last_reply: parse_outreach_reply(&row.last_reply_disposition)?,
                in_flight: row.in_flight,
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct BeaconDiscoveryRow {
    event_id: Uuid,
    event_starts_at: OffsetDateTime,
    known_local_beacons: i64,
    last_discovery_at: Option<OffsetDateTime>,
    in_flight: bool,
}

pub(in crate::autopilot) async fn load_beacon_discovery_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, BeaconDiscoveryRow>(
        r#"
        SELECT event.id AS event_id, event.starts_at AS event_starts_at,
               (SELECT count(*)::bigint
                FROM viryaos_beacons beacon
                WHERE beacon.workspace_id=event.workspace_id
                  AND beacon.city_id=event.city_id
                  AND beacon.active AND beacon.verified AND beacon.accepts_outreach
                  AND NOT beacon.do_not_contact
                  AND beacon.contact_email IS NOT NULL) AS known_local_beacons,
               (SELECT max(action.finished_at)
                FROM viryaos_autopilot_actions action
                WHERE action.workspace_id=event.workspace_id
                  AND action.context='beacon'
                  AND action.subject_kind='event'
                  AND action.subject_id=event.id
                  AND action.action_kind='beacon.discovery.request'
                  AND action.status='succeeded') AS last_discovery_at,
               EXISTS (
                   SELECT 1 FROM viryaos_autopilot_actions action
                   WHERE action.workspace_id=event.workspace_id
                     AND action.context='beacon'
                     AND action.subject_kind='event'
                     AND action.subject_id=event.id
                     AND action.action_kind='beacon.discovery.request'
                     AND action.status IN ('awaiting_approval','queued','processing')
               ) AS in_flight
        FROM events event
        WHERE event.workspace_id=$1
          AND event.status='published'
          AND event.city_id IS NOT NULL
          AND event.starts_at BETWEEN $2 AND $2 + INTERVAL '60 days'
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

    rows.into_iter()
        .map(|row| {
            Ok(BeaconDiscoverySnapshot {
                event_id: EventId::from_uuid(row.event_id),
                event_starts_at: row.event_starts_at,
                known_local_beacons: u16::try_from(row.known_local_beacons)
                    .map_err(|_| RepositoryError::Unexpected)?,
                last_discovery_at: row.last_discovery_at,
                in_flight: row.in_flight,
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct BeaconCampaignRow {
    beacon_id: Uuid,
    beacon_version: i64,
    event_id: Uuid,
    beacon_kind: String,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    relationship_score: i32,
    relevance_basis_points: i32,
    confidence_basis_points: i32,
    event_starts_at: OffsetDateTime,
    last_outreach_at: Option<OffsetDateTime>,
    followup_count: i32,
    last_reply_disposition: String,
    in_flight: bool,
}

pub(in crate::autopilot) async fn load_beacon_campaign_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, BeaconCampaignRow>(
        r#"
        SELECT
            beacon.id AS beacon_id,
            beacon.version AS beacon_version,
            event.id AS event_id,
            beacon.beacon_kind,
            beacon.active,
            beacon.verified,
            beacon.accepts_outreach,
            beacon.do_not_contact,
            beacon.relationship_score,
            beacon.relevance_basis_points,
            beacon.confidence_basis_points,
            event.starts_at AS event_starts_at,
            campaign.last_outreach_at,
            COALESCE(campaign.followup_count, 0) AS followup_count,
            COALESCE(campaign.last_reply_disposition, 'none') AS last_reply_disposition,
            EXISTS (
                SELECT 1
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = beacon.workspace_id
                  AND action.context = 'beacon'
                  AND action.subject_id = beacon.id
                  AND action.status IN ('awaiting_approval','queued','processing')
            ) AS in_flight
        FROM viryaos_beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id
         AND event.status IN ('published','completed')
         AND event.starts_at BETWEEN $2 - INTERVAL '5 days' AND $2 + INTERVAL '60 days'
         AND (
             beacon.city_id IS NULL
             OR beacon.city_id = event.city_id
         )
        LEFT JOIN viryaos_beacon_campaigns AS campaign
          ON campaign.workspace_id = beacon.workspace_id
         AND campaign.beacon_id = beacon.id
         AND campaign.event_id = event.id
        WHERE beacon.workspace_id = $1
          AND beacon.active
          AND COALESCE(campaign.status, 'candidate') NOT IN ('declined','suppressed','closed')
        ORDER BY event.starts_at, beacon.relevance_basis_points DESC,
                 beacon.relationship_score DESC, beacon.id
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(BeaconCampaignSnapshot {
                beacon_id: BeaconId::from_uuid(row.beacon_id),
                beacon_version: row.beacon_version,
                event_id: EventId::from_uuid(row.event_id),
                kind: parse_beacon_kind(&row.beacon_kind)?,
                active: row.active,
                verified: row.verified,
                accepts_outreach: row.accepts_outreach,
                do_not_contact: row.do_not_contact,
                relationship_score: u16::try_from(row.relationship_score)
                    .map_err(|_| RepositoryError::Unexpected)?,
                relevance_basis_points: u16::try_from(row.relevance_basis_points)
                    .map_err(|_| RepositoryError::Unexpected)?,
                evidence_confidence: parse_confidence(row.confidence_basis_points)?,
                event_starts_at: row.event_starts_at,
                last_outreach_at: row.last_outreach_at,
                followup_count: u16::try_from(row.followup_count)
                    .map_err(|_| RepositoryError::Unexpected)?,
                last_reply: parse_beacon_reply(&row.last_reply_disposition)?,
                in_flight: row.in_flight,
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct ContentRow {
    source_id: Uuid,
    source_kind: String,
    source_version: i64,
    occurred_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    completed_artifacts: Vec<String>,
    inflight_artifacts: Vec<String>,
}

pub(in crate::autopilot) async fn load_content_supply_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<ContentSupplySnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, ContentRow>(
        r#"
        SELECT
            source.id AS source_id,
            source.source_kind,
            source.version AS source_version,
            source.occurred_at,
            source.expires_at,
            COALESCE(ARRAY(
                SELECT DISTINCT action.payload->>'artifact'
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.subject_id = source.id
                  AND action.status = 'succeeded'
                  AND EXISTS (
                      SELECT 1
                      FROM viryaos_autopilot_execution_reports AS report
                      WHERE report.workspace_id = action.workspace_id
                        AND report.action_id = action.id
                        AND report.status = 'succeeded'
                  )
            ), ARRAY[]::text[]) AS completed_artifacts,
            COALESCE(ARRAY(
                SELECT DISTINCT action.payload->>'artifact'
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = source.workspace_id
                  AND action.context = 'content_supply'
                  AND action.subject_id = source.id
                  AND (
                      action.status IN ('awaiting_approval','queued','processing')
                      OR (
                          action.status = 'succeeded'
                          AND EXISTS (
                              SELECT 1
                              FROM viryaos_autopilot_action_emissions AS emission
                              WHERE emission.workspace_id = action.workspace_id
                                AND emission.action_id = action.id
                          )
                          AND NOT EXISTS (
                              SELECT 1
                              FROM viryaos_autopilot_execution_reports AS report
                              WHERE report.workspace_id = action.workspace_id
                                AND report.action_id = action.id
                                AND report.status IN ('succeeded','failed')
                          )
                      )
                  )
            ), ARRAY[]::text[]) AS inflight_artifacts
        FROM viryaos_content_sources AS source
        WHERE source.workspace_id = $1
          AND source.active
        ORDER BY source.occurred_at DESC, source.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(ContentSupplySnapshot {
                source_id: ContentSourceId::from_uuid(row.source_id),
                source_kind: parse_content_source_kind(&row.source_kind)?,
                source_version: row.source_version,
                occurred_at: row.occurred_at,
                expires_at: row.expires_at,
                completed_artifacts: row
                    .completed_artifacts
                    .iter()
                    .map(|value| parse_artifact(value))
                    .collect::<Result<_, _>>()?,
                in_flight_artifacts: row
                    .inflight_artifacts
                    .iter()
                    .map(|value| parse_artifact(value))
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct ExperimentRow {
    experiment_id: Uuid,
    experiment_version: i64,
    metric_kind: String,
    status: String,
    variant_id: Uuid,
    allocation_basis_points: i32,
    exposures: i64,
    conversions: i64,
    value_minor: i64,
    active: bool,
}

pub(in crate::autopilot) async fn load_experiment_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<ExperimentSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, ExperimentRow>(
        r#"
        SELECT
            experiment.id AS experiment_id,
            experiment.version AS experiment_version,
            experiment.metric_kind,
            experiment.status,
            variant.id AS variant_id,
            variant.allocation_basis_points,
            variant.exposures,
            variant.conversions,
            variant.value_minor,
            variant.active
        FROM viryaos_experiments AS experiment
        JOIN viryaos_experiment_variants AS variant
          ON variant.workspace_id = experiment.workspace_id
         AND variant.experiment_id = experiment.id
        WHERE experiment.workspace_id = $1
          AND experiment.status = 'running'
        ORDER BY experiment.id, variant.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_SNAPSHOTS_PER_CONTEXT * 8)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    let mut grouped: HashMap<Uuid, ExperimentSnapshot> = HashMap::new();
    for row in rows {
        let metric = parse_experiment_metric(&row.metric_kind)?;
        let entry = grouped
            .entry(row.experiment_id)
            .or_insert_with(|| ExperimentSnapshot {
                experiment_id: ExperimentId::from_uuid(row.experiment_id),
                version: row.experiment_version,
                metric,
                running: row.status == "running",
                variants: Vec::new(),
            });
        entry.variants.push(ExperimentVariantSnapshot {
            variant_id: ExperimentVariantId::from_uuid(row.variant_id),
            exposures: u64::try_from(row.exposures).map_err(|_| RepositoryError::Unexpected)?,
            conversions: u64::try_from(row.conversions).map_err(|_| RepositoryError::Unexpected)?,
            value_minor: row.value_minor,
            allocation_basis_points: u16::try_from(row.allocation_basis_points)
                .map_err(|_| RepositoryError::Unexpected)?,
            active: row.active,
        });
    }
    let mut snapshots: Vec<_> = grouped.into_values().collect();
    snapshots.sort_by_key(|snapshot| snapshot.experiment_id);
    snapshots.truncate(
        usize::try_from(MAX_SNAPSHOTS_PER_CONTEXT).map_err(|_| RepositoryError::Unexpected)?,
    );
    Ok(snapshots)
}

#[derive(Debug, FromRow)]
struct ShowRow {
    event_id: Uuid,
    starts_at: OffsetDateTime,
    item_key: String,
    already_done: bool,
    verifiable_fact: bool,
    last_escalated_at: Option<OffsetDateTime>,
}

pub(in crate::autopilot) async fn load_show_task_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<ShowTaskSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, ShowRow>(
        r#"
        WITH task(item_key) AS (
            VALUES
                ('announcement_published'),
                ('ticketing_verified'),
                ('staff_assigned'),
                ('offline_snapshot_ready'),
                ('gate_device_charged'),
                ('backup_device_ready'),
                ('network_tested'),
                ('guestlist_checked'),
                ('post_show_reconciliation'),
                ('post_show_report')
        )
        SELECT
            event.id AS event_id,
            event.starts_at,
            task.item_key,
            COALESCE(checklist.status = 'done', false) AS already_done,
            CASE task.item_key
                WHEN 'announcement_published' THEN event.status IN ('published','completed')
                WHEN 'ticketing_verified' THEN EXISTS (
                    SELECT 1
                    FROM ticket_sales AS sale
                    WHERE sale.workspace_id = event.workspace_id
                      AND sale.event_id = event.id
                      AND sale.active
                      AND sale.sales_open_at < sale.sales_close_at
                      AND EXISTS (
                          SELECT 1
                          FROM ticket_types AS type
                          WHERE type.workspace_id = sale.workspace_id
                            AND type.ticket_sale_id = sale.id
                            AND type.active
                      )
                )
                ELSE false
            END AS verifiable_fact,
            (SELECT max(action.finished_at)
             FROM viryaos_autopilot_actions AS action
             WHERE action.workspace_id = event.workspace_id
               AND action.context = 'show_operations'
               AND action.action_kind = 'show.task.escalate'
               AND action.subject_id = event.id
               AND action.status = 'succeeded'
               AND action.payload->>'task' = task.item_key) AS last_escalated_at
        FROM events AS event
        CROSS JOIN task
        LEFT JOIN show_checklist_items AS checklist
          ON checklist.workspace_id = event.workspace_id
         AND checklist.event_id = event.id
         AND checklist.item_key = task.item_key
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          AND event.starts_at BETWEEN $2 - INTERVAL '7 days' AND $2 + INTERVAL '14 days'
        ORDER BY event.starts_at, task.item_key
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(ShowTaskSnapshot {
                event_id: EventId::from_uuid(row.event_id),
                task: parse_show_task(&row.item_key)?,
                starts_at: row.starts_at,
                already_done: row.already_done,
                verifiable_fact: row.verifiable_fact,
                last_escalated_at: row.last_escalated_at,
            })
        })
        .collect()
}

pub(in crate::autopilot) fn parse_beacon_kind(value: &str) -> Result<BeaconKind, RepositoryError> {
    match value {
        "radio" => Ok(BeaconKind::Radio),
        "local_press" => Ok(BeaconKind::LocalPress),
        "television" => Ok(BeaconKind::Television),
        "reviewer" => Ok(BeaconKind::Reviewer),
        "creator" => Ok(BeaconKind::Creator),
        "photographer" => Ok(BeaconKind::Photographer),
        "promoter" => Ok(BeaconKind::Promoter),
        "venue" => Ok(BeaconKind::Venue),
        "scene_partner" => Ok(BeaconKind::ScenePartner),
        "patron" => Ok(BeaconKind::Patron),
        "community" => Ok(BeaconKind::Community),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_beacon_reply(value: &str) -> Result<BeaconReplyDisposition, RepositoryError> {
    match value {
        "none" => Ok(BeaconReplyDisposition::None),
        "received" => Ok(BeaconReplyDisposition::Received),
        "interested" => Ok(BeaconReplyDisposition::Interested),
        "partner" => Ok(BeaconReplyDisposition::Partner),
        "declined" => Ok(BeaconReplyDisposition::Declined),
        "do_not_contact" => Ok(BeaconReplyDisposition::DoNotContact),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_outreach_kind(value: &str) -> Result<OutreachTargetKind, RepositoryError> {
    match value {
        "playlist" => Ok(OutreachTargetKind::Playlist),
        "radio" => Ok(OutreachTargetKind::Radio),
        "press" => Ok(OutreachTargetKind::Press),
        "creator" => Ok(OutreachTargetKind::Creator),
        "support_slot" => Ok(OutreachTargetKind::SupportSlot),
        "endorsement" => Ok(OutreachTargetKind::Endorsement),
        "media_patronage" => Ok(OutreachTargetKind::MediaPatronage),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_outreach_reply(
    value: &str,
) -> Result<OutreachReplyDisposition, RepositoryError> {
    match value {
        "none" => Ok(OutreachReplyDisposition::None),
        "received" => Ok(OutreachReplyDisposition::Received),
        "positive" => Ok(OutreachReplyDisposition::Positive),
        "declined" => Ok(OutreachReplyDisposition::Declined),
        "do_not_contact" => Ok(OutreachReplyDisposition::DoNotContact),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_content_source_kind(value: &str) -> Result<ContentSourceKind, RepositoryError> {
    match value {
        "event" => Ok(ContentSourceKind::Event),
        "release" => Ok(ContentSourceKind::Release),
        "show_completed" => Ok(ContentSourceKind::ShowCompleted),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_artifact(value: &str) -> Result<ContentArtifactKind, RepositoryError> {
    match value {
        "signal_push" => Ok(ContentArtifactKind::SignalPush),
        "newsletter_block" => Ok(ContentArtifactKind::NewsletterBlock),
        "social_feed" => Ok(ContentArtifactKind::SocialFeed),
        "social_story" => Ok(ContentArtifactKind::SocialStory),
        "live_listing" => Ok(ContentArtifactKind::LiveListing),
        "press_hook" => Ok(ContentArtifactKind::PressHook),
        "post_show_recap" => Ok(ContentArtifactKind::PostShowRecap),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_experiment_metric(value: &str) -> Result<ExperimentMetric, RepositoryError> {
    match value {
        "conversion" => Ok(ExperimentMetric::Conversion),
        "revenue_per_exposure" => Ok(ExperimentMetric::RevenuePerExposure),
        _ => Err(RepositoryError::Unexpected),
    }
}

pub(super) fn parse_show_task(value: &str) -> Result<ShowTaskKind, RepositoryError> {
    match value {
        "announcement_published" => Ok(ShowTaskKind::AnnouncementPublished),
        "ticketing_verified" => Ok(ShowTaskKind::TicketingVerified),
        "staff_assigned" => Ok(ShowTaskKind::StaffAssigned),
        "offline_snapshot_ready" => Ok(ShowTaskKind::OfflineSnapshotReady),
        "gate_device_charged" => Ok(ShowTaskKind::GateDeviceCharged),
        "backup_device_ready" => Ok(ShowTaskKind::BackupDeviceReady),
        "network_tested" => Ok(ShowTaskKind::NetworkTested),
        "guestlist_checked" => Ok(ShowTaskKind::GuestlistChecked),
        "post_show_reconciliation" => Ok(ShowTaskKind::PostShowReconciliation),
        "post_show_report" => Ok(ShowTaskKind::PostShowReport),
        _ => Err(RepositoryError::Unexpected),
    }
}

#[derive(Debug, FromRow)]
struct BeaconInviteRow {
    beacon_id: Uuid,
    beacon_version: i64,
    event_id: Uuid,
    beacon_kind: String,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    relationship_score: i32,
    hours_until_event: i64,
    hours_since_last_invite_batch: Option<i64>,
}

/// Verified scene nodes with one upcoming show in their own city, and the
/// cooldown clock for their last invite ask. The domain decides whether any
/// of it is worth an action; this only reports the facts, bounded like every
/// snapshot read.
pub(in crate::autopilot) async fn load_beacon_invite_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<BeaconInviteSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, BeaconInviteRow>(
        r#"
        SELECT DISTINCT ON (beacon.id, event.id)
            beacon.id AS beacon_id,
            beacon.version AS beacon_version,
            event.id AS event_id,
            beacon.beacon_kind,
            beacon.active,
            beacon.verified,
            beacon.accepts_outreach,
            beacon.do_not_contact,
            beacon.relationship_score,
            FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 3600)::bigint
                AS hours_until_event,
            GREATEST(
                0,
                FLOOR(EXTRACT(EPOCH FROM ($2 - last_ask.asked_at)) / 3600)
            )::bigint AS hours_since_last_invite_batch
        FROM viryaos_beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id
         AND event.status = 'published'
         AND event.starts_at BETWEEN $2 AND $2 + INTERVAL '60 days'
         AND (beacon.city_id IS NULL OR beacon.city_id = event.city_id)
        LEFT JOIN LATERAL (
            SELECT max(action.created_at) AS asked_at
            FROM viryaos_autopilot_actions AS action
            WHERE action.workspace_id = beacon.workspace_id
              AND action.context = 'beacon'
              AND action.subject_id = beacon.id
              AND action.action_kind = 'beacon.invite_batch.request'
              AND action.status IN ('awaiting_approval', 'queued', 'processing', 'succeeded')
        ) AS last_ask ON true
        WHERE beacon.workspace_id = $1
          AND beacon.active
          AND beacon.verified
          AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
        ORDER BY beacon.id, event.id, event.starts_at
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(BeaconInviteSnapshot {
                beacon_id: BeaconId::from_uuid(row.beacon_id),
                beacon_version: row.beacon_version,
                event_id: EventId::from_uuid(row.event_id),
                kind: parse_beacon_kind(&row.beacon_kind)?,
                active: row.active,
                verified: row.verified,
                accepts_outreach: row.accepts_outreach,
                do_not_contact: row.do_not_contact,
                relationship_score: u16::try_from(row.relationship_score)
                    .map_err(|_| RepositoryError::Unexpected)?,
                hours_until_event: row.hours_until_event,
                hours_since_last_invite_batch: row
                    .hours_since_last_invite_batch
                    .map(|hours| u32::try_from(hours).unwrap_or(u32::MAX)),
            })
        })
        .collect()
}

/// The booking pipeline's supply: contactable targets today, plus the cooldown
/// clock on the last discovery request. Bounded like every snapshot read; the
/// domain decides whether any of it is worth an action.
pub(in crate::autopilot) async fn load_booking_supply_snapshot(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<BookingSupplySnapshot, RepositoryError> {
    repo.bounded(async {
        let row = sqlx::query_as::<_, (i64, Option<i64>)>(
            r#"
            SELECT
                (
                    SELECT count(*)::bigint
                    FROM viryaos_booking_targets AS target
                    WHERE target.workspace_id = $1
                      AND target.active
                      AND target.accepts_booking
                ),
                (
                    SELECT GREATEST(
                        0,
                        FLOOR(EXTRACT(EPOCH FROM ($2 - max(d.evaluated_at))) / 3600)
                    )::bigint
                    FROM viryaos_autopilot_decisions AS d
                    WHERE d.workspace_id = $1
                      AND d.context = 'booking_opportunity'
                      AND d.decision_kind = 'request_booking_target_discovery'
                )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .fetch_one(&repo.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(BookingSupplySnapshot {
            active_eligible_targets: bounded_u32(row.0)?,
            hours_since_last_request: row.1.and_then(|hours| u32::try_from(hours).ok()),
        })
    })
    .await
}
