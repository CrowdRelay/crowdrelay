// The Prometheus snapshot: queue health, worker liveness and brain health in
// one round trip.
//
// Split out of `query_support.rs`, which crossed the 1000-line chunk limit
// when the brain gauges arrived. This is one query and one mapping, and the
// only read on that file serving a scrape rather than an operator page, so it
// is the piece that leaves.

async fn load_metrics_snapshot(state: &OpsState) -> Result<OpsMetricsSnapshot, OpsError> {
    // Prometheus scrapes do not need the 24-hour delivered counters used by the
    // admin summary. Keep this query narrow to reduce CPU and buffer churn.
    let row = sqlx::query_as::<_, OpsMetricsRow>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                count(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries
            WHERE workspace_id = $1
        ),
        push AS (
            SELECT
                count(*) FILTER (WHERE status IN ('queued','retry_wait'))::bigint AS pending,
                count(*) FILTER (WHERE status IN ('claimed','provider_started','provider_accepted'))::bigint AS processing,
                count(*) FILTER (WHERE status IN ('failed','ambiguous') AND error_code IS DISTINCT FROM 'preference_disabled')::bigint AS dead,
                count(*) FILTER (WHERE status = 'failed' AND error_code = 'preference_disabled')::bigint AS suppressed,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status IN ('queued','retry_wait') AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM fan_push_deliveries
            WHERE workspace_id = $1
        ),
        -- Worker liveness, read from the leadership lease.
        --
        -- The worker exposes no HTTP surface, so Prometheus cannot scrape it
        -- and `up{job=...}` does not exist for it. It does renew this lease
        -- every 15 seconds, which makes the lease age the one honest heartbeat
        -- available — and the API, which is scraped, can read it.
        --
        -- Without this the process running the entire brain, all outbox
        -- delivery and every metric sync could die unnoticed. It did: killed by
        -- a deploy, dead for over fifteen minutes, and nothing said so.
        --
        -- Not workspace-scoped; leadership is per deployment, not per tenant.
        -- No row at all reads as maximally stale rather than as healthy.
        worker AS (
            SELECT COALESCE((
                SELECT EXTRACT(EPOCH FROM (
                    now() - (expires_at - INTERVAL '60 seconds')
                ))::bigint
                FROM worker_leadership WHERE id = 1
            ), 999999) AS lease_age_seconds
        ),
        -- Brain health, on the same unauthenticated surface as queue health.
        --
        -- Whether the brain is cycling, deciding, acting and learning was
        -- readable only through the authenticated control plane, so answering
        -- "is it actually doing anything" needed a key and, when the key was
        -- wrong, a database shell on the host. Everything else that can stall
        -- silently -- outbox, deliveries, push, the worker's own heartbeat --
        -- is already a gauge here. The brain is the part that matters most and
        -- was the only part invisible.
        --
        -- Cycles, decisions and actions are 24-hour windows because the
        -- question they answer is "is it running now", and a lifetime total
        -- cannot answer that: a brain that stopped a week ago reports the same
        -- large number as one that is working. Measurements are lifetime,
        -- because a measurement scheduled a fortnight ago and still unresolved
        -- is exactly the thing worth seeing.
        brain AS (
            SELECT
                (SELECT count(*) FROM viryaos_autopilot_cycle_runs
                 WHERE workspace_id = $1 AND started_at > now() - INTERVAL '24 hours'
                )::bigint AS cycles_24h,
                (SELECT count(*) FROM viryaos_autopilot_cycle_runs
                 WHERE workspace_id = $1 AND started_at > now() - INTERVAL '24 hours'
                   AND outcome <> 'succeeded'
                )::bigint AS cycles_degraded_24h,
                -- Seconds since the last cycle started. The cycle runs every
                -- five minutes, so this crossing a few hundred means the brain
                -- has stopped thinking even while the worker lease looks fine:
                -- the two failures are different and this tells them apart.
                COALESCE((SELECT EXTRACT(EPOCH FROM (now() - max(started_at)))::bigint
                 FROM viryaos_autopilot_cycle_runs WHERE workspace_id = $1
                ), 999999) AS seconds_since_cycle,
                (SELECT count(*) FROM viryaos_autopilot_decisions
                 WHERE workspace_id = $1 AND evaluated_at > now() - INTERVAL '24 hours'
                )::bigint AS decisions_24h,
                (SELECT count(*) FROM viryaos_autopilot_actions
                 WHERE workspace_id = $1 AND created_at > now() - INTERVAL '24 hours'
                )::bigint AS actions_24h,
                (SELECT count(*) FROM viryaos_autopilot_actions
                 WHERE workspace_id = $1 AND created_at > now() - INTERVAL '24 hours'
                   AND status = 'failed'
                )::bigint AS actions_failed_24h,
                (SELECT count(*) FROM viryaos_autopilot_measurements
                 WHERE workspace_id = $1 AND status = 'pending'
                )::bigint AS measurements_pending,
                (SELECT count(*) FROM viryaos_autopilot_measurements
                 WHERE workspace_id = $1 AND status = 'succeeded'
                )::bigint AS measurements_resolved,
                -- Age of the oldest measurement that is due and has not
                -- resolved. Zero when nothing is overdue -- a measurement
                -- waiting for its horizon is not late, it is early, and
                -- counting it would make a healthy brain look stuck for the
                -- fourteen days its longest window legitimately takes.
                COALESCE((SELECT EXTRACT(EPOCH FROM (now() - min(due_at)))::bigint
                 FROM viryaos_autopilot_measurements
                 WHERE workspace_id = $1 AND status = 'pending' AND due_at <= now()
                ), 0) AS measurement_oldest_overdue_seconds,
                -- Agent outcomes are the LLM half of the loop. Rejected means
                -- the worker produced something the data-quality gate refused,
                -- which is invisible in a task-level success count.
                (SELECT count(*) FROM agent_outcomes
                 WHERE workspace_id = $1 AND created_at > now() - INTERVAL '24 hours'
                   AND status = 'processed'
                )::bigint AS agent_outcomes_processed_24h,
                (SELECT count(*) FROM agent_outcomes
                 WHERE workspace_id = $1 AND created_at > now() - INTERVAL '24 hours'
                   AND status = 'rejected'
                )::bigint AS agent_outcomes_rejected_24h,
                -- Communities the brain wants to post to and cannot, because
                -- nobody has joined them. Joining is a prerequisite of posting,
                -- so an unjoined community with a promoted target on it is a
                -- post the brain has already decided it wants and cannot
                -- dispatch. The predicate is the join worker's own definition
                -- of demand, so both surfaces agree on what "wanted" means.
                --
                -- This is the number that explains an idle-looking Reddit
                -- channel: production carried 119 discovered communities, none
                -- joined, and every candidate was gated out with nothing said.
                (SELECT count(*)
                   FROM discovery_places AS place
                  WHERE place.workspace_id = $1
                    AND place.place_kind = 'subreddit'
                    AND place.membership_state = 'not_joined'
                    AND place.status = 'active'
                    AND EXISTS (
                          SELECT 1 FROM agent_outreach_targets AS t
                           WHERE t.workspace_id = place.workspace_id
                             AND t.place_id = place.id
                             AND t.status = 'promoted'
                             AND t.target_kind = 'community'
                             AND t.subreddit IS NOT NULL
                             AND t.screening_verdict IS DISTINCT FROM 'refused'
                        )
                )::bigint AS communities_blocked_on_join,
                -- The two numbers that make the one above readable.
                --
                -- `blocked_on_join` reaching zero has two opposite meanings:
                -- every wanted community was joined, or every wanted community
                -- was lost. Production hit the second -- 71 places went to
                -- `rejected` in one hour because our own agent service
                -- answered /reddit/join with 502 and 503, and `rejected` is
                -- terminal because the join worker only ever claims
                -- `not_joined`. The gauge read zero and green while the entire
                -- Reddit channel was being retired.
                --
                -- A gauge that cannot tell "solved" from "given up on" is
                -- worse than no gauge, so the denominator ships beside it.
                (SELECT count(*) FROM discovery_places
                  WHERE workspace_id = $1 AND place_kind = 'subreddit'
                    AND membership_state = 'joined'
                )::bigint AS communities_joined,
                (SELECT count(*) FROM discovery_places
                  WHERE workspace_id = $1 AND place_kind = 'subreddit'
                    AND membership_state = 'rejected'
                )::bigint AS communities_rejected,
                -- ── The loop-closed canary ──
                --
                -- Every gauge above says the brain is busy. None of them says
                -- it is getting better, and those are different claims: 290
                -- cycles a day, 58 decisions and 27 actions are all compatible
                -- with a system that has learned nothing at all. Production is
                -- that case -- 54 evidence rows, none resolved, causal model
                -- n=0 -- and the only way to see it was a database shell.
                --
                -- A resolved evidence row is the single event proving the full
                -- loop closed: predicted, dispatched, executed, measured,
                -- observed. Its count and its age answer "is this working"
                -- without a credential, in days rather than in however long it
                -- takes somebody to get suspicious.
                (SELECT count(*) FROM viryaos_growth_evidence
                  WHERE workspace_id = $1 AND resolved_at IS NOT NULL
                )::bigint AS evidence_resolved,
                -- Zero means it has never happened, deliberately rather than a
                -- large sentinel: the count beside it already separates never
                -- from stale, and a fake age would poison any threshold.
                COALESCE((SELECT EXTRACT(EPOCH FROM (now() - max(resolved_at)))::bigint
                   FROM viryaos_growth_evidence WHERE workspace_id = $1
                ), 0) AS seconds_since_evidence_resolved,
                -- Throughput on the same terms. A post that reached a platform
                -- is the input the loop above cannot run without.
                COALESCE((SELECT EXTRACT(EPOCH FROM (now() - max(posted_at)))::bigint
                   FROM community_posts WHERE workspace_id = $1
                ), 0) AS seconds_since_publication
        )
        SELECT
            outbox.pending AS outbox_pending,
            outbox.processing AS outbox_processing,
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.pending AS delivery_pending,
            deliveries.processing AS delivery_processing,
            deliveries.dead AS delivery_dead,
            deliveries.cancelled AS delivery_cancelled,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds,
            push.pending AS push_pending,
            push.processing AS push_processing,
            push.dead AS push_dead,
            push.suppressed AS push_suppressed,
            push.oldest_pending_seconds AS push_oldest_pending_seconds,
            worker.lease_age_seconds AS worker_lease_age_seconds,
            brain.cycles_24h AS brain_cycles_24h,
            brain.cycles_degraded_24h AS brain_cycles_degraded_24h,
            brain.seconds_since_cycle AS brain_seconds_since_cycle,
            brain.decisions_24h AS brain_decisions_24h,
            brain.actions_24h AS brain_actions_24h,
            brain.actions_failed_24h AS brain_actions_failed_24h,
            brain.measurements_pending AS brain_measurements_pending,
            brain.measurements_resolved AS brain_measurements_resolved,
            brain.measurement_oldest_overdue_seconds AS brain_measurement_oldest_overdue_seconds,
            brain.agent_outcomes_processed_24h AS brain_agent_outcomes_processed_24h,
            brain.agent_outcomes_rejected_24h AS brain_agent_outcomes_rejected_24h,
            brain.communities_blocked_on_join AS brain_communities_blocked_on_join,
            brain.communities_joined AS brain_communities_joined,
            brain.communities_rejected AS brain_communities_rejected,
            brain.evidence_resolved AS brain_evidence_resolved,
            brain.seconds_since_evidence_resolved AS brain_seconds_since_evidence_resolved,
            brain.seconds_since_publication AS brain_seconds_since_publication
        FROM outbox CROSS JOIN deliveries CROSS JOIN push CROSS JOIN worker
             CROSS JOIN brain
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    Ok(OpsMetricsSnapshot {
        outbox_pending: row.outbox_pending,
        outbox_processing: row.outbox_processing,
        outbox_dead: row.outbox_dead,
        outbox_oldest_pending_seconds: row.outbox_oldest_pending_seconds,
        delivery_pending: row.delivery_pending,
        delivery_processing: row.delivery_processing,
        delivery_dead: row.delivery_dead,
        delivery_cancelled: row.delivery_cancelled,
        delivery_oldest_pending_seconds: row.delivery_oldest_pending_seconds,
        push_pending: row.push_pending,
        push_processing: row.push_processing,
        push_dead: row.push_dead,
        push_suppressed: row.push_suppressed,
        push_oldest_pending_seconds: row.push_oldest_pending_seconds,
        worker_lease_age_seconds: row.worker_lease_age_seconds,
        brain_cycles_24h: row.brain_cycles_24h,
        brain_cycles_degraded_24h: row.brain_cycles_degraded_24h,
        brain_seconds_since_cycle: row.brain_seconds_since_cycle,
        brain_decisions_24h: row.brain_decisions_24h,
        brain_actions_24h: row.brain_actions_24h,
        brain_actions_failed_24h: row.brain_actions_failed_24h,
        brain_measurements_pending: row.brain_measurements_pending,
        brain_measurements_resolved: row.brain_measurements_resolved,
        brain_measurement_oldest_overdue_seconds: row.brain_measurement_oldest_overdue_seconds,
        brain_agent_outcomes_processed_24h: row.brain_agent_outcomes_processed_24h,
        brain_agent_outcomes_rejected_24h: row.brain_agent_outcomes_rejected_24h,
        brain_communities_blocked_on_join: row.brain_communities_blocked_on_join,
        brain_communities_joined: row.brain_communities_joined,
        brain_communities_rejected: row.brain_communities_rejected,
        brain_evidence_resolved: row.brain_evidence_resolved,
        brain_seconds_since_evidence_resolved: row.brain_seconds_since_evidence_resolved,
        brain_seconds_since_publication: row.brain_seconds_since_publication,
    })
}

