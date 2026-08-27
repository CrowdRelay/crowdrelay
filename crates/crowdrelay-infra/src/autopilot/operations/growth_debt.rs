//! One set-oriented observation query for growth debt.
//!
//! Three debt kinds, one `UNION ALL`, one round trip. Each branch reports the
//! same four facts — how long the outstanding work has been untouched, how much
//! of it is outstanding, how much was tracked, and what date applies — because
//! every horizon and threshold lives in `GrowthDebtPolicy`. Nothing here decides
//! whether the neglect is worth raising.
//!
//! Two things this query deliberately does not do. It does not compute a ratio
//! or a priority: those are the domain's, and a SQL copy would drift from it.
//! And it does not fabricate a clock — a subject with no interaction history at
//! all is dated from its own `created_at`, which is a fact, rather than from an
//! assumed zero.

use super::*;
use crowdrelay_domain::{
    BeaconId, BookingTargetId, EventId, OutreachTargetId, ReleasePlanId,
    growth_debt::{GrowthDebtKind, GrowthDebtObservation, GrowthDebtSubject},
};
use std::collections::HashMap;

#[derive(Debug, FromRow)]
struct GrowthDebtRow {
    debt_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    idle_hours: i64,
    outstanding_items: i64,
    tracked_items: i64,
    relationship_score: Option<i32>,
    hours_until_deadline: Option<i64>,
}

#[derive(Debug, FromRow)]
struct LastDebtSignalRow {
    subject_id: Uuid,
    decision_kind: String,
    evaluated_at: OffsetDateTime,
}

/// Milestones a release plan is expected to record, from the CHECK constraint
/// on `viryaos_release_milestones` (migration 0039). The denominator is the
/// declared set, not the recorded rows — otherwise a plan that recorded one
/// milestone and stopped would report as 0% outstanding.
const RELEASE_MILESTONE_COUNT: i64 = 8;

fn subject_of(row: &GrowthDebtRow) -> Option<GrowthDebtSubject> {
    match row.subject_kind.as_str() {
        "booking_target" => Some(GrowthDebtSubject::BookingTarget(
            BookingTargetId::from_uuid(row.subject_id),
        )),
        "outreach_target" => Some(GrowthDebtSubject::OutreachTarget(
            OutreachTargetId::from_uuid(row.subject_id),
        )),
        "beacon" => Some(GrowthDebtSubject::Beacon(BeaconId::from_uuid(
            row.subject_id,
        ))),
        "event" => Some(GrowthDebtSubject::Event(EventId::from_uuid(row.subject_id))),
        "release_plan" => Some(GrowthDebtSubject::ReleasePlan(ReleasePlanId::from_uuid(
            row.subject_id,
        ))),
        _ => None,
    }
}

