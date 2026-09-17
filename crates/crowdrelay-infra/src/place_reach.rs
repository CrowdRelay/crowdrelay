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

/// Reachable, consented people for one city slug.
///
/// `Ok(None)` means the city is not in the catalogue or has no coordinates —
/// unmeasurable, which is not the same as nobody being there. Callers must
/// carry that distinction rather than folding it to zero.
///
/// # Errors
///
/// Propagates the database error.
pub async fn reachable_in_city(
    pool: &PgPool,
    workspace_id: Uuid,
    city_slug: &str,
) -> Result<Option<u32>, sqlx::Error> {
    let value = sqlx::query_scalar::<_, Option<i64>>(
        r#"
        SELECT count(DISTINCT fan.id)::bigint
        FROM cities AS target
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
        WHERE target.slug = $2
          AND target.latitude IS NOT NULL
          AND target.longitude IS NOT NULL
          AND EXISTS (
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
        "#,
    )
    .bind(workspace_id)
    .bind(city_slug)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(value.and_then(|count| u32::try_from(count).ok()))
}
