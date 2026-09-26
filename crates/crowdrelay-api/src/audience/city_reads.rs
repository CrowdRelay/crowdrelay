// City-scoped reads shared by the Places tab and the city view: the funnel
// rows and the registry rows, each optionally narrowed to one city. Kept in
// one file so the handlers in `audience.rs` and `console_views.rs` call the
// same query rather than two copies of it.

/// The funnel rows, optionally narrowed to one city by catalogue slug — the
/// city page asks for its own row through the same query the Places table
/// reads, so the two can never disagree about a city's fans or reach.
pub(crate) async fn city_funnel_rows(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    only_city: Option<&str>,
) -> Result<Vec<CityFunnelRow>, sqlx::Error> {
    let result = sqlx::query_as::<_, CityFunnelRow>(
        r#"
        WITH city_fans AS (
            SELECT
                city.id AS city_id,
                city.slug AS city_slug,
                city.name AS city_name,
                city.country_code,
                city.region,
                fan.normalized_email,
                interest.created_at AS interest_created_at,
                -- fan_consents is append-only: consent means the LATEST
                -- row for the purpose is granted, not that any row ever
                -- was — a check-in withdrawal must drop the fan out of
                -- every count on this row.
                EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                      AND consent.granted
                      AND consent.id = (
                          SELECT newest.id
                          FROM fan_consents AS newest
                          WHERE newest.workspace_id = consent.workspace_id
                            AND newest.fan_id = consent.fan_id
                            AND newest.purpose = consent.purpose
                          ORDER BY newest.recorded_at DESC, newest.id DESC
                          LIMIT 1
                      )
                ) AS consented,
                fan_last_meaningful_action(
                    fan.workspace_id, fan.id, fan.normalized_email
                ) AS last_meaningful_action_at
            FROM fan_city_interests AS interest
            JOIN cities AS city
              ON city.id = interest.city_id
            JOIN fans AS fan
              ON fan.workspace_id = interest.workspace_id
             AND fan.id = interest.fan_id
            WHERE interest.workspace_id = $1
              -- 'merged' rows are dedup tombstones whose identity moved
              -- to the surviving fan; everything else, pending included,
              -- is a real member of the fanbase a city count should name.
              AND fan.status <> 'merged'
              AND ($3::text IS NULL OR city.slug = $3)
        ),
        agg AS (
            SELECT
                city_id,
                city_slug,
                city_name,
                country_code,
                region,
                count(*)::bigint AS fans,
                count(*) FILTER (
                    WHERE interest_created_at BETWEEN $2 - INTERVAL '30 days' AND $2
                )::bigint AS new_30d,
                count(*) FILTER (
                    WHERE last_meaningful_action_at IS NOT NULL
                      AND last_meaningful_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
                      AND consented
                )::bigint AS active_30d,
                count(*) FILTER (WHERE consented)::bigint AS consented
            FROM city_fans
            GROUP BY city_id, city_slug, city_name, country_code, region
        ),
        reachable AS (
            -- The nearby-gig gate applied city-by-city over the funnel's
            -- own candidate set: fans whose location preference is enabled,
            -- account active, marketing consent granted, and whose city sits
            -- inside their radius of this one. A fan following Kraków from
            -- Katowice still counts for Kraków — the emitter would page them.
            SELECT
                agg.city_id,
                count(*)::bigint AS reachable
            FROM agg
            JOIN cities AS city
              ON city.id = agg.city_id
             AND city.latitude IS NOT NULL
             AND city.longitude IS NOT NULL
            JOIN fan_location_preferences AS preferences
              ON preferences.workspace_id = $1
             AND preferences.nearby_gigs_enabled
            JOIN cities AS fan_city
              ON fan_city.id = preferences.city_id
             AND fan_city.latitude IS NOT NULL
             AND fan_city.longitude IS NOT NULL
            JOIN fans AS fan
              ON fan.workspace_id = preferences.workspace_id
             AND fan.id = preferences.fan_id
             AND fan.status = 'active'
            WHERE EXISTS (
                SELECT 1
                FROM fan_consents AS consent
                WHERE consent.workspace_id = fan.workspace_id
                  AND consent.fan_id = fan.id
                  AND consent.purpose = 'marketing'
                  AND consent.granted
                  AND consent.id = (
                      SELECT newest.id
                      FROM fan_consents AS newest
                      WHERE newest.workspace_id = consent.workspace_id
                        AND newest.fan_id = consent.fan_id
                        AND newest.purpose = consent.purpose
                      ORDER BY newest.recorded_at DESC, newest.id DESC
                      LIMIT 1
                  )
            )
              -- The emitter's own bound: one degree of latitude is
              -- 111.19 km wherever you stand, so a pair further apart
              -- than the radius in latitude alone can never be inside it.
              AND abs(fan_city.latitude - city.latitude)
                  <= (preferences.radius_km + 1)::double precision / 111.0
              -- Rounded, matching the emitter's distance_km comparison: a
              -- fan at 50.4 km with radius 50 is paged, not dropped.
              AND ROUND(6371 * 2 * ASIN(LEAST(1.0, SQRT(
                    POWER(SIN(RADIANS(fan_city.latitude - city.latitude) / 2), 2)
                    + COS(RADIANS(city.latitude)) * COS(RADIANS(fan_city.latitude))
                    * POWER(SIN(RADIANS(fan_city.longitude - city.longitude) / 2), 2)
                  ))))::integer <= preferences.radius_km
            GROUP BY agg.city_id
        ),
        supply AS (
            -- Confirmed bookable inventory per city: booking targets with a
            -- real route that are still on the board. Candidates awaiting
            -- screening do not count — "what's there" means what we could
            -- actually write to this week.
            SELECT
                agg.city_id,
                count(*) FILTER (WHERE target.target_kind = 'venue')::bigint AS venues,
                count(*) FILTER (WHERE target.target_kind = 'promoter')::bigint AS promoters,
                count(*) FILTER (WHERE target.target_kind = 'festival')::bigint AS festivals
            FROM agg
            JOIN booking_targets AS target
              ON target.workspace_id = $1
             AND target.city_id = agg.city_id
             AND target.active
             AND target.accepts_booking
             -- "What we could write to this week" excludes a venue-kind
             -- target whose room is on record as closed — the resolved
             -- status decides, so a newer 'active' claim lifts it.
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
            GROUP BY agg.city_id
        ),
        shows AS (
            -- The gap edge: when we last played each city and when we next
            -- will. NULL last = never on record; NULL next = nothing
            -- coming — fans + no next show is the organise-now signal.
            SELECT
                agg.city_id,
                max(events.starts_at) FILTER (WHERE events.starts_at <= $2) AS last_show_at,
                min(events.starts_at) FILTER (WHERE events.starts_at > $2) AS next_show_at
            FROM agg
            JOIN events
              ON events.workspace_id = $1
             AND events.city_id = agg.city_id
             AND events.status IN ('published', 'completed')
            GROUP BY agg.city_id
        )
        SELECT
            agg.city_slug,
            agg.city_name,
            agg.country_code,
            agg.region,
            agg.fans,
            agg.new_30d,
            agg.active_30d,
            agg.consented,
            (agg.active_30d >= 50)::bool AS bookable,
            COALESCE(reach.reachable, 0)::bigint AS reachable,
            COALESCE(supply.venues, 0)::bigint AS venues,
            COALESCE(supply.promoters, 0)::bigint AS promoters,
            COALESCE(supply.festivals, 0)::bigint AS festivals,
            shows.last_show_at,
            shows.next_show_at
        FROM agg
        LEFT JOIN reachable AS reach
          ON reach.city_id = agg.city_id
        LEFT JOIN supply
          ON supply.city_id = agg.city_id
        LEFT JOIN shows
          ON shows.city_id = agg.city_id
        ORDER BY agg.active_30d DESC, agg.fans DESC, agg.city_slug, agg.city_id
        LIMIT 100
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(only_city)
    .fetch_all(pool)
    .await;
    result.map(|mut rows| {
        let today = now.date();
        for row in &mut rows {
            row.months_since_show = row.last_show_at.map(|played| {
                let months = (today.year() - played.date().year()) * 12
                    + i32::from(u8::from(today.month()))
                    - i32::from(u8::from(played.date().month()));
                i64::from(months.max(0))
            });
            row.organise_score_bp = i64::from(crowdrelay_domain::place::organise_score(
                &crowdrelay_domain::place::OrganiseCityInputs {
                    reachable: row.reachable.max(0) as u32,
                    fans: row.fans.max(0) as u32,
                    new_30d: row.new_30d.max(0) as u32,
                    venues: row.venues.max(0) as u32,
                    promoters: row.promoters.max(0) as u32,
                    festivals: row.festivals.max(0) as u32,
                    months_since_show: row.months_since_show.map(|m| m.max(0) as u32),
                    next_show_booked: row.next_show_at.is_some(),
                },
            ));
        }
        rows
    })
}

