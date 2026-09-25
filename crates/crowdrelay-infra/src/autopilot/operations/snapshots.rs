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
    title: String,
    sender_name: String,
    city_name: Option<String>,
    venue: Option<String>,
    ticket_url: Option<String>,
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
            event.title,
            workspace.name AS sender_name,
            city.name AS city_name,
            event.venue,
            event.ticket_url,
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
        JOIN workspaces AS workspace
          ON workspace.id = event.workspace_id
        -- `cities` is a shared catalogue with no workspace_id; the tenant
        -- boundary is `event.workspace_id = $1`.
        LEFT JOIN cities AS city
          ON city.id = event.city_id
        LEFT JOIN ecosystem_feature_flags AS flag
          ON flag.workspace_id = event.workspace_id
         AND flag.key = 'communication_campaigns_enabled'
        LEFT JOIN campaign_lifecycle_emissions AS emission
          ON emission.workspace_id = event.workspace_id
         AND emission.event_id = event.id
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          AND event.starts_at BETWEEN $2 - INTERVAL '14 days' AND $2 + INTERVAL '121 days'
        GROUP BY event.id, flag.enabled, workspace.name, city.name
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
                title: row.title,
                sender_name: row.sender_name,
                city_name: row.city_name,
                venue: row.venue,
                ticket_url: row.ticket_url,
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
                FROM autopilot_actions AS action
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
        LEFT JOIN merch_product_economics AS ea
          ON ea.workspace_id = $1 AND ea.product_id = pairs.product_a
        LEFT JOIN merch_product_economics AS eb
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
    lifetime_outbound: i32,
    target_ever_replied: bool,
    last_reply_disposition: String,
    in_flight: bool,
    wave_only: bool,
    thread_followup: bool,
}

/// The live-opportunity read shared by the cycle's snapshot load and the
/// conversation drawer's "what happens next" — one statement so the drawer
/// explains the same rules the evaluator applies, not a second opinion of
/// them. `$3` scopes to one target; `NULL` loads them all.
const OUTREACH_SNAPSHOT_SQL: &str = r#"
        SELECT
            opportunity.id AS opportunity_id,
            target.id AS target_id,
            target.target_kind,
            target.version AS target_version,
            opportunity.active AND target.active AND NOT target.do_not_contact AS active,
            target.verified,
            target.accepts_outreach,
            opportunity.relevance_basis_points,
            opportunity.confidence_basis_points,
            opportunity.observed_at,
            opportunity.expires_at,
            (SELECT max(interaction.occurred_at)
             FROM outreach_interactions AS interaction
             WHERE interaction.workspace_id = opportunity.workspace_id
               AND interaction.opportunity_id = opportunity.id
               AND interaction.direction = 'outbound') AS last_outreach_at,
            -- A thread's silence clock is the imported unlinked message the
            -- seed anchored on, not the denormalized column: any ledger
            -- writer that forgets to stamp `last_outreach_at` would hold a
            -- standing thread at InvalidSnapshot forever.
            CASE WHEN opportunity.source = 'thread_followup' THEN (
                SELECT message.occurred_at
                FROM outreach_interactions AS message
                WHERE message.workspace_id = opportunity.workspace_id
                  AND message.target_id = target.id
                  AND message.direction = 'outbound'
                  AND message.opportunity_id IS NULL
                  AND message.occurred_at <= now()
                  AND NOT EXISTS (
                      SELECT 1
                      FROM outreach_interactions AS later
                      WHERE later.workspace_id = opportunity.workspace_id
                        AND later.target_id = target.id
                        AND later.occurred_at > message.occurred_at
                  )
                ORDER BY message.occurred_at DESC, message.id DESC
                LIMIT 1
            ) ELSE target.last_outreach_at END AS target_last_outreach_at,
            (SELECT count(*)::integer
             FROM outreach_interactions AS interaction
             WHERE interaction.workspace_id = opportunity.workspace_id
               AND interaction.opportunity_id = opportunity.id
               AND interaction.direction = 'outbound'
               AND interaction.phase = 'followup') AS followup_count,
            -- Scoped to the target, not the opportunity: this is the count the
            -- person on the other end experiences.
            (SELECT count(*)::integer
             FROM outreach_interactions AS interaction
             WHERE interaction.workspace_id = opportunity.workspace_id
               AND interaction.target_id = target.id
               AND interaction.direction = 'outbound') AS lifetime_outbound,
            EXISTS (
                SELECT 1
                FROM outreach_interactions AS interaction
                WHERE interaction.workspace_id = opportunity.workspace_id
                  AND interaction.target_id = target.id
                  AND interaction.direction = 'inbound'
            ) AS target_ever_replied,
            COALESCE((
                SELECT interaction.disposition
                FROM outreach_interactions AS interaction
                WHERE interaction.workspace_id = opportunity.workspace_id
                  AND interaction.opportunity_id = opportunity.id
                  AND interaction.direction = 'inbound'
                  AND interaction.phase = 'reply'
                ORDER BY interaction.occurred_at DESC, interaction.id DESC
                LIMIT 1
            ), 'none') AS last_reply_disposition,
            EXISTS (
                SELECT 1
                FROM autopilot_actions AS action
                WHERE action.workspace_id = $1
                  AND action.context = 'outreach'
                  AND action.subject_id = opportunity.id
                  AND action.status IN ('awaiting_approval','queued','processing')
            ) AS in_flight,
            -- Catalogue pitches and hand-thread follow-ups go out in waves or
            -- not at all; see `OutreachSnapshot::wave_only`.
            opportunity.source IN ('catalogue_autopilot', 'thread_followup') AS wave_only,
            opportunity.source = 'thread_followup' AS thread_followup
        FROM outreach_opportunities AS opportunity
        JOIN outreach_targets AS target
          ON target.workspace_id = opportunity.workspace_id
         AND target.id = opportunity.target_id
        WHERE opportunity.workspace_id = $1
          AND opportunity.active
          -- Representation targets are approached by the band's own request,
          -- never by an auto-pitched opportunity; a stray row for one must not
          -- reach `parse_outreach_kind` and poison the whole context.
          AND target.target_kind IN ('playlist','radio','press','creator','support_slot','endorsement','media_patronage')
          AND ($3::uuid IS NULL OR target.id = $3)
        ORDER BY opportunity.relevance_basis_points DESC, opportunity.id
        LIMIT $2