pub(in crate::autopilot) async fn load_growth_debt_observations(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<GrowthDebtObservation>, RepositoryError> {
    let workspace = workspace_id.into_uuid();

    let rows = sqlx::query_as::<_, GrowthDebtRow>(
        r#"
        WITH quiet_relationships AS (
            SELECT
                'relationship_quiet' AS debt_kind,
                'booking_target' AS subject_kind,
                target.id AS subject_id,
                -- GREATEST over the contact timestamps only, then COALESCE to
                -- `created_at`. Putting `created_at` inside the GREATEST makes
                -- it a ceiling on idleness rather than a floor: a row created
                -- today with an outreach timestamp from last year reads as
                -- touched today, and no relationship is ever quiet. GREATEST
                -- ignores NULLs and returns NULL only when all of them are,
                -- which is exactly when the fallback should apply.
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - COALESCE(
                            GREATEST(touch.last_interaction_at, target.last_outreach_at),
                            target.created_at
                        )
                    )) / 3600)
                )::bigint AS idle_hours,
                1::bigint AS outstanding_items,
                1::bigint AS tracked_items,
                target.relationship_score,
                NULL::bigint AS hours_until_deadline
            FROM viryaos_booking_targets AS target
            LEFT JOIN LATERAL (
                SELECT max(interaction.occurred_at) AS last_interaction_at
                FROM viryaos_booking_interactions AS interaction
                WHERE interaction.workspace_id = target.workspace_id
                  AND interaction.target_id = target.id
            ) AS touch ON true
            WHERE target.workspace_id = $1
              AND target.active
              AND target.accepts_booking

            UNION ALL

            SELECT
                'relationship_quiet',
                'outreach_target',
                target.id,
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - COALESCE(
                            GREATEST(
                                touch.last_interaction_at,
                                target.last_outreach_at,
                                target.last_reply_at
                            ),
                            target.created_at
                        )
                    )) / 3600)
                )::bigint,
                1::bigint,
                1::bigint,
                target.relationship_score,
                NULL::bigint
            FROM viryaos_outreach_targets AS target
            LEFT JOIN LATERAL (
                SELECT max(interaction.occurred_at) AS last_interaction_at
                FROM viryaos_outreach_interactions AS interaction
                WHERE interaction.workspace_id = target.workspace_id
                  AND interaction.target_id = target.id
            ) AS touch ON true
            WHERE target.workspace_id = $1
              AND target.active
              AND target.accepts_outreach
              AND NOT target.do_not_contact
        ),
        skipped_levers AS (
            SELECT
                'event_levers_skipped' AS debt_kind,
                'event' AS subject_kind,
                surface.event_id AS subject_id,
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - min(COALESCE(surface.last_checked_at, surface.updated_at))
                    )) / 3600)
                )::bigint AS idle_hours,
                count(*) FILTER (
                    WHERE surface.status IN ('unknown','ready','manual','blocked')
                )::bigint AS outstanding_items,
                count(*)::bigint AS tracked_items,
                NULL::integer AS relationship_score,
                FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 3600)::bigint
                    AS hours_until_deadline
            FROM viryaos_show_growth_surfaces AS surface
            JOIN events AS event
              ON event.workspace_id = surface.workspace_id
             AND event.id = surface.event_id
            WHERE surface.workspace_id = $1
              AND event.status = 'published'
              AND event.starts_at > $2
              -- 'skipped' and 'retired' are decisions somebody made. Counting
              -- them as debt would report deliberate choices as neglect.
              AND surface.status <> 'skipped'
              AND surface.status <> 'retired'
            GROUP BY surface.event_id, event.starts_at
        ),
        missed_milestones AS (
            SELECT
                'release_milestones_missed' AS debt_kind,
                'release_plan' AS subject_kind,
                plan.id AS subject_id,
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - COALESCE(recorded.last_completed_at, plan.created_at)
                    )) / 3600)
                )::bigint AS idle_hours,
                GREATEST(0, $3::bigint - COALESCE(recorded.completed, 0))::bigint
                    AS outstanding_items,
                $3::bigint AS tracked_items,
                NULL::integer AS relationship_score,
                FLOOR(EXTRACT(EPOCH FROM (plan.release_at - $2)) / 3600)::bigint
                    AS hours_until_deadline
            FROM viryaos_release_plans AS plan
            LEFT JOIN LATERAL (
                SELECT
                    count(*)::bigint AS completed,
                    max(milestone.completed_at) AS last_completed_at
                FROM viryaos_release_milestones AS milestone
                WHERE milestone.workspace_id = plan.workspace_id
                  AND milestone.release_id = plan.id
            ) AS recorded ON true
            WHERE plan.workspace_id = $1
              AND plan.active
              AND plan.release_at > $2
        ),
        missing_assets AS (
            SELECT
                'release_assets_missing' AS debt_kind,
                'release_plan' AS subject_kind,
                plan.id AS subject_id,
                -- The idle clock is the plan's own edit time: an operator who
                -- keeps touching the plan is on it, whatever the flags say,
                -- and one who declared it incomplete and walked away is not.
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - plan.updated_at
                    )) / 3600)
                )::bigint AS idle_hours,
                -- The two declarations the operator controls themselves, so
                -- both are facts about our records rather than guesses about
                -- the world.
                (
                    CASE WHEN plan.listen_url IS NULL THEN 1 ELSE 0 END
                    + CASE WHEN NOT plan.assets_ready THEN 1 ELSE 0 END
                )::bigint AS outstanding_items,
                2::bigint AS tracked_items,
                NULL::integer AS relationship_score,
                FLOOR(EXTRACT(EPOCH FROM (plan.release_at - $2)) / 3600)::bigint
                    AS hours_until_deadline
            FROM viryaos_release_plans AS plan
            WHERE plan.workspace_id = $1
              AND plan.active
              AND plan.release_at > $2
              AND (plan.listen_url IS NULL OR NOT plan.assets_ready)
        ),
        stale_contacts AS (
            -- Contact routes that have not been verified within the policy
            -- window. `contact_verified_at` is set only by a reply, a
            -- successful send, or an operator's explicit verification — never
            -- by an edit — so its age is the truest signal of whether the
            -- route still works. NULL means never verified, which reads as
            -- stale from `created_at`.
            SELECT
                'stale_contact_data' AS debt_kind,
                'outreach_target' AS subject_kind,
                target.id AS subject_id,
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - COALESCE(target.contact_verified_at, target.created_at)
                    )) / 3600)
                )::bigint AS idle_hours,
                1::bigint AS outstanding_items,
                1::bigint AS tracked_items,
                target.relationship_score,
                NULL::bigint AS hours_until_deadline
            FROM viryaos_outreach_targets AS target
            WHERE target.workspace_id = $1
              AND target.active
              AND target.accepts_outreach
              AND NOT target.do_not_contact

            UNION ALL

            SELECT
                'stale_contact_data',
                'booking_target',
                target.id,
                GREATEST(
                    0,
                    FLOOR(EXTRACT(EPOCH FROM (
                        $2 - COALESCE(target.contact_verified_at, target.created_at)
                    )) / 3600)
                )::bigint,
                1::bigint,
                1::bigint,
                target.relationship_score,
                NULL::bigint
            FROM viryaos_booking_targets AS target
            WHERE target.workspace_id = $1
              AND target.active
              AND target.accepts_booking
        ),
        calendar_routing_conflicts AS (
            -- Two confirmed shows on consecutive days with an impractical
            -- distance between them. The domain module estimates inter-show
            -- distance as `sum_from_home - shorter_leg` (an upper bound), so
            -- the SQL provides each show's distance from home base. Shows
            -- without a cost ledger entry have no distance and the domain
            -- rule returns `Ok` — no distance, no claim.
            WITH published_with_distance AS (
                SELECT
                    event.id AS event_id,
                    event.starts_at::date AS show_date,
                    cost.distance_km
                FROM events AS event
                LEFT JOIN viryaos_show_cost_ledger AS cost
                  ON cost.workspace_id = event.workspace_id
                 AND cost.event_id = event.id
                WHERE event.workspace_id = $1
                  AND event.status = 'published'
                  AND event.starts_at > $2
            ),
            -- Pair each show with the next show within 2 days.
            show_pairs AS (
                SELECT
                    earlier.event_id AS earlier_event_id,
                    earlier.show_date AS earlier_date,
                    earlier.distance_km AS earlier_distance_km,
                    later.event_id AS later_event_id,
                    later.show_date AS later_date,
                    later.distance_km AS later_distance_km,
                    (later.show_date - earlier.show_date) AS gap_days
                FROM published_with_distance AS earlier
                JOIN published_with_distance AS later
                  ON later.show_date > earlier.show_date
                 AND later.show_date <= earlier.show_date + INTERVAL '2 days'
            )
            SELECT
                'calendar_routing_conflict' AS debt_kind,
                'event' AS subject_kind,
                pairs.later_event_id AS subject_id,
                0::bigint AS idle_hours,
                1::bigint AS outstanding_items,
                1::bigint AS tracked_items,
                NULL::integer AS relationship_score,
                FLOOR(EXTRACT(EPOCH FROM (
                    (SELECT starts_at FROM events WHERE id = pairs.later_event_id) - $2
                )) / 3600)::bigint AS hours_until_deadline
            FROM show_pairs AS pairs
            -- Only raise when both distances are known and the estimated
            -- inter-show distance exceeds the threshold. The domain rule's
            -- upper bound: sum - shorter_leg. Default thresholds: 400 km
            -- for consecutive days, 800 km for 2-day gap.
            WHERE pairs.earlier_distance_km IS NOT NULL
              AND pairs.later_distance_km IS NOT NULL
              AND (
                  (pairs.gap_days = 1
                   AND pairs.earlier_distance_km + pairs.later_distance_km
                       - LEAST(pairs.earlier_distance_km, pairs.later_distance_km) > 400)
                  OR
                  (pairs.gap_days = 2
                   AND pairs.earlier_distance_km + pairs.later_distance_km
                       - LEAST(pairs.earlier_distance_km, pairs.later_distance_km) > 800)
              )
        ),
        ticket_sales_behind_pace AS (
            -- Upcoming shows selling far below the workspace's own historical
            -- pace at the same lead time. The response is owned-audience and
            -- free: a message to consented fans in the show's city.
            --
            -- The historical baseline is the average paid tickets of completed
            -- shows at the same lead time (±3 days). With fewer than 2
            -- matching completed shows, the detector holds — no pace to
            -- compare against.
            WITH completed_pace AS (
                SELECT
                    completed.event_id,
                    GREATEST(
                        0,
                        FLOOR(EXTRACT(EPOCH FROM (
                            completed.starts_at - orders.paid_at
                        )) / 86400)
                    )::bigint AS days_to_event,
                    SUM(item.quantity)::bigint AS paid_tickets
                FROM events AS completed
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = completed.workspace_id
                 AND sale.event_id = completed.id
                 AND sale.active
                JOIN ticket_orders AS orders
                  ON orders.workspace_id = completed.workspace_id
                 AND orders.ticket_sale_id = sale.id
                 AND orders.status IN ('paid', 'partially_refunded')
                JOIN ticket_order_items AS item
                  ON item.workspace_id = orders.workspace_id
                 AND item.ticket_order_id = orders.id
                WHERE completed.workspace_id = $1
                  AND completed.status = 'completed'
                  AND orders.paid_at <= completed.starts_at
                GROUP BY completed.event_id, orders.paid_at
            ),
            upcoming_sales AS (
                SELECT
                    event.id AS event_id,
                    event.starts_at,
                    GREATEST(
                        0,
                        FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 86400)
                    )::bigint AS days_to_event,
                    COALESCE(SUM(item.quantity) FILTER (
                        WHERE orders.status IN ('paid', 'partially_refunded')
                    ), 0)::bigint AS paid_tickets
                FROM events AS event
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = event.workspace_id
                 AND sale.event_id = event.id
                 AND sale.active
                LEFT JOIN ticket_orders AS orders
                  ON orders.workspace_id = event.workspace_id
                 AND orders.ticket_sale_id = sale.id
                LEFT JOIN ticket_order_items AS item
                  ON item.workspace_id = orders.workspace_id
                 AND item.ticket_order_id = orders.id
                WHERE event.workspace_id = $1
                  AND event.status = 'published'
                  AND event.starts_at > $2
                  AND event.starts_at <= $2 + INTERVAL '21 days'
                GROUP BY event.id, event.starts_at
            ),
            pace_comparison AS (
                SELECT
                    upcoming.event_id,
                    upcoming.days_to_event,
                    upcoming.paid_tickets,
                    AVG(history.paid_tickets)::bigint AS historical_average
                FROM upcoming_sales AS upcoming
                JOIN completed_pace AS history
                  ON ABS(history.days_to_event - upcoming.days_to_event) <= 3
                GROUP BY upcoming.event_id, upcoming.days_to_event, upcoming.paid_tickets
                HAVING count(*) >= 2
            )
            SELECT
                'ticket_sales_behind_pace' AS debt_kind,
                'event' AS subject_kind,
                comparison.event_id AS subject_id,
                -- Structural, not time-based: idle_hours is 0 (the horizon is
                -- 0 for this kind). The debt exists the moment sales fall
                -- behind, not after some idle period.
                0::bigint AS idle_hours,
                -- 1 outstanding item: the show needs a reach push.
                1::bigint AS outstanding_items,
                1::bigint AS tracked_items,
                NULL::integer AS relationship_score,
                FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 3600)::bigint
                    AS hours_until_deadline
            FROM pace_comparison AS comparison
            JOIN events AS event
              ON event.workspace_id = $1
             AND event.id = comparison.event_id
            -- Only raise when current sales are below 70% of historical pace.
            WHERE comparison.historical_average > 0
              AND comparison.paid_tickets * 10_000 < comparison.historical_average * 7_000
        )
        SELECT * FROM quiet_relationships
        UNION ALL
        SELECT * FROM skipped_levers
        UNION ALL
        SELECT * FROM missed_milestones
        UNION ALL
        SELECT * FROM missing_assets
        UNION ALL
        SELECT * FROM stale_contacts
        UNION ALL
        SELECT * FROM calendar_routing_conflicts
        UNION ALL
        SELECT * FROM ticket_sales_behind_pace
        ORDER BY idle_hours DESC, subject_id
        LIMIT $4
        "#,
    )
    .bind(workspace)
    .bind(now)
    .bind(RELEASE_MILESTONE_COUNT)
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    // The cooldown is read back per (subject, debt kind): one event can owe both
    // skipped levers and a stalled release plan, and raising one must not
    // silence the other. That is why the decision kind carries the debt kind.
    let last_signals = sqlx::query_as::<_, LastDebtSignalRow>(
        r#"
        SELECT subject_id, decision_kind, max(evaluated_at) AS evaluated_at
        FROM viryaos_autopilot_decisions
        WHERE workspace_id = $1
          AND context = 'growth_debt'
        GROUP BY subject_id, decision_kind
        "#,
    )
    .bind(workspace)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    let last_signal_at: HashMap<(Uuid, String), OffsetDateTime> = last_signals
        .into_iter()
        .map(|row| ((row.subject_id, row.decision_kind), row.evaluated_at))
        .collect();

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let kind = GrowthDebtKind::parse(&row.debt_kind)?;
            let subject = subject_of(&row)?;
            let hours_since_last_signal = last_signal_at
                .get(&(row.subject_id, kind.decision_kind().to_owned()))
                .map(|at| u32::try_from((now - *at).whole_hours().max(0)).unwrap_or(u32::MAX));
            Some(GrowthDebtObservation {
                kind,
                subject,
                idle_hours: u32::try_from(row.idle_hours).unwrap_or(u32::MAX),
                outstanding_items: u32::try_from(row.outstanding_items).unwrap_or(u32::MAX),
                tracked_items: u32::try_from(row.tracked_items).unwrap_or(u32::MAX),
                relationship_score: row
                    .relationship_score
                    .and_then(|score| u8::try_from(score).ok()),
                hours_until_deadline: row.hours_until_deadline,
                hours_since_last_signal,
            })
        })
        .collect())
}
