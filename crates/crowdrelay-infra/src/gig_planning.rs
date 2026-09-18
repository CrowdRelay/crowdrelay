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

/// How long after an approval a show in that city still counts as the
/// proposal's outcome.
///
/// A promoter who answers takes days; a date that comes of it takes weeks. One
/// hundred and twenty days covers the answer, the negotiation and the
/// announcement, and stops before the next season — where a booking has its own
/// reasons and this proposal is not one of them.
///
/// The window is the whole of the attribution. Unbounded, every city the band
/// ever plays eventually marks every proposal ever made for it as a success,
/// and a tally in which every reason works is a tally nobody can act on.
const SHOW_ATTRIBUTION_WINDOW_DAYS: i64 = 120;

#[derive(Debug, sqlx::FromRow)]
struct CityRow {
    city_id: Uuid,
    city_slug: String,
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
                   max(event.starts_at) FILTER (WHERE event.starts_at <= $2) AS last_show_at,
                   min(event.starts_at) FILTER (
                       WHERE event.starts_at > $2 AND event.status = 'published'
                   ) AS next_show_at
            FROM events AS event
            JOIN cities AS city ON city.id = event.city_id
            WHERE event.workspace_id = $1
              AND event.status IN ('published', 'completed')
            GROUP BY city.id, city.slug
        )
        -- Identity is the id, not the slug: the catalogue is unique on
        -- (country_code, slug), so a bare slug can merge two cities' evidence
        -- into one phantom opportunity.
        SELECT COALESCE(interested.city_id, shows.city_id) AS city_id,
               COALESCE(interested.city_slug, shows.city_slug) AS city_slug,
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
                 COALESCE(interested.city_slug, shows.city_slug)
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
                          FROM viryaos_band_listings AS listing
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
        LEFT JOIN viryaos_booking_targets AS target
          ON target.workspace_id = $1
         AND (
             target.venue_id = venue.id
             OR EXISTS (
                 SELECT 1
                 FROM viryaos_booking_target_venues AS edge
                 WHERE edge.workspace_id = target.workspace_id
                   AND edge.target_id = target.id
                   AND edge.venue_id = venue.id
             )
         )
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
                   SELECT 1 FROM viryaos_booking_interactions AS interaction
                   WHERE interaction.workspace_id = target.workspace_id
                     AND interaction.target_id = target.id
                     AND interaction.direction = 'inbound'
               ) AS answered_last_time
        FROM viryaos_booking_targets AS target
        WHERE target.workspace_id = $1
          AND target.city_id = $2
          AND target.target_kind IN ('promoter', 'venue')
          AND target.active
          AND target.accepts_booking
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
    city_opportunities_inner(pool, workspace_id, now, true).await
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
    city_opportunities_inner(pool, workspace_id, now, false).await
}

async fn city_opportunities_inner(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    gather_co_bill: bool,
) -> Result<Vec<CityOpportunity>, sqlx::Error> {
    let cities = candidate_cities(pool, workspace_id, now).await?;
    // The tenant's own genre set, fetched once per pass — the "mine" half
    // of every venue's comparable-acts test, canonicalised through the
    // shared alias map the same way `city_venues` does it. One read per
    // pass rather than one per city also keeps the snapshot consistent: a
    // listing edited mid-loop cannot give two cities two different "mine".
    let my_genres = sqlx::query_scalar::<_, Vec<String>>(
        r#"
        SELECT COALESCE(array_agg(DISTINCT lower(btrim(tag))), '{}')
        FROM viryaos_band_listings AS listing, unnest(listing.genre_tags) AS tag
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
        opportunities.push(CityOpportunity {
            city_id: crowdrelay_domain::CityId::from_uuid(city.city_id),
            city: city.city_slug.clone(),
            reachable_fans: reachable,
            active_fans_30d: bounded_u16(city.active_30d).into(),
            months_since_show: months_between(city.last_show_at, now),
            has_upcoming_show: city.next_show_at.is_some(),
            venue: best_venue(pool, workspace_id, city.city_id, now, &my_genres).await?,
            promoters: promoters_in_city(pool, workspace_id, city.city_id).await?,
            co_bill,
        });
    }
    Ok(opportunities)
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
    headliner: String,
    days_until_show: i64,
}

