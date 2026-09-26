// The gig page's one night, as data: every workspace-scoped artifact row the
// T-21→T+7 ladder reads. `timeline.rs` turns these facts into the nine
// steps; this file owns the row shapes and the loader.
//
// Every query here reads an existing workspace-scoped artifact table;
// nothing in this file writes, and nothing fetched carries a campaign
// credential or a per-fan row. Missing evidence is null, not zero — a
// measurement that does not exist yet loads as no row, not a fabricated
// count.

#[derive(Debug, FromRow)]
struct TimelineEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    venue_address: Option<String>,
    status: String,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
    counterparty_name: Option<String>,
    counterparty_email: Option<String>,
    /// The shared night this event landed on, when the venue registry
    /// resolved one — the rendezvous block reads from here (4V.6b).
    place_event_id: Option<Uuid>,
    /// The negotiation that produced this night, when one did. `None` for a
    /// show nobody had to win — a hometown gig is still a show.
    booking_opportunity_id: Option<Uuid>,
    /// The catalogue city's name, when the night names one.
    city: Option<String>,
    /// Fans who asked to be told about this night.
    interested: i64,
}

/// What it took to win the night: the negotiation behind this show.
///
/// Loaded only when the event carries a `booking_opportunity_id`. A show
/// nobody had to win has no row here and the step reports that rather than an
/// empty negotiation.
#[derive(Debug, FromRow)]
struct TimelineBookingRow {
    organization: String,
    state: String,
    offered_fee_minor: Option<i64>,
    settled_at: Option<OffsetDateTime>,
    counter_rounds: Option<i32>,
    currency: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineActRow {
    act_slug: String,
    act_name: String,
    /// Where on the bill. The order is the night's own — headliner last is a
    /// choice somebody made, and re-sorting it alphabetically on the page
    /// would quietly rewrite it.
    position: i32,
    /// This act's own ticket link, when the bill carries one. A support act
    /// selling through its own page is the normal case at this size, and a
    /// page that lists the act without the link sends the reader looking.
    ticket_url: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineCrossbillEdgeRow {
    max_campaigns_per_month: i16,
    cooldown_days: i16,
    deliveries_this_month: i64,
    /// Whether a reverse-direction consent has ever carried the edge
    /// owner's announcement to this workspace's crowd — the delivery
    /// ledger is the proof, and revocation does not erase it.
    reciprocated: bool,
}

#[derive(Debug, FromRow)]
struct TimelineBeaconRow {
    display_name: String,
    beacon_kind: String,
    status: String,
    last_reply_disposition: String,
    last_outreach_at: Option<OffsetDateTime>,
    notes: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineRecapCampaignRow {
    slug: String,
    subject: Option<String>,
    status: String,
    scheduled_at: Option<OffsetDateTime>,
    delivered_count: Option<i32>,
    recipient_count: Option<i32>,
}

#[derive(Debug, FromRow)]
struct LifecycleEmissionRow {
    phase: String,
    emitted_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct TimelineSurfaceRow {
    surface_key: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct ShowGrowthActionRow {
    id: Uuid,
    status: String,
    lever: Option<String>,
    available_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct TimelineAssignmentRow {
    source_kind: String,
    source_ref: Option<String>,
    action_id: Option<Uuid>,
    status: String,
    due_at: Option<OffsetDateTime>,
    // display_name is nullable on workspace_members — a NULL must not decode
    // into a 503 for the whole page. The owner slot stays empty for an
    // unnamed assignee rather than leaking the member's sign-in address.
    display_name: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelinePlayStepRow {
    step_kind: String,
    due_at: OffsetDateTime,
    settled_at: Option<OffsetDateTime>,
    skip_reason: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineChecklistRow {
    item_key: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct TimelineCountsRow {
    nearby_notified: i64,
    qr_campaigns: i64,
    checkins: i64,
}

#[derive(Debug, FromRow)]
struct TimelinePaceRow {
    capacity: Option<i64>,
    paid_tickets: i64,
    paid_tickets_last_7d: i64,
}

#[derive(Debug, FromRow)]
struct TimelineDecisionRow {
    reason: String,
    evaluated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct TimelineHarvestRow {
    occurred_at: Option<OffsetDateTime>,
    pending_requests: i64,
    succeeded_requests: i64,
}

#[derive(Debug, FromRow)]
struct TimelineCostRow {
    predicted_total_cost_minor: Option<i64>,
    settled_total_cost_minor: Option<i64>,
    fee_received_minor: Option<i64>,
    accuracy: Option<String>,
    prediction_missing_input: Option<String>,
}

struct TimelineFacts {
    event: TimelineEventRow,
    emissions: Vec<LifecycleEmissionRow>,
    surfaces: Vec<TimelineSurfaceRow>,
    pace: TimelinePaceRow,
    latest_decision: Option<TimelineDecisionRow>,
    growth_actions: Vec<ShowGrowthActionRow>,
    assignments: Vec<TimelineAssignmentRow>,
    play_steps: Vec<TimelinePlayStepRow>,
    checklist: Vec<TimelineChecklistRow>,
    counts: TimelineCountsRow,
    /// What the scan produced — counts only, assembled in infra so this
    /// module never touches fan identity.
    room: crowdrelay_infra::concert_room::RoomSplit,
    harvest: TimelineHarvestRow,
    cost: Option<TimelineCostRow>,
    booking: Option<TimelineBookingRow>,
    crossbill_acts: Vec<TimelineActRow>,
    crossbill_edge: Option<TimelineCrossbillEdgeRow>,
    venue_beacons: Vec<TimelineBeaconRow>,
    recap_campaign: Option<TimelineRecapCampaignRow>,
}

async fn load_timeline_facts(
    state: &ConcertQrState,
    event_slug: &str,
) -> Result<Option<TimelineFacts>, sqlx::Error> {
    let Some(event) = sqlx::query_as::<_, TimelineEventRow>(
        r#"
        SELECT event.id, event.slug, event.title, event.venue, event.venue_address,
               event.status, event.starts_at, event.ends_at,
               event.counterparty_name, event.counterparty_email, event.place_event_id,
               event.booking_opportunity_id,
               city.name AS city,
               (SELECT count(*) FROM event_interests AS interest
                WHERE interest.workspace_id = event.workspace_id
                  AND interest.event_id = event.id)::bigint AS interested
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.slug = $2
          -- `draft` included since 0329. The ladder's first rung is
          -- "Announced", so a show that is booked and unannounced is the one
          -- case the timeline most needs to render, and it was the one case
          -- that resolved 404.
          AND event.status IN ('draft','published','completed')
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_optional(&state.database)
    .await?
    else {
        return Ok(None);
    };
    let workspace_id = state.workspace_id.into_uuid();
    let event_id = event.id;

    let emissions = sqlx::query_as::<_, LifecycleEmissionRow>(
        r#"
        SELECT phase, emitted_at
        FROM campaign_lifecycle_emissions
        WHERE workspace_id = $1 AND event_id = $2
          AND phase = 'announcement'
        ORDER BY emitted_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let surfaces = sqlx::query_as::<_, TimelineSurfaceRow>(
        r#"
        SELECT surface_key, status
        FROM show_growth_surfaces
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY surface_key
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // The same predicates the show-growth snapshot uses: an active sale's
    // paid and partially-refunded orders, capacity taken from the sale first
    // and the largest admission pool as the fallback — the brain's own
    // COALESCE(ticket_sale.capacity, admission.capacity).
    // The pace verdict itself stays in the brain's decisions — this surface
    // reports the numbers, not a re-derived opinion.
    let pace = sqlx::query_as::<_, TimelinePaceRow>(
        r#"
        SELECT COALESCE(ticket_sale.capacity, admission.capacity) AS capacity,
               COALESCE(ticket.paid_tickets, 0)::bigint AS paid_tickets,
               COALESCE(ticket.paid_tickets_last_7d, 0)::bigint AS paid_tickets_last_7d
        FROM events AS event
        LEFT JOIN LATERAL (
            SELECT MAX(sale.capacity)::bigint AS capacity
            FROM ticket_sales AS sale
            WHERE sale.workspace_id = event.workspace_id
              AND sale.event_id = event.id
              AND sale.active
        ) AS ticket_sale ON true
        LEFT JOIN LATERAL (
            SELECT SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                )::bigint AS paid_tickets,
                SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                      AND orders.paid_at >= now() - INTERVAL '7 days'
                )::bigint AS paid_tickets_last_7d
            FROM ticket_sales AS sale
            JOIN ticket_orders AS orders
              ON orders.workspace_id = sale.workspace_id
             AND orders.ticket_sale_id = sale.id
            JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE sale.workspace_id = event.workspace_id
              AND sale.event_id = event.id
              AND sale.active
        ) AS ticket ON true
        LEFT JOIN LATERAL (
            SELECT MAX(pool.capacity)::bigint AS capacity
            FROM admission_pools AS pool
            WHERE pool.workspace_id = event.workspace_id
              AND pool.event_id = event.id
        ) AS admission ON true
        WHERE event.workspace_id = $1 AND event.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    let latest_decision = sqlx::query_as::<_, TimelineDecisionRow>(
        r#"
        SELECT reason, evaluated_at
        FROM autopilot_decisions
        WHERE workspace_id = $1 AND context = 'show_growth' AND subject_id = $2
        ORDER BY evaluated_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_optional(&state.database)
    .await?;

    let growth_actions = sqlx::query_as::<_, ShowGrowthActionRow>(
        r#"
        SELECT id, status, payload ->> 'lever' AS lever,
               available_at, finished_at
        FROM autopilot_actions
        WHERE workspace_id = $1 AND context = 'show_growth' AND subject_id = $2
        ORDER BY created_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Both assignment shapes key the event through `source_id`: a show_task
    // stores the event id there directly (source_ref names the checklist
    // item), an autopilot_action stores the action's subject — the event —
    // the same way. `source_ref` distinguishes the task inside the event.
    // Capture-plan assignments key the *plan* instead, so the second arm
    // walks plan → production day → gig; the synthesized source_ref names
    // the step the assignment owns.
    let assignments = sqlx::query_as::<_, TimelineAssignmentRow>(
        r#"
        SELECT assignment.source_kind, assignment.source_ref,
               assignment.action_id,
               assignment.status, assignment.due_at,
               member.display_name
        FROM team_assignments AS assignment
        JOIN workspace_members AS member
          ON member.workspace_id = assignment.workspace_id
         AND member.id = assignment.assignee_member_id
        WHERE assignment.workspace_id = $1 AND assignment.source_id = $2
        UNION ALL
        SELECT assignment.source_kind, 'capture_plan'::text AS source_ref,
               assignment.action_id,
               assignment.status, assignment.due_at,
               member.display_name
        FROM team_assignments AS assignment
        JOIN capture_plans AS plan
          ON plan.workspace_id = assignment.workspace_id
         AND plan.id = assignment.source_id
        JOIN production_events AS day
          ON day.workspace_id = plan.workspace_id
         AND day.id = plan.production_event_id
        JOIN workspace_members AS member
          ON member.workspace_id = assignment.workspace_id
         AND member.id = assignment.assignee_member_id
        WHERE assignment.workspace_id = $1
          AND assignment.source_kind = 'capture_plan'
          AND day.event_id = $2
        ORDER BY due_at NULLS LAST
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let play_steps = sqlx::query_as::<_, TimelinePlayStepRow>(
        r#"
        SELECT step.step_kind, step.due_at, step.settled_at, step.skip_reason
        FROM play_steps AS step
        JOIN plays AS play
          ON play.workspace_id = step.workspace_id
         AND play.id = step.play_id
        WHERE step.workspace_id = $1
          AND play.anchor_kind = 'event'
          AND play.anchor_id = $2
        ORDER BY step.due_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let checklist = sqlx::query_as::<_, TimelineChecklistRow>(
        r#"
        SELECT item_key, status
        FROM show_checklist_items
        WHERE workspace_id = $1 AND event_id = $2
          AND item_key IN ('capture_plan','post_show_report')
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Three counts in one round trip: the dedupe ledger proves a fan was
    // told, the campaign row proves the scan had a door, and the check-in
    // count is the scan itself. Delivered-vs-queued push detail stays on
    // the ops surface — the ladder needs the fact, not the funnel.
    let counts = sqlx::query_as::<_, TimelineCountsRow>(
        r#"
        SELECT
            (SELECT count(*)::bigint FROM nearby_gig_notifications
             WHERE workspace_id = $1 AND event_id = $2) AS nearby_notified,
            (SELECT count(*)::bigint FROM concert_qr_campaigns
             WHERE workspace_id = $1 AND event_id = $2
               AND active AND revoked_at IS NULL
               AND valid_until > now()) AS qr_campaigns,
            (SELECT count(*)::bigint FROM concert_checkins
             WHERE workspace_id = $1 AND event_id = $2) AS checkins
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;
    let room =
        crowdrelay_infra::concert_room::room_split(&state.database, workspace_id, event_id)
            .await?;

    // Harvest anchors on the content source the completed-show trigger
    // projects; artifact requests are content-supply actions whose subject is
    // that source. The pending predicate is the supply snapshot's own: an
    // action is in flight while it is open, or while it succeeded at the
    // request layer without a terminal execution report yet.
    let harvest = sqlx::query_as::<_, TimelineHarvestRow>(
        r#"
        SELECT
            (SELECT MAX(source.occurred_at)
             FROM content_sources AS source
             WHERE source.workspace_id = $1
               AND source.source_kind = 'show_completed'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
            ) AS occurred_at,
            (SELECT count(*)::bigint
             FROM autopilot_actions AS action
             JOIN content_sources AS source
               ON source.workspace_id = action.workspace_id
              AND source.id = action.subject_id
             WHERE action.workspace_id = $1
               AND action.context = 'content_supply'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
               AND (
                   action.status IN ('awaiting_approval','queued','processing')
                   OR (
                       action.status = 'succeeded'
                       AND EXISTS (
                           SELECT 1
                           FROM autopilot_action_emissions AS emission
                           WHERE emission.workspace_id = action.workspace_id
                             AND emission.action_id = action.id
                       )
                       AND NOT EXISTS (
                           SELECT 1
                           FROM autopilot_execution_reports AS report
                           WHERE report.workspace_id = action.workspace_id
                             AND report.action_id = action.id
                             AND report.status IN ('succeeded','failed')
                       )
                   )
               )
            ) AS pending_requests,
            (SELECT count(*)::bigint
             FROM autopilot_actions AS action
             JOIN content_sources AS source
               ON source.workspace_id = action.workspace_id
              AND source.id = action.subject_id
             WHERE action.workspace_id = $1
               AND action.context = 'content_supply'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
               AND action.status = 'succeeded'
               AND EXISTS (
                   SELECT 1
                   FROM autopilot_execution_reports AS report
                   WHERE report.workspace_id = action.workspace_id
                     AND report.action_id = action.id
                     AND report.status = 'succeeded'
               )
            ) AS succeeded_requests
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    let cost = sqlx::query_as::<_, TimelineCostRow>(
        r#"
        SELECT predicted_total_cost_minor, settled_total_cost_minor,
               fee_received_minor, accuracy, prediction_missing_input
        FROM show_cost_ledger
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY predicted_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_optional(&state.database)
    .await?;

    // What the band went through to get the date. `None` when the show was
    // not won through a negotiation, which is an ordinary answer and not a
    // gap: the step says "no negotiation behind this night" rather than
    // rendering an empty one.
    let booking = match event.booking_opportunity_id {
        Some(opportunity_id) => {
            sqlx::query_as::<_, TimelineBookingRow>(
                r#"
                SELECT opportunity.organization,
                       COALESCE(terms.state, opportunity.status) AS state,
                       terms.offered_fee_minor,
                       terms.settled_at,
                       terms.counter_rounds,
                       terms.currency
                FROM team_opportunities AS opportunity
                LEFT JOIN team_opportunity_terms AS terms
                  ON terms.workspace_id = opportunity.workspace_id
                 AND terms.opportunity_id = opportunity.id
                WHERE opportunity.workspace_id = $1 AND opportunity.id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(opportunity_id)
            .fetch_optional(&state.database)
            .await?
        }
        None => None,
    };

    // The shared bill is the T-21 artifact: who else plays, and whether an
    // active crossbill edge lets the system push the night to a bill-mate's
    // audience. The cap travels with it — the consent's monthly ceiling and
    // cooldown are the honest bound, not a made-up per-show limit.
    let crossbill_acts = sqlx::query_as::<_, TimelineActRow>(
        r#"
        SELECT act_slug, act_name, position, ticket_url
        FROM event_acts
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY position, act_slug
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Only an edge whose beneficiary is this workspace amplifies its shows —
    // `from` owns the audience, `to` benefits (same direction rule as
    // `event_crossbill` in fan_context). The cap counts CAMPAIGNS, not fan
    // rows — `count(DISTINCT campaign_reference)` matches the enforcement
    // count in infra/portfolio.rs exactly, same calendar-month window.
    let crossbill_edge = if facts_crossbill_needed(&crossbill_acts) {
        sqlx::query_as::<_, TimelineCrossbillEdgeRow>(
            r#"
            SELECT edge.max_campaigns_per_month, edge.cooldown_days,
                (SELECT count(DISTINCT d.campaign_reference)
                 FROM amplification_deliveries AS d
                 WHERE d.consent_id = edge.id AND d.to_workspace_id = $1
                   AND d.delivered_at >= date_trunc('month', now())) AS deliveries_this_month,
                EXISTS (
                    SELECT 1
                    FROM amplification_consents AS reverse_edge
                    JOIN amplification_deliveries AS reverse_ledger
                      ON reverse_ledger.consent_id = reverse_edge.id
                    WHERE reverse_edge.from_workspace_id = $1
                      AND reverse_edge.to_workspace_id = edge.from_workspace_id
                ) AS reciprocated
            FROM amplification_consents AS edge
            WHERE edge.to_workspace_id = $1
              AND edge.purpose = 'event_crossbill'
              AND edge.status = 'active'
            -- With several inbound edges, describe the one that can carry:
            -- a reciprocated edge sorts first so this state agrees with the
            -- staff dashboard's any-reciprocated EXISTS.
            ORDER BY reciprocated DESC, edge.created_at
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&state.database)
        .await?
    } else {
        None
    };

    // Venue knowledge lives on the show: the beacon-campaign record keyed
    // by event_id — who the room is to us (relationship status, the last
    // reply's disposition, the operator's notes). No fuzzy venue matching.
    let venue_beacons = sqlx::query_as::<_, TimelineBeaconRow>(
        r#"
        SELECT beacon.display_name, beacon.beacon_kind,
               campaign.status, campaign.last_reply_disposition,
               campaign.last_outreach_at, campaign.notes
        FROM beacon_campaigns AS campaign
        JOIN beacons AS beacon
          ON beacon.workspace_id = campaign.workspace_id
         AND beacon.id = campaign.beacon_id
        WHERE campaign.workspace_id = $1 AND campaign.event_id = $2
        ORDER BY campaign.last_outreach_at DESC NULLS LAST, beacon.display_name
        LIMIT 8
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // The T+1 recall's artifact is the recap campaign itself — tagged by the
    // same `content->>'event_id'` link the report's campaign read uses, with
    // the lever narrowing it to the recap rather than any event send.
    let recap_campaign = sqlx::query_as::<_, TimelineRecapCampaignRow>(
        r#"
        -- The recap INSERT never sets `subject`; `name` is the populated
        -- human label, so the detail falls back to it rather than always
        -- reading null.
        SELECT slug, COALESCE(subject, name) AS subject, status,
               scheduled_at, delivered_count, recipient_count
        FROM communication_campaigns
        WHERE workspace_id = $1 AND content->>'event_id' = $2
          AND content->>'lever' = 'post_show_recap'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id.to_string())
    .fetch_optional(&state.database)
    .await?;

    Ok(Some(TimelineFacts {
        event,
        emissions,
        surfaces,
        pace,
        latest_decision,
        growth_actions,
        assignments,
        play_steps,
        checklist,
        counts,
        room,
        harvest,
        cost,
        booking,
        crossbill_acts,
        crossbill_edge,
        venue_beacons,
        recap_campaign,
    }))
}

/// The edge lookup only matters when a bill exists to share — a solo show
/// never has a crossbill, so the consent edge is not worth a query.
fn facts_crossbill_needed(acts: &[TimelineActRow]) -> bool {
    acts.len() > 1
}

