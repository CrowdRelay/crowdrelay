//! The capture-plan lifecycle — §4b-3's "one shoot → ≥3 sources".
//!
//! Three phases share the team-handoff sweep's transaction:
//!
//! - **Project**: every published gig is also a production day, so the
//!   show ladder's calendar feeds `production_events` without
//!   anyone entering the same date twice.
//! - **Settle**: open plans are judged against what the day actually
//!   produced — content sources logged inside the harvest window — and
//!   land done or abandoned. Runs before roster checks so a memberless
//!   workspace keeps the lifecycle even though it loses the routing.
//! - **Issue**: days landing today or tomorrow get a shot list built
//!   from the formats the open suggestions and the active arc still
//!   need, routed to the member who holds the camera through the same
//!   assignment + notice machinery every human handoff uses.

use super::{
    team::PendingInitialNotice,
    team_routing::{TeamRoutingRow, parse_team_skill, select_member_index},
    *,
};
use crowdrelay_brain::capture_plans::{
    CapturePlanVerdict, CaptureShot, HARVEST_WINDOW_DAYS, ShotCandidate, capture_need,
    capture_plan_verdict, shot_list,
};
use crowdrelay_domain::content_engine::{
    CapturePlanStatus, ProductionEventKind, ProductionEventStatus,
};
use time::Duration as TimeDuration;

/// The local date of `starts_at` in the venue's timezone — a 00:30 gig
/// is the band's "that night", not the UTC date that started an hour
/// earlier. `events.timezone` is free text, so an unparseable zone
/// degrades to the UTC date rather than aborting the projection (the
/// same guard `show_growth` uses for its morning-after arithmetic).
const LOCAL_EVENT_DAY: &str = r#"
    CASE WHEN EXISTS (
        SELECT 1 FROM pg_timezone_names AS zone WHERE zone.name = event.timezone
    ) THEN (event.starts_at AT TIME ZONE event.timezone)::date
        ELSE (event.starts_at AT TIME ZONE 'UTC')::date
    END
"#;