/// The registry rows, optionally narrowed to one city by catalogue slug —
/// the city page's rooms are the Places tab's rooms, filtered, never a
/// second read that could rank or assess them differently.
pub(crate) async fn city_venue_rows(
    state: &crate::AppState,
    only_city: Option<&str>,
) -> Result<Vec<CityVenueRow>, sqlx::Error> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    // The requesting tenant's own genre set, one scalar up front — the
    // "mine" half of every comparability test below. A failed read fails the
    // request (see `city_venues`): it must never degrade to an empty set.
    let my_genres = sqlx::query_scalar::<_, Vec<String>>(
        r#"
        SELECT COALESCE(array_agg(DISTINCT lower(btrim(g))), '{}')
        FROM band_listings AS bl, unnest(bl.genre_tags) AS g
        WHERE bl.workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(&state.database)
    .await?;
    let mut result = sqlx::query_as::<_, CityVenueRow>(
        r#"
        WITH marks AS (
            SELECT mark.venue_id, mark.event_id, mark.workspace_id,
                   event.starts_at, event.status
            FROM place_venue_marks AS mark
            JOIN events AS event
              ON event.id = mark.event_id
        ), per_show_draw AS (
            SELECT marks.venue_id, marks.event_id,
                   count(ticket_order.id)::double precision AS paid_orders
            FROM marks
            JOIN ticket_sales AS sale
              ON sale.workspace_id = marks.workspace_id
             AND sale.event_id = marks.event_id
            LEFT JOIN ticket_orders AS ticket_order
              ON ticket_order.workspace_id = sale.workspace_id
             AND ticket_order.ticket_sale_id = sale.id
             AND ticket_order.status IN ('paid', 'partially_refunded')
            GROUP BY marks.venue_id, marks.event_id
        ), repeaters AS (
            -- A repeat attender is a person who PAID for shows at the room
            -- at least twice — declared interest is not attendance, and the
            -- buyer's email is the only identity that survives across
            -- tenants. Two bands sharing a room's regulars is exactly the
            -- cross-tenant knowledge this registry exists to surface.
            SELECT marked.venue_id, count(*)::bigint AS repeat_attenders
            FROM (
                SELECT mark.venue_id, lower(btrim(ticket_order.buyer_email)) AS buyer
                FROM ticket_orders AS ticket_order
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = ticket_order.workspace_id
                 AND sale.id = ticket_order.ticket_sale_id
                JOIN place_venue_marks AS mark
                  ON mark.event_id = sale.event_id
                 AND mark.workspace_id = sale.workspace_id
                WHERE ticket_order.status IN ('paid', 'partially_refunded')
                GROUP BY mark.venue_id, lower(btrim(ticket_order.buyer_email))
                HAVING count(DISTINCT sale.event_id) >= 2
            ) AS marked
            GROUP BY marked.venue_id
        ), resolved AS (
            -- The first non-expired fact per attribute in provenance trust
            -- order: played beats researched beats evidence beats a
            -- directory. workspace_id IS NULL is the load-bearing filter —
            -- this read is global, and a contributor's private fact (a
            -- booking address, a fit judgement) must never surface here.
            -- `expires_at` is a deletion deadline, not a staleness hint —
            -- the hourly `venue_fact_expiry` sweep deletes the row outright;
            -- this filter covers only the lag between the deadline passing
            -- and the next sweep.
            SELECT DISTINCT ON (f.venue_id, f.attribute)
                   f.venue_id, f.attribute, f.value, f.provenance, f.observed_at
            FROM place_venue_facts AS f
            WHERE (f.expires_at IS NULL OR f.expires_at > now())
              AND f.workspace_id IS NULL
            ORDER BY f.venue_id, f.attribute,
                     CASE f.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2 WHEN 'open_directory' THEN 3
                         ELSE 4 END,
                     f.observed_at DESC
        ), act_genres AS (
            -- An act's genre set: tenant acts carry theirs on the band
            -- listing, peer acts on the attributed place_peer_act_genres
            -- rows. Tags stay raw here — normalization happens once, at
            -- comparison time in `comparable`.
            SELECT a.act_workspace_id AS act_id_ws, NULL::uuid AS peer_id, g AS genre
            FROM event_acts AS a
            JOIN band_listings AS bl
              ON bl.workspace_id = a.act_workspace_id
            CROSS JOIN LATERAL unnest(bl.genre_tags) AS g
            UNION ALL
            SELECT NULL, pag.peer_act_id, pag.genre_tag
            FROM place_peer_act_genres AS pag
        ), comparable AS (
            -- A bill act counts when its genre set intersects the requesting
            -- workspace's own. Both sides resolve through the alias map:
            -- free text is the display form, canonical is what matches.
            SELECT m.venue_id,
                   count(DISTINCT COALESCE(a.act_workspace_id::text, a.peer_act_id::text))::bigint
                       AS comparable_acts
            FROM place_venue_marks AS m
            JOIN event_acts AS a
              ON a.event_id = m.event_id
             AND a.workspace_id = m.workspace_id
            -- Never the asking band itself. Its own genres intersect its own
            -- by construction, so counting it made "acts from your genre have
            -- played here" partly mean "you have played here" — which the row
            -- already says, under `shows_played`, and which proves nothing
            -- about whether the room books the genre from anyone else.
            WHERE a.act_workspace_id IS DISTINCT FROM $1
              AND EXISTS (
                SELECT 1
                FROM (
                    SELECT COALESCE(mine_alias.canonical, mine_tag.genre) AS genre
                    FROM unnest($2::text[]) AS mine_tag(genre)
                    LEFT JOIN place_genre_aliases AS mine_alias
                      ON mine_alias.alias = mine_tag.genre
                ) AS mine
                JOIN (
                    SELECT COALESCE(their_alias.canonical, lower(btrim(ag.genre))) AS genre
                    FROM act_genres AS ag
                    LEFT JOIN place_genre_aliases AS their_alias
                      ON their_alias.alias = lower(btrim(ag.genre))
                    WHERE ag.act_id_ws = a.act_workspace_id
                       OR ag.peer_id = a.peer_act_id
                ) AS theirs ON theirs.genre = mine.genre
            )
            GROUP BY m.venue_id
        )
        SELECT
            venue.id AS venue_id,
            venue.display_name,
            city.slug AS city_slug,
            city.name AS city_name,
            city.country_code,
            -- Played is past tense — a booked future night is not a show
            -- the room has seen yet; it reads separately.
            count(marks.event_id) FILTER (
                WHERE marks.starts_at <= now()
            )::bigint AS shows_played,
            count(marks.event_id) FILTER (
                WHERE marks.starts_at > now() AND marks.status = 'published'
            )::bigint AS shows_booked,
            count(DISTINCT marks.workspace_id)::bigint AS contributors,
            avg(draw.paid_orders) AS typical_draw,
            COALESCE(repeaters.repeat_attenders, 0)::bigint AS repeat_attenders,
            COALESCE(comparable.comparable_acts, 0)::bigint AS comparable_acts,
            max(marks.starts_at) FILTER (WHERE marks.starts_at <= now()) AS last_played_at,
            min(marks.starts_at) FILTER (
                WHERE marks.starts_at > now() AND marks.status = 'published'
            ) AS next_show_at,
            -- Each resolved join yields at most one row per venue —
            -- DISTINCT ON (venue_id, attribute) — so max() lifts the single
            -- surviving fact out of the aggregate rather than choosing
            -- between rival claims.
            max(cap.value) AS capacity_fact,
            max(cap.provenance) AS capacity_provenance,
            max(cap.observed_at) AS capacity_observed_at,
            max(gen.value) AS genres_fact,
            max(gen.provenance) AS genres_provenance,
            max(gen.observed_at) AS genres_observed_at,
            max(web.value) AS website_fact,
            max(web.provenance) AS website_provenance,
            max(web.observed_at) AS website_observed_at,
            max(addr.value) AS address_fact,
            max(addr.provenance) AS address_provenance,
            max(addr.observed_at) AS address_observed_at,
            max(stat.value) AS status_fact,
            max(stat.provenance) AS status_provenance,
            max(stat.observed_at) AS status_observed_at
        FROM place_venues AS venue
        JOIN cities AS city
          ON city.id = venue.city_id
        LEFT JOIN marks
          ON marks.venue_id = venue.id
        LEFT JOIN per_show_draw AS draw
          ON draw.venue_id = marks.venue_id
         AND draw.event_id = marks.event_id
        LEFT JOIN repeaters
          ON repeaters.venue_id = venue.id
        LEFT JOIN comparable
          ON comparable.venue_id = venue.id
        LEFT JOIN resolved AS cap
          ON cap.venue_id = venue.id AND cap.attribute = 'capacity'
        LEFT JOIN resolved AS gen
          ON gen.venue_id = venue.id AND gen.attribute = 'genres'
        LEFT JOIN resolved AS web
          ON web.venue_id = venue.id AND web.attribute = 'website'
        LEFT JOIN resolved AS addr
          ON addr.venue_id = venue.id AND addr.attribute = 'address'
        LEFT JOIN resolved AS stat
          ON stat.venue_id = venue.id AND stat.attribute = 'status'
        -- A room resolved closed is a dead lead; the list does not show it.
        WHERE lower(btrim(COALESCE(stat.value, ''))) <> 'closed'
          AND ($3::text IS NULL OR city.slug = $3)
        GROUP BY venue.id, venue.display_name, city.slug, city.name,
                 city.country_code, repeaters.repeat_attenders,
                 comparable.comparable_acts
        ORDER BY shows_played DESC, venue.display_name, venue.id
        LIMIT 500
        "#,
    )
    .bind(workspace_id)
    .bind(&my_genres)
    .bind(only_city)
    .fetch_all(&state.database)
    .await?;
    assess_venue_rows(state, &mut result).await;
    Ok(result)
}