// ── 4G.5: what an approved proposal actually produced ──────────────────────
//
// The reasons a proposal carries are structured precisely so they can be
// scored: "a room that books our genre", "240 reachable people", "the booker
// answered last time" are hypotheses, and the honest question is which of
// them turned out to predict a reply or a show. That tally is computed here
// rather than written beside the decision — the decision's `input_snapshot`
// already holds the reasons verbatim, and a second store would be a second
// truth the first time they disagree.

/// One approved proposal and what came of it, newest first.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProposalOutcome {
    pub city: String,
    pub venue: String,
    pub approved_at: OffsetDateTime,
    /// The action's own status — `queued` for a letter parked on a missing
    /// executor, `succeeded` once the outreach actually ran. A score only
    /// exists once the letter left.
    pub action_status: String,
    /// Promoters the letter went to.
    pub recipients: u32,
    /// The catalogue id of `city`, from the decision's subject. The slug in
    /// `city` is for reading; this is the identity a display-name lookup or a
    /// same-slug sibling needs.
    pub city_id: Uuid,
    /// Promoters who answered inside the seven-day window.
    pub replies: u32,
    /// Reply windows still open — a proposal with any of these is in flight
    /// and does not score yet.
    pub unfinished_measurements: u32,
    /// A show in the proposal's city, booked inside
    /// `SHOW_ATTRIBUTION_WINDOW_DAYS` of the approval.
    ///
    /// The window is the whole of the attribution. Unbounded, every city the
    /// band ever plays eventually scores every proposal ever made for it, and
    /// the reason tally converges on "every reason works" — which is the same
    /// as no tally at all.
    pub show_booked: bool,
    /// The stronger version: the show inside the window is at the room the
    /// proposal named. A city booking is evidence the letter helped; the named
    /// room is evidence it worked.
    pub show_booked_at_venue: bool,
    /// The reasons the approved proposal carried, as they were stored.
    pub reasons: Vec<crowdrelay_domain::gig_plan::Reason>,
    /// Reasons in the snapshot this build cannot read, counted rather than
    /// dropped silently.
    ///
    /// A snapshot is history: it was written by whatever the vocabulary was
    /// that day. Renaming or removing a `Reason` variant later must not make
    /// the whole track record unreadable, and it must not quietly shrink an
    /// old proposal's reason list either — a proposal that scored on three
    /// reasons and now reads as two is a silently rewritten record.
    pub unreadable_reasons: u32,
}

impl ProposalOutcome {
    /// Every reply window has closed and the letter genuinely left — the two
    /// conditions under which "nobody answered" is a fact rather than a guess.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.action_status == "succeeded"
            && self.recipients > 0
            && self.unfinished_measurements == 0
    }

    /// The letter was cancelled before it left, so nothing about it is
    /// evidence about the reasons it carried.
    ///
    /// Kept distinct from "settled with no reply", which is a real answer from
    /// a real promoter. A console that shows these as the same thing teaches
    /// the band that their reasons do not work, when the truth is that nothing
    /// was ever sent.
    #[must_use]
    pub fn never_sent(&self) -> bool {
        self.action_status == "cancelled"
    }
}

/// The tally the learning question is asked with: of the settled proposals
/// that carried this reason, how many produced a reply, and how many a show.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ReasonScore {
    /// The `Reason` variant tag — the vocabulary the proposals were made in.
    pub kind: &'static str,
    /// Settled proposals that carried it.
    pub proposals: u32,
    /// Of those, how many got at least one promoter reply.
    pub replies: u32,
    /// Of those, how many produced a show in the city.
    pub shows: u32,
}

/// What approved proposals have produced, and which reasons were on the ones
/// that worked.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GigPlanTrackRecord {
    /// Every approved proposal, newest first — including the in-flight ones,
    /// because a letter parked on a missing executor is a fact too.
    pub proposals: Vec<ProposalOutcome>,
    /// Per reason kind, over settled proposals only.
    pub by_reason: Vec<ReasonScore>,
}