/// Every published gig is also a production day: the capture plan for it
/// hangs off `production_events`, so the show ladder's calendar
/// projects forward into the content engine's. Idempotent on
/// `event_id` — the partial unique index is the race guard, `ON
/// CONFLICT DO NOTHING` the acknowledgement.
///
/// The UPDATE beneath the INSERT is the other half of the projection:
/// a gig that moves, is retitled, cancelled or completed has its day
/// carried with it. Without it a cancelled show kept `scheduled` and
/// the sweep would still hand someone a shot list for a dead day;
/// terminal production states (`done`, `cancelled`) never move again.
pub(in crate::autopilot) async fn project_shows_to_production_events(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(&format!(
        r#"
        INSERT INTO production_events (id, workspace_id, kind, title, scheduled_for, event_id)
        SELECT uuidv7(), event.workspace_id,
               -- 6.3: a festival slot is a production day of its own kind —
               -- the shot list and harvest already know the difference.
               CASE WHEN event.festival_name IS NOT NULL
                    THEN 'festival' ELSE 'show' END,
               event.title,
               {LOCAL_EVENT_DAY}, event.id
        FROM events event
        WHERE event.workspace_id = $1
          AND event.status IN ('published','completed')
          AND event.starts_at >= $2 - INTERVAL '2 days'
          AND event.starts_at <= $2 + INTERVAL '60 days'
        ON CONFLICT DO NOTHING
        "#,
    ))
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // Sync what the gig became into the projected day. `done`/`cancelled`
    // production events are terminal, so only `scheduled`/`in_progress`
    // rows follow the source — a cancelled-then-republished gig cannot
    // resurrect a dead day through the back door.
    sqlx::query(&format!(
        r#"
        UPDATE production_events day
        SET title = sync.title,
            scheduled_for = sync.day,
            status = COALESCE(sync.terminal_status, day.status),
            kind = sync.kind,
            updated_at = now()
        FROM (
            SELECT event.id, event.title,
                   {LOCAL_EVENT_DAY} AS day,
                   CASE event.status
                       WHEN 'cancelled' THEN 'cancelled'
                       WHEN 'completed' THEN 'done'
                   END AS terminal_status,
                   -- The festival mark can land after the day projected —
                   -- the kind follows it the same way the date does.
                   CASE WHEN event.festival_name IS NOT NULL
                        THEN 'festival' ELSE 'show' END AS kind
            FROM events event
            WHERE event.workspace_id = $1
              AND event.status IN ('published','cancelled','completed')
        ) sync
        WHERE day.workspace_id = $1
          AND day.event_id = sync.id
          AND day.kind IN ('show','festival')
          AND day.status IN ('scheduled','in_progress')
          AND (day.title IS DISTINCT FROM sync.title
               OR day.scheduled_for IS DISTINCT FROM sync.day
               OR day.kind IS DISTINCT FROM sync.kind
               OR (sync.terminal_status IS NOT NULL
                   AND day.status <> sync.terminal_status))
        "#,
    ))
    .bind(workspace_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // A moved day moves the member's deadline with it — the plan row
    // follows the new date through the join, but the assignment's stamped
    // due_at was computed against the old one. The chase bookkeeping resets
    // with it: a deadline that moved cannot owe anybody a stale count.
    sqlx::query(
        r#"
        UPDATE team_assignments assignment
        SET due_at = (day.scheduled_for + 1)::timestamp AT TIME ZONE 'UTC',
            last_reminded_at = NULL,
            first_overdue_reminder_at = NULL,
            reminder_count = 0
        FROM capture_plans plan
        JOIN production_events day
          ON day.workspace_id = plan.workspace_id
         AND day.id = plan.production_event_id
        WHERE assignment.workspace_id = $1
          AND assignment.source_kind = 'capture_plan'
          AND assignment.source_id = plan.id
          AND assignment.status = 'open'
          AND plan.workspace_id = $1
          AND day.status IN ('scheduled','in_progress')
          AND assignment.due_at IS DISTINCT FROM
              (day.scheduled_for + 1)::timestamp AT TIME ZONE 'UTC'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Every published gig gets a door QR campaign whether or not anyone
/// remembered to create one. The 2026-09-11 show ran its whole announce leg
/// and never had a scan leg at all — `create_campaign` only answers an
/// explicit POST, so "print the QR" could not happen without somebody knowing
/// the campaign had to exist first. The machine mints the campaign; the only
/// human step left is printing.
///
/// Idempotent on the event: any existing campaign row for the event —
/// operator-made, revoked, or an earlier auto-mint — blocks the insert, so a
/// deliberate revoke is never undone and a hand-entered campaign is never
/// duplicated. Runs over the shows whose door is still open — the projection's
/// window narrowed at the near end, because a campaign for a night that
/// finished yesterday is born expired — and so still backfills every show
/// published before this existed whose QR could still be scanned.
///
/// The window is the door's, keyed off the show: opens four hours before
/// start (early doors, soundcheck crowds) and closes twelve hours after —
/// inside the API's −24h/+36h bounds and the table's 14-day CHECK.
pub(in crate::autopilot) async fn mint_door_campaigns(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        WITH minted AS (
            INSERT INTO concert_qr_campaigns (
                id, workspace_id, event_id, label, valid_from, valid_until,
                announced_from_stage, created_at, updated_at
            )
            SELECT uuidv7(), event.workspace_id, event.id, 'Door',
                   event.starts_at - INTERVAL '4 hours',
                   event.starts_at + INTERVAL '12 hours',
                   false, $2, $2
            FROM events event
            WHERE event.workspace_id = $1
              AND event.status = 'published'
              -- Not the projection's −2d floor: a show whose door window
              -- has already closed gets a dead-on-arrival campaign, and the
              -- API's own validation would refuse that insert anyway.
              AND event.starts_at + INTERVAL '12 hours' > $2
              AND event.starts_at <= $2 + INTERVAL '60 days'
              AND NOT EXISTS (
                  SELECT 1
                  FROM concert_qr_campaigns existing
                  WHERE existing.workspace_id = event.workspace_id
                    AND existing.event_id = event.id
              )
            RETURNING workspace_id, id, event_id
        )
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        )
        SELECT minted.workspace_id, 'service', 'concert_qr.auto_minted',
               'concert_qr_campaign', minted.id::text,
               jsonb_build_object(
                   'event_id', minted.event_id,
                   'source', 'show_projection'
               )
        FROM minted
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // A moved gig moves the door with it. The QR's signed token carries
    // `valid_until`, so a show rescheduled after the mint leaves every
    // printed code dead AND blocks re-mint (a row exists) — the exact dead
    // scan leg the mint exists to prevent. Auto-minted rows are recognisable
    // by their exact shape ('Door' label, all context fields unset); once a
    // check-in lands the window is history rather than policy, so it stops
    // following. An operator who PATCHes context onto the campaign breaks
    // the shape match and owns the window from then on.
    sqlx::query(
        r#"
        UPDATE concert_qr_campaigns campaign
        SET valid_from = event.starts_at - INTERVAL '4 hours',
            valid_until = event.starts_at + INTERVAL '12 hours'
        FROM events event
        WHERE event.workspace_id = $1
          AND event.id = campaign.event_id
          AND campaign.workspace_id = $1
          AND event.status = 'published'
          AND event.starts_at >= $2 - INTERVAL '2 days'
          AND event.starts_at <= $2 + INTERVAL '60 days'
          AND campaign.label = 'Door'
          AND campaign.placement IS NULL
          AND campaign.incentive IS NULL
          AND campaign.max_checkins IS NULL
          AND campaign.announced_from_stage = false
          AND campaign.active
          AND campaign.revoked_at IS NULL
          AND (campaign.valid_from IS DISTINCT FROM event.starts_at - INTERVAL '4 hours'
               OR campaign.valid_until IS DISTINCT FROM event.starts_at + INTERVAL '12 hours')
          AND NOT EXISTS (
              SELECT 1
              FROM concert_checkins checkin
              WHERE checkin.workspace_id = campaign.workspace_id
                AND checkin.campaign_id = campaign.id
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

#[derive(Debug, FromRow)]
struct OpenCapturePlanRow {
    id: Uuid,
    plan_status: String,
    scheduled_for: time::Date,
    event_status: String,
}

/// Settles open capture plans against what the day actually produced.
/// The yield is active content sources logged inside the harvest window
/// — counted by *kind*, not key prefix: only `video` and `story` are
/// captured material. `event`, `release` and `show_completed` rows are
/// machine projections of the calendar and the discography (the event
/// and release-plan triggers, the Spotify announce path, the
/// post-show projection) — a show happening is not footage of the
/// show, and a release dropping nearby is not either.
pub(in crate::autopilot) async fn settle_capture_plans(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    today: time::Date,
) -> Result<(), RepositoryError> {
    let plans = sqlx::query_as::<_, OpenCapturePlanRow>(
        r#"
        SELECT plan.id, plan.status AS plan_status,
               event.scheduled_for, event.status AS event_status
        FROM capture_plans plan
        JOIN production_events event
          ON event.workspace_id = plan.workspace_id
         AND event.id = plan.production_event_id
        WHERE plan.workspace_id = $1 AND plan.status IN ('draft','issued')
        ORDER BY event.scheduled_for, plan.id
        LIMIT 64
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    for plan in plans {
        let (Some(plan_status), Some(event_status)) = (
            CapturePlanStatus::parse(&plan.plan_status),
            ProductionEventStatus::parse(&plan.event_status),
        ) else {
            continue;
        };
        let sources_landed = if plan_status == CapturePlanStatus::Issued {
            let window_start = plan.scheduled_for.midnight().assume_utc();
            let window_end = (plan.scheduled_for + TimeDuration::days(HARVEST_WINDOW_DAYS + 1))
                .midnight()
                .assume_utc();
            sqlx::query_scalar::<_, i64>(
                r#"SELECT COUNT(*) FROM content_sources s
                   WHERE s.workspace_id=$1 AND s.active
                     AND s.source_kind IN ('video','story','social_post')
                     AND s.occurred_at >= $2 AND s.occurred_at < $3"#,
            )
            .bind(workspace_id.into_uuid())
            .bind(window_start)
            .bind(window_end)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_sqlx)?
        } else {
            0
        };
        let next = match capture_plan_verdict(
            plan_status,
            event_status,
            sources_landed,
            plan.scheduled_for,
            today,
        ) {
            CapturePlanVerdict::Keep => {
                // The running yield is honest even mid-harvest — issued
                // plans keep their landed count current until the verdict.
                if plan_status == CapturePlanStatus::Issued {
                    sqlx::query(
                        "UPDATE capture_plans SET sources_landed=$3 \
                         WHERE id=$2 AND workspace_id=$1 AND status='issued'",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(plan.id)
                    .bind(i32::try_from(sources_landed).unwrap_or(i32::MAX))
                    .execute(&mut **tx)
                    .await
                    .map_err(map_sqlx)?;
                }
                continue;
            }
            CapturePlanVerdict::Done => "done",
            CapturePlanVerdict::Abandon => "abandoned",
        };
        // The verdict writes the final yield with it — NULL for a draft
        // that was never measured, the count for anything issued.
        sqlx::query(
            "UPDATE capture_plans SET status=$3, sources_landed=$5, updated_at=now() \
             WHERE id=$2 AND workspace_id=$1 AND status=$4",
        )
        .bind(workspace_id.into_uuid())
        .bind(plan.id)
        .bind(next)
        .bind(plan.plan_status)
        .bind(if plan_status == CapturePlanStatus::Issued {
            Some(i32::try_from(sources_landed).unwrap_or(i32::MAX))
        } else {
            None
        })
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}

#[derive(Debug, FromRow)]
struct UnplannedProductionDayRow {
    id: Uuid,
    kind: String,
    title: String,
    scheduled_for: time::Date,
    event_id: Option<Uuid>,
}

#[derive(Debug, FromRow)]
struct ShotCandidateRow {
    key: String,
    name: String,
    skill: String,
}

/// Issues capture plans for production days that land today or tomorrow
/// and have no open plan. The shot list is built from the formats the
/// open suggestions and the active arc still need, filtered to what this
/// kind of day can actually cover — a studio session cannot shoot an
/// aftermovie. The plan always exists as a row; the assignment and the
/// email exist only when a member can hold the camera, the same bar
/// every other handoff has to clear.
pub(in crate::autopilot) async fn issue_capture_plans(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    // `today` arrives as the caller's crew-local date: `scheduled_for`
    // is a venue-local DATE, so the day window must compare dates on
    // the same clock the crew lives on, not UTC. Binding it as a DATE
    // also keeps the connection's timezone from picking the day.
    // `other` days never produce a shot list, so they are filtered here
    // rather than burning a slot every sweep; a settled-but-not-
    // abandoned plan also blocks re-issue — a `done` plan already
    // harvested its day and a second list would only nag.
    today: time::Date,
    mutable_team: &mut [TeamRoutingRow],
    crew_locale: crowdrelay_application::autopilot::BriefingLocale,
    pending_notices: &mut Vec<PendingInitialNotice>,
) -> Result<u32, RepositoryError> {
    let days = sqlx::query_as::<_, UnplannedProductionDayRow>(
        r#"
        SELECT event.id, event.kind, event.title, event.scheduled_for, event.event_id
        FROM production_events event
        WHERE event.workspace_id=$1 AND event.status='scheduled'
          AND event.kind <> 'other'
          AND event.scheduled_for BETWEEN $2 AND $2 + 1
          AND NOT EXISTS (
              SELECT 1 FROM capture_plans plan
              WHERE plan.production_event_id = event.id
                AND plan.status <> 'abandoned'
          )
        ORDER BY event.scheduled_for, event.id
        FOR UPDATE OF event SKIP LOCKED
        LIMIT 8
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(today)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // A draft is a plan waiting for a holder — while its day is still
    // inside the issue window every sweep offers it to the roster again,
    // so a member who joins mid-week still gets the shot list.
    let drafts = sqlx::query_as::<_, DraftPlanDayRow>(
        r#"
        SELECT plan.id AS plan_id, plan.assignee_member_id,
               event.kind, event.title, event.scheduled_for, event.event_id
        FROM capture_plans plan
        JOIN production_events event
          ON event.workspace_id = plan.workspace_id
         AND event.id = plan.production_event_id
        WHERE plan.workspace_id=$1 AND plan.status='draft' AND event.status='scheduled'
          AND event.kind <> 'other'
          AND event.scheduled_for BETWEEN $2 AND $2 + 1
        ORDER BY event.scheduled_for, plan.id
        FOR UPDATE OF plan SKIP LOCKED
        LIMIT 8
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(today)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if days.is_empty() && drafts.is_empty() {
        return Ok(0);
    }

    // A raised suggestion whose approval window already lapsed is dead
    // work — only live windows and approved commitments feed the list.
    // The spine guard keeps one malformed arc row from aborting the
    // whole sweep: `jsonb_array_elements` refuses a non-array.
    let candidates = sqlx::query_as::<_, ShotCandidateRow>(
        r#"
        SELECT DISTINCT fmt.key, fmt.name, fmt.skill
        FROM (
            SELECT format_key AS key
            FROM content_suggestions
            WHERE workspace_id=$1
              AND (status='approved'
                   OR (status='raised' AND (expires_at IS NULL OR expires_at > now())))
              AND format_key IS NOT NULL
            UNION
            SELECT beat->>'format_key' AS key
            FROM arcs arc
            CROSS JOIN LATERAL jsonb_array_elements(arc.spine) beat
            WHERE arc.workspace_id=$1 AND arc.status='active'
              AND jsonb_typeof(arc.spine) = 'array'
        ) needed
        JOIN content_format_entries fmt
          ON fmt.key = needed.key AND fmt.active
        ORDER BY fmt.key
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .filter_map(|row| {
        parse_team_skill(&row.skill).map(|skill| ShotCandidate {
            format_key: row.key,
            name: row.name,
            skill,
        })
    })
    .collect::<Vec<_>>();

    let mut assigned = 0_u32;
    for day in days {
        let Some(kind) = ProductionEventKind::parse(&day.kind) else {
            continue;
        };
        let shots = shot_list(kind, &candidates);
        if shots.is_empty() {
            // A day the catalogue cannot name has no list to hand anyone.
            continue;
        }
        let member_index = select_member_index(mutable_team, capture_need(kind));
        let items = items_json(&shots)?;
        let plan_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO capture_plans (
                id, workspace_id, production_event_id, items,
                assignee_member_id, status, issued_at
            ) VALUES ($1,$2,$3,$4,$5,$6, CASE WHEN $6 = 'issued' THEN now() END)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(day.id)
        .bind(&items)
        .bind(member_index.and_then(|index| mutable_team.get(index).map(|member| member.member_id)))
        .bind(if member_index.is_some() {
            "issued"
        } else {
            "draft"
        })
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        let (Some(member_index), Some(plan_id)) = (member_index, plan_id) else {
            continue;
        };
        assigned = assigned.saturating_add(
            route_capture_plan(
                tx,
                workspace_id,
                plan_id,
                member_index,
                kind,
                &day.title,
                day.scheduled_for,
                day.event_id,
                &shots,
                mutable_team,
                crew_locale,
                pending_notices,
            )
            .await?,
        );
    }

    for draft in drafts {
        let Some(kind) = ProductionEventKind::parse(&draft.kind) else {
            continue;
        };
        let shots = shot_list(kind, &candidates);
        if shots.is_empty() {
            continue;
        }
        // An assignee stamped when the plan was drafted (the manual path)
        // is the routing's first answer — the roster pick is the fallback
        // for when that member is no longer routable.
        let member_index = draft
            .assignee_member_id
            .and_then(|preset| {
                mutable_team
                    .iter()
                    .position(|member| member.member_id == preset)
            })
            .or_else(|| select_member_index(mutable_team, capture_need(kind)));
        let Some(member_index) = member_index else {
            continue;
        };
        let member_id = mutable_team
            .get(member_index)
            .ok_or(RepositoryError::Unexpected)?
            .member_id;
        // `items` is refreshed with the issue: the member reads the same
        // list the plan stores, not the day-old list the draft was
        // parked with. The EXISTS matches the repository's own
        // issue_capture_plan — a day cancelled between the SELECT and
        // this write must not ship a shot list.
        let issued = sqlx::query_scalar::<_, Uuid>(
            r#"UPDATE capture_plans
               SET status='issued', assignee_member_id=$3, items=$4,
                   issued_at=now(), updated_at=now()
               WHERE id=$2 AND workspace_id=$1 AND status='draft'
                 AND EXISTS (
                     SELECT 1 FROM production_events event
                     WHERE event.workspace_id=$1
                       AND event.id=capture_plans.production_event_id
                       AND event.status='scheduled'
                 )
               RETURNING id"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(draft.plan_id)
        .bind(member_id)
        .bind(items_json(&shots)?)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        if issued.is_none() {
            continue;
        }
        assigned = assigned.saturating_add(
            route_capture_plan(
                tx,
                workspace_id,
                draft.plan_id,
                member_index,
                kind,
                &draft.title,
                draft.scheduled_for,
                draft.event_id,
                &shots,
                mutable_team,
                crew_locale,
                pending_notices,
            )
            .await?,
        );
    }
    Ok(assigned)
}

/// The `items` JSONB the member and the harvest read — each shot carries
/// the catalogue `format_key` it feeds when it has one, so a landed
/// source can later be attributed back to the need that asked for it.
fn items_json(shots: &[CaptureShot]) -> Result<serde_json::Value, RepositoryError> {
    serde_json::to_value(
        shots
            .iter()
            .map(|shot| {
                serde_json::json!({
                    "item": shot.item,
                    "skill": shot.skill.as_str(),
                    "format_key": shot.format_key,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|_| RepositoryError::Unexpected)
}

/// The routed half of a plan's life: the assignment row, the shot-list
/// email, the checklist's "does this show have a plan" item. Shared by
/// the fresh-issue and the draft-retry paths so both handoffs look the
/// same to the member holding the camera.
#[allow(clippy::too_many_arguments)]
async fn route_capture_plan(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    plan_id: Uuid,
    member_index: usize,
    kind: ProductionEventKind,
    day_title: &str,
    scheduled_for: time::Date,
    event_id: Option<Uuid>,
    shots: &[CaptureShot],
    mutable_team: &mut [TeamRoutingRow],
    crew_locale: crowdrelay_application::autopilot::BriefingLocale,
    pending_notices: &mut Vec<PendingInitialNotice>,
) -> Result<u32, RepositoryError> {
    let member = mutable_team
        .get_mut(member_index)
        .ok_or(RepositoryError::Unexpected)?;
    let due_at = (scheduled_for + TimeDuration::days(1))
        .midnight()
        .assume_utc();
    let assignment_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO team_assignments (
            id, workspace_id, action_id, source_kind, source_id, source_ref,
            assignee_member_id, required_skill, due_at
        ) VALUES ($1,$2,NULL,'capture_plan',$3,NULL,$4,$5,$6)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(assignment_id)
    .bind(workspace_id.into_uuid())
    .bind(plan_id)
    .bind(member.member_id)
    .bind(capture_need(kind).primary_skill.as_str())
    .bind(due_at)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if inserted.is_none() {
        return Ok(0);
    }

    pending_notices.push(PendingInitialNotice {
        assignment_id,
        context: "show_operations".to_owned(),
        recipient_email: member.normalized_email.clone(),
        recipient_name: member.display_name.clone(),
        title: match crew_locale {
            crowdrelay_application::autopilot::BriefingLocale::Pl => {
                format!("Zabezpiecz materiał: {day_title}")
            }
            crowdrelay_application::autopilot::BriefingLocale::En => {
                format!("Secure the footage: {day_title}")
            }
        },
        detail: capture_plan_detail(
            day_title,
            scheduled_for,
            &shots
                .iter()
                .map(|shot| shot.item.clone())
                .collect::<Vec<_>>(),
            crew_locale,
        ),
        due_at: Some(due_at),
        source_action_id: None,
        source_kind: "capture_plan",
    });

    // The checklist's bare 'capture_plan' item meant "does this show
    // have a plan" — it does now. Marking it done keeps the checklist
    // honest instead of nagging beside the real plan.
    if let Some(event_id) = event_id {
        sqlx::query(
            r#"INSERT INTO show_checklist_items (workspace_id, event_id, item_key, status, note)
               VALUES ($1,$2,'capture_plan','done','Plan ujęć wydany — lista trafiła do przypisanej osoby.')
               ON CONFLICT (workspace_id, event_id, item_key)
               DO UPDATE SET status='done', updated_at=now()
               -- 'skipped'/'blocked' are operator calls — a verified fact
               -- must not silently erase the note saying why.
               WHERE show_checklist_items.status NOT IN ('done','skipped','blocked')"#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }

    member.open_assignments = member.open_assignments.saturating_add(1);
    member.recent_assignments = member.recent_assignments.saturating_add(1);
    member.asks_last_7d = member.asks_last_7d.saturating_add(1);
    Ok(1)
}

#[derive(Debug, FromRow)]
struct DraftPlanDayRow {
    plan_id: Uuid,
    assignee_member_id: Option<Uuid>,
    kind: String,
    title: String,
    scheduled_for: time::Date,
    event_id: Option<Uuid>,
}

pub(super) fn capture_plan_detail(
    day_title: &str,
    scheduled_for: time::Date,
    items: &[String],
    locale: crowdrelay_application::autopilot::BriefingLocale,
) -> String {
    use crowdrelay_application::autopilot::BriefingLocale;
    let mut text = match locale {
        BriefingLocale::Pl => format!(
            "Dzień produkcyjny: {day_title}. Data: {scheduled_for}.\n\nUjęcia do zrobienia:"
        ),
        BriefingLocale::En => {
            format!("Production day: {day_title}. Date: {scheduled_for}.\n\nShots to take:")
        }
    };
    for (i, item) in items.iter().enumerate() {
        text.push_str(&format!("\n{}. {}.", i + 1, item));
    }
    text.push_str(match locale {
        BriefingLocale::Pl =>
            "\n\nPo dniu wrzuć materiał jako źródła treści — harvest liczy ujęcia do 3 dni po dniu produkcyjnym.",
        BriefingLocale::En =>
            "\n\nAfter the day, upload the material as content sources — the harvest counts shots up to 3 days after the production day.",
    });
    text
}
