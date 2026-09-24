//! What a show's scan produced — counts, never people.
//!
//! The timeline surface counts the room and must never return who is in it;
//! its contract bans fan identity from the timeline module outright. The
//! join that tells a new fan from a returning one has to touch fan identity,
//! so it lives here, beside the concert-QR writes that create those fans, and
//! crosses to the surface as [`RoomSplit`] — a type that can only hold counts.
//! A later edit to the timeline cannot leak the room through a struct that
//! has nowhere to put a person.

use sqlx::PgPool;
use uuid::Uuid;

/// One show's check-ins, split by what they produced.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, sqlx::FromRow)]
pub struct RoomSplit {
    /// Check-ins that created the fan — the room's actual aggregation. The
    /// rest were people the band already had, scanning again.
    pub new_fans: i64,
    /// Of those, the ones reachable now: `fan_activation_kpi`'s own
    /// `reachable_consented` rule — active, with their latest marketing
    /// consent granted. §4e-7 #1: a scan that yields nobody we can contact
    /// is a vanity number.
    pub new_reachable: i64,
    /// Of those, the ones who gave an address and never confirmed it. The
    /// follow-up is what turns these into reachable fans, so this is the
    /// number that says whether it is working.
    pub new_unconfirmed: i64,
}

/// Splits one event's check-ins, scoped to the workspace.
///
/// A scan that created its fan leaves a `concert_qr` conversion on that
/// fan's provenance, tagged with the campaign it came through
/// (`record_fan_arrival`). An existing fan scanning again leaves none, and a
/// fan another night's door created carries that door's campaign — so
/// matching on the check-in's own campaign counts only this room.
pub async fn room_split(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
) -> Result<RoomSplit, sqlx::Error> {
    sqlx::query_as::<_, RoomSplit>(
        r#"
        WITH room_arrival AS (
            SELECT fan.status, coalesce(consent.granted, false) AS consented
            FROM concert_checkins AS checkin
            JOIN fans AS fan
              ON fan.workspace_id = checkin.workspace_id
             AND fan.id = checkin.fan_id
            LEFT JOIN LATERAL (
                SELECT granted FROM fan_consents AS c
                WHERE c.workspace_id = fan.workspace_id
                  AND c.fan_id = fan.id
                  AND c.purpose = 'marketing'
                ORDER BY c.recorded_at DESC, c.id DESC
                LIMIT 1
            ) AS consent ON true
            WHERE checkin.workspace_id = $1
              AND checkin.event_id = $2
              AND EXISTS (
                  SELECT 1 FROM fan_provenance_events AS arrival
                  WHERE arrival.workspace_id = checkin.workspace_id
                    AND arrival.fan_id = checkin.fan_id
                    AND arrival.event_kind = 'conversion'
                    AND arrival.channel = 'concert_qr'
                    AND arrival.campaign_id = checkin.campaign_id
              )
        )
        SELECT
            count(*)::bigint AS new_fans,
            count(*) FILTER (WHERE status = 'active' AND consented)::bigint AS new_reachable,
            count(*) FILTER (WHERE status = 'pending')::bigint AS new_unconfirmed
        FROM room_arrival
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(pool)
    .await
}
