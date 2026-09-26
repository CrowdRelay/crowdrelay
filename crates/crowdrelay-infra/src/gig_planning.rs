//! Builds what `domain::gig_plan` and `domain::roster_plan` decide on.
//!
//! The policy is in the domain: which city is worth playing, who headlines,
//! what refuses and why. This file only gathers the evidence, and the split is
//! load-bearing — every rule in those modules is a pure function over a struct,
//! so the rules are testable without a database and this file cannot quietly
//! introduce a sixth one in SQL.
//!
//! # Absent is not zero, all the way down
//!
//! Every read here preserves the difference. A city with no `place_venues` row
//! has `venue: None`, which refuses with "no room on record" rather than
//! proposing a nameless one. A room with no ticketed show has
//! `typical_draw: None`, which becomes a caveat rather than a draw of zero. A
//! city the band has never played has `months_since_show: None`, which is a
//! different proposal from one they played last year.
//!
//! Folding any of those to zero would make the planner confident about things
//! nobody measured, and a confident wrong proposal costs a band a week.
//!
//! # What is gathered, and what it is not
//!
//! `co_bill` names the roster siblings a city proposal could ask onto the
//! bill, priced by the part of their audience that is genuinely new there.
//! The roster read skips it on purpose: a package move prices its own pair
//! through `choose_support`, and a surfacer-relative bill would name the
//! wrong act's crowd.

use crowdrelay_domain::gig_plan::{CityOpportunity, PromoterRef, TenantIntent, VenueEvidence};
use crowdrelay_domain::roster_plan::{CityReach, OpenSupportSlot, RosterAct, RosterOpportunity};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::place_reach::{audience_overlaps_by_city, reachable_in_city};
use crate::tenant_settings::TenantSettingsRepository;

/// How many cities the planner considers in one pass.
///
/// The domain ranks and caps what it proposes; this only bounds the read. Forty
/// is well past the point where a tenant has meaningful audience anywhere, and
/// the query is per-city so an unbounded version would fan out badly on a
/// roster.
const MAX_CITIES_CONSIDERED: i64 = 40;

#[derive(Debug, sqlx::FromRow)]
struct CityRow {
    city_id: Uuid,
    city_slug: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
    active_30d: i64,
    last_show_at: Option<OffsetDateTime>,
    next_show_at: Option<OffsetDateTime>,
}

