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
            // band's own festival and competition list, imported and then held
            // every cycle for a reason no surface reported.
            //
            // Actionable in exactly two ways, which is why it is worth an alert:
            // enrich the rows so the unreachable 40 points become reachable
            // (strategic value, or a city and distance so the trip can be
            // costed), or lower `minimum_score` for a tenant whose opportunities
            // legitimately arrive without either. Both are decisions; neither can
            // be made while the hold is invisible.
            key: "growth.unscoreable_live_opportunities",
            severity: "warning",
            summary: "Live opportunities are held because their score ceiling is below the bar",
            active: snapshot.unscoreable_live_opportunities > 0,
            details: json!({
                "unscoreable": snapshot.unscoreable_live_opportunities,
                "score_ceiling": 60,
                "minimum_score": 65,
                "remedy": "fit, reputation and confidence together cap at 60 of \
                           100; the missing 40 are strategic_value_basis_points \
                           and the economics score, which needs distance_km and \
                           nights_away to cost a trip. Fill either, or lower \
                           minimum_score in the live_opportunity policy",
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
