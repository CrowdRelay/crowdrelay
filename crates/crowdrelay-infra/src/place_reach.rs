//! One definition of "how many people could we actually tell about a show
//! here".
//!
//! # Why this is its own module
//!
//! Three surfaces need this number and all three must agree: the console's
//! city funnel, an audience attestation a label reads, and the gig planner
//! that decides whether a city is worth playing. A band showing a promoter 300
//! while its own dashboard shows 260 is worse than showing neither, because
//! the disagreement is the thing the promoter remembers.
//!
//! It had already been written twice — once in `audience::city_funnel`, once
//! in `attestation`. A third copy was one call away from existing, and copies
//! of a gate do not stay equal. This is the gate.
//!
//! # What "reachable" means, exactly
//!
//! A person is reachable in a city when **all four** hold:
//!
//! 1. Their fan record is `active` in this workspace.
//! 2. The **latest** row in the append-only `fan_consents` for `marketing` is
//!    granted. Not "any row ever was" — a check-in withdrawal has to drop the
//!    person out of every count, and reading any-row-granted is how a
//!    withdrawal becomes decorative.
//! 3. They turned nearby-gig notifications on.
//! 4. Their own city is inside the radius **they** chose, by great-circle
//!    distance.
//!
//! The fourth is the one that surprises people: this is not "fans who live
//! here", it is "fans who asked to hear about shows within reach of here",
//! which is both larger and more honest. Somebody 40 km away who set a 50 km
//! radius counts; somebody in the city who set 10 km and turned notifications
//! off does not.

use sqlx::PgPool;
use uuid::Uuid;

/// The consent half of "reachable", verbatim — the latest `marketing` row in
/// the append-only ledger must be a grant.
///
/// Both reads in this file embed it rather than rephrasing it: a check-in
/// withdrawal has to drop the person out of every count, and two paraphrases
/// of the same gate are how a withdrawal quietly survives in one of them.
/// `{fan}` is substituted with the alias the caller gave `fans`.
const LATEST_MARKETING_GRANT: &str = r#"
    EXISTS (
        SELECT 1
        FROM fan_consents AS consent
        WHERE consent.workspace_id = {fan}.workspace_id
          AND consent.fan_id = {fan}.id
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
    )"#;

/// The people an act could tell about *some* show — the union of the city
/// gate over every catalogue city, which collapses to a stable predicate:
/// active, consented, notifications on, and a home city with coordinates
/// (their own city is always inside their own radius, so having coordinates
/// is what puts them in the union at all).
///
/// `normalized_email` is the cross-workspace identity — the same person
/// following two acts in one organisation is two fan rows with one address.
/// Only the count ever leaves the query: a roster may learn that two acts
/// share 40% of an audience, never which people (4G.3b).
const REACHABLE_AUDIENCE_EMAILS: &str = r#"
    SELECT fan.workspace_id, fan.normalized_email
    FROM fans AS fan
    WHERE fan.workspace_id = {workspace}
      AND fan.status = 'active'
      AND {consent}
      AND EXISTS (
          SELECT 1
          FROM fan_location_preferences AS preferences
          JOIN cities AS fan_city ON fan_city.id = preferences.city_id
          WHERE preferences.workspace_id = fan.workspace_id
            AND preferences.fan_id = fan.id
            AND preferences.nearby_gigs_enabled
            AND fan_city.latitude IS NOT NULL
            AND fan_city.longitude IS NOT NULL
      )
"#;

/// Reachable, consented people for one city.
///
/// `Ok(None)` means the city is not in the catalogue or has no coordinates —
/// unmeasurable, which is not the same as nobody being there. Callers must
/// carry that distinction rather than folding it to zero. A plain aggregate
/// over the join always returns a row, so the target's own survival of the
/// WHERE clause is what separates "measured nobody" from "cannot measure":
/// `count(DISTINCT target.id)` is zero exactly when the city fell out.
///
/// Keyed by id rather than slug: the catalogue is unique on
/// `(country_code, slug)`, so a bare slug can name two cities and union two
/// cities' radii into one count.
///
/// # Errors
///
/// Propagates the database error.
pub async fn reachable_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<Option<u32>, sqlx::Error> {
    let consent = LATEST_MARKETING_GRANT.replace("{fan}", "fan");
    let value = sqlx::query_scalar::<_, Option<i64>>(&format!(
        r#"
        -- LEFT JOINs keep the target row when nobody qualifies, which is
        -- what separates the two honest answers: the city survives the WHERE
        -- clause exactly when it exists and carries coordinates, so
        -- count(target.id) > 0 means measurable and count(DISTINCT fan.id)
        -- is then the measured number — including a real zero. When the city
        -- fell out, both counts are zero and the answer is NULL.
        SELECT CASE WHEN count(target.id) = 0 THEN NULL
                    ELSE count(DISTINCT fan.id) END::bigint
        FROM cities AS target
        LEFT JOIN fan_location_preferences AS preferences
          ON preferences.workspace_id = $1
         AND preferences.nearby_gigs_enabled
        LEFT JOIN cities AS fan_city
          ON fan_city.id = preferences.city_id
         AND fan_city.latitude IS NOT NULL
         AND fan_city.longitude IS NOT NULL
        LEFT JOIN fans AS fan
          ON fan.workspace_id = preferences.workspace_id
         AND fan.id = preferences.fan_id
         AND fan.status = 'active'
         AND {consent}
          -- The emitter's own pre-filter: one degree of latitude is 111.19 km
          -- wherever you stand, so a pair further apart than the radius in
          -- latitude alone can never be inside it. Kept because dropping it
          -- changes the plan from an index scan to a full haversine over every
          -- preference row.
         AND abs(fan_city.latitude - target.latitude)
              <= (preferences.radius_km + 1)::double precision / 111.0
          -- Rounded, matching the nearby-gig emitter exactly: a fan at 50.4 km
          -- with a 50 km radius is paged, so they are reachable. An unrounded
          -- comparison here would promise a promoter people the system will
          -- never actually notify.
         AND ROUND(6371 * 2 * ASIN(LEAST(1.0, SQRT(
                POWER(SIN(RADIANS(fan_city.latitude - target.latitude) / 2), 2)
                + COS(RADIANS(target.latitude)) * COS(RADIANS(fan_city.latitude))
                * POWER(SIN(RADIANS(fan_city.longitude - target.longitude) / 2), 2)
              ))))::integer <= preferences.radius_km
        WHERE target.id = $2
          AND target.latitude IS NOT NULL
          AND target.longitude IS NOT NULL
        "#
    ))
    .bind(workspace_id)
    .bind(city_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(value.and_then(|count| u32::try_from(count).ok()))
}

/// One unordered pair of workspaces and what their audiences share.
///
/// `shared` is the count of consented, reachable email addresses present in
/// both — the numerator of either direction's share. Each workspace's own
/// reachable total travels with it, because the share of *A's* audience that
/// is also B's is not the share of *B's* that is also A's, and the planner
/// wants the first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudienceOverlap {
    pub workspace_a: Uuid,
    pub workspace_b: Uuid,
    /// Reachable people `workspace_a` has at all.
    pub reachable_a: u32,
    /// Reachable people `workspace_b` has at all.
    pub reachable_b: u32,
    /// People reachable by both. A count, never a list.
    pub shared: u32,
}