#[derive(Debug, sqlx::FromRow)]
struct VenueRow {
    display_name: String,
    shows_last_12_months: i64,
    days_since_last_event: Option<i64>,
    typical_draw: Option<f64>,
    comparable_acts: i64,
    capacity: Option<i32>,
    has_booking_route: bool,
    contact_verified_days_ago: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct PromoterRow {
    id: Uuid,
    version: i64,
    display_name: String,
    relationship_score: i32,
    answered_last_time: bool,
}

/// Cities where this workspace has any audience at all, newest interest first.
///
/// Scoped to cities the tenant has a reason to care about — one with no fans
/// and no history is not an opportunity, it is a map. `active_30d` counts fans
/// with a recorded meaningful action, which is the same definition the funnel
/// uses.
async fn candidate_cities(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<CityRow>, sqlx::Error> {
    sqlx::query_as::<_, CityRow>(
        r#"
        WITH interested AS (
            SELECT city.slug AS city_slug,
                   city.id AS city_id,
                   city.latitude,
                   city.longitude,
                   fan.id AS fan_id,
                   fan_last_meaningful_action(
                       fan.workspace_id, fan.id, fan.normalized_email
                   ) AS last_action_at
            FROM fan_city_interests AS interest
            JOIN cities AS city ON city.id = interest.city_id
            JOIN fans AS fan
              ON fan.workspace_id = interest.workspace_id
             AND fan.id = interest.fan_id
             AND fan.status = 'active'
            WHERE interest.workspace_id = $1
        ), shows AS (
            -- Played is past tense and booked is future. Collapsing them would
            -- make a city with a show next month look like a city that was
            -- served last month, and those want opposite proposals.
            SELECT city.id AS city_id,
                   city.slug AS city_slug,
                   city.latitude,
                   city.longitude,
                   max(event.starts_at) FILTER (WHERE event.starts_at <= $2) AS last_show_at,
                   min(event.starts_at) FILTER (
                       WHERE event.starts_at > $2 AND event.status = 'published'
                   ) AS next_show_at
            FROM events AS event
            JOIN cities AS city ON city.id = event.city_id
            WHERE event.workspace_id = $1
              AND event.status IN ('published', 'completed')
            GROUP BY city.id, city.slug, city.latitude, city.longitude
        )
        -- Identity is the id, not the slug: the catalogue is unique on
        -- (country_code, slug), so a bare slug can merge two cities' evidence
        -- into one phantom opportunity.
        SELECT COALESCE(interested.city_id, shows.city_id) AS city_id,
               COALESCE(interested.city_slug, shows.city_slug) AS city_slug,
               COALESCE(interested.latitude, shows.latitude) AS latitude,
               COALESCE(interested.longitude, shows.longitude) AS longitude,
               COALESCE(count(interested.fan_id) FILTER (
                   WHERE interested.last_action_at > $2 - INTERVAL '30 days'
               ), 0)::bigint AS active_30d,
               max(shows.last_show_at) AS last_show_at,
               min(shows.next_show_at) AS next_show_at
        FROM interested
        -- FULL JOIN because a city the band has played and has no fans in yet
        -- is still a real opportunity — it is the one where the room already
        -- knows them.
        FULL JOIN shows ON shows.city_id = interested.city_id
        GROUP BY COALESCE(interested.city_id, shows.city_id),
                 COALESCE(interested.city_slug, shows.city_slug),
                 COALESCE(interested.latitude, shows.latitude),
                 COALESCE(interested.longitude, shows.longitude)
        ORDER BY active_30d DESC, city_slug
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(MAX_CITIES_CONSIDERED)
    .fetch_all(pool)
    .await
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
async fn best_venue(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    now: OffsetDateTime,
    my_genres: &[String],
) -> Result<Option<VenueEvidence>, sqlx::Error> {
    let row = sqlx::query_as::<_, VenueRow>(
        r#"
        WITH marks AS (
            SELECT mark.venue_id, event.starts_at, sale.id AS sale_id,
                   mark.workspace_id, mark.event_id
            FROM place_venue_marks AS mark
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
        JOIN cities AS city ON city.id = venue.city_id AND city.id = $2
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
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(now)
    .bind(my_genres)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| VenueEvidence {
        name: row.display_name,
        shows_last_12_months: bounded_u16(row.shows_last_12_months),
        comparable_acts: bounded_u16(row.comparable_acts),
        capacity: row.capacity.and_then(|value| u32::try_from(value).ok()),
        typical_draw: row
            .typical_draw
            .map(|draw| u32::try_from(draw.round() as i64).unwrap_or(u32::MAX)),
        contact_verified_days_ago: row
            .contact_verified_days_ago
            .and_then(|days| u16::try_from(days.max(0)).ok()),
        days_since_last_event: row
            .days_since_last_event
            .and_then(|days| u16::try_from(days.max(0)).ok()),
        has_booking_route: row.has_booking_route,
    }))
}

/// Promoters this workspace can write to in a city.
///
/// `answered_last_time` is the strongest cheap signal there is about whether an
/// approach is worth making, and it comes from the interaction ledger rather
/// than from the relationship score, which moves for other reasons too.
/// The same promoters the proposal names, carrying the row identity the
/// outreach needs (4G.4).
///
/// `PromoterRef` is a domain type and has no identifiers on purpose — the
/// planner decides on evidence, not on rows. But the letter has to be
/// addressed, and matching the proposal's names back to rows afterwards would
/// be a second read with its own ordering, its own `LIMIT`, and the chance of
/// resolving a name to a different promoter than the one judged. So both come
/// from this one query.
#[derive(Clone, Debug)]
pub struct PromoterTarget {
    pub target_id: Uuid,
    pub target_version: i64,
    pub name: String,
    pub relationship_score: u16,
    pub answered_last_time: bool,
}

impl PromoterTarget {
    /// What the planner judges: the same facts, without the row.
    #[must_use]
    pub fn as_promoter_ref(&self) -> PromoterRef {
        PromoterRef {
            // The booking target's own id. The domain never reads it — it
            // carries it so the letter is addressed to the row the proposal
            // judged, rather than to whoever happens to share that name.
            key: self.target_id.to_string(),
            name: self.name.clone(),
            relationship_score: self.relationship_score,
            answered_last_time: self.answered_last_time,
            // Selected only when active and accepting booking, and
            // `contact_email` is NOT NULL, so a selected row is contactable by
            // construction.
            has_route: true,
        }
    }
}

/// Everybody who books in this city, strongest relationship first.
///
/// # Errors
///
/// Propagates the database error.
pub async fn promoter_targets_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<Vec<PromoterTarget>, sqlx::Error> {
    let rows = sqlx::query_as::<_, PromoterRow>(
        r#"
        SELECT target.id,
               target.version,
               target.display_name,
               target.relationship_score,
               EXISTS (
                   SELECT 1 FROM booking_interactions AS interaction
                   WHERE interaction.workspace_id = target.workspace_id
                     AND interaction.target_id = target.id
                     AND interaction.direction = 'inbound'
               ) AS answered_last_time
        FROM booking_targets AS target
        WHERE target.workspace_id = $1
          AND target.city_id = $2
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
        ORDER BY target.relationship_score DESC, target.display_name
        LIMIT 8
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PromoterTarget {
            target_id: row.id,
            target_version: row.version,
            name: row.display_name,
            relationship_score: bounded_u16(i64::from(row.relationship_score)),
            answered_last_time: row.answered_last_time,
        })
        .collect())
}

async fn promoters_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<Vec<PromoterRef>, sqlx::Error> {
    Ok(promoter_targets_in_city(pool, workspace_id, city_id)
        .await?
        .iter()
        .map(PromoterTarget::as_promoter_ref)
        .collect())
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
    city_id: Uuid,
) -> Result<(u32, Vec<(String, u32)>), sqlx::Error> {
    let rows = sqlx::query_as::<_, (Option<String>, i64)>(
        r#"
        SELECT provenance.channel,
               COUNT(DISTINCT provenance.fan_id)::bigint AS fans
        FROM fan_provenance_events AS provenance
        JOIN fan_city_interests AS interest
          ON interest.workspace_id = provenance.workspace_id
         AND interest.fan_id = provenance.fan_id
         AND interest.city_id = $2
        WHERE provenance.workspace_id = $1
          AND provenance.event_kind = 'conversion'
          AND provenance.fan_id IS NOT NULL
          AND provenance.occurred_at >= now() - interval '90 days'
        GROUP BY ROLLUP (provenance.channel)
        ORDER BY fans DESC, provenance.channel
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .fetch_all(pool)
    .await?;
    let mut total = 0u32;
    let mut channels = Vec::with_capacity(rows.len());
    for (channel, fans) in rows {
        match channel {
            // The ROLLUP row — the distinct-fan total across all channels.
            None => total = bounded_u16(fans).into(),
            Some(channel) => channels.push((channel, bounded_u16(fans).into())),
        }
    }
    Ok((total, channels))
}

fn bounded_u16(value: i64) -> u16 {
    u16::try_from(value.clamp(0, i64::from(u16::MAX))).unwrap_or(u16::MAX)
}

/// Months between two instants, floored. `None` in means `None` out — a band
/// that has never played a city is not a band that played it zero months ago.
fn months_between(from: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<u16> {
    from.map(|then| {
        let months = (i32::from(now.year() as i16) - i32::from(then.year() as i16)) * 12
            + i32::from(u8::from(now.month()))
            - i32::from(u8::from(then.month()));
        bounded_u16(i64::from(months.max(0)))
    })
}

/// Everything one workspace's gig planner decides on.
///
/// # Errors
///
/// Propagates the database error.
pub async fn city_opportunities(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<CityOpportunity>, sqlx::Error> {
    city_opportunities_inner(pool, workspace_id, now, true, None).await
}

/// The roster's per-member read. Every city's `co_bill` is relative to the
/// workspace that surfaced it — the wrong lens for a package move, which the
/// roster prices headliner-relative through `choose_support`. Skipping the
/// gather keeps it honest and saves an all-pairs overlap pass per member.
pub async fn city_opportunities_for_roster_member(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<CityOpportunity>, sqlx::Error> {
    city_opportunities_inner(pool, workspace_id, now, false, None).await
}

/// One city's evidence for the city page — the same pass, narrowed by slug
/// before the per-city reads run, so the page and the plan cannot disagree.
pub async fn city_opportunity(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    city_slug: &str,
) -> Result<Option<CityOpportunity>, sqlx::Error> {
    Ok(
        city_opportunities_inner(pool, workspace_id, now, true, Some(city_slug))
            .await?
            .pop(),
    )
}

async fn city_opportunities_inner(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    gather_co_bill: bool,
    only_city: Option<&str>,
) -> Result<Vec<CityOpportunity>, sqlx::Error> {
    let mut cities = candidate_cities(pool, workspace_id, now).await?;
    cities.retain(|city| only_city.is_none_or(|slug| city.city_slug == slug));
    // The tenant's own genre set, fetched once per pass — the "mine" half
    // of every venue's comparable-acts test, canonicalised through the
    // shared alias map the same way `city_venues` does it. One read per
    // pass rather than one per city also keeps the snapshot consistent: a
    // listing edited mid-loop cannot give two cities two different "mine".
    let my_genres = sqlx::query_scalar::<_, Vec<String>>(
        r#"
        SELECT COALESCE(array_agg(DISTINCT lower(btrim(tag))), '{}')
        FROM band_listings AS listing, unnest(listing.genre_tags) AS tag
        WHERE listing.workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    // The bill half of the package matcher: who else on the roster could be
    // asked onto it, and what their audience adds here. Only organisation
    // siblings qualify — an act outside the roster is a peer, and a peer
    // cannot be named because nobody consented for them. A workspace with no
    // organisation has no siblings and every city degrades to a solo
    // proposal, which is correct rather than incomplete.
    let siblings = if gather_co_bill {
        sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT sibling.id, sibling.name
            FROM workspaces AS me
            JOIN workspaces AS sibling
              ON sibling.organization_id = me.organization_id
             AND sibling.id <> me.id
            WHERE me.id = $1
              AND me.organization_id IS NOT NULL
            -- Ties break on id so the same evidence produces the same bill:
            -- a proposal that reshuffles between runs reads as a new one.
            ORDER BY sibling.name, sibling.id
            -- The roster's own member cap: a larger org is a label the
            -- roster read itself never shows, and the bill cannot name
            -- acts the rest of the system cannot see.
            LIMIT 60
            "#,
        )
        .bind(workspace_id)
        .fetch_all(pool)
        .await?
    } else {
        Vec::new()
    };
    // "Already agreed to be proposed" is an active event_crossbill consent
    // *offered by the sibling*: `from` is the audience owner who acted, `to`
    // is the beneficiary. A tenant-to-sibling edge proves only that the
    // tenant consented — either side may approve one, so naming the sibling
    // on its strength could announce an act that never touched the row.
    let consented: Vec<Uuid> = if siblings.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT consent.from_workspace_id
            FROM amplification_consents AS consent
            JOIN workspaces AS me ON me.id = $1
            WHERE consent.organization_id = me.organization_id
              AND consent.purpose = 'event_crossbill'
              AND consent.status = 'active'
              AND consent.to_workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .fetch_all(pool)
        .await?
    };
    // One overlap pass measures every (tenant, sibling) pair in every
    // candidate city — the same gate the roster read uses, so a city's
    // proposal and the roster's answer can never disagree about a share.
    let city_ids: Vec<Uuid> = cities.iter().map(|city| city.city_id).collect();
    let overlaps = if siblings.is_empty() || city_ids.is_empty() {
        Vec::new()
    } else {
        let mut ids: Vec<Uuid> = siblings.iter().map(|(id, _)| *id).collect();
        ids.push(workspace_id);
        audience_overlaps_by_city(pool, &ids, &city_ids).await?
    };

    let mut opportunities = Vec::with_capacity(cities.len());
    for city in cities {
        // `None` means the city cannot be measured — no coordinates on
        // record. Handed through as `None` so the planner refuses it as
        // unmeasurable rather than inventing a zero it never counted.
        let reachable = reachable_in_city(pool, workspace_id, city.city_id).await?;
        let local_acts = local_peer_acts(pool, workspace_id, city.city_id, &my_genres).await?;
        let co_bill = siblings
            .iter()
            .filter_map(|(sibling_id, sibling_name)| {
                let overlap = overlaps.iter().find(|overlap| {
                    overlap.city_id == city.city_id
                        && (overlap.workspace_a == *sibling_id
                            || overlap.workspace_b == *sibling_id)
                        && (overlap.workspace_a == workspace_id
                            || overlap.workspace_b == workspace_id)
                })?;
                // A pair row exists only when both sides reach somebody
                // here, so the sibling's side is measured — `shared` may
                // still be zero, which is the package worth proposing.
                let reachable_here = if overlap.workspace_a == *sibling_id {
                    overlap.reachable_a
                } else {
                    overlap.reachable_b
                };
                Some(crowdrelay_domain::gig_plan::CoBillAct {
                    workspace: crowdrelay_domain::WorkspaceId::from_uuid(*sibling_id),
                    name: sibling_name.clone(),
                    reachable_here,
                    // The measured intersection, carried exactly — rebuilding
                    // it from the basis-point share would lose people to
                    // rounding in a number a promoter reads.
                    shared_with_tenant: overlap.shared,
                    audience_overlap_basis_points: overlap.share_of(*sibling_id)?,
                    consented_to_share_bills: consented.contains(sibling_id),
                })
            })
            .collect();
        // Attributed arrivals per channel in this city — the fans who came
        // through a tracked link or an owned path, grouped so the top channel
        // is the one that actually delivers here. A city with no attributed
        // arrivals returns empty: the count is a measured zero, not a guess.
        let (converted_fans_90d, conversion_channels) =
            converted_fans_by_channel(pool, workspace_id, city.city_id).await?;
        let (top_conversion_channel, top_conversion_channel_fans) = conversion_channels
            .first()
            .map(|(channel, count)| (Some(channel.clone()), *count))
            .unwrap_or_default();
        opportunities.push(CityOpportunity {
            city_id: crowdrelay_domain::CityId::from_uuid(city.city_id),
            city: city.city_slug.clone(),
            // The catalogue's own pin — `None` means the city was never
            // geocoded, and an unlocatable city cannot join a corridor.
            latitude: city.latitude,
            longitude: city.longitude,
            reachable_fans: reachable,
            active_fans_30d: bounded_u16(city.active_30d).into(),
            months_since_show: months_between(city.last_show_at, now),
            has_upcoming_show: city.next_show_at.is_some(),
            converted_fans_90d,
            top_conversion_channel,
            top_conversion_channel_fans,
            venue: best_venue(pool, workspace_id, city.city_id, now, &my_genres).await?,
            promoters: promoters_in_city(pool, workspace_id, city.city_id).await?,
            co_bill,
            local_acts,
        });
    }
    Ok(opportunities)
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
    city_id: Uuid,
    my_genres: &[String],
) -> Result<Vec<crowdrelay_domain::gig_plan::LocalAct>, sqlx::Error> {
    let rows = sqlx::query_as::<_, LocalActRow>(
        r#"
        SELECT act.display_name,
               COALESCE(shared.shared_genres, '{}') AS shared_genres,
               billing.billed_rooms,
               CASE
                   WHEN lead.value IS NOT NULL THEN 'email on file'
                   WHEN billing.billed_rooms > 0 THEN 'billed in tracked rooms'
                   ELSE 'public page'
               END AS reachable_via
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
        WHERE (act.home_city_id = $1
               OR EXISTS (
                   SELECT 1
                   FROM event_acts AS billed
                   JOIN place_venue_marks AS mark
                       ON mark.event_id = billed.event_id
                   JOIN place_venues AS venue ON venue.id = mark.venue_id
                   WHERE billed.peer_act_id = act.id
                     AND venue.city_id = $1
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
        "#,
    )
    .bind(city_id)
    .bind(my_genres)
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| crowdrelay_domain::gig_plan::LocalAct {
            name: row.display_name,
            shared_genres: row.shared_genres,
            billed_rooms: bounded_u16(row.billed_rooms),
            reachable_via: row.reachable_via,
        })
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct LocalActRow {
    display_name: String,
    shared_genres: Vec<String>,
    billed_rooms: i64,
    reachable_via: String,
}

/// What one workspace has stated it is working on, or `Unstated`.
///
/// One resolution for the band route and the roster route. Two readers of the
/// same setting would eventually disagree about what an unreadable value means,
/// and the disagreement would show up as a band receiving proposals on one
/// surface and not the other.
///
/// A stored value nobody recognises resolves to `Unstated`: the vocabulary is
/// validated on write, so an unreadable row is a hand edit, and proposing with
/// the timing marked unverified is the weakest thing the planner can do with it.
/// It is never read as `HeadsDown` — silently withholding every proposal on the
/// strength of a typo is the failure nobody would report.
///
/// # Errors
///
/// Propagates the database error.
pub async fn stated_intent(
    settings: &TenantSettingsRepository,
    workspace_id: Uuid,
) -> Result<TenantIntent, sqlx::Error> {
    Ok(settings
        .tenant_intent(workspace_id)
        .await?
        .as_deref()
        .and_then(TenantIntent::parse)
        .unwrap_or_default())
}

/// The roster's view: every act in the organisation, and the cities any of them
/// could play.
///
/// Cities are gathered from the whole organisation rather than per act, because
/// a city one act has an audience in is a city the roster can play — the
/// planner decides which act, and it needs the option in front of it to do
/// that.
///
/// # Errors
///
/// Propagates the database error.
pub async fn roster_opportunity(
    pool: &PgPool,
    organization_id: Uuid,
    packages_this_period: u16,
    now: OffsetDateTime,
) -> Result<RosterOpportunity, sqlx::Error> {
    let members = sqlx::query(
        r#"
        SELECT workspace.id, workspace.name
        FROM workspaces AS workspace
        WHERE workspace.organization_id = $1
        ORDER BY workspace.name, workspace.id
        LIMIT 60
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut acts = Vec::with_capacity(members.len());
    let mut cities: Vec<CityOpportunity> = Vec::new();
    let settings = TenantSettingsRepository::new(pool.clone());

    // Every act's evidence first, then the overlaps, then the acts — because
    // an overlap is now a per-city fact (4G.3b) and the cities are whatever
    // this roster's own funnels surfaced.
    let mut per_act: Vec<(Uuid, String, Vec<CityOpportunity>, TenantIntent)> =
        Vec::with_capacity(members.len());
    for member in &members {
        let act_workspace: Uuid = member.get("id");
        let act_name: String = member.get("name");
        let per_city = city_opportunities_for_roster_member(pool, act_workspace, now).await?;
        let intent = stated_intent(&settings, act_workspace).await?;
        per_act.push((act_workspace, act_name, per_city, intent));
    }

    // What each pair of acts' audiences share, per city, measured in one pass.
    // `normalized_email` is the only cross-workspace identity, and only counts
    // come back — a roster learns that two acts share 40% of a city, never
    // which people. A pair absent for a city has an empty side there, which
    // the planner reads as unmeasured rather than as separate.
    let member_ids: Vec<Uuid> = per_act.iter().map(|(id, ..)| *id).collect();
    let mut city_ids: Vec<Uuid> = Vec::new();
    for (_, _, per_city, _) in &per_act {
        for city in per_city {
            let id = city.city_id.into_uuid();
            if !city_ids.contains(&id) {
                city_ids.push(id);
            }
        }
    }
    let overlaps = audience_overlaps_by_city(pool, &member_ids, &city_ids).await?;

    for (act_workspace, act_name, per_city, intent) in per_act {
        let reach_by_city = per_city
            .iter()
            .map(|city| CityReach {
                city: city.city.clone(),
                city_id: city.city_id,
                reachable: city.reachable_fans,
            })
            .collect();

        let months_since_last_show = per_city
            .iter()
            .filter_map(|city| city.months_since_show)
            .min();

        let overlap_with = overlaps
            .iter()
            .filter(|overlap| {
                overlap.workspace_a == act_workspace || overlap.workspace_b == act_workspace
            })
            .filter_map(|overlap| {
                let other_workspace = if overlap.workspace_a == act_workspace {
                    overlap.workspace_b
                } else {
                    overlap.workspace_a
                };
                let other_name = members
                    .iter()
                    .find(|member| member.get::<Uuid, _>("id") == other_workspace)
                    .map(|member| member.get::<String, _>("name"))?;
                // The share is of *this* act's audience in that city: how much
                // of what the support could bring there is already the
                // headliner's.
                let basis_points = overlap.share_of(act_workspace)?;
                Some(crowdrelay_domain::roster_plan::ActOverlap {
                    other_act: other_name,
                    city_id: crowdrelay_domain::CityId::from_uuid(overlap.city_id),
                    overlap_basis_points: basis_points,
                })
            })
            .collect();

        acts.push(RosterAct {
            name: act_name,
            // Each act's own word, read from its own workspace (4G.2). A label
            // cannot state it for them and the roster planner cannot overrule
            // it: an act that says it is recording is not proposed, whatever
            // the roster would prefer.
            intent,
            months_since_last_show,
            reach_by_city,
            overlap_with,
        });

        // One entry per city across the roster. The first act to surface a
        // city contributes its evidence; the planner substitutes whichever
        // act's reach it is judging, so the room and promoter facts are what
        // matter here and those are workspace-independent for the registry
        // half.
        for city in per_city {
            if !cities.iter().any(|seen| seen.city_id == city.city_id) {
                cities.push(city);
            }
        }
    }

    let open_slots = open_support_slots(pool, &member_ids, now).await?;

    Ok(RosterOpportunity {
        acts,
        cities,
        open_slots,
        packages_this_period,
    })
}

/// Confirmed shows across the organisation that have room on the bill (4V.5).
///
/// The roster's cheapest move: the room is held, the promoter is committed and
/// the date is set, so filling the slot costs an ask rather than a booking.
///
/// Only what a person declared. `events.open_support_slots` is set when the
/// promoter offers the place; a bill nobody has spoken about is NULL and is not
/// a slot. Inferring one from the bill's length would have the roster ask for a
/// place that was never offered, which costs the relationship the proposal
/// exists to build.
///
/// Published shows only, and only ahead of now: a slot on a night that already
/// happened is not an opportunity, and a draft show is not a commitment
/// anybody made.
///
/// # Errors
///
/// Propagates the database error.
async fn open_support_slots(
    pool: &PgPool,
    member_ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<Vec<OpenSupportSlot>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SupportSlotRow>(
        r#"
        SELECT city.slug AS city_slug,
               city.id AS city_id,
               COALESCE(NULLIF(btrim(event.venue), ''), 'the room') AS venue,
               event.festival_name,
               workspace.name AS headliner,
               FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 86400)::bigint
                   AS days_until_show
        FROM events AS event
        JOIN cities AS city ON city.id = event.city_id
        JOIN workspaces AS workspace ON workspace.id = event.workspace_id
        WHERE event.workspace_id = ANY($1)
          AND event.status = 'published'
          AND event.starts_at > $2
          AND event.open_support_slots > 0
        ORDER BY event.starts_at
        LIMIT 40
        "#,
    )
    .bind(member_ids)
    .bind(now)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| OpenSupportSlot {
            city: row.city_slug,
            city_id: crowdrelay_domain::CityId::from_uuid(row.city_id),
            venue: row.venue,
            festival_name: row.festival_name,
            headliner: row.headliner,
            days_until_show: bounded_u16(row.days_until_show),
        })
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct SupportSlotRow {
    city_slug: String,
    city_id: Uuid,
    venue: String,
    festival_name: Option<String>,
    headliner: String,
    days_until_show: i64,
}

mod track_record;

pub use track_record::{GigPlanTrackRecord, ProposalOutcome, ReasonScore, proposal_track_record};
#[cfg(test)]
mod tests {
    use super::*;

    /// `None` in, `None` out. A band that has never played a city is not a band
    /// that played it zero months ago, and the whole proposal differs between
    /// those two.
    #[test]
    fn a_city_never_played_stays_absent_rather_than_becoming_zero() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(months_between(None, now), None);
    }

    #[test]
    fn months_are_floored_and_never_negative() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(400);
        let then = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(months_between(Some(then), now), Some(13));

        // A show timestamped in the future clamps to zero rather than
        // underflowing into a very large gap, which would read as an overdue
        // return to a city the band is playing next week.
        let future = now + time::Duration::days(60);
        assert_eq!(months_between(Some(future), now), Some(0));
    }

    #[test]
    fn counts_clamp_instead_of_wrapping() {
        assert_eq!(bounded_u16(-5), 0);
        assert_eq!(bounded_u16(i64::from(u16::MAX) + 10), u16::MAX);
        assert_eq!(bounded_u16(14), 14);
    }
}
