//! The per-city evidence the gig planner reads, for every candidate city at
//! once.
//!
//! The planner used to ask five questions per city, one city after another:
//! the best room, the promoters, the local acts, the converted fans by channel
//! and the reachable audience. At the 40-city cap that was two hundred serial
//! round trips per plan. Each question is now one statement over the whole
//! city list — the single-city SQL unchanged inside, keyed by `city_id` — and
//! the five run concurrently. The single-city entry points
//! (`promoter_targets_in_city`, `reachable_in_city`) call these with one city,
//! so there is one copy of each query.

use std::collections::HashMap;

use crowdrelay_domain::gig_plan::{LocalAct, VenueEvidence};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{LocalActRow, PromoterRow, PromoterTarget, VenueRow, bounded_u16};

/// What one city contributes to its proposal.
#[derive(Default)]
pub(super) struct CityEvidence {
    pub venue: Option<VenueEvidence>,
    pub promoters: Vec<PromoterTarget>,
    pub local_acts: Vec<LocalAct>,
    pub converted_fans_90d: u32,
    pub conversion_channels: Vec<(String, u32)>,
    /// `None` when the city cannot be measured — see `reachable_in_cities`.
    pub reachable: Option<u32>,
}

/// All five questions for every city, concurrently.
pub(super) async fn load(
    pool: &PgPool,
    workspace_id: Uuid,
    city_ids: &[Uuid],
    now: OffsetDateTime,
    my_genres: &[String],
) -> Result<HashMap<Uuid, CityEvidence>, sqlx::Error> {
    let (venues, promoters, local_acts, converted, reachable) = tokio::try_join!(
        best_venues(pool, workspace_id, city_ids, now, my_genres),
        promoter_targets_in_cities(pool, workspace_id, city_ids),
        local_peer_acts(pool, workspace_id, city_ids, my_genres),
        converted_fans_by_channel(pool, workspace_id, city_ids),
        crate::place_reach::reachable_in_cities(pool, workspace_id, city_ids),
    )?;
    let mut evidence: HashMap<Uuid, CityEvidence> = HashMap::with_capacity(city_ids.len());
    for (city_id, venue) in venues {
        evidence.entry(city_id).or_default().venue = Some(venue);
    }
    for (city_id, targets) in promoters {
        evidence.entry(city_id).or_default().promoters = targets;
    }
    for (city_id, acts) in local_acts {
        evidence.entry(city_id).or_default().local_acts = acts;
    }
    for (city_id, (total, channels)) in converted {
        let entry = evidence.entry(city_id).or_default();
        entry.converted_fans_90d = total;
        entry.conversion_channels = channels;
    }
    for (city_id, count) in reachable {
        evidence.entry(city_id).or_default().reachable = count;
    }
    Ok(evidence)
}

#[derive(Debug, sqlx::FromRow)]
struct CityVenueRow {
    city_id: Uuid,
    #[sqlx(flatten)]
    venue: VenueRow,
}