impl AudienceOverlap {
    /// Share of `workspace`'s own reachable audience that the other also
    /// reaches, in basis points. `None` when the asked side reaches nobody —
    /// undefined, which a caller must read as *unmeasured* rather than zero.
    #[must_use]
    pub fn share_of(&self, workspace: Uuid) -> Option<u16> {
        let total = if workspace == self.workspace_a {
            self.reachable_a
        } else if workspace == self.workspace_b {
            self.reachable_b
        } else {
            return None;
        };
        if total == 0 {
            return None;
        }
        let bp = u64::from(self.shared) * 10_000 / u64::from(total);
        Some(u16::try_from(bp).unwrap_or(u16::MAX))
    }
}

/// Audience overlap between every pair of the given workspaces, computed in
/// one pass (4G.3b).
///
/// A roster's acts live in sibling workspaces of one organisation, and the
/// same person following two of them is two fan rows with one
/// `normalized_email` — that address is the only join key that means "same
/// human" across the boundary. The intersection happens inside Postgres and
/// only counts return: a roster may learn that two acts share 40% of an
/// audience, never which people.
///
/// Pairs where both sides reach somebody are present even when they share
/// nobody — a measured zero is the pairing the planner *should* propose,
/// which is exactly what an absent row would forbid. Only a side with no
/// reachable audience at all is absent, because a share of nothing is
/// undefined, and the planner reads undefined as unmeasured rather than zero.
///
/// # Errors
///
/// Propagates the database error.
pub async fn audience_overlaps(
    pool: &PgPool,
    workspace_ids: &[Uuid],
) -> Result<Vec<AudienceOverlap>, sqlx::Error> {
    let consent = LATEST_MARKETING_GRANT.replace("{fan}", "fan");
    let rows = sqlx::query_as::<_, (Uuid, Uuid, i64, i64, i64)>(&format!(
        r#"
        WITH reachable AS ({reachable}),
             totals AS (
                 SELECT workspace_id, count(*)::bigint AS n
                 FROM reachable GROUP BY workspace_id
             ),
             pairs AS (
                 SELECT a.workspace_id AS a, b.workspace_id AS b
                 FROM totals AS a JOIN totals AS b
                   ON a.workspace_id < b.workspace_id
             ),
             shared AS (
                 SELECT a.workspace_id AS a, b.workspace_id AS b,
                        count(*)::bigint AS n
                 FROM reachable AS a
                 JOIN reachable AS b USING (normalized_email)
                 GROUP BY a.workspace_id, b.workspace_id
             )
        SELECT pairs.a, pairs.b, totals_a.n, totals_b.n,
               COALESCE(shared.n, 0)
        FROM pairs
        JOIN totals AS totals_a ON totals_a.workspace_id = pairs.a
        JOIN totals AS totals_b ON totals_b.workspace_id = pairs.b
        LEFT JOIN shared ON shared.a = pairs.a AND shared.b = pairs.b
        "#,
        reachable = REACHABLE_AUDIENCE_EMAILS
            .replace("{workspace}", "ANY($1)")
            .replace("{consent}", &consent)
    ))
    .bind(workspace_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(a, b, total_a, total_b, shared)| {
            Some(AudienceOverlap {
                workspace_a: a,
                workspace_b: b,
                reachable_a: u32::try_from(total_a).ok()?,
                reachable_b: u32::try_from(total_b).ok()?,
                shared: u32::try_from(shared).ok()?,
            })
        })
        .collect())
}
