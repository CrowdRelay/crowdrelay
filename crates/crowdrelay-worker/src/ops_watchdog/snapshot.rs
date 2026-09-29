// The snapshot read — split out of `ops_watchdog.rs` for the source-size
// ratchet, same split `conditions.rs` already made. One big SELECT because
// the conditions are one consistent reading of the same moment: twenty
// separate queries would each see a different now.
//
// `include!`d into `ops_watchdog.rs`, sharing its scope.

async fn load_snapshot(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<OpsSnapshot, sqlx::Error> {
    let mut snapshot = sqlx::query_as::<_, OpsSnapshot>(
        r#"
        SELECT
            count(*)::bigint AS executor_registered,
            count(*) FILTER (WHERE expires_at>now())::bigint AS executor_active,
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1 AND a.status='unknown')::bigint AS unknown_actions,
            -- stale_unknown_actions: unknown actions whose unknown_age
            -- (from the action ledger's state_entered_at) exceeds the
            -- alert threshold. This avoids alerting on transient unknowns
            -- that the reconciliation sweep is actively resolving.
            (SELECT count(*) FROM autopilot_actions a
             JOIN action_ledger al ON al.action_id = a.id
             WHERE a.workspace_id=$1 AND a.status='unknown'
               AND al.state='UNKNOWN'
               AND al.state_entered_at < now() - make_interval(secs => $2::double precision)
            )::bigint AS stale_unknown_actions,
            -- contradicted_actions: the standing Conflict population. The
            -- *newest* terminal receipt is the comparison, not any receipt —
            -- an older failure followed by a success is an ordinary history,
            -- not a contradiction. Both directions count: a failure receipt
            -- refused against a provider-confirmed success, and a success
            -- receipt refused against a persisted failure.
            (SELECT count(*) FROM autopilot_actions a
             JOIN LATERAL (
                 SELECT r.status
                 FROM autopilot_execution_reports r
                 WHERE r.workspace_id = a.workspace_id AND r.action_id = a.id
                   AND r.status IN ('succeeded', 'failed')
                 ORDER BY r.occurred_at DESC, r.id DESC
                 LIMIT 1
             ) latest ON true
             WHERE a.workspace_id=$1
               AND ((a.status = 'succeeded' AND latest.status = 'failed')
                 OR (a.status = 'failed' AND latest.status = 'succeeded'))
            )::bigint AS contradicted_actions,
            -- Feed health, by platform. `health` is a generated column, so this
            -- is the same answer `/ops/connections` gives and cannot drift from
            -- it. Both halves are needed: failing feeds alone is a credential to
            -- fix, failing feeds with nothing working is the brain having no
            -- channel left to reason through.
            (SELECT count(DISTINCT platform) FROM fanbase_connections c
             WHERE c.workspace_id=$1 AND c.health='failing')::bigint
                AS failing_platforms,
            (SELECT count(DISTINCT platform) FROM fanbase_connections c
             WHERE c.workspace_id=$1 AND c.health='working')::bigint
                AS working_platforms,
            -- Cities that fans requested but the geocoder gave up on, and
            -- that an active fan is actually waiting in. These are
            -- unreachable by the nearby-show loop until a human fixes the
            -- name or enters coordinates by hand. The attempt threshold
            -- matches `city_geocoding::MAX_GEOCODE_ATTEMPTS` (5).
            --
            -- The fan predicate is what makes the finding mean what it says.
            -- Without it this counted rows, not people: production raised the
            -- warning for a day over "Example City, Example Region" and
            -- "Tes5, Test" -- two test entries the geocoder correctly refused
            -- five times because neither exists -- while the summary claimed
            -- fans there were unreachable. No fan had ever selected either.
            --
            -- A stuck city nobody is waiting in is a data-quality note, not
            -- an operator alarm. It starts mattering the moment a fan picks
            -- it, and that is exactly when this now fires.
            (SELECT count(*) FROM cities ct
             WHERE ct.latitude IS NULL
               AND ct.moderation_status IN ('pending', 'approved')
               AND ct.geocode_attempts >= 5
               AND EXISTS (
                     SELECT 1 FROM fan_location_preferences p
                     JOIN fans f ON f.id = p.fan_id
                     WHERE p.city_id = ct.id
                       AND p.workspace_id = $1
                       AND f.status = 'active'
                   )
            )::bigint AS stuck_ungeocoded_cities,
            -- How many people are behind that count. One city with thirty
            -- fans waiting and thirty cities with one each are different
            -- problems, and the count of cities alone cannot tell them apart.
            (SELECT count(DISTINCT p.fan_id) FROM fan_location_preferences p
             JOIN fans f ON f.id = p.fan_id
             JOIN cities ct ON ct.id = p.city_id
             WHERE p.workspace_id = $1
               AND f.status = 'active'
               AND ct.latitude IS NULL
               AND ct.moderation_status IN ('pending', 'approved')
               AND ct.geocode_attempts >= 5
            )::bigint AS fans_awaiting_geocoding,
            -- Agent outcomes refused in the last day for want of a grounding
            -- check, and the accepted count beside it. A day, not all time:
            -- this asks whether the loop is running now, and a rejection from
            -- last month is history rather than an alarm.
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='rejected'
               AND o.rejection_reason LIKE 'NOT_GROUNDING_CHECKED%'
               AND o.created_at > now() - interval '1 day'
            )::bigint AS outcomes_rejected_unverified,
            -- Accepted *actionable* outcomes only.
            --
            -- The grounding gate applies to `require_approval` kinds alone, so
            -- the refusals counted above are all actionable. Counting every
            -- accepted kind against them compares two different populations:
            -- `recommend_only` insights and segments pass the gate untouched
            -- and keep arriving, so the accepted total is never zero and the
            -- condition could never fire — in exactly the state it exists to
            -- report. Production proved that on the first cycle after deploy:
            -- refusals present, alarm silent.
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='processed'
               AND o.kind IN ('press_pitch','social_post','signal_push','outreach_targets')
               AND o.created_at > now() - interval '1 day'
            )::bigint AS outcomes_accepted,
            -- Succeeded publishing actions with no artifact in any of the four
            -- post tables. The 30-minute floor is the executors' poll window:
            -- below it an action is in flight, not orphaned.
            --
            -- Bounded at 7 days, matching `refused_growth_deliveries`, because
            -- an orphaned draft is unrecoverable: nothing publishes it later
            -- and there is no acknowledgement mechanism, so an unbounded count
            -- made this condition permanently active once a single orphan
            -- existed. An alarm that can never clear is one an operator learns
            -- to ignore. The all-time count still travels in the details, so
            -- the history is reported, just not alarmed on forever.
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='succeeded'
               AND a.action_kind IN ('agent.content.request',
                                     'community.engage.request')
               AND a.finished_at < now() - interval '30 minutes'
               AND a.finished_at > now() - interval '7 days'
               AND NOT EXISTS (SELECT 1 FROM community_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM telegram_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM discord_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM social_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
            )::bigint AS orphaned_publishing_actions,
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='succeeded'
               AND a.action_kind IN ('agent.content.request',
                                     'community.engage.request')
               AND a.finished_at < now() - interval '30 minutes'
               AND NOT EXISTS (SELECT 1 FROM community_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM telegram_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM discord_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM social_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
            )::bigint AS orphaned_publishing_actions_all_time,
            -- Growth-carrying events whose delivery was permanently refused.
            --
            -- The discriminator is the payload, not an event-type list: a
            -- delivery carrying a named recipient is a letter the brain
            -- drafted for a specific human — a pitch, an approach, an
            -- application, an invite. Two older event types stay named
            -- explicitly because their payloads carry drafted work without an
            -- email-shaped key. A refused `ops.status_changed` is a stale
            -- consumer contract and lands in `refused_other_deliveries` as a
            -- warning instead.
            --
            -- Measured 2026-09-15: an `opportunity.application_requested`
            -- carrying `contact_email` and a `post_show_report_due` carrying
            -- `recipients` both died 422 cancelled while this predicate's
            -- two-type list watched neither — the list had silently encoded
            -- "the only outward letters are these two".
            --
            -- Cancelled, not dead, which is why nothing reported this. A 4xx is
            -- `http_permanent_status` and the outbox correctly stops retrying —
            -- so the delivery leaves the pending set, never becomes dead, and
            -- `ops/attention`'s `dead_deliveries` stays empty while the event is
            -- just as undelivered. The only trace was the `cancelled` count,
            -- a bare number with no breakdown.
            (SELECT count(*) FROM webhook_deliveries d
             JOIN outbox_events e ON e.id = d.outbox_event_id
             WHERE d.workspace_id=$1
               AND d.status='cancelled'
               AND d.cancelled_at > now() - interval '7 days'
               AND (e.event_type IN ('crowdrelay.agent.content_requested',
                                     'crowdrelay.community.engagement_requested')
                    OR e.payload ? 'contact_email'
                    OR e.payload ? 'recipient_email'
                    OR e.payload ? 'recipients')
            )::bigint AS refused_growth_deliveries,
            -- Refused deliveries that are not letters — stale consumer
            -- contracts, unrouted internal events. Warning-class context for
            -- the same cancelled-instead-of-dead blind spot.
            (SELECT count(*) FROM webhook_deliveries d
             JOIN outbox_events e ON e.id = d.outbox_event_id
             WHERE d.workspace_id=$1
               AND d.status='cancelled'
               AND d.cancelled_at > now() - interval '7 days'
               AND e.event_type NOT IN ('crowdrelay.agent.content_requested',
                                        'crowdrelay.community.engagement_requested')
               AND NOT (e.payload ? 'contact_email')
               AND NOT (e.payload ? 'recipient_email')
               AND NOT (e.payload ? 'recipients')
            )::bigint AS refused_other_deliveries,
            -- Live opportunities the brain scored and then denied.
            --
            -- Read off the decisions the brain actually recorded rather than by
            -- recomputing its arithmetic here. The first version of this condition
            -- counted rows with `strategic_value_basis_points = 0` and no logistics,
            -- which was the true reason on the day it was written — and the moment
            -- an import filled strategic value, the alarm went quiet while all 430
            -- opportunities stayed held for a different reason. A proxy for a hold
            -- stops tracking the hold; the decision row does not.
            --
            -- `apply_live_opportunity` with `deny` is a precise state: the
            -- opportunity cleared `minimum_score`, so it was worth scoring, and was
            -- refused anyway. In production that is the confidence gate —
            -- `disposition()` denies below `minimum_confidence`, and a live
            -- opportunity's confidence is `7500 + (score - minimum_score) * 100`,
            -- so a `minimum_confidence` of 8000 makes the effective floor five
            -- points above the score floor an operator set. 65 in the policy is 70
            -- in practice, and nothing said so.
            (SELECT count(*) FROM autopilot_decisions d
             WHERE d.workspace_id=$1
               AND d.decision_kind='apply_live_opportunity'
               AND d.disposition='deny'
               AND d.evaluated_at > now() - interval '1 day'
            )::bigint AS unscoreable_live_opportunities,
            -- Unpublished drafts that target the same community as another.
            --
            -- Reddit treats subreddit names case-insensitively, so `r/MetalMemes`
            -- and `r/metalmemes` are one place. Duplicate `discovery_places` rows
            -- drafted one post each, and migration 0259 collapsed the places but
            -- deliberately left the drafts alone: they carry different text the
            -- band wrote, and deleting either is not a migration's call.
            --
            -- Two posts to one community months apart are ordinary. Two sitting in
            -- the queue at once are a double-post waiting for whoever publishes
            -- them, which is the spam the North Star rules out — so the condition
            -- is scoped to drafts that are *both* still unpublished.
            (SELECT COALESCE(sum(drafts - 1), 0) FROM (
                SELECT count(*) AS drafts
                FROM community_posts
                WHERE workspace_id=$1
                  AND status='awaiting_manual_post'
                GROUP BY normalize_subreddit(subreddit)
                HAVING count(*) > 1
             ) AS duplicated)::bigint AS duplicate_community_drafts,
            -- Phases that failed in EVERY one of the last N closed cycles.
            --
            -- `HAVING count(*) = (SELECT count(*) FROM recent)` is the
            -- intersection of the recent phase arrays: a phase appearing once per
            -- cycle in all of them failed all of them. The second condition
            -- requires a full window, so a worker that has only just started does
            -- not report its first two cycles as a relentless failure.
            --
            -- Only cycles that recorded the column. Rows from before migration
            -- 0261 carry NULL, which means "did not look" rather than "nothing
            -- failed", and counting them as clean would suppress the alarm.
            (SELECT string_agg(relentless.phase, ',' ORDER BY relentless.phase)
             FROM (
                WITH recent AS (
                    SELECT degraded_phases
                    FROM autopilot_cycle_runs
                    WHERE workspace_id=$1
                      AND finished_at IS NOT NULL
                      AND degraded_phases IS NOT NULL
                    ORDER BY started_at DESC
                    LIMIT $3
                )
                SELECT failures.phase
                FROM (SELECT unnest(degraded_phases) AS phase FROM recent) AS failures
                GROUP BY failures.phase
                HAVING count(*) = (SELECT count(*) FROM recent)
                   AND (SELECT count(*) FROM recent) >= $3
             ) AS relentless
            ) AS relentless_degraded_phases,
            -- Guards that refused an outcome in the last day, with counts.
            --
            -- The reason is free text ending in the offending value, so the
            -- prefix before the first colon is the guard identity. Splitting on
            -- it groups "MISSING_TARGET_IDENTITY: display_name is missing" with
            -- every other instance instead of reporting each as unique.
            (SELECT string_agg(guard.name || '=' || guard.hits, ',' ORDER BY guard.name)
             FROM (
                SELECT split_part(o.rejection_reason, ':', 1) AS name,
                       count(*) AS hits
                FROM agent_outcomes o
                WHERE o.workspace_id=$1
                  AND o.status='rejected'
                  AND o.rejection_reason IS NOT NULL
                  AND o.created_at > now() - interval '1 day'
                GROUP BY split_part(o.rejection_reason, ':', 1)
             ) AS guard
            ) AS outcome_rejection_reasons,
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='rejected'
               AND o.rejection_reason LIKE 'OFF_PLATFORM_PUSH_TARGET%'
               AND o.created_at > now() - interval '1 day'
            )::bigint AS off_platform_push_attempts,
            (SELECT count(*) FROM community_posts p
             WHERE p.workspace_id=$1 AND p.status='awaiting_manual_post'
               AND p.platform='reddit'
            )::bigint AS reddit_drafts_waiting,
            -- Failed drafts in the last day, as `count×reason`. Truncated,
            -- because the reason can carry a provider body and this travels in
            -- an alert payload.
            (SELECT string_agg(f.hits || '×' || f.reason, '; ' ORDER BY f.hits DESC)
             FROM (
                SELECT count(*) AS hits, left(coalesce(p.error_message, 'unknown'), 120) AS reason
                FROM community_posts p
                WHERE p.workspace_id=$1
                  AND p.platform='reddit'
                  AND p.status='failed'
                  AND p.updated_at > now() - interval '1 day'
                GROUP BY left(coalesce(p.error_message, 'unknown'), 120)
             ) AS f
            ) AS reddit_drafts_failed,
            -- Reddit posting demand: drafts that need a live session to go
            -- out. `pending` waits for its first attempt; `rate_limited`
            -- covers transient deferrals retrying through the session gap
            -- and subreddit-cooldown holds — a dead session with work queued
            -- is worth reporting regardless of why the work is queued.
            (SELECT count(*) FROM community_posts p
             WHERE p.workspace_id=$1 AND p.status IN ('pending','rate_limited')
               AND p.platform='reddit'
            )::bigint AS reddit_posting_demand,
            -- Approvals the operator never answered. `last_error_kind` is the
            -- only thing distinguishing these from an operator's own rejection,
            -- and the claim sweep is the only writer of that value.
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='cancelled'
               AND a.last_error_kind='approval_expired'
               AND a.finished_at > now() - interval '7 days'
            )::bigint AS approvals_expired_7d,
            -- What is about to go, not only what went. Cast because EXTRACT
            -- returns numeric on PostgreSQL 14+ and sqlx cannot decode that
            -- into an integer; `sql-result-types.py` refuses an uncast one.
            (SELECT floor(EXTRACT(EPOCH FROM (min(a.approval_expires_at) - now())) / 3600)::bigint
             FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='awaiting_approval'
               AND a.approval_expires_at IS NOT NULL
            ) AS hours_to_next_approval_expiry,
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1 AND a.status='awaiting_approval'
               -- Batched relay deliveries ask through the batch card, so
               -- they are one outstanding ask, not one per community.
               AND NOT (a.action_kind='community.engage.request'
                        AND a.payload->>'source_id' IS NOT NULL)
            )::bigint AS approvals_outstanding,
            -- Past the deadline by more than the sweep's cadence and still
            -- `awaiting_approval`: neither the per-cycle claim sweep nor the
            -- hourly retention pass has reached this workspace. Two hours is
            -- two retention intervals, so a nonzero count cannot be jitter.
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1 AND a.status='awaiting_approval'
               AND a.approval_expires_at IS NOT NULL
               AND a.approval_expires_at <= now() - interval '2 hours'
            )::bigint AS unswept_lapsed_approvals,
            -- Work waiting on a capability no live executor advertises. The
            -- park sweep writes `last_error_kind='awaiting_executor'` and
            -- re-parks every cycle, so the row is current demand, not a
            -- snapshot of a moment ago.
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='queued'
               AND a.last_error_kind='awaiting_executor'
            )::bigint AS awaiting_executor_actions,
            -- The same need made permanent: parked past the 24-hour grace,
            -- cancelled with `no_executor`. Bounded at a week for the same
            -- reason the orphaned-draft count is — an alarm that can never
            -- clear is one an operator learns to ignore.
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='cancelled'
               AND a.last_error_kind='no_executor'
               AND a.finished_at > now() - interval '7 days'
            )::bigint AS no_executor_cancelled_7d,
            -- Which work classes cannot run. `action_kind` is the table's own
            -- vocabulary; the capability each kind needs lives in Rust
            -- (`executor_capability_for_payload`) and must not be re-mapped
            -- here as a second copy.
            (SELECT string_agg(DISTINCT a.action_kind, ',' ORDER BY a.action_kind)
             FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND ((a.status='queued' AND a.last_error_kind='awaiting_executor')
                 OR (a.status='cancelled' AND a.last_error_kind='no_executor'
                     AND a.finished_at > now() - interval '7 days'))
            ) AS unclaimed_action_kinds,
            (SELECT string_agg(p.context || ' until '
                               || to_char(p.guarded_until AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI')
                               || ' UTC', ', ' ORDER BY p.context)
             FROM autopilot_policies p
             WHERE p.workspace_id=$1
               AND p.guardrail_reason IS NOT NULL
               AND p.guarded_until > now()
            ) AS guarded_policies,
            (SELECT count(*) FROM autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='failed'
               AND a.approved_at IS NOT NULL
               -- A person's approval: `policy:bounded_auto` and
               -- `system:team-router` approve the machine's own work.
               AND a.approved_by LIKE 'operator%'
               AND a.finished_at > now() - interval '24 hours'
            )::bigint AS approved_failed_24h,
            (SELECT string_agg(failure.kinds || ' ×' || failure.n::text, ', ' ORDER BY failure.n DESC)
             FROM (
                 SELECT a.action_kind || ': ' || COALESCE(a.last_error_kind, 'unknown') AS kinds,
                        count(*) AS n
                 FROM autopilot_actions a
                 WHERE a.workspace_id=$1
                   AND a.status='failed'
                   AND a.approved_at IS NOT NULL
                   AND a.approved_by LIKE 'operator%'
                   AND a.finished_at > now() - interval '24 hours'
                 GROUP BY 1
             ) AS failure
            ) AS approved_failed_summary,
            -- Filled in by the guarded follow-up below. The real values live
            -- in `agent_service_credentials`, which the agents service owns —
            -- on a CrowdRelay-only deployment the relation does not exist,
            -- and naming it here would abort the whole snapshot (and every
            -- condition the watchdog evaluates) with it. Defaults read
            -- "no usable session" — honest, since nothing can post.
            0::bigint AS reddit_session_usable,
            NULL::text AS reddit_credential_status,
            NULL::text AS reddit_credential_error,
            (SELECT count(*) FROM autopilot_decisions d
             WHERE d.workspace_id=$1)::bigint AS decisions_total,
            -- The brain's own count, read out of its checkpoint. `fans.global.n`
            -- is the observation count on the pooled posterior; it is zero until
            -- a resolved outcome reaches it. Text-extracted then cast, because a
            -- checkpoint written before this level existed has no such key and
            -- must read NULL rather than fail the whole snapshot.
            (SELECT (bs.state #>> '{fans,global,n}')::bigint
             FROM brain_state bs
             WHERE bs.workspace_id=$1 AND bs.module='causal_model'
            ) AS causal_observations,
            -- A drop inside its surge window with no fan-facing receipt. The
            -- source predicate mirrors `drop_surge_eligible`: fresh or
            -- recently promoted, promotable (a URL to point fans at), and at
            -- least one surge lane raised three-plus hours ago — the grace
            -- that covers executor poll lag and a normal approval cadence.
            -- The receipt check asks the honest question — did anything a
            -- fan can see land — through every lane's own proof: post rows
            -- bound via the posting action's source_id, push deliveries and
            -- the email campaign bound via the surge lane's action key.
            (SELECT jsonb_agg(jsonb_build_object(
                'title', left(stalled.title, 80),
                'age_hours', round(EXTRACT(EPOCH FROM (now() - stalled.occurred_at)) / 3600.0, 1),
                'lanes', stalled.lanes))
             FROM (
                SELECT source.id, source.title, source.occurred_at,
                       (SELECT string_agg(split_part(a.idempotency_key, ':', 4) || '=' || a.status, ', ')
                        FROM autopilot_actions a
                        WHERE a.workspace_id = source.workspace_id
                          AND a.idempotency_key LIKE 'action:drop_surge:' || source.id::text || ':%') AS lanes
                FROM content_sources AS source
                WHERE source.workspace_id = $1
                  AND source.active
                  AND source.source_kind IN ('video','release')
                  AND source.occurred_at <= now()
                  AND source.expires_at > now()
                  AND COALESCE(source.metadata->>'url', '') LIKE 'http%'
                  AND (
                      source.occurred_at >= now() - make_interval(hours =>
                          COALESCE((SELECT (p.config->>'drop_surge_hours')::int
                                    FROM autopilot_policies p
                                    WHERE p.workspace_id = $1
                                      AND p.context = 'content_supply'
                                      AND p.enabled),
                                   72))
                      OR (source.metadata->>'surge_requested_at' ~ '^\d{4}-\d{2}-\d{2}T'
                          AND (source.metadata->>'surge_requested_at')::timestamptz
                              >= now() - interval '24 hours')
                  )
                  AND (SELECT min(a.created_at) FROM autopilot_actions a
                       WHERE a.workspace_id = source.workspace_id
                         AND a.idempotency_key LIKE 'action:drop_surge:' || source.id::text || ':%')
                      <= now() - interval '3 hours'
                  AND NOT EXISTS (
                      SELECT 1 FROM autopilot_actions act
                      WHERE act.workspace_id = source.workspace_id
                        AND (act.payload->>'source_id' = source.id::text
                             OR act.payload->'draft'->>'source_id' = source.id::text)
                        AND (
                            EXISTS (SELECT 1 FROM social_posts p WHERE p.workspace_id=act.workspace_id AND p.action_id=act.id AND p.status='posted')
                            OR EXISTS (SELECT 1 FROM telegram_posts p WHERE p.workspace_id=act.workspace_id AND p.action_id=act.id AND p.status='posted')
                            OR EXISTS (SELECT 1 FROM discord_posts p WHERE p.workspace_id=act.workspace_id AND p.action_id=act.id AND p.status='posted')
                            OR EXISTS (SELECT 1 FROM community_posts p WHERE p.workspace_id=act.workspace_id AND p.action_id=act.id AND p.status='posted')
                        )
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM autopilot_actions act
                      JOIN fan_push_deliveries d
                        ON d.workspace_id = act.workspace_id AND d.source_id = act.id
                      WHERE act.workspace_id = source.workspace_id
                        AND act.idempotency_key LIKE 'action:drop_surge:' || source.id::text || ':signal_push%'
                        AND d.status <> 'failed'
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM autopilot_actions act
                      JOIN outbox_events e
                        ON e.workspace_id = act.workspace_id AND e.action_id = act.id
                      JOIN communication_campaigns c
                        ON c.workspace_id = e.workspace_id AND c.dispatch_event_id = e.id
                      WHERE act.workspace_id = source.workspace_id
                        AND act.idempotency_key LIKE 'action:drop_surge:' || source.id::text || ':email%'
                        AND c.status IN ('scheduled','completed')
                  )
             ) AS stalled
            ) AS stalled_drops
        FROM executor_instances WHERE workspace_id=$1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(UNKNOWN_ALERT_AGE_THRESHOLD.as_secs() as i64)
    .bind(RELENTLESS_CYCLE_WINDOW)
    .fetch_one(&mut **transaction)
    .await?;

    // Whether session material exists that can actually establish a posting
    // session. Cookies alone cannot: the agents service's establishSession
    // refuses before seeding them when no credential row is eligible.
    // Mirrors getRedditCredentials' eligibility — 'active', or 'cooldown'
    // past its six-hour window (LOGIN_COOLDOWN_HOURS in crowdrelay-agents).
    //
    // `agent_service_credentials` is foreign to this deployment — probed
    // first so its absence skips the read instead of aborting the
    // transaction (same pattern as receipt_reconciliation's task sweep).
    // When it is absent the defaults stand: no credential service means no
    // session can post, so `usable = 0` is the honest reading.
    let credentials_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('agent_service_credentials')::text")
            .fetch_one(&mut **transaction)
            .await?;
    if credentials_table.is_some() {
        let row: (i64, Option<String>, Option<String>) = sqlx::query_as(
            r#"
            SELECT
                count(*) FILTER (
                    WHERE c.status='active'
                       OR (c.status='cooldown'
                           AND (c.last_validated_at IS NULL
                                OR c.last_validated_at < now() - interval '6 hours'))
                )::bigint,
                (SELECT c2.status FROM agent_service_credentials c2
                 WHERE c2.workspace_id=$1 AND c2.provider='reddit-browser'),
                (SELECT left(c3.last_validation_error, 200) FROM agent_service_credentials c3
                 WHERE c3.workspace_id=$1 AND c3.provider='reddit-browser')
            FROM agent_service_credentials c
            WHERE c.workspace_id=$1 AND c.provider='reddit-browser'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        snapshot.reddit_session_usable = row.0;
        snapshot.reddit_credential_status = row.1;
        snapshot.reddit_credential_error = row.2;
    }
    Ok(snapshot)
}
