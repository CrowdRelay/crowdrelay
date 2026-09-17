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

/// Every (city, fan) pair that satisfies all four reachability conditions —
/// the rows behind every number this module produces.
///
/// One definition, two readers: the per-city count and the per-city overlap
/// between two acts. The overlap used to run over a different predicate
/// entirely — the union across all cities — which measured a national share
/// and then had it applied to one city's bill. Two acts can share 5% of their
/// audiences nationally and 90% in the one city where both are local.
///
/// `{workspace}` and `{cities}` are substituted with the caller's own bind
/// expressions, so one act and a whole roster read the same gate.
///
/// `normalized_email` is the cross-workspace identity — the same person
/// following two acts in one organisation is two fan rows with one address.
/// Only counts ever leave: a roster may learn that two acts share 40% of a
/// city, never which people (4G.3b).
const REACHABLE_ROWS: &str = r#"
    SELECT target.id AS city_id,
           fan.workspace_id AS workspace_id,
           fan.id AS fan_id,
           fan.normalized_email AS normalized_email
    FROM cities AS target
    JOIN fan_location_preferences AS preferences
      ON preferences.workspace_id = {workspace}
     AND preferences.nearby_gigs_enabled
    JOIN cities AS fan_city
      ON fan_city.id = preferences.city_id
     AND fan_city.latitude IS NOT NULL
     AND fan_city.longitude IS NOT NULL
    JOIN fans AS fan
      ON fan.workspace_id = preferences.workspace_id
     AND fan.id = preferences.fan_id
     AND fan.status = 'active'
     AND {consent}
    WHERE {cities}
      AND target.latitude IS NOT NULL
      AND target.longitude IS NOT NULL
      -- The emitter's own pre-filter: one degree of latitude is 111.19 km
      -- wherever you stand, so a pair further apart than the radius in
      -- latitude alone can never be inside it. Kept because dropping it
      -- changes the plan from an index scan to a full haversine over every
      -- preference row.
      AND abs(fan_city.latitude - target.latitude)
          <= (preferences.radius_km + 1)::double precision / 111.0
      -- Rounded, matching the nearby-gig emitter exactly: a fan at 50.4 km
      -- with a 50 km radius is paged, so they are reachable. An unrounded
      -- comparison here would promise a promoter people the system will never
      -- actually notify.
      AND ROUND(6371 * 2 * ASIN(LEAST(1.0, SQRT(
            POWER(SIN(RADIANS(fan_city.latitude - target.latitude) / 2), 2)
            + COS(RADIANS(target.latitude)) * COS(RADIANS(fan_city.latitude))
            * POWER(SIN(RADIANS(fan_city.longitude - target.longitude) / 2), 2)
          ))))::integer <= preferences.radius_km
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
    let value = sqlx::query_scalar::<_, Option<i64>>(&format!(
        r#"
        WITH reachable AS ({rows})
        -- Measurable is a property of the city, not of the count: a city that
        -- is not in the catalogue or carries no coordinates answers NULL, and
        -- a city that exists with nobody inside anybody's radius answers a
        -- real zero. Reading the empty row set as zero would erase that.
        SELECT CASE WHEN NOT EXISTS (
                        SELECT 1 FROM cities
                        WHERE id = $2
                          AND latitude IS NOT NULL
                          AND longitude IS NOT NULL
                    ) THEN NULL
                    ELSE (SELECT count(DISTINCT fan_id) FROM reachable)
               END::bigint
        "#,
        rows = REACHABLE_ROWS
            .replace("{workspace}", "$1")
            .replace("{cities}", "target.id = $2")
            .replace("{consent}", &LATEST_MARKETING_GRANT.replace("{fan}", "fan"))
    ))
    .bind(workspace_id)
    .bind(city_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(value.and_then(|count| u32::try_from(count).ok()))
}