"#;

fn outreach_row_to_snapshot(row: OutreachRow) -> Result<OutreachSnapshot, RepositoryError> {
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
        lifetime_outbound: u16::try_from(row.lifetime_outbound)
            .map_err(|_| RepositoryError::Unexpected)?,
        target_ever_replied: row.target_ever_replied,
        last_reply: parse_outreach_reply(&row.last_reply_disposition)?,
        in_flight: row.in_flight,
        wave_only: row.wave_only,
        thread_followup: row.thread_followup,
    })
}

pub(in crate::autopilot) async fn load_outreach_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, OutreachRow>(OUTREACH_SNAPSHOT_SQL)
        .bind(workspace_id.into_uuid())
        .bind(MAX_SNAPSHOTS_PER_CONTEXT)
        .bind(Option::<Uuid>::None)
        .fetch_all(&repo.pool)
        .await
        .map_err(map_sqlx)?;

    rows.into_iter().map(outreach_row_to_snapshot).collect()
}

impl PostgresAutopilotRepository {
    /// One contact's live opportunities — the conversation drawer's "what
    /// happens next" runs the evaluator on exactly these, so the explanation
    /// it gives is the same one the cycle acts on.
    pub async fn load_target_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        target_id: OutreachTargetId,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, OutreachRow>(OUTREACH_SNAPSHOT_SQL)
                .bind(workspace_id.into_uuid())
                .bind(MAX_SNAPSHOTS_PER_CONTEXT)
                .bind(target_id.into_uuid())
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
            rows.into_iter().map(outreach_row_to_snapshot).collect()
        })
        .await
    }
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
                FROM beacons beacon
                WHERE beacon.workspace_id=event.workspace_id
                  AND beacon.city_id=event.city_id
                  AND beacon.active AND beacon.verified AND beacon.accepts_outreach
                  AND NOT beacon.do_not_contact
                  AND beacon.contact_email IS NOT NULL) AS known_local_beacons,
               (SELECT max(action.finished_at)
                FROM autopilot_actions action
                WHERE action.workspace_id=event.workspace_id
                  AND action.context='beacon'
                  AND action.subject_kind='event'
                  AND action.subject_id=event.id
                  AND action.action_kind='beacon.discovery.request'
                  AND action.status='succeeded') AS last_discovery_at,
               EXISTS (
                   SELECT 1 FROM autopilot_actions action
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
                FROM autopilot_actions AS action
                WHERE action.workspace_id = beacon.workspace_id
                  AND action.context = 'beacon'
                  AND action.subject_id = beacon.id
                  AND action.status IN ('awaiting_approval','queued','processing')
            ) AS in_flight
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id
         AND event.status IN ('published','completed')
         AND event.starts_at BETWEEN $2 - INTERVAL '5 days' AND $2 + INTERVAL '60 days'
         AND (
             beacon.city_id IS NULL
             OR beacon.city_id = event.city_id
         )
        LEFT JOIN beacon_campaigns AS campaign
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