/// The best room on record in a city, with what we know about reaching it.
///
/// "Best" is the one with the most marked shows — the registry's own measure of
/// a room that programmes. Capacity and the booking route come from the
/// tenant's own booking target joined through migration 0296's `venue_id`,
/// which is why that join had to exist before this could. `comparable_acts`
/// counts distinct billed acts at the room whose genres intersect the
/// tenant's — the peer-act graph (4V.6) read through `my_genres`, the same
/// shape `city_venues` and the booking snapshot use, so one answer follows
/// the room wherever it is asked about.
async fn best_venues(
    pool: &PgPool,
    workspace_id: Uuid,
    city_ids: &[Uuid],
    now: OffsetDateTime,
    my_genres: &[String],
) -> Result<HashMap<Uuid, VenueEvidence>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CityVenueRow>(
        r#"
        SELECT want.city_id, best.*
        FROM unnest($2::uuid[]) AS want(city_id)
        CROSS JOIN LATERAL (
        WITH marks AS (
            SELECT mark.venue_id, event.starts_at, sale.id AS sale_id,
                   mark.workspace_id, mark.event_id
            FROM place_venue_marks AS mark
            -- Only this city's rooms: the query below reads no other, and
            -- unscoped this CTE materialised every mark on the platform
            -- once per city asked about.
            JOIN place_venues AS mark_venue
              ON mark_venue.id = mark.venue_id
             AND mark_venue.city_id = want.city_id
            JOIN events AS event ON event.id = mark.event_id
            LEFT JOIN ticket_sales AS sale
              ON sale.workspace_id = mark.workspace_id
             AND sale.event_id = mark.event_id
        ), per_show AS (
            SELECT marks.venue_id, marks.event_id,
                   count(ticket_order.id)::double precision AS paid_orders
            FROM marks
            JOIN ticket_orders AS ticket_order
              ON ticket_order.workspace_id = marks.workspace_id
             AND ticket_order.ticket_sale_id = marks.sale_id
             AND ticket_order.status IN ('paid', 'partially_refunded')
            GROUP BY marks.venue_id, marks.event_id
        )
        SELECT venue.display_name,
               -- DISTINCT because the target join can fan a venue's marks
               -- out — two targets pointing at one room, or one promoter
               -- edge-joined beside its own primary link, must not double
               -- the room's show count.
               count(DISTINCT marks.event_id) FILTER (
                   WHERE marks.starts_at > $3 - INTERVAL '12 months'
                     AND marks.starts_at <= $3
               )::bigint AS shows_last_12_months,
               -- NULL when the room has never hosted anything we know about,
               -- which the planner reads as "never seen" rather than "a long
               -- time ago". The two need different next steps.
               FLOOR(EXTRACT(EPOCH FROM (
                   $3 - max(marks.starts_at) FILTER (WHERE marks.starts_at <= $3)
               )) / 86400)::bigint AS days_since_last_event,
               -- Averaged over ticketed shows only. An unticketed night is
               -- unmeasurable, not a night nobody came to, so it stays out of
               -- the mean instead of dragging it down.
               avg(per_show.paid_orders) AS typical_draw,
               -- Distinct bill acts at the room whose genres intersect the
               -- tenant's, both sides canonicalised through
               -- place_genre_aliases — the same shape `city_venues` and the
               -- booking snapshot use, so one answer follows the room
               -- wherever it is asked about. The requesting workspace's own
               -- acts never count: a tenant is not its own comparable.
               COALESCE(max(comparable.comparable_acts), 0) AS comparable_acts,
               max(target.capacity) AS capacity,
               COALESCE(bool_or(target.active AND target.accepts_booking), false)
                   AS has_booking_route,
               FLOOR(EXTRACT(EPOCH FROM ($3 - max(target.last_outreach_at))) / 86400)::bigint
                   AS contact_verified_days_ago
        FROM place_venues AS venue
        JOIN cities AS city ON city.id = venue.city_id AND city.id = want.city_id
        LEFT JOIN marks ON marks.venue_id = venue.id
        LEFT JOIN per_show
          ON per_show.venue_id = marks.venue_id
         AND per_show.event_id = marks.event_id
        LEFT JOIN LATERAL (
            SELECT count(DISTINCT COALESCE(
                       act.act_workspace_id::text, act.peer_act_id::text))::bigint
                   AS comparable_acts
            FROM place_venue_marks AS mark
            JOIN event_acts AS act
              ON act.event_id = mark.event_id
             AND act.workspace_id = mark.workspace_id
            WHERE mark.venue_id = venue.id
              AND (act.act_workspace_id IS NULL OR act.act_workspace_id <> $1)
              AND EXISTS (
                  SELECT 1
                  FROM (
                      SELECT COALESCE(mine_alias.canonical, mine_tag.genre) AS genre
                      FROM unnest($4::text[]) AS mine_tag(genre)
                      LEFT JOIN place_genre_aliases AS mine_alias
                        ON mine_alias.alias = mine_tag.genre
                  ) AS mine
                  JOIN (
                      SELECT COALESCE(their_alias.canonical,
                                      lower(btrim(their_genre.genre))) AS genre
                      FROM (
                          SELECT unnest(listing.genre_tags) AS genre
                          FROM band_listings AS listing
                          WHERE listing.workspace_id = act.act_workspace_id
                          UNION ALL
                          SELECT peer_genre.genre_tag
                          FROM place_peer_act_genres AS peer_genre
                          WHERE peer_genre.peer_act_id = act.peer_act_id
                      ) AS their_genre
                      LEFT JOIN place_genre_aliases AS their_alias
                        ON their_alias.alias = lower(btrim(their_genre.genre))
                  ) AS theirs ON theirs.genre = mine.genre
              )
        ) AS comparable ON true
        -- The tenant's own booking targets for this room, if they have one —
        -- the primary venue_id union the promoter↔venue edges, so a room a
        -- promoter works reads as reachable even when the target's primary
        -- link names another room (§12-5 entity 6).
        -- Scoped to the workspace: capacity is shared knowledge, but whether
        -- *we* can write to the room is ours.
        LEFT JOIN booking_targets AS target
          ON target.workspace_id = $1
         AND (
             target.venue_id = venue.id
             OR EXISTS (
                 SELECT 1
                 FROM booking_target_venues AS edge
                 WHERE edge.workspace_id = target.workspace_id
                   AND edge.target_id = target.id
                   AND edge.venue_id = venue.id
             )
         )
        -- A room reported shut is not a proposal — but only the *resolved*
        -- status may disqualify: a stale 'closed' must lose to a newer
        -- 'active', or a reopened room would stay excluded forever. The
        -- ladder is the registry's own — provenance trust order, then the
        -- newest claim — applied to the global facts and this tenant's
        -- private marks alike. No status fact at all is the common case and
        -- carries no verdict.
        WHERE COALESCE((
            SELECT lower(btrim(closed_fact.value))
            FROM place_venue_facts AS closed_fact
            WHERE closed_fact.venue_id = venue.id
              AND closed_fact.attribute = 'status'
              AND (closed_fact.workspace_id IS NULL
                   OR closed_fact.workspace_id = $1)
              AND (closed_fact.expires_at IS NULL
                   OR closed_fact.expires_at > $3)
            ORDER BY CASE closed_fact.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2
                         WHEN 'open_directory' THEN 3
                         ELSE 4 END,
                     closed_fact.observed_at DESC
            LIMIT 1
        ), '') <> 'closed'
        GROUP BY venue.id, venue.display_name
        -- DISTINCT here is load-bearing, not cosmetic: the target join can
        -- fan a venue's marks out, and a raw count would let a room win on
        -- booking-target cardinality rather than shows. `venue.id` is the
        -- deterministic tiebreak `city_venues` uses.
        ORDER BY count(DISTINCT marks.event_id) DESC, venue.id
        LIMIT 1
        ) AS best
        "#,
    )
    .bind(workspace_id)
    .bind(city_ids)
    .bind(now)
    .bind(my_genres)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.city_id, super::venue_evidence(row.venue)))
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct CityPromoterRow {
    city_id: Uuid,
    #[sqlx(flatten)]
    promoter: PromoterRow,
}