#[derive(Debug, sqlx::FromRow)]
struct ProposalOutcomeRow {
    evaluated_at: OffsetDateTime,
    city_id: Uuid,
    city: Option<String>,
    venue: Option<String>,
    reasons: serde_json::Value,
    action_status: String,
    recipients: i64,
    replies: i64,
    unfinished: i64,
    show_booked: bool,
    show_booked_at_venue: bool,
}

/// Reads the decision → action → measurement → event chain for every
/// band-approved proposal (4G.5).
///
/// A reply is an inbound booking interaction measured by `BookingReply7d`
/// inside seven days; a show is a non-cancelled event in the proposal's city
/// created after the approval. Neither attribution is stronger than that —
/// a reply timed to the outreach is the promoter's answer, and a show that
/// appeared after the band wrote is the outcome the proposal was for.
///
/// # Errors
///
/// Propagates the database error.
pub async fn proposal_track_record(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<GigPlanTrackRecord, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProposalOutcomeRow>(
        r#"
        WITH proposals AS (
            SELECT decision.id AS decision_id,
                   decision.evaluated_at,
                   decision.subject_id AS city_id,
                   decision.input_snapshot ->> 'city' AS city,
                   decision.input_snapshot ->> 'venue' AS venue,
                   decision.input_snapshot -> 'reasons' AS reasons,
                   action.id AS action_id,
                   action.status AS action_status,
                   action.payload AS action_payload
            FROM viryaos_autopilot_decisions AS decision
            JOIN viryaos_autopilot_actions AS action
              ON action.workspace_id = decision.workspace_id
             AND action.decision_id = decision.id
            WHERE decision.workspace_id = $1
              AND decision.decision_kind = 'gig.proposal.approved'
        ), reply_counts AS (
            SELECT outcome.action_id,
                   count(*) FILTER (WHERE outcome.observed_value > 0)::bigint AS replies
            FROM viryaos_autopilot_outcomes AS outcome
            JOIN proposals ON proposals.action_id = outcome.action_id
            WHERE outcome.workspace_id = $1
              AND outcome.metric_key = 'effect.booking_reply_7d'
            GROUP BY outcome.action_id
        ), unfinished AS (
            SELECT measurement.action_id, count(*)::bigint AS n
            FROM viryaos_autopilot_measurements AS measurement
            JOIN proposals ON proposals.action_id = measurement.action_id
            WHERE measurement.workspace_id = $1
              AND measurement.status IN ('pending', 'processing')
            GROUP BY measurement.action_id
        )
        SELECT proposals.evaluated_at,
               proposals.city_id,
               proposals.city,
               proposals.venue,
               proposals.reasons,
               proposals.action_status,
               -- The room the letter addressed, from the action's own payload.
               -- Measurement outcomes would undercount it: a recipient whose
               -- observation failed is still somebody we wrote to.
               COALESCE(
                   jsonb_array_length(proposals.action_payload -> 'recipients'), 0
               )::bigint AS recipients,
               COALESCE(reply_counts.replies, 0) AS replies,
               COALESCE(unfinished.n, 0) AS unfinished,
               EXISTS (
                   SELECT 1 FROM events AS event
                   WHERE event.workspace_id = $1
                     AND event.city_id = proposals.city_id
                     AND event.created_at >= proposals.evaluated_at
                     AND event.created_at < proposals.evaluated_at + $2::interval
                     AND event.status IN ('published', 'completed')
               ) AS show_booked,
               EXISTS (
                   SELECT 1 FROM events AS event
                   WHERE event.workspace_id = $1
                     AND event.city_id = proposals.city_id
                     AND event.created_at >= proposals.evaluated_at
                     AND event.created_at < proposals.evaluated_at + $2::interval
                     AND event.status IN ('published', 'completed')
                     AND proposals.venue IS NOT NULL
                     AND lower(btrim(event.venue)) = lower(btrim(proposals.venue))
               ) AS show_booked_at_venue
        FROM proposals
        LEFT JOIN reply_counts ON reply_counts.action_id = proposals.action_id
        LEFT JOIN unfinished ON unfinished.action_id = proposals.action_id
        ORDER BY proposals.evaluated_at DESC
        "#,
    )
    .bind(workspace_id)
    .bind(time::Duration::days(SHOW_ATTRIBUTION_WINDOW_DAYS))
    .fetch_all(pool)
    .await?;

    let mut proposals = Vec::with_capacity(rows.len());
    for row in rows {
        // A snapshot is history, written in whatever the reason vocabulary was
        // that day. Decoding the list as a whole made one unreadable entry
        // fail the entire read — so renaming or retiring a `Reason` variant
        // would take every past proposal's track record down with it, for
        // good, on a surface whose entire job is to remember. Each entry is
        // decoded on its own; what this build cannot read is counted and
        // reported rather than dropped, because a proposal that quietly loses
        // a reason has had its record rewritten.
        let mut reasons = Vec::new();
        let mut unreadable_reasons = 0u32;
        match row.reasons {
            serde_json::Value::Array(entries) => {
                for entry in entries {
                    match serde_json::from_value::<crowdrelay_domain::gig_plan::Reason>(entry) {
                        Ok(reason) => reasons.push(reason),
                        Err(_) => unreadable_reasons = unreadable_reasons.saturating_add(1),
                    }
                }
            }
            // Not an array at all: the snapshot predates the field or was
            // written by something else. Counted as one unreadable reason so
            // the row still lists, with its record honestly incomplete.
            serde_json::Value::Null => {}
            _ => unreadable_reasons = 1,
        }
        proposals.push(ProposalOutcome {
            city_id: row.city_id,
            city: row.city.unwrap_or_default(),
            venue: row.venue.unwrap_or_default(),
            approved_at: row.evaluated_at,
            action_status: row.action_status,
            recipients: u32::try_from(row.recipients).unwrap_or(u32::MAX),
            replies: u32::try_from(row.replies).unwrap_or(u32::MAX),
            unfinished_measurements: u32::try_from(row.unfinished).unwrap_or(u32::MAX),
            show_booked: row.show_booked,
            show_booked_at_venue: row.show_booked_at_venue,
            reasons,
            unreadable_reasons,
        });
    }

    let mut by_reason_map: std::collections::BTreeMap<&'static str, ReasonScore> =
        std::collections::BTreeMap::new();
    for proposal in proposals.iter().filter(|proposal| proposal.is_settled()) {
        for reason in &proposal.reasons {
            let kind = reason_kind(reason);
            let entry = by_reason_map.entry(kind).or_insert(ReasonScore {
                kind,
                proposals: 0,
                replies: 0,
                shows: 0,
            });
            entry.proposals += 1;
            if proposal.replies > 0 {
                entry.replies += 1;
            }
            if proposal.show_booked {
                entry.shows += 1;
            }
        }
    }
    let mut by_reason: Vec<ReasonScore> = by_reason_map.into_values().collect();
    // The reasons that have worked before come first — the whole point of the
    // tally is that a track record should reorder what the band reads.
    by_reason.sort_by(|left, right| {
        right
            .shows
            .cmp(&left.shows)
            .then_with(|| right.replies.cmp(&left.replies))
            .then_with(|| left.kind.cmp(right.kind))
    });

    Ok(GigPlanTrackRecord {
        proposals,
        by_reason,
    })
}

/// The variant tag, which is the vocabulary the tally speaks in.
fn reason_kind(reason: &crowdrelay_domain::gig_plan::Reason) -> &'static str {
    use crowdrelay_domain::gig_plan::Reason;
    match reason {
        Reason::ComparableActsPlayedHere { .. } => "comparable_acts_played_here",
        Reason::ReachableAudience { .. } => "reachable_audience",
        Reason::RoomDraws { .. } => "room_draws",
        Reason::NeverPlayedButHasFans { .. } => "never_played_but_has_fans",
        Reason::OverdueReturn { .. } => "overdue_return",
        Reason::CoBillAddsAudience { .. } => "co_bill_adds_audience",
        Reason::WarmPromoter { .. } => "warm_promoter",
        Reason::RoomIsActive { .. } => "room_is_active",
    }
}

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
