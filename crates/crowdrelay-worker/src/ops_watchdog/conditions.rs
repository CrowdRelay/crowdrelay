// The conditions the watchdog reports, and the reasoning behind each severity.
//
// `include!`d into `ops_watchdog.rs` so it shares that module's scope, matching
// how `agent_outcomes.rs` carries its quality guard. Split out to keep the parent
// inside the source-size ratchet, and because this is the one part a reader comes
// to the watchdog for: what counts as worth waking somebody for, and why each
// answer is a warning rather than critical.

fn conditions(snapshot: &OpsSnapshot) -> Vec<Condition> {
    vec![
        Condition {
            // Critical, and deliberately not a warning. Nothing downstream of
            // this works: an outcome that is refused never becomes an action,
            // so no post is drafted, no artifact is published, and the
            // measurement path correctly declines to resolve evidence for a
            // dispatch that never reached anybody. Every posterior stays on
            // its prior. The brain is not degraded, it is disconnected, and no
            // other condition on this list can tell you so.
            //
            // The predicate needs both halves. Refusals alongside healthy
            // traffic are a verifier doing its job on bad output; refusals
            // with nothing accepted is the loop stopped.
            key: "learning.outcomes_unverified",
            severity: "critical",
            summary: "Every agent outcome is being refused for want of a grounding check",
            active: snapshot.outcomes_rejected_unverified > 0 && snapshot.outcomes_accepted == 0,
            details: json!({
                "rejected_unverified": snapshot.outcomes_rejected_unverified,
                "accepted": snapshot.outcomes_accepted,
                // Every guard that fired, not only this one. The reason was
                // written on each refused row and read by nothing, so a count of
                // four refusals could not be told apart from a dead connector,
                // one bad model answer, or a broken verifier.
                "guards_fired": snapshot.outcome_rejection_reasons,
                "window": "1 day",
                "remedy": "check the agent service's verifier: the reason is in \
                           agent_outcomes.payload->'provenance'->'verification'->>'verifier_error'",
            }),
        },
        Condition {
            // Warning, not critical: measurement already refuses to score
            // these, so nothing is being corrupted and no wrong lesson is
            // learned. What is lost is the work — a draft nobody will ever
            // publish, and a dispatch budget spent on it.
            key: "publishing.orphaned_draft",
            severity: "warning",
            summary: "A publishing action succeeded but no executor produced a post",
            active: snapshot.orphaned_publishing_actions > 0,
            details: json!({
                "orphaned_actions": snapshot.orphaned_publishing_actions,
                "orphaned_actions_all_time": snapshot.orphaned_publishing_actions_all_time,
                "window": "7 days",
                "remedy": "an executor's claim predicate does not cover this draft — \
                           compare the agent task's template_id and the draft's \
                           platform against the three executors' WHERE clauses",
            }),
        },
        Condition {
            // Critical, because unlike `publishing.orphaned_draft` this one has
            // already spent everything: the outcome was verified, the action was
            // created, the event was built with its recipient and handed to the
            // outbox — and the consumer refused it permanently. The work is done
            // and lands nowhere.
            //
            // It was invisible, and the reason is worth keeping. A 4xx is
            // `http_permanent_status`, so the outbox correctly stops retrying:
            // the delivery is `cancelled`, never `dead`. `ops/attention` reports
            // dead deliveries and a bare `cancelled` count, so a permanently
            // refused growth event showed up as one increment in a number with
            // no breakdown. Measured in production 2026-09-13: four
            // `agent.content_requested` deliveries refused 422 by
            // n8n.virya.music, the newest that day, alongside 485 delivered —
            // and `crowdrelay.agent.content_requested` appears in no n8n
            // example workflow or executor contract in this repository, so the
            // consumer has most likely never known the event.
            key: "delivery.growth_event_refused",
            severity: "critical",
            summary: "A growth event was permanently refused by its consumer",
            active: snapshot.refused_growth_deliveries > 0,
            details: json!({
                "refused_deliveries": snapshot.refused_growth_deliveries,
                "window": "7 days",
                "remedy": "join webhook_deliveries to outbox_events on \
                           outbox_event_id for status='cancelled' and read \
                           last_response_status: 4xx is the consumer refusing \
                           the payload, not the outbox failing to send it",
            }),
        },
        Condition {
            // Warning, not critical: nothing is corrupted and no wrong lesson is
            // learned. What is happening is that real work sits untouched — the
            // band's own festival and competition list, scored by the brain and
            // then refused on a threshold no surface reported.
            //
            // Derived from the brain's own denied decisions rather than from a
            // guess at why. The first version counted rows missing strategic value
            // and logistics, which was true that morning; an import filled
            // strategic value and the alarm went quiet while every opportunity
            // stayed held on the confidence gate instead. A proxy for a hold stops
            // tracking the hold.
            //
            // Actionable in three ways, and all three are judgement rather than
            // code: assert the festival's standing through
            // `reputation_basis_points`, which is 7 of 15 points at its default and
            // the only score component still unclaimed; lower
            // `minimum_confidence`, remembering that it is what turns a score floor
            // of 65 into an effective 70; or lower `minimum_score` so less reaches
            // scoring at all.
            key: "growth.unscoreable_live_opportunities",
            severity: "warning",
            summary: "The brain scored live opportunities and denied every one",
            active: snapshot.unscoreable_live_opportunities > 0,
            details: json!({
                "denied_decisions": snapshot.unscoreable_live_opportunities,
                "window": "1 day",
                "remedy": "a live opportunity's confidence is 7500 + (score - \
                           minimum_score) * 100, and `disposition()` denies below \
                           minimum_confidence — so minimum_confidence 8000 against \
                           minimum_score 65 means the real floor is 70. Raise \
                           reputation_basis_points on the opportunities worth \
                           playing, or move one of the two thresholds",
            }),
        },
        Condition {
            // Warning: nothing has gone out yet, which is the whole point of
            // catching it here. Publishing both is the fault, and it has not
            // happened — Reddit posting is manual, so the queue is the last place
            // this is still cheap to fix.
            key: "publishing.duplicate_community_draft",
            severity: "warning",
            summary: "Two unpublished drafts target the same community",
            active: snapshot.duplicate_community_drafts > 0,
            details: json!({
                "redundant_drafts": snapshot.duplicate_community_drafts,
                "remedy": "group community_posts by lower(subreddit) where status \
                           is awaiting_manual_post; publish one and cancel the \
                           rest. Reddit is case-insensitive, so r/MetalMemes and \
                           r/metalmemes are one place and posting both is posting \
                           twice under the band's name",
            }),
        },
        Condition {
            // Critical, because this is the one reading that separates the two
            // meanings of a degraded cycle. Phase isolation exists so that one
            // phase failing cannot stop already-authorized work, and a
            // transient failure being absorbed is the design working — that is
            // why `degraded` is not `failed`. A phase that has failed in every
            // cycle for an hour is not being absorbed; that part of the brain
            // has simply stopped, and every cycle since has silently done less
            // than it reported.
            //
            // Production measured why this was needed: 296 cycles in 24 hours,
            // 40 degraded, and no way to tell which of the two situations that
            // was without grepping worker logs by timestamp — so the answer
            // expired with the logs.
            //
            // Consecutiveness rather than a share, deliberately. What fraction
            // of cycles counts as broken is arbitrary and would need tuning
            // against a number nobody has; a phase failing every cycle for an
            // hour is not transient under any reading.
            key: "brain.phase_failing_every_cycle",
            severity: "critical",
            summary: "A cycle phase has failed in every recent cycle",
            active: snapshot.relentless_degraded_phases.is_some(),
            details: json!({
                "phases": snapshot.relentless_degraded_phases,
                "consecutive_cycles": RELENTLESS_CYCLE_WINDOW,
                "remedy": "read /v1/admin/ops/cycles?state=degraded for the phase \
                           names, then the worker log for that phase's own warning \
                           line — each phase logs its cause before the cycle \
                           reports itself degraded",
            }),
        },
        Condition {
            // Critical, and deliberately not conditioned on anything else
            // failing. The guard refused it, so nothing was sent and no fan was
            // harmed — which is exactly why this would otherwise be invisible.
            //
            // `signal_push.target_path` is an in-app route. A model writing an
            // absolute URL or a scheme there proposes to send the whole fanbase
            // to a destination nobody approved, and the approval click shows the
            // copy rather than the link, so a human reviewer would not see it
            // either. The guard is the only thing between that proposal and the
            // audience, and a model producing it repeatedly is a fact about the
            // prompt or the model, not a transient data error.
            //
            // Counted over one day. A single refusal months ago is history; one
            // today means the next one is coming.
            key: "safety.off_platform_push_proposed",
            severity: "critical",
            summary: "A proposed Signal push would have sent fans off-platform",
            active: snapshot.off_platform_push_attempts > 0,
            details: json!({
                "attempts": snapshot.off_platform_push_attempts,
                "window": "1 day",
                "remedy": "read agent_outcomes where rejection_reason LIKE \
                           'OFF_PLATFORM_PUSH_TARGET%' for the target the model \
                           wrote, then fix the prompt or template that produced \
                           it. Nothing was sent; the guard refused it.",
            }),
        },
        Condition {
            key: "executor.offline",
            severity: "critical",
            summary: "ViryaOS executor registry has no live executor",
            active: snapshot.executor_registered > 0 && snapshot.executor_active == 0,
            details: json!({
                "registered": snapshot.executor_registered,
                "active": snapshot.executor_active,
            }),
        },
        Condition {
            key: "execution.unknown_outcome",
            severity: "warning",
            summary: "Autopilot actions stuck in unknown execution outcome",
            active: snapshot.stale_unknown_actions > 0,
            details: json!({
                "unknown_actions": snapshot.unknown_actions,
                "stale_unknown_actions": snapshot.stale_unknown_actions,
            }),
        },
        Condition {
            key: "execution.contradicted_outcome",
            // Warning, not critical: the state machine already refused to
            // act on the contradiction, so nothing is being corrupted. What
            // is missing is a person deciding which source was right.
            severity: "warning",
            summary: "Autopilot action status contradicted by its newest executor receipt",
            active: snapshot.contradicted_actions > 0,
            details: json!({
                "contradicted_actions": snapshot.contradicted_actions,
            }),
        },
        Condition {
            key: "growth.feed_failing",
            // Warning: the tenant still has a channel the brain can work
            // through, and nothing is being lost while this stands.
            severity: "warning",
            summary: "A growth feed's last sync attempt failed",
            active: snapshot.failing_platforms > 0 && snapshot.working_platforms > 0,
            details: json!({
                "failing_platforms": snapshot.failing_platforms,
                "working_platforms": snapshot.working_platforms,
            }),
        },
        Condition {
            // Every feed the tenant has is failing. The brain will not plan
            // discovery through them -- see
            // `GrowthStrategy::discovery_channels_are_silent` -- so the top of
            // the funnel is shut until a person restores a credential. There is
            // no automatic recovery from this and nothing else on this list
            // says it.
            //
            // Critical rather than warning, and separate from
            // `growth.feed_failing` rather than an escalation of it, because
            // the two ask for different things: one is a feed to repair, this
            // is the whole acquisition side of the North Star stopped.
            key: "growth.all_feeds_failing",
            severity: "critical",
            summary: "Every growth feed is failing — the brain cannot discover through any channel",
            active: snapshot.failing_platforms > 0 && snapshot.working_platforms == 0,
            details: json!({
                "failing_platforms": snapshot.failing_platforms,
            }),
        },
        Condition {
            // Cities that fans requested but the geocoder gave up on. Every
            // fan sitting in one is unreachable by the nearby-show loop — the
            // only automatic reason an installed app reopens itself — and
            // nothing recovers this on its own. The geocoding worker may be
            // disabled (missing contact config) or the provider may not
            // recognize the name; either way a human must fix it.
            //
            // Warning, not critical: the nearby-show loop still reaches fans
            // in geocoded cities, and the stuck cities are a growing gap, not
            // a total stop.
            key: "growth.stuck_ungeocoded_cities",
            severity: "warning",
            summary: "Fan-requested cities have exhausted geocoding — fans there are unreachable by nearby-show",
            // Both counts come from the same predicate, so a city only
            // reaches this finding when a fan is behind it. The summary's
            // claim is now load-bearing rather than decorative.
            active: snapshot.stuck_ungeocoded_cities > 0,
            details: json!({
                "stuck_ungeocoded_cities": snapshot.stuck_ungeocoded_cities,
                "fans_awaiting_geocoding": snapshot.fans_awaiting_geocoding,
            }),
        },
    ]
}