/// Everybody who books in each city, strongest relationship first, eight per
/// city.
pub(super) async fn promoter_targets_in_cities(
    pool: &PgPool,
    workspace_id: Uuid,
    city_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<PromoterTarget>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CityPromoterRow>(
        r#"
        SELECT ranked.city_id, ranked.id, ranked.version, ranked.display_name,
               ranked.relationship_score, ranked.answered_last_time
        FROM (
        SELECT target.city_id,
               target.id,
               target.version,
               target.display_name,
               target.relationship_score,
               EXISTS (
                   SELECT 1 FROM booking_interactions AS interaction
                   WHERE interaction.workspace_id = target.workspace_id
                     AND interaction.target_id = target.id
                     AND interaction.direction = 'inbound'
               ) AS answered_last_time,
               row_number() OVER (
                   PARTITION BY target.city_id
                   ORDER BY target.relationship_score DESC, target.display_name
               ) AS city_rank
        FROM booking_targets AS target
        WHERE target.workspace_id = $1
          AND target.city_id = ANY($2::uuid[])
          AND target.target_kind IN ('promoter', 'venue')
          AND target.active
          AND target.accepts_booking
          -- A venue-kind target whose room is on record as closed is not a
          -- proposal recipient — the resolved status decides, so a newer
          -- 'active' claim lifts the exclusion (same ladder `best_venue`
          -- uses). A promoter linked to a dead room stays: the room is
          -- dead, the booker is not.
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
                                   WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                                   WHEN 'event_evidence' THEN 2
                                   WHEN 'open_directory' THEN 3
                                   ELSE 4 END,
                               status_fact.observed_at DESC
                      LIMIT 1
                  ), '') = 'closed'
              )
          )
        ) AS ranked
        -- The per-city LIMIT 8 of the single-city read, per partition.
        WHERE ranked.city_rank <= 8
        ORDER BY ranked.city_id, ranked.city_rank
        "#,
    )
    .bind(workspace_id)
    .bind(city_ids)
    .fetch_all(pool)
    .await?;
    let mut by_city: HashMap<Uuid, Vec<PromoterTarget>> = HashMap::new();
    for row in rows {
        by_city
            .entry(row.city_id)
            .or_default()
            .push(super::promoter_target(row.promoter));
    }
    Ok(by_city)
}

