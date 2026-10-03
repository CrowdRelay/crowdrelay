//! Where this tenant's fans say they came from, split by what the system can
//! and cannot prove.
//!
//! `fan_sources` answers one question: which fans did *CrowdRelay's own actions*
//! bring. On 2026-10-03 that was none of the 20, and every one of them read
//! `public_signup` — so nothing could say which of the band's own channels
//! (its site, Instagram, Facebook, a bio link) the real growth came through,
//! and the brain had no signal about the only growth that exists. Signups carry
//! their campaign tags in `fan_ad_attribution` (`utm_source` and friends) when
//! the join came through a tagged link; this reads them next to behaviour.
//!
//! It claims no more than the rows say. A fan with no tag is `(untagged)`, not
//! guessed into a channel, and `system_attributed` counts only fans a conversion
//! row ties to an action — the same rule the North Star uses.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// One tag value and how its joins have behaved.
#[derive(Debug, Eq, PartialEq, Serialize, FromRow)]
pub struct SignupChannel {
    /// `utm_source`, or `(untagged)` when the join carried none.
    pub source: String,
    /// `utm_medium`; empty when absent.
    pub medium: String,
    /// Active fans who joined inside the window with this tag.
    pub joined: i64,
    /// Of them, how many opened Signal in the last 30 days.
    pub opened_signal_30d: i64,
    /// Of them, how many a conversion row ties to a CrowdRelay action.
    pub system_attributed: i64,
}

/// The window is bounded: a readout over all time would let one old campaign
/// speak for the present.
pub const MAX_WINDOW_DAYS: i32 = 365;

/// Active fans who joined in the last `days` days, grouped by their signup tag.
///
/// # Errors
///
/// Propagates the database error.
pub async fn signup_channels(
    pool: &PgPool,
    workspace_id: Uuid,
    days: i32,
) -> Result<Vec<SignupChannel>, sqlx::Error> {
    sqlx::query_as::<_, SignupChannel>(
        r#"
        SELECT COALESCE(NULLIF(btrim(attribution.utm_source), ''), '(untagged)') AS source,
               COALESCE(btrim(attribution.utm_medium), '') AS medium,
               count(*)::bigint AS joined,
               count(*) FILTER (WHERE EXISTS (
                   SELECT 1 FROM fan_sessions session
                    WHERE session.workspace_id = fan.workspace_id
                      AND session.fan_id = fan.id
                      AND session.last_seen_at > now() - interval '30 days'
               ))::bigint AS opened_signal_30d,
               count(*) FILTER (WHERE EXISTS (
                   SELECT 1 FROM fan_provenance_events event
                    WHERE event.workspace_id = fan.workspace_id
                      AND event.fan_id = fan.id
                      AND event.event_kind = 'conversion'
                      AND event.action_id IS NOT NULL
               ))::bigint AS system_attributed
        FROM fans AS fan
        LEFT JOIN fan_ad_attribution AS attribution
               ON attribution.workspace_id = fan.workspace_id
              AND attribution.fan_id = fan.id
        WHERE fan.workspace_id = $1
          AND fan.status = 'active'
          AND fan.deleted_at IS NULL
          AND fan.merged_into_fan_id IS NULL
          AND fan.created_at > now() - make_interval(days => $2)
        GROUP BY 1, 2
        ORDER BY joined DESC, source, medium
        "#,
    )
    .bind(workspace_id)
    .bind(days.clamp(1, MAX_WINDOW_DAYS))
    .fetch_all(pool)
    .await
}
