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
//! # What is not gathered yet, and why that is fine
//!
//! `comparable_acts` needs the peer-act graph (4V.6) and arrives as `0`.
//! `co_bill` needs the support-slot entity (4V.5) and arrives empty. Both
//! degrade correctly: zero comparable acts produces the "no act from your
//! genre has played here on record" caveat, which is true, and an empty
//! co-bill produces a solo proposal. The planner is built to say less rather
//! than to guess, so it runs today and improves when those land.

use crowdrelay_domain::gig_plan::{CityOpportunity, PromoterRef, TenantIntent, VenueEvidence};
use crowdrelay_domain::roster_plan::{CityReach, RosterAct, RosterOpportunity};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::place_reach::reachable_in_city;

/// How many cities the planner considers in one pass.
///
/// The domain ranks and caps what it proposes; this only bounds the read. Forty
/// is well past the point where a tenant has meaningful audience anywhere, and
/// the query is per-city so an unbounded version would fan out badly on a
/// roster.
const MAX_CITIES_CONSIDERED: i64 = 40;

#[derive(Debug, sqlx::FromRow)]
struct CityRow {
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
    capacity: Option<i32>,
    has_booking_route: bool,
    contact_verified_days_ago: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct PromoterRow {
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
            SELECT city.slug AS city_slug,
                   max(event.starts_at) FILTER (WHERE event.starts_at <= $2) AS last_show_at,
                   min(event.starts_at) FILTER (
                       WHERE event.starts_at > $2 AND event.status = 'published'
                   ) AS next_show_at
            FROM events AS event
            JOIN cities AS city ON city.id = event.city_id
            WHERE event.workspace_id = $1
              AND event.status IN ('published', 'completed')
            GROUP BY city.slug
        )
        SELECT COALESCE(interested.city_slug, shows.city_slug) AS city_slug,
               COALESCE(count(interested.fan_id) FILTER (
                   WHERE interested.last_action_at > $2 - INTERVAL '30 days'
               ), 0)::bigint AS active_30d,
               max(shows.last_show_at) AS last_show_at,
               min(shows.next_show_at) AS next_show_at
        FROM interested
        -- FULL JOIN because a city the band has played and has no fans in yet
        -- is still a real opportunity — it is the one where the room already
        -- knows them.
        FULL JOIN shows ON shows.city_slug = interested.city_slug
        GROUP BY COALESCE(interested.city_slug, shows.city_slug)
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
/// which is why that join had to exist before this could.
async fn best_venue(
    pool: &PgPool,
    workspace_id: Uuid,
    city_slug: &str,
    now: OffsetDateTime,
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
               count(marks.event_id) FILTER (
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
               max(target.capacity) AS capacity,
               COALESCE(bool_or(target.active AND target.accepts_booking), false)
                   AS has_booking_route,
               FLOOR(EXTRACT(EPOCH FROM ($3 - max(target.last_outreach_at))) / 86400)::bigint
                   AS contact_verified_days_ago
        FROM place_venues AS venue
        JOIN cities AS city ON city.id = venue.city_id
        LEFT JOIN marks ON marks.venue_id = venue.id
        LEFT JOIN per_show
          ON per_show.venue_id = marks.venue_id
         AND per_show.event_id = marks.event_id
        -- The tenant's own booking target for this room, if they have one.
        -- Scoped to the workspace: capacity is shared knowledge, but whether
        -- *we* can write to the room is ours.
        LEFT JOIN viryaos_booking_targets AS target
          ON target.venue_id = venue.id
         AND target.workspace_id = $1
        WHERE city.slug = $2
        GROUP BY venue.id, venue.display_name
        ORDER BY count(marks.event_id) DESC, venue.display_name
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(city_slug)
    .bind(now)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| VenueEvidence {
        name: row.display_name,
        shows_last_12_months: bounded_u16(row.shows_last_12_months),
        // Needs the peer-act graph (4V.6). Zero is honest here: it produces the
        // "no act from your genre has played here on record" caveat, which is
        // exactly what we know.
        comparable_acts: 0,
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
async fn promoters_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_slug: &str,
) -> Result<Vec<PromoterRef>, sqlx::Error> {
    let rows = sqlx::query_as::<_, PromoterRow>(
        r#"
        SELECT target.display_name,
               target.relationship_score,
               EXISTS (
                   SELECT 1 FROM viryaos_booking_interactions AS interaction
                   WHERE interaction.workspace_id = target.workspace_id
                     AND interaction.target_id = target.id
                     AND interaction.direction = 'inbound'
               ) AS answered_last_time
        FROM viryaos_booking_targets AS target
        JOIN cities AS city ON city.id = target.city_id
        WHERE target.workspace_id = $1
          AND city.slug = $2
          AND target.target_kind IN ('promoter', 'venue')
          AND target.active
          AND target.accepts_booking
        ORDER BY target.relationship_score DESC, target.display_name
        LIMIT 8
        "#,
    )
    .bind(workspace_id)
    .bind(city_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PromoterRef {
            name: row.display_name,
            relationship_score: bounded_u16(i64::from(row.relationship_score)),
            answered_last_time: row.answered_last_time,
            // The row is only selected when it is active and accepts booking,
            // and `viryaos_booking_targets.contact_email` is NOT NULL, so a
            // selected row is by construction contactable.
            has_route: true,
        })
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
    let cities = candidate_cities(pool, workspace_id, now).await?;
    let mut opportunities = Vec::with_capacity(cities.len());
    for city in cities {
        let reachable = reachable_in_city(pool, workspace_id, &city.city_slug)
            .await?
            // A city with no coordinates cannot be measured for reach. Zero is
            // the honest floor here rather than a guess: the planner will
            // refuse it for being under the threshold, which is the right
            // answer for a city we cannot size.
            .unwrap_or(0);
        opportunities.push(CityOpportunity {
            city: city.city_slug.clone(),
            reachable_fans: reachable,
            active_fans_30d: bounded_u16(city.active_30d).into(),
            months_since_show: months_between(city.last_show_at, now),
            has_upcoming_show: city.next_show_at.is_some(),
            venue: best_venue(pool, workspace_id, &city.city_slug, now).await?,
            promoters: promoters_in_city(pool, workspace_id, &city.city_slug).await?,
            // Needs the support-slot entity (4V.5). Empty produces a solo
            // proposal, which is correct rather than incomplete.
            co_bill: Vec::new(),
        });
    }
    Ok(opportunities)
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

    for member in members {
        let act_workspace: Uuid = member.get("id");
        let act_name: String = member.get("name");
        let per_city = city_opportunities(pool, act_workspace, now).await?;

        let reach_by_city = per_city
            .iter()
            .map(|city| CityReach {
                city: city.city.clone(),
                reachable: city.reachable_fans,
            })
            .collect();

        let months_since_last_show = per_city
            .iter()
            .filter_map(|city| city.months_since_show)
            .min();

        acts.push(RosterAct {
            name: act_name,
            // Per-act intent is a tenant setting that does not exist yet
            // (4G.2). `Unstated` is the honest default: it proposes on
            // evidence and says the timing is unverified, rather than
            // inferring a plan the act never stated.
            intent: TenantIntent::Unstated,
            months_since_last_show,
            reach_by_city,
            // Cross-workspace overlap is 4G.3b. Empty means the planner will
            // not pair acts at all, which is the safe direction: assuming two
            // audiences are separate is the mistake the number exists to
            // prevent.
            overlap_with: Vec::new(),
        });

        // One entry per city across the roster. The first act to surface a
        // city contributes its evidence; the planner substitutes whichever
        // act's reach it is judging, so the room and promoter facts are what
        // matter here and those are workspace-independent for the registry
        // half.
        for city in per_city {
            if !cities.iter().any(|seen| seen.city == city.city) {
                cities.push(city);
            }
        }
    }

    Ok(RosterOpportunity {
        acts,
        cities,
        // The support-slot entity is 4V.5. Empty means the planner proposes
        // only new bookings — it loses its cheapest move and keeps every other
        // one.
        open_slots: Vec::new(),
        packages_this_period,
    })
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