#[derive(Debug, sqlx::FromRow)]
struct CityLocalActRow {
    city_id: Uuid,
    #[sqlx(flatten)]
    act: LocalActRow,
}

/// Non-tenant acts tied to a city — the "who could we ask onto this bill"
/// half of the peer registry.
///
/// An act qualifies on either tie: its researched home town is this city
/// (`home_city_id`), or it has billed at one of the city's rooms
/// (`event_acts` → `place_venue_marks` → the room's city). Genre is a
/// ranking signal, not a gate: an act whose genres intersect the tenant's —
/// both sides canonicalised through `place_genre_aliases`, the same shape
/// the comparable-acts count uses — outranks one whose genre nobody has
/// stated, but a local act with no genre on record still answers the
/// support-slot question, because "who is based here" is the part the sheet
/// seed exists to answer.
///
/// `billed_rooms` counts the whole registry, not this tenant's view — the
/// same shared-knowledge rule `propose_peers` applies to tracked rooms.
///
/// The gate that keeps a suggestion realistic is reachability: an act joins
/// the list only when there is a way to actually ask — this workspace holds
/// its `contact_email` lead, the act has billed a tracked room (the venue
/// or promoter on that billing is the intro route), or a public page is on
/// record (`link:social`/`link:website` facts). A name in a directory with
/// none of the three — however famous, however genre-fitting — is research
/// debt, not a support suggestion, which is the whole difference between
/// "ask the opener from last month's bill" and "ask Rammstein".
///
/// # Errors
///
/// Propagates the database error.
async fn local_peer_acts(
    pool: &PgPool,
    workspace_id: Uuid,
    city_ids: &[Uuid],
    my_genres: &[String],
) -> Result<HashMap<Uuid, Vec<LocalAct>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CityLocalActRow>(
        r#"
        SELECT want.city_id, acts.display_name, acts.shared_genres, acts.billed_rooms,
               acts.reachable_via
        FROM unnest($1::uuid[]) AS want(city_id)
        CROSS JOIN LATERAL (
        SELECT act.display_name,
               COALESCE(shared.shared_genres, '{}') AS shared_genres,
               billing.billed_rooms,
               CASE
                   WHEN lead.value IS NOT NULL THEN 'email on file'
                   WHEN billing.billed_rooms > 0 THEN 'billed in tracked rooms'
                   ELSE 'public page'
               END AS reachable_via,
               COALESCE(array_length(shared.shared_genres, 1), 0) AS shared_count,
               act.name_key
        FROM place_peer_acts AS act
        CROSS JOIN LATERAL (
            SELECT array_agg(DISTINCT their.genre ORDER BY their.genre)
                AS shared_genres
            FROM (
                SELECT COALESCE(their_alias.canonical,
                                lower(btrim(their_genre.genre_tag))) AS genre
                FROM place_peer_act_genres AS their_genre
                LEFT JOIN place_genre_aliases AS their_alias
                    ON their_alias.alias = lower(btrim(their_genre.genre_tag))
                WHERE their_genre.peer_act_id = act.id
            ) AS their
            JOIN (
                SELECT COALESCE(mine_alias.canonical, mine_tag.genre) AS genre
                FROM unnest($2::text[]) AS mine_tag(genre)
                LEFT JOIN place_genre_aliases AS mine_alias
                    ON mine_alias.alias = mine_tag.genre
            ) AS mine ON mine.genre = their.genre
        ) AS shared
        CROSS JOIN LATERAL (
            -- Reachability is about rooms that can still be played: a room
            -- on record as closed is not one the act can be reached through
            -- now, so it does not count. The resolved status decides — a
            -- newer 'active' claim lifts the exclusion.
            SELECT count(DISTINCT mark.venue_id) AS billed_rooms
            FROM event_acts AS billed
            JOIN place_venue_marks AS mark
                ON mark.event_id = billed.event_id
            WHERE billed.peer_act_id = act.id
              AND COALESCE((
                  SELECT lower(btrim(status_fact.value))
                  FROM place_venue_facts AS status_fact
                  WHERE status_fact.venue_id = mark.venue_id
                    AND status_fact.attribute = 'status'
                    AND (status_fact.workspace_id IS NULL
                         OR status_fact.workspace_id = $3)
                    AND (status_fact.expires_at IS NULL
                         OR status_fact.expires_at > now())
                  ORDER BY CASE status_fact.provenance
                               WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                               WHEN 'event_evidence' THEN 2
                               WHEN 'open_directory' THEN 3 ELSE 4 END,
                           status_fact.observed_at DESC
                  LIMIT 1
              ), '') <> 'closed'
        ) AS billing
        LEFT JOIN LATERAL (
            SELECT fact.value
            FROM place_peer_act_facts AS fact
            WHERE fact.peer_act_id = act.id
              AND fact.workspace_id = $3
              AND fact.attribute = 'contact_email'
            ORDER BY fact.observed_at DESC
            LIMIT 1
        ) AS lead ON true
        WHERE (act.home_city_id = want.city_id
               OR EXISTS (
                   SELECT 1
                   FROM event_acts AS billed
                   JOIN place_venue_marks AS mark
                       ON mark.event_id = billed.event_id
                   JOIN place_venues AS venue ON venue.id = mark.venue_id
                   WHERE billed.peer_act_id = act.id
                     AND venue.city_id = want.city_id
               ))
          -- Reachability: a suggestion the tenant cannot act on is the
          -- absurd case this gate exists to kill. The private lead is this
          -- tenant's alone; the link facts and the billing are the shared
          -- half of the registry.
          AND (lead.value IS NOT NULL
               OR billing.billed_rooms > 0
               OR EXISTS (
                   SELECT 1
                   FROM place_peer_act_facts AS fact
                   WHERE fact.peer_act_id = act.id
                     AND fact.workspace_id IS NULL
                     AND fact.attribute IN ('link:social', 'link:website')
               ))
          -- A band reported dead is not a suggestion — the same resolved-
          -- status rule rooms follow: the winning claim decides, so a stale
          -- 'inactive' loses to a newer 'active' and a re-formed band comes
          -- back. Peer facts have no 'played' provenance; the ladder is the
          -- registry's own order minus it.
          AND COALESCE((
              SELECT lower(btrim(status_fact.value))
              FROM place_peer_act_facts AS status_fact
              WHERE status_fact.peer_act_id = act.id
                AND status_fact.attribute = 'status'
                AND (status_fact.workspace_id IS NULL
                     OR status_fact.workspace_id = $3)
                AND (status_fact.expires_at IS NULL
                     OR status_fact.expires_at > now())
              ORDER BY CASE status_fact.provenance
                           WHEN 'researched' THEN 0
                           WHEN 'event_evidence' THEN 1
                           WHEN 'open_directory' THEN 2 ELSE 3 END,
                       status_fact.observed_at DESC
              LIMIT 1
          ), '') <> 'inactive'
        ORDER BY COALESCE(array_length(shared.shared_genres, 1), 0) DESC,
                 billing.billed_rooms DESC,
                 act.name_key
        -- A proposal can hold a handful of names; a longer list is a
        -- directory dump, and the room it decorates is the same.
        LIMIT 8
        ) AS acts
        -- The lateral's own order, restated: row order out of a join is
        -- not guaranteed, and the proposal lists acts in this order.
        ORDER BY want.city_id, acts.shared_count DESC, acts.billed_rooms DESC, acts.name_key
        "#,
    )
    .bind(city_ids)
    .bind(my_genres)
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    let mut by_city: HashMap<Uuid, Vec<LocalAct>> = HashMap::new();
    for row in rows {
        by_city.entry(row.city_id).or_default().push(LocalAct {
            name: row.act.display_name,
            shared_genres: row.act.shared_genres,
            billed_rooms: bounded_u16(row.act.billed_rooms),
            reachable_via: row.act.reachable_via,
        });
    }
    Ok(by_city)
}