/// The most a single synced post may fan out to. One caption carried into
/// fifty admitted communities was twenty-plus identical approvals on the
/// board — and, if approved, the same photo landing in every metal subreddit
/// in one afternoon, which is the shape moderators ban for. Three per post
/// is a mention, not a carpet-bombing; the rotation below spreads the picks
/// across the admitted set over successive posts instead of always naming
/// the same three.
const MAX_RELAY_COMMUNITIES_PER_POST: i64 = 3;

/// The communities a synced band post may be relayed into. The predicate is
/// the same one the community executor re-checks at post time — admitted by
/// screening and promoted — so a relay can only name a place the second wall
/// would still let through.
///
/// Ordering is a rotation, not a ranking of worth: communities the relay has
/// never drafted for come first, then the least recently drafted-for. A live
/// draft (pending, awaiting manual post) counts as a turn — stacking a second
/// approval on a community already queued for one is the flood this ordering
/// exists to prevent. `failed`/`cancelled` rows do not count: a draft that
/// never landed consumed nothing from the community.
pub(in crate::autopilot) async fn load_relay_community_targets(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<CommunityRelayTarget>, RepositoryError> {
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        r#"
        SELECT t.id, t.subreddit, t.language
        FROM agent_outreach_targets t
        LEFT JOIN discovery_places place ON place.id = t.place_id
        LEFT JOIN LATERAL (
            SELECT MAX(cp.created_at) AS last_draft_at
            FROM community_posts cp
            WHERE cp.workspace_id = t.workspace_id
              AND cp.target_id = t.id
              AND cp.status IN ('pending', 'awaiting_manual_post', 'posted')
        ) last ON true
        WHERE t.workspace_id = $1
          AND t.target_kind = 'community'
          AND t.screening_verdict = 'admitted'
          AND t.status = 'promoted'
          AND t.subreddit IS NOT NULL
          AND btrim(t.subreddit) <> ''
          -- The audience graph's own judgement overrides the target row —
          -- a community blocked in the console drops out of the relay pool
          -- on the next pass, the same predicate the growth-intelligence
          -- loader applies. A target with no place row predates the link
          -- and stays eligible: unknown is not refused.
          AND (place.id IS NULL
               OR (place.status = 'active'
                   AND place.membership_state NOT IN ('rejected', 'not_a_fit')))
        ORDER BY last.last_draft_at ASC NULLS FIRST, t.created_at, t.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_RELAY_COMMUNITIES_PER_POST)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|(id, subreddit, language)| {
            Ok(CommunityRelayTarget {
                target_id: OutreachTargetId::from_uuid(id),
                subreddit,
                language,
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
        FROM experiments AS experiment
        JOIN experiment_variants AS variant
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
                ('capture_plan'),
                ('qr_from_stage'),
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
                -- The announce beat is proven by first-party state: the
                -- campaign flag flipped, or a scan already landing — the
                -- strongest proof a QR got shown is somebody using it.
                WHEN 'qr_from_stage' THEN EXISTS (
                    SELECT 1 FROM concert_qr_campaigns AS campaign
                    WHERE campaign.workspace_id = event.workspace_id
                      AND campaign.event_id = event.id
                      AND campaign.active AND campaign.revoked_at IS NULL
                      AND (campaign.announced_from_stage
                           OR EXISTS (SELECT 1 FROM concert_checkins AS checkin
                                      WHERE checkin.workspace_id = campaign.workspace_id
                                        AND checkin.campaign_id = campaign.id))
                )
                ELSE false
            END AS verifiable_fact,
            -- Any terminal outcome counts, not only success: finished_at is
            -- stamped on succeeded, failed and cancelled alike, so a dead
            -- escalation still advances the idempotency epoch. Filtering to
            -- 'succeeded' left last_escalated_at NULL after a terminal
            -- failure, which regenerated the same action key forever and
            -- suppressed every retry of a task that still needed doing.
            (SELECT max(action.finished_at)
             FROM autopilot_actions AS action
             WHERE action.workspace_id = event.workspace_id
               AND action.context = 'show_operations'
               AND action.action_kind = 'show.task.escalate'
               AND action.subject_id = event.id
               AND action.finished_at IS NOT NULL
               AND action.payload->>'task' = task.item_key) AS last_escalated_at
        FROM events AS event
        CROSS JOIN task
        LEFT JOIN show_checklist_items AS checklist
          ON checklist.workspace_id = event.workspace_id
         AND checklist.event_id = event.id
         AND checklist.item_key = task.item_key
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          -- The trailing edge must outlast the T+7 report's due time plus
          -- evaluation-cycle slack, or a show ages out of the snapshot the
          -- morning its report comes due and the artifact never ships.
          AND event.starts_at BETWEEN $2::timestamptz - INTERVAL '9 days' AND $2::timestamptz + INTERVAL '14 days'
          -- The announce beat exists only where a live campaign does — a
          -- task telling the band to announce a QR that was never minted
          -- would be noise wearing a checklist's clothes.
          AND (task.item_key <> 'qr_from_stage'
               OR EXISTS (SELECT 1 FROM concert_qr_campaigns AS campaign
                          WHERE campaign.workspace_id = event.workspace_id
                            AND campaign.event_id = event.id
                            AND campaign.active AND campaign.revoked_at IS NULL))
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
        "video" => Ok(ContentSourceKind::Video),
        "story" => Ok(ContentSourceKind::Story),
        "social_post" => Ok(ContentSourceKind::SocialPost),
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
        "capture_plan" => Ok(ShowTaskKind::CapturePlan),
        "qr_from_stage" => Ok(ShowTaskKind::QrFromStage),
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
            CASE
                WHEN last_ask.asked_at IS NULL THEN NULL
                ELSE GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM ($2 - last_ask.asked_at)) / 3600)
                )::bigint
            END AS hours_since_last_invite_batch
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id
         AND event.status = 'published'
         AND event.starts_at BETWEEN $2 AND $2 + INTERVAL '60 days'
         AND (beacon.city_id IS NULL OR beacon.city_id = event.city_id)
        LEFT JOIN LATERAL (
            SELECT max(action.created_at) AS asked_at
            FROM autopilot_actions AS action
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
                    FROM booking_targets AS target
                    WHERE target.workspace_id = $1
                      AND target.active
                      AND target.accepts_booking
                      -- Supply means somebody we could write to this week —
                      -- a venue-kind target whose room is on record as
                      -- closed is not that, and counting it would suppress
                      -- the discovery ask the dry pipeline needs. The
                      -- resolved status decides; the linked set is the
                      -- primary link plus the venue edges, as in
                      -- `booking_reads`.
                      AND NOT (
                          target.target_kind = 'venue'
                          AND EXISTS (
                              SELECT 1
                              FROM (
                                  SELECT target.venue_id AS linked_venue_id
                                  UNION
                                  SELECT edge.venue_id
                                  FROM booking_target_venues AS edge
                                  WHERE edge.workspace_id = target.workspace_id
                                    AND edge.target_id = target.id
                              ) AS linked
                              WHERE COALESCE((
                                  SELECT lower(btrim(status_fact.value))
                                  FROM place_venue_facts AS status_fact
                                  WHERE status_fact.venue_id = linked.linked_venue_id
                                    AND status_fact.attribute = 'status'
                                    AND (status_fact.workspace_id IS NULL
                                         OR status_fact.workspace_id = $1)
                                    AND (status_fact.expires_at IS NULL
                                         OR status_fact.expires_at > now())
                                  ORDER BY CASE status_fact.provenance
                                               WHEN 'played' THEN 0
                                               WHEN 'researched' THEN 1
                                               WHEN 'event_evidence' THEN 2
                                               WHEN 'open_directory' THEN 3
                                               ELSE 4 END,
                                           status_fact.observed_at DESC
                                  LIMIT 1
                              ), '') = 'closed'
                          )
                      )
                ),
                (
                    SELECT CASE
                        WHEN max(d.evaluated_at) IS NULL THEN NULL
                        ELSE GREATEST(
                            0,
                            FLOOR(EXTRACT(EPOCH FROM ($2 - max(d.evaluated_at))) / 3600)
                        )::bigint
                    END
                    FROM autopilot_decisions AS d
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
