// The control-plane show list: next up first, then past, with the numbers
// the Shows page opens on — city, the registry room, tickets against
// capacity, interested fans and whether the door was ever measured — so the
// page needs no read per night.

#[derive(Debug, FromRow)]
struct ControlPlaneEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
    scan_count: i64,
    upcoming: bool,
    status: String,
    city: Option<String>,
    room: Option<String>,
    capacity: Option<i64>,
    tickets_sold: Option<i64>,
    tickets_7d: Option<i64>,
    interested: i64,
    door_campaigns: i64,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventView {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: String,
    ends_at: Option<String>,
    scan_count: u64,
    /// `true` while the show is ahead of or inside the staff surface's
    /// now-36h window — the "next up" block the gig page leads with.
    upcoming: bool,
    /// `draft`/`published`/`completed` — a hand-entered show can sit on the
    /// list unannounced, and a list that hid the difference would let an
    /// operator believe a draft is live.
    status: String,
    /// The catalogue city's name; null when the night names none.
    city: Option<String>,
    /// The registry room a mark ties the night to — the event row's own
    /// `venue` column has carried tour names and titles, so the list reads
    /// the room first and falls back to that text only when no mark exists.
    room: Option<String>,
    /// The sale's capacity, else the largest admission pool (the
    /// show-growth snapshot's rule). Null when neither exists.
    capacity: Option<i64>,
    /// Paid tickets — null when the night has no active ticket sale, which
    /// is an unmeasured night, not a night that sold none.
    tickets_sold: Option<i64>,
    tickets_7d: Option<i64>,
    /// Fans who asked to be told about this night.
    interested: i64,
    /// Door QR campaigns on record: 0 means `scan_count` was never measured.
    door_campaigns: i64,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventsResponse {
    events: Vec<ControlPlaneEventView>,
}

/// `GET /v1/control-plane/events` — the tenant's show list for the gig page:
/// next up first, then past, newest first, over a 90-day lookback — the T+7
/// lifecycle keeps a played show relevant for a week and ninety days covers
/// the season the band remembers without becoming an archive. Events only —
/// campaigns carry signing tokens and stay on the admin/staff surfaces.
pub async fn control_plane_events(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let rows = match crate::ops::hold(
        &state.read_budget,
        load_control_plane_events(&state.concert_qr),
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "control-plane events query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let events = rows
        .into_iter()
        .map(|row| ControlPlaneEventView {
            id: row.id,
            slug: row.slug,
            title: row.title,
            venue: row.venue,
            starts_at: format_time(row.starts_at),
            ends_at: row.ends_at.map(format_time),
            scan_count: u64::try_from(row.scan_count).unwrap_or_default(),
            upcoming: row.upcoming,
            status: row.status,
            city: row.city,
            room: row.room,
            capacity: row.capacity,
            tickets_sold: row.tickets_sold,
            tickets_7d: row.tickets_7d,
            interested: row.interested,
            door_campaigns: row.door_campaigns,
        })
        .collect();
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ControlPlaneEventsResponse { events }),
    )
        .into_response()
}

async fn load_control_plane_events(
    state: &ConcertQrState,
) -> Result<Vec<ControlPlaneEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ControlPlaneEventRow>(
        r#"
        WITH listed AS (
            SELECT event.id, event.workspace_id, event.slug, event.title, event.venue,
                   event.starts_at, event.ends_at, event.city_id,
                   (event.starts_at >= now() - interval '36 hours') AS upcoming,
                   event.status::text AS status
            FROM events AS event
            WHERE event.workspace_id = $1
              -- `draft` included since 0329: a show created by accepting a
              -- negotiation is booked and not yet announced, and announcing it
              -- is the ladder's first step. A list that hid drafts hid exactly
              -- the shows with work outstanding.
              AND event.status IN ('draft','published','completed')
              AND event.starts_at >= now() - interval '90 days'
        ), sales AS (
            -- Paid tickets by the show-growth snapshot's predicates: an
            -- active sale's paid and partially-refunded orders.
            SELECT sale.event_id,
                   MAX(sale.capacity)::bigint AS capacity,
                   COALESCE(SUM(item.quantity) FILTER (
                       WHERE orders.status IN ('paid','partially_refunded')), 0)::bigint AS sold,
                   COALESCE(SUM(item.quantity) FILTER (
                       WHERE orders.status IN ('paid','partially_refunded')
                         AND orders.paid_at >= now() - interval '7 days'), 0)::bigint AS sold_7d
            FROM ticket_sales AS sale
            LEFT JOIN ticket_orders AS orders
              ON orders.workspace_id = sale.workspace_id
             AND orders.ticket_sale_id = sale.id
            LEFT JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE sale.workspace_id = $1 AND sale.active
              AND sale.event_id IN (SELECT id FROM listed)
            GROUP BY sale.event_id
        )
        SELECT listed.id, listed.slug, listed.title, listed.venue, listed.starts_at,
               listed.ends_at,
               (SELECT count(*) FROM concert_checkins AS checkin
                WHERE checkin.workspace_id = listed.workspace_id
                  AND checkin.event_id = listed.id)::bigint AS scan_count,
               listed.upcoming,
               listed.status,
               city.name AS city,
               (SELECT venue.display_name
                FROM place_venue_marks AS mark
                JOIN place_venues AS venue ON venue.id = mark.venue_id
                WHERE mark.workspace_id = listed.workspace_id
                  AND mark.event_id = listed.id
                ORDER BY venue.display_name
                LIMIT 1) AS room,
               COALESCE(sales.capacity,
                   (SELECT MAX(pool.capacity)::bigint FROM admission_pools AS pool
                    WHERE pool.workspace_id = listed.workspace_id
                      AND pool.event_id = listed.id)) AS capacity,
               sales.sold AS tickets_sold,
               sales.sold_7d AS tickets_7d,
               (SELECT count(*) FROM event_interests AS interest
                WHERE interest.workspace_id = listed.workspace_id
                  AND interest.event_id = listed.id)::bigint AS interested,
               (SELECT count(*) FROM concert_qr_campaigns AS campaign
                WHERE campaign.workspace_id = listed.workspace_id
                  AND campaign.event_id = listed.id)::bigint AS door_campaigns
        FROM listed
        LEFT JOIN cities AS city ON city.id = listed.city_id
        LEFT JOIN sales ON sales.event_id = listed.id
        ORDER BY listed.upcoming DESC,
                 CASE WHEN listed.upcoming THEN listed.starts_at END,
                 CASE WHEN NOT listed.upcoming THEN listed.starts_at END DESC
        LIMIT $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(MAX_STAFF_EVENTS_LIMIT)
    .fetch_all(&state.database)
    .await
}