/// Fans attributed to this city, grouped by the channel that produced them —
/// strongest channel first, ties broken by name so the ranking is stable —
/// plus the city's true distinct-fan total.
///
/// The join is `fan_city_interests`: where a fan said they live is the only
/// city claim the fanbase makes, and a conversion row with no city interest
/// is honestly unlocated rather than guessed. The total is a separate
/// `ROLLUP` row because `DISTINCT` does not commute with the sum: a fan who
/// carries two attributions — a tracked click and a referral — counts once
/// per channel but must still count once in the city's total.
async fn converted_fans_by_channel(
    pool: &PgPool,
    workspace_id: Uuid,
    city_ids: &[Uuid],
) -> Result<HashMap<Uuid, (u32, Vec<(String, u32)>)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, Option<String>, i64)>(
        r#"
        SELECT interest.city_id,
               provenance.channel,
               COUNT(DISTINCT provenance.fan_id)::bigint AS fans
        FROM fan_provenance_events AS provenance
        JOIN fan_city_interests AS interest
          ON interest.workspace_id = provenance.workspace_id
         AND interest.fan_id = provenance.fan_id
         AND interest.city_id = ANY($2::uuid[])
        WHERE provenance.workspace_id = $1
          AND provenance.event_kind = 'conversion'
          AND provenance.fan_id IS NOT NULL
          AND provenance.occurred_at >= now() - interval '90 days'
        GROUP BY interest.city_id, ROLLUP (provenance.channel)
        ORDER BY interest.city_id, fans DESC, provenance.channel
        "#,
    )
    .bind(workspace_id)
    .bind(city_ids)
    .fetch_all(pool)
    .await?;
    let mut by_city: HashMap<Uuid, (u32, Vec<(String, u32)>)> = HashMap::new();
    for (city_id, channel, fans) in rows {
        let entry = by_city.entry(city_id).or_default();
        match channel {
            // The ROLLUP row — the distinct-fan total across all channels.
            None => entry.0 = bounded_u16(fans).into(),
            Some(channel) => entry.1.push((channel, bounded_u16(fans).into())),
        }
    }
    Ok(by_city)
}