/// One unordered pair of workspaces, in one city, and what their audiences
/// share there.
///
/// `shared` is the count of consented, reachable email addresses present in
/// both — the numerator of either direction's share. Each workspace's own
/// reachable total in that city travels with it, because the share of *A's*
/// audience that is also B's is not the share of *B's* that is also A's, and
/// the planner wants the first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudienceOverlap {
    /// The city this was measured in. An overlap is a local fact — the same
    /// two acts share different proportions in different cities, and the bill
    /// is decided in one of them.
    pub city_id: Uuid,
    pub workspace_a: Uuid,
    pub workspace_b: Uuid,
    /// Reachable people `workspace_a` has in this city.
    pub reachable_a: u32,
    /// Reachable people `workspace_b` has in this city.
    pub reachable_b: u32,
    /// People reachable by both, here. A count, never a list.
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

/// Audience overlap between every pair of the given workspaces, **per city**,
/// computed in one pass (4G.3b).
///
/// A roster's acts live in sibling workspaces of one organisation, and the
/// same person following two of them is two fan rows with one
/// `normalized_email` — that address is the only join key that means "same
/// human" across the boundary. The intersection happens inside Postgres and
/// only counts return: a roster may learn that two acts share 40% of a city,
/// never which people.
///
/// Measured per city because that is where the planner spends it. A national
/// share applied to one city's bill is the mistake the ceiling exists to
/// prevent, wearing the ceiling's own clothes: two acts who are both local to
/// one city overlap there far more than their catalogues suggest.
///
/// Pairs where both sides reach somebody in a city are present even when they
/// share nobody — a measured zero is the pairing the planner *should* propose,
/// which is exactly what an absent row would forbid. A side that reaches
/// nobody in that city is absent, because a share of nothing is undefined, and
/// the planner reads undefined as unmeasured rather than as separate.
///
/// # Errors
///
/// Propagates the database error.
pub async fn audience_overlaps_by_city(
    pool: &PgPool,
    workspace_ids: &[Uuid],
    city_ids: &[Uuid],
) -> Result<Vec<AudienceOverlap>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid, i64, i64, i64)>(&format!(
        r#"
        WITH reachable AS ({rows}),
             totals AS (
                 SELECT city_id, workspace_id,
                        count(DISTINCT normalized_email)::bigint AS n
                 FROM reachable GROUP BY city_id, workspace_id
             ),
             pairs AS (
                 SELECT a.city_id, a.workspace_id AS a, b.workspace_id AS b,
                        a.n AS n_a, b.n AS n_b
                 FROM totals AS a
                 JOIN totals AS b
                   ON b.city_id = a.city_id
                  AND a.workspace_id < b.workspace_id
             ),
             shared AS (
                 SELECT a.city_id, a.workspace_id AS a, b.workspace_id AS b,
                        count(DISTINCT a.normalized_email)::bigint AS n
                 FROM reachable AS a
                 JOIN reachable AS b
                   ON b.city_id = a.city_id
                  AND b.normalized_email = a.normalized_email
                  AND a.workspace_id < b.workspace_id
                 GROUP BY a.city_id, a.workspace_id, b.workspace_id
             )
        SELECT pairs.city_id, pairs.a, pairs.b, pairs.n_a, pairs.n_b,
               COALESCE(shared.n, 0)
        FROM pairs
        LEFT JOIN shared
          ON shared.city_id = pairs.city_id
         AND shared.a = pairs.a
         AND shared.b = pairs.b
        "#,
        rows = REACHABLE_ROWS
            .replace("{workspace}", "ANY($1)")
            .replace("{cities}", "target.id = ANY($2)")
            .replace("{consent}", &LATEST_MARKETING_GRANT.replace("{fan}", "fan"))
    ))
    .bind(workspace_ids)
    .bind(city_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(city_id, a, b, total_a, total_b, shared)| {
            Some(AudienceOverlap {
                city_id,
                workspace_a: a,
                workspace_b: b,
                reachable_a: u32::try_from(total_a).ok()?,
                reachable_b: u32::try_from(total_b).ok()?,
                shared: u32::try_from(shared).ok()?,
            })
        })
        .collect())
}
