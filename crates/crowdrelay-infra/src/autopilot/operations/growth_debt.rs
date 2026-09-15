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
use crate::tenant_settings::TenantSettingsRepository;
use crowdrelay_domain::{
    BeaconId, BookingTargetId, EventId, OutreachTargetId, ReleasePlanId,
    content_supply::{
        ContentSupplyDecision, ContentSupplyHoldReason, ContentSupplyPolicy,
        evaluate_content_supply,
    },
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
/// on `viryaos_release_milestones` (migration 0039, widened to nine by
/// `editorial_pitch` in 0100). The denominator is the declared set, not the
/// recorded rows — otherwise a plan that recorded one milestone and stopped
/// would report as 0% outstanding.
const RELEASE_MILESTONE_COUNT: i64 = 9;

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
        "workspace" => Some(GrowthDebtSubject::Workspace(WorkspaceId::from_uuid(
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
                -- The denominator is the milestones this plan can actually
                -- owe: a no-press plan never records start_press, so counting
                -- it as debt would report a deliberate switch as neglect.
                GREATEST(
                    0,
                    $3::bigint
                        - CASE WHEN plan.press_enabled THEN 0 ELSE 1 END
                        - COALESCE(recorded.completed, 0)
                )::bigint AS outstanding_items,
                ($3::bigint
                    - CASE WHEN plan.press_enabled THEN 0 ELSE 1 END)::bigint
                    AS tracked_items,
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
              -- A filler plan owes no vertical — the tier is the band's call,
              -- not nine missed milestones.
              AND plan.tier <> 'filler'
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
                    completed.id AS event_id,
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
                GROUP BY completed.id, orders.paid_at
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

    let mut observations: Vec<GrowthDebtObservation> = rows
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
        .collect();

    // §4i-0c: the cadence's other half. The supply evaluator already schedules
    // fillers from what the workspace holds; this is the boundary where the
    // machine asks the band instead — fillers are on and no video, story, or
    // show-harvest source can still produce an artifact. The shelf is a
    // property of the whole inventory, so the workspace itself is the subject.
    let cadence = TenantSettingsRepository::new(repo.pool().clone())
        .cadence_settings(workspace)
        .await
        .map_err(map_sqlx)?;
    if cadence.fillers_enabled {
        let supply_policy = load_content_supply_policy(repo, workspace).await?;
        let supply = load_content_supply_snapshots(repo, workspace_id, now).await?;
        if !filler_shelf_stocked(&supply, supply_policy, now) {
            observations.push(GrowthDebtObservation {
                kind: GrowthDebtKind::FillerShelfEmpty,
                subject: GrowthDebtSubject::Workspace(workspace_id),
                idle_hours: 0,
                outstanding_items: 1,
                tracked_items: 1,
                relationship_score: None,
                hours_until_deadline: None,
                hours_since_last_signal: last_signal_at
                    .get(&(
                        workspace,
                        GrowthDebtKind::FillerShelfEmpty.decision_kind().to_owned(),
                    ))
                    .map(|at| u32::try_from((now - *at).whole_hours().max(0)).unwrap_or(u32::MAX)),
            });
        }
    }

    // §4i-0d: the same mechanism pointed at the cadence commitment itself.
    // One missed interval is information; two in a row with nothing scheduled
    // is the signal — and it reports, it does not scold. The content-source
    // projection is the serious-moment register: events, releases, and videos
    // all land there with the moment's own timestamp, so the count is a fact
    // about the record rather than an inference about intent.
    let interval_hours = 730_u64 / u64::from(cadence.serious_moments_per_month);
    let (moments, workspace_created_at) = load_moment_register(repo, workspace).await?;
    if cadence_moment_missed(&moments, workspace_created_at, interval_hours, now) {
        observations.push(GrowthDebtObservation {
            kind: GrowthDebtKind::CadenceMomentMissed,
            subject: GrowthDebtSubject::Workspace(workspace_id),
            idle_hours: 0,
            outstanding_items: 1,
            tracked_items: 1,
            relationship_score: None,
            hours_until_deadline: None,
            hours_since_last_signal: last_signal_at
                .get(&(
                    workspace,
                    GrowthDebtKind::CadenceMomentMissed
                        .decision_kind()
                        .to_owned(),
                ))
                .map(|at| u32::try_from((now - *at).whole_hours().max(0)).unwrap_or(u32::MAX)),
        });
    }

    Ok(observations)
}

/// The workspace's serious-moment timestamps plus its own creation time —
/// the two facts the slippage rule needs. Moment kinds are the §4i-0c serious
/// classes only: `event`, `release`, `video`. Stories and harvest are filler
/// material, `show_completed` would double-count a night the event row
/// already represents, and a `filler`-tier release is cadence *output*, not
/// a serious moment — counting it would let demos satisfy the very
/// commitment they exist to cover for. `occurred_at` runs both ways — a
/// published future show is a scheduled moment, a past one a held moment.
async fn load_moment_register(
    repo: &PostgresAutopilotRepository,
    workspace: Uuid,
) -> Result<(Vec<OffsetDateTime>, Option<OffsetDateTime>), RepositoryError> {
    let moments = sqlx::query_scalar::<_, OffsetDateTime>(
        r#"
        SELECT occurred_at
        FROM viryaos_content_sources
        WHERE workspace_id = $1
          AND active
          AND source_kind IN ('event','release','video')
          AND (
              source_kind <> 'release'
              OR COALESCE(metadata->>'tier', '') <> 'filler'
          )
        "#,
    )
    .bind(workspace)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let workspace_created_at =
        sqlx::query_scalar::<_, OffsetDateTime>("SELECT created_at FROM workspaces WHERE id = $1")
            .bind(workspace)
            .fetch_optional(&repo.pool)
            .await
            .map_err(map_sqlx)?;
    Ok((moments, workspace_created_at))
}

/// True when the cadence has slipped twice running. The rule needs two facts
/// beyond the count itself: the tenant has to be old enough to have had an
/// interval to miss (a workspace younger than one interval gets the benefit
/// of the doubt, not a finding), and a moment already scheduled inside the
/// next half-interval means the rhythm is recovering on its own — reporting
/// debt that resolves itself is how an operator learns to ignore the queue.
/// `interval_hours` is 730 divided by the committed moments per month — the
/// average month in hours, integer math like the rest of the domain.
fn cadence_moment_missed(
    moments: &[OffsetDateTime],
    workspace_created_at: Option<OffsetDateTime>,
    interval_hours: u64,
    now: OffsetDateTime,
) -> bool {
    let interval = time::Duration::hours(i64::try_from(interval_hours).unwrap_or(i64::MAX));
    if workspace_created_at.is_none_or(|created| now - created < interval) {
        return false;
    }
    let recent = moments.iter().any(|at| *at <= now && *at > now - interval);
    let prior = moments
        .iter()
        .any(|at| *at <= now - interval && *at > now - interval * 2);
    let recovering = moments
        .iter()
        .any(|at| *at > now && *at <= now + interval / 2);
    !(recent || prior) && !recovering
}

/// The workspace's content-supply policy for the shelf check — the same row
/// the supply context itself evaluates under, so "still produces" means the
/// same thing here as it does where the artifacts are actually requested.
/// Missing or unreadable config resolves to the shipped defaults, matching
/// how the policy reader treats a workspace nobody has tuned.
async fn load_content_supply_policy(
    repo: &PostgresAutopilotRepository,
    workspace: Uuid,
) -> Result<ContentSupplyPolicy, RepositoryError> {
    let raw = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT config FROM viryaos_autopilot_policies \
         WHERE workspace_id = $1 AND context = 'content_supply'",
    )
    .bind(workspace)
    .fetch_optional(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let Some(config) = raw else {
        return Ok(ContentSupplyPolicy::default());
    };
    match serde_json::from_value::<ContentSupplyPolicy>(config) {
        Ok(policy) => Ok(policy),
        Err(error) => {
            // The defaults are safe, so the shelf check still runs. What is
            // not safe is letting an operator believe a tuned policy applies
            // when it never parsed — same contract as the growth_metrics
            // reader this mirrors.
            tracing::error!(
                %error,
                workspace_id = %workspace,
                "stored content_supply policy is unreadable; falling back to defaults"
            );
            Ok(ContentSupplyPolicy::default())
        }
    }
}

/// True while any filler-kind source can still produce an artifact. A source
/// counts as stock while the supply evaluator would request an artifact from
/// it — and while its harvest window is open, because material for a pending
/// harvest is committed, not missing. `Complete`, stale, and invalid sources
/// are what an empty shelf is made of. One deliberate edge: a source whose
/// remaining artifacts are all in-flight reads `Complete` to the evaluator,
/// so the ask can fire one approval cycle before the shelf is literally
/// bare — restocking takes the band longer than that anyway.
///
/// The kind list is the §4i-0c filler inventory: evergreen video and story
/// material plus show harvest. Events and releases are serious moments, not
/// filler stock — a month of moments with no filler behind them is exactly
/// the gap the cadence exists to fill.
fn filler_shelf_stocked(
    snapshots: &[crowdrelay_domain::content_supply::ContentSupplySnapshot],
    policy: ContentSupplyPolicy,
    now: OffsetDateTime,
) -> bool {
    snapshots.iter().any(|snapshot| {
        matches!(
            snapshot.source_kind,
            ContentSourceKind::Video | ContentSourceKind::Story | ContentSourceKind::ShowCompleted
        ) && matches!(
            evaluate_content_supply(snapshot, policy, now),
            ContentSupplyDecision::Request { .. }
                | ContentSupplyDecision::Hold(ContentSupplyHoldReason::HarvestPending)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::ContentSourceId;
    use time::Duration;

    fn snapshot(
        kind: ContentSourceKind,
        completed: Vec<ContentArtifactKind>,
    ) -> crowdrelay_domain::content_supply::ContentSupplySnapshot {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        crowdrelay_domain::content_supply::ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: kind,
            source_version: 1,
            occurred_at: now - Duration::days(10),
            expires_at: now + Duration::days(30),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: completed,
            in_flight_artifacts: Vec::new(),
        }
    }

    #[test]
    fn an_untouched_video_is_shelf_stock() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let supply = vec![snapshot(ContentSourceKind::Video, Vec::new())];
        assert!(filler_shelf_stocked(
            &supply,
            ContentSupplyPolicy::default(),
            now
        ));
    }

    #[test]
    fn a_source_with_every_artifact_done_is_not_stock() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let exhausted = snapshot(
            ContentSourceKind::Story,
            vec![
                ContentArtifactKind::SocialFeed,
                ContentArtifactKind::SocialStory,
            ],
        );
        assert!(!filler_shelf_stocked(
            &[exhausted],
            ContentSupplyPolicy::default(),
            now
        ));
    }

    #[test]
    fn a_pending_harvest_still_counts_as_stock() {
        // Twenty hours after the show the harvest window is still open — the
        // material is committed, so the shelf is not empty even though the
        // evaluator would not draft from it yet.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let mut fresh_show = snapshot(ContentSourceKind::ShowCompleted, Vec::new());
        fresh_show.occurred_at = now - Duration::hours(20);
        assert!(filler_shelf_stocked(
            &[fresh_show],
            ContentSupplyPolicy::default(),
            now
        ));
    }

    #[test]
    fn moments_are_not_filler_stock() {
        // A show and a release can each have plenty of unproduced artifacts
        // and still leave the filler shelf empty — they are the serious
        // moments the fillers are supposed to sit between.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let supply = vec![
            snapshot(ContentSourceKind::Event, Vec::new()),
            snapshot(ContentSourceKind::Release, Vec::new()),
        ];
        assert!(!filler_shelf_stocked(
            &supply,
            ContentSupplyPolicy::default(),
            now
        ));
    }

    #[test]
    fn an_empty_inventory_is_an_empty_shelf() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        assert!(!filler_shelf_stocked(
            &[],
            ContentSupplyPolicy::default(),
            now
        ));
    }

    fn days_ago(now: OffsetDateTime, days: i64) -> OffsetDateTime {
        now - Duration::days(days)
    }

    #[test]
    fn two_empty_intervals_with_nothing_scheduled_is_a_missed_cadence() {
        // One moment a month: interval ≈ 730h ≈ 30.4d. The last held moment
        // was 80 days ago — the last two 30-day intervals are both empty and
        // the schedule ahead is bare.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let created = Some(days_ago(now, 400));
        let moments = vec![days_ago(now, 80)];
        assert!(cadence_moment_missed(&moments, created, 730, now));
    }

    #[test]
    fn one_empty_interval_is_information_not_debt() {
        // A moment 20 days ago fills the current interval even though the
        // one before it was dry — that is a recovered rhythm, not slippage.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let created = Some(days_ago(now, 400));
        let moments = vec![days_ago(now, 20), days_ago(now, 100)];
        assert!(!cadence_moment_missed(&moments, created, 730, now));
    }

    #[test]
    fn a_scheduled_moment_means_the_rhythm_is_recovering() {
        // Two dry intervals behind, but a show is announced inside the next
        // half-interval — the miss resolves itself without an ask.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let created = Some(days_ago(now, 400));
        let moments = vec![now + Duration::days(10)];
        assert!(!cadence_moment_missed(&moments, created, 730, now));
        // The same moment placed beyond the half-interval does not count as
        // recovery — "eventually" is not a cadence.
        let far = vec![now + Duration::days(20)];
        assert!(cadence_moment_missed(&far, created, 730, now));
    }

    #[test]
    fn a_young_workspace_cannot_have_missed_a_rhythm_yet() {
        // Twenty days old at a monthly cadence: the record is empty but the
        // tenant has not had a full interval to miss.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let created = Some(days_ago(now, 20));
        assert!(!cadence_moment_missed(&[], created, 730, now));
    }

    #[test]
    fn no_creation_fact_means_no_claim() {
        // Without the workspace's age the two-empty-intervals rule cannot be
        // evaluated honestly, so the detector stays quiet.
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        assert!(!cadence_moment_missed(&[], None, 730, now));
    }
}
