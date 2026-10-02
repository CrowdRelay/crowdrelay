//! The booking pipeline's supply snapshot, split out of `snapshots.rs`.

use super::*;
use crate::autopilot::bounded_u32;
use crowdrelay_domain::booking_discovery::BookingSupplySnapshot;

/// The booking pipeline's supply: contactable targets today, plus the cooldown
/// clock on the last discovery request. Bounded like every snapshot read; the
/// domain decides whether any of it is worth an action.
pub(in crate::autopilot) async fn load_booking_supply_snapshot(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<BookingSupplySnapshot, RepositoryError> {
    repo.bounded(async {
        let row = sqlx::query_as::<_, (i64, Option<i64>)>(
            r#"
            SELECT
                (
                    SELECT count(*)::bigint
                    FROM booking_targets AS target
                    WHERE target.workspace_id = $1
                      AND target.active
                      AND target.accepts_booking
                      -- Supply means somebody we could write to this week —
                      -- a venue-kind target whose room is on record as
                      -- closed is not that, and counting it would suppress
                      -- the discovery ask the dry pipeline needs. The
                      -- resolved status decides; the linked set is the
                      -- primary link plus the venue edges, as in
                      -- `booking_reads`.
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
                ),
                (
                    SELECT CASE
                        WHEN max(d.evaluated_at) IS NULL THEN NULL
                        ELSE GREATEST(
                            0,
                            FLOOR(EXTRACT(EPOCH FROM ($2 - max(d.evaluated_at))) / 3600)
                        )::bigint
                    END
                    FROM autopilot_decisions AS d
                    WHERE d.workspace_id = $1
                      AND d.context = 'booking_opportunity'
                      AND d.decision_kind = 'request_booking_target_discovery'
                )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .fetch_one(&repo.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(BookingSupplySnapshot {
            active_eligible_targets: bounded_u32(row.0)?,
            hours_since_last_request: row.1.and_then(|hours| u32::try_from(hours).ok()),
        })
    })
    .await
}
