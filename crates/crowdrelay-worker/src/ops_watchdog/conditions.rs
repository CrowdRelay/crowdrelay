// The conditions the watchdog reports, and the reasoning behind each severity.
//
// `include!`d into `ops_watchdog.rs` so it shares that module's scope, matching
// how `agent_outcomes.rs` carries its quality guard. Split out to keep the parent
// inside the source-size ratchet, and because this is the one part a reader comes
// to the watchdog for: what counts as worth waking somebody for, and why each
// answer is a warning rather than critical.

fn conditions(snapshot: &OpsSnapshot, posture: PublishingPosture) -> Vec<Condition> {
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
            //
            // The count counts letters by what they carry, not by a type list:
            // `contact_email`, `recipient_email` or `recipients` in the payload
            // means a specific human was the addressee. 2026-09-15 showed why —
            // an approved festival application and the T+7 show report both
            // died 422 cancelled while a two-type list watched neither.
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
            // Warning, not critical: these refused deliveries carry no named
            // recipient — status pings, internal markers, unrouted events that
            // never drafted anything. The same cancelled-instead-of-dead blind
            // spot hides them, so the count exists for visibility; the letters
            // above keep the critical alarm.
            key: "delivery.event_refused",
            severity: "warning",
            summary: "Deliveries were permanently refused by their consumers",
            active: snapshot.refused_other_deliveries > 0,
            details: json!({
                "refused_deliveries": snapshot.refused_other_deliveries,
                "window": "7 days",
                "remedy": "join webhook_deliveries to outbox_events on \
                           outbox_event_id for status='cancelled' — a refused \
                           non-letter event is usually a stale consumer \
                           contract or a route pin pointing at a workflow that \
                           never learned the event type",
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
                "remedy": "group community_posts by normalize_subreddit(subreddit) where status \
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
            // Warning, not critical: nothing is broken and nothing is lost. The
            // drafts are intact and an operator can publish them by hand today.
            //
            // It exists because the operator has no other way to learn this.
            // Reddit publishing needs three switches, the write switch is
            // checked first and overrides the other two, and it was in no
            // `.env.example` — so somebody who approved every suggestion and
            // believed they had set autopilot everywhere watched drafts pile up
            // with nothing anywhere naming what was missing. The growth
            // readiness log reported the community executor as enabled, because
            // it reported that the worker had been constructed rather than that
            // it would post.
            //
            // Both halves are required. Drafts with no publisher is the fault;
            // an empty queue with publishing off is a setting, and a manual
            // channel with nothing waiting is nothing to report.
            key: "publishing.drafts_with_no_publisher",
            severity: "warning",
            summary: "Reddit drafts are waiting and nothing will publish them",
            active: snapshot.reddit_drafts_waiting > 0 && !posture.reddit.publishes(),
            details: json!({
                "drafts_waiting": snapshot.reddit_drafts_waiting,
                "missing_switch": posture.reddit.missing_switch(),
                "remedy": "either publish them by hand and register each URL \
                           through POST /v1/control-plane/community-posts/{id}/\
                           register-manual, or set the named switch in \
                           deploy/.env.production and restart the worker. \
                           Reddit needs CROWDRELAY_REDDIT_WRITE_ENABLED and \
                           CROWDRELAY_COMMUNITY_AUTO_POST together, plus an \
                           agent-service key — the write switch is checked \
                           first, so the other two have no effect without it.",
            }),
        },
        Condition {
            // Warning: the content still exists in the row, so this is
            // recoverable — but only by a person, and only if they know it
            // happened. The brain cannot recover it: the parent action is
            // terminal once a draft fails, and the seven-day subreddit cooldown
            // stops it drafting that community again.
            //
            // The reason is the whole point. "Reddit refused this" and "the
            // agents service was unreachable" call for opposite responses, and
            // `error_message` was written on every failed row and read by
            // nothing.
            key: "publishing.drafts_failed",
            severity: "warning",
            summary: "Reddit drafts failed and the content is not being retried",
            active: snapshot.reddit_drafts_failed.is_some(),
            details: json!({
                "reasons": snapshot.reddit_drafts_failed,
                "window": "1 day",
                "remedy": "read the reasons. A transport error or an unreachable \
                           agents service means the content is fine and can be \
                           requeued by setting community_posts.status back to \
                           'pending' and the parent action back to 'succeeded'. \
                           A refusal from Reddit means the content or the account \
                           needs attention first — requeuing would repeat it.",
            }),
        },
        Condition {
            // Warning, not critical: drafts survive for a while — the
            // executor defers a session failure and retries for about an
            // hour before giving up — and nothing is corrupted. But the
            // demand is real and nothing fixes it alone: `invalid` needs a
            // new credential, a missing row needs a first one, and a
            // six-hour cooldown outlives the retry cover, so every draft in
            // the queue dies unless a person acts inside the window.
            //
            // The predicate is the agents service's own eligibility rule,
            // not a proxy: an 'active' reddit-browser credential, or one in
            // a cooldown whose window has passed. Stored cookies alone do
            // not count — establishSession refuses to use them without a
            // credential, so an 'active' cookie row beside a dead
            // credential is a session that still cannot post.
            key: "publishing.session_dead",
            severity: "warning",
            summary: "Reddit work is queued and no session can post it",
            active: snapshot.reddit_posting_demand > 0 && snapshot.reddit_session_usable == 0,
            details: json!({
                "drafts_waiting": snapshot.reddit_posting_demand,
                "credential_status": snapshot.reddit_credential_status,
                "credential_error": snapshot.reddit_credential_error,
                "remedy": "status 'invalid' means Reddit rejected the password \
                           — store fresh credentials via POST /reddit/credentials \
                           on the agents service. No row at all means credentials \
                           were never stored. 'cooldown' means a challenge or a \
                           crash — it clears itself, but drafts only retry for \
                           about an hour. After fixing, failed drafts can be \
                           requeued by setting community_posts.status back to \
                           'pending'.",
            }),
        },
        Condition {
            // Critical, and it reports the one failure nothing else can see: not
            // that something broke, but that something never started.
            //
            // The causal model learns only from resolved evidence. Until an
            // outcome resolves, every prediction it makes IS the prior —
            // `DEFAULT_EXPECTED_FANS`, a number chosen once and never checked
            // against this tenant. The brain then ranks, dispatches and reports
            // confidence against a belief no observation has ever touched, and
            // every other signal looks healthy while it happens: cycles succeed,
            // decisions are written, actions are created.
            //
            // Measured production state that motivated this: 733 decisions, 0
            // resolved evidence, empty strategy posterior, 0 belief revisions,
            // and 22 fans none of which any action can claim. The prior says two
            // per dispatch. Nothing anywhere said the number was untested.
            //
            // It is also self-reinforcing. An uncorrected optimistic prior keeps
            // `has_positive_candidates` true, which lets `min_dispatches`
            // override WAIT every cycle, which spends the budget on candidates
            // whose value is an assumption.
            //
            // Both halves are required. A young workspace with few decisions and
            // no observations is a system that has not run yet, not one that
            // cannot learn.
            key: "learning.posterior_never_updated",
            severity: "critical",
            summary: "The brain has never corrected its causal prior with an observation",
            active: snapshot.decisions_total >= DECISIONS_BEFORE_LEARNING_EXPECTED
                && snapshot.causal_observations.unwrap_or(0) == 0,
            details: json!({
                "decisions": snapshot.decisions_total,
                "causal_observations": snapshot.causal_observations,
                "checkpoint_present": snapshot.causal_observations.is_some(),
                "remedy": "every prediction the brain makes is currently its prior. \
                           Fan outcomes count only fans traced to an action's own \
                           tracked links (since #325); a dispatch whose posts carry \
                           no live tracked link is abandoned as no_tracked_link and \
                           teaches nothing, and rows measured before that change \
                           (outcome_basis = workspace_window) are not learned from. \
                           Correction needs a tracked post to go live and a fan to \
                           sign up through it — check publishing and tracked-link \
                           coverage first. Until then, treat the brain's rankings \
                           as assumptions.",
            }),
        },
        Condition {
            // The loss that scales with the operator being the bottleneck.
            //
            // An approval is written with `approval_expires_at = now() + 72
            // hours`, and the claim sweep cancels it there with
            // `last_error_kind = 'approval_expired'`. Each one is a proposal the
            // brain made, ranked and queued, that nobody answered in time and the
            // system then discarded. On this deployment the approval queue *is*
            // the throughput limit, and it empties itself every three days
            // whether or not anybody looked.
            //
            // No other condition sees it, because nothing is broken when it
            // happens: executors are live, feeds sync, drafts publish, the brain
            // cycles. The only trace is `last_error_kind` on a cancelled row, and
            // that is also the only thing separating an expiry from an operator's
            // own deliberate rejection.
            //
            // Warning rather than critical. Nothing is corrupted and the
            // opportunity may come round again — but it is the operator's own
            // work being thrown away, so it is reported with the next deadline
            // rather than only the past losses: a count of what was lost says
            // somebody was too slow, a deadline says what to do today.
            key: "approval.expired_unanswered",
            severity: "warning",
            summary: "Approvals were cancelled because nobody answered them in time",
            active: snapshot.approvals_expired_7d > 0,
            details: json!({
                "expired": snapshot.approvals_expired_7d,
                "window": "7 days",
                "outstanding_now": snapshot.approvals_outstanding,
                "hours_to_next_expiry": snapshot.hours_to_next_approval_expiry,
                "approval_window_hours": 72,
                "remedy": "read /v1/control-plane/ops/attention for what is \
                           awaiting_approval now — that list is ordered by \
                           soonest deadline. The expired ones are in \
                           autopilot_actions with status 'cancelled' and \
                           last_error_kind 'approval_expired'; they are not \
                           retried and the brain will only propose them again if \
                           the underlying opportunity is still open. If the queue \
                           is consistently outrunning you, the answer is either a \
                           higher autonomy level for the contexts you trust or a \
                           longer approval window, not a faster operator.",
            }),
        },
        Condition {
            // The brain demoted itself: two consecutive `worsened` outcomes
            // in a context drop its policy to `require_approval` for a week.
            // Right when it happens the operator must hear it — every action
            // in that context now waits for them — and the event that says so
            // is refused by the n8n router. Warning: nothing is broken, the
            // throughput limit just moved onto a person.
            key: "autopilot.authority_guarded",
            severity: "warning",
            summary: "The autopilot demoted itself to ask-first after worsened outcomes",
            active: snapshot.guarded_policies.is_some(),
            details: json!({
                "policies": snapshot.guarded_policies,
                "remedy": "every action in the named contexts now waits for approval \
                           until the guard lapses. Read the two latest outcomes \
                           in the context (autopilot_outcomes joined to \
                           autopilot_actions) — if they are noise rather than harm, \
                           restore the policy's autonomy_level yourself; if they \
                           are real, leave the guard and review what shipped.",
            }),
        },
        Condition {
            // The deadline is a contract, and this is the contract being
            // broken. An ask past `approval_expires_at` must have already
            // died — the claim sweep runs it every cycle and the retention
            // worker runs the same sweep globally every hour. A row still
            // `awaiting_approval` two intervals later means neither ran:
            // the reads keep it hidden from the queue (it is too late to
            // answer) while the state keeps it uncounted as a loss (it was
            // never cancelled). The lapsed read calls these
            // `awaiting_sweep`.
            //
            // Warning: nothing was corrupted and the fix is the sweep
            // running, not operator action — but while it lasts the queue's
            // dead pile up invisible on both sides.
            key: "approval.sweep_lagging",
            severity: "warning",
            summary: "Approvals past their deadline are still marked awaiting_approval",
            active: snapshot.unswept_lapsed_approvals > 0,
            details: json!({
                "unswept": snapshot.unswept_lapsed_approvals,
                "sweep_grace": "2 hours",
                "remedy": "the claim path sweeps on every autopilot cycle and \
                           the retention worker sweeps globally every hour — \
                           rows this stale mean neither ran for this workspace. \
                           Check the worker is alive, that retention cycles \
                           complete (look for the 'retention cycle completed' \
                           log), and whether the tenant is parked with asks \
                           still outstanding.",
            }),
        },
        Condition {
            key: "executor.offline",
            severity: "critical",
            summary: "CrowdRelay executor registry has no live executor",
            active: snapshot.executor_registered > 0 && snapshot.executor_active == 0,
            details: json!({
                "registered": snapshot.executor_registered,
                "active": snapshot.executor_active,
            }),
        },
        Condition {
            // Warning, and deliberately paired with `executor.offline`: that
            // one is the registry dead entirely, this is one capability dark
            // while the rest heartbeat. Production carried it for eleven days
            // — `team.email` fell out of the advertised set when its
            // attestation aged past the freshness window, and every emission
            // queued, waited out the 24-hour grace and cancelled with
            // `no_executor`, a `last_error_kind` nobody reads.
            //
            // The demand side is the honest signal. The n8n heartbeat deletes
            // and re-inserts its capability rows on every beat, so a dropped
            // capability leaves no expired row to notice — what remains is the
            // work it stopped doing. `executor_active > 0` is the other half:
            // an empty registry is a deployment without executors, not a
            // dropped capability, and `executor.offline` already says so.
            //
            // The same predicate fires for a capability that was never wired
            // — an executor nobody built yet parks actions identically. That
            // is still the finding: work classes that cannot run, named.
            key: "executor.capability_unadvertised",
            severity: "warning",
            summary: "Actions are parked or cancelled for a capability no live executor advertises",
            active: snapshot.executor_active > 0
                && (snapshot.awaiting_executor_actions > 0
                    || snapshot.no_executor_cancelled_7d > 0),
            details: json!({
                "awaiting_executor": snapshot.awaiting_executor_actions,
                "cancelled_no_executor_7d": snapshot.no_executor_cancelled_7d,
                "action_kinds": snapshot.unclaimed_action_kinds,
                "remedy": "the named action kinds need a capability no live \
                           executor advertises. Compare against the executor \
                           contract's capability list and the n8n heartbeat's \
                           advertised set — a capability that fell out on a \
                           stale attestation needs a fresh attestation pass; \
                           one that was never wired needs its executor \
                           imported and activated",
            }),
        },
        Condition {
            // Work a person signed off that never happened. Warning: nothing
            // is corrupted, but the operator is the throughput limit and
            // believes it went out. Bounded at 24 hours so it clears.
            key: "execution.approved_actions_failed",
            severity: "warning",
            summary: "Actions you approved failed instead of going out",
            active: snapshot.approved_failed_24h > 0,
            details: json!({
                "failed": snapshot.approved_failed_24h,
                "window": "24 hours",
                "by_kind_and_error": snapshot.approved_failed_summary,
                "remedy": "read the failed rows (autopilot_actions status='failed', \
                           approved_at set) and the worker's warning for each \
                           conflict_reason — a state_changed failure logs the \
                           sentence that caused it. Fix the cause before \
                           re-approving; the same draft will fail the same way.",
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
        Condition {
            // A fresh drop's fan-out produced nothing a fan can see. The
            // drop window is where a new video earns its reach — a stalled
            // surge is the difference between a premiere and a non-event,
            // and on 2026-09-28 one burned its whole first day silent while
            // the artifact lane reported success. Warning rather than
            // critical: nothing is corrupted and the window has not fully
            // closed — but it is closing, which is why the alarm exists at
            // all. `approval.expired_unanswered` covers the opposite failure
            // (asks nobody answered); this one covers asks that were never
            // even made visible.
            key: "growth.drop_surge_stalled",
            severity: "warning",
            summary: "A fresh drop's fan-out stalled — surge lanes raised but nothing reached fans",
            active: snapshot
                .stalled_drops
                .as_ref()
                .is_some_and(|drops| drops.as_array().is_some_and(|a| !a.is_empty())),
            details: json!({
                "stalled_drops": snapshot.stalled_drops,
                "remedy": "each stalled drop lists its lanes with their \
                           action status. awaiting_approval means the fix is \
                           an approval click in /ops/attention; failed means \
                           the executor's error needs reading; succeeded \
                           with no receipt means the post materializer never \
                           claimed it — check executor heartbeats and the \
                           template join.",
            }),
        },
        Condition {
            // A claimed campaign delivery is a send in flight. The executor
            // claims the row, mails the fan, then reports a result — and
            // the API's claim lease marks the row `failed` with
            // `claim_expired_unknown` when that result never arrives. This
            // condition counts the ones the lease sweep rescued: every
            // `claim_expired_unknown` in the last day is an executor that
            // died between claim and result, and the mail's real fate is
            // unknown — that is what "failed closed" means here, not "the
            // send definitely did not happen".
            //
            // Measured 2026-09-30: the n8n campaign executor (VOSCAM) died
            // at the Gmail step for every run, leaving 116 rows claimed
            // across five campaigns for weeks — the API's expiry only runs
            // when the executor calls back, and a dead executor never does.
            // The sweep now runs on the watchdog tick, so this alarm is the
            // remaining symptom: abandoned sends, named.
            //
            // `deliveries_stuck_claimed` — a still-claimed row older than
            // the alert age — travels in the details rather than driving
            // the predicate: the sweep runs before the snapshot in this
            // same transaction, so a row that old means the sweep did not
            // run at all, and nothing here would report it anyway. It is
            // kept as a reading because a stale deployment running an old
            // worker would show it.
            //
            // Warning, not critical: the send may well have happened, the
            // fanbase is not being re-mailed, and the recovery already ran.
            // What is lost is certainty per fan and the wave itself.
            key: "communications.deliveries_stuck_claimed",
            severity: "warning",
            summary: "Campaign deliveries were claimed and never reported",
            active: snapshot.abandoned_claims_24h > 0,
            details: json!({
                "abandoned_claims_24h": snapshot.abandoned_claims_24h,
                "still_claimed_past_lease": snapshot.deliveries_stuck_claimed,
                "remedy": "check the n8n workflow 'CrowdRelayOS communication \
                           campaign executor' (VOSCAM000000001) — its claim \
                           calls land but no result call follows. The Gmail \
                           send or the report node is where the run dies; \
                           failed rows carry error_code 'claim_expired_unknown' \
                           and the campaigns behind them stay 'scheduled' until \
                           every recipient resolves.",
            }),
        },
        Condition {
            // A video inside its window that is under half of the views its
            // age expects is the "+1000 in 14 days" plan visibly failing.
            // Warning, not critical: the window has not closed, and the
            // card already knows what is missing — the details hand the
            // operator the video and its top blockers rather than a bare
            // number to investigate.
            key: "video.behind_pace",
            severity: "warning",
            summary: "A video in its 14-day window is behind its CrowdRelay-driven view pace",
            active: snapshot
                .video_cards
                .iter()
                .any(|card| card.pace == Pace::Behind),
            details: json!({
                "behind": snapshot
                    .video_cards
                    .iter()
                    .filter(|card| card.pace == Pace::Behind)
                    .map(|card| json!({
                        "title": card.title,
                        "source_id": card.source_id,
                        "age_days": card.age_days,
                        "attributed_views": card.attributed_views,
                        "expected_by_now": card.expected_by_now,
                        "missing": card
                            .missing
                            .iter()
                            .take(3)
                            .map(|reason| serde_json::to_value(reason)
                                .unwrap_or_default()
                                .get("reason")
                                .cloned()
                                .unwrap_or_default())
                            .collect::<Vec<Value>>(),
                    }))
                    .collect::<Vec<Value>>(),
                "remedy": "open the video's scorecard — /v1/control-plane/\
                           content/videos/{source_id}/scorecard — and clear \
                           the top missing reason: halted Reddit standing, \
                           an unseeded press wave, or undelivered fan mail.",
            }),
        },
        Condition {
            // Info, not warning: nothing is broken in what ran — the grant
            // simply does not exist, so no `traffic:*` series can ever land
            // and every card reads unmeasured. A reconnect fixes it; the
            // finding exists so "we cannot tell" is never mistaken for
            // "nobody came".
            key: "video.unmeasured",
            severity: "info",
            summary: "YouTube Analytics is not connected — CrowdRelay-driven views cannot be told from organic ones",
            active: snapshot.video_cards.iter().any(|card| {
                card.missing
                    .iter()
                    .any(|reason| matches!(reason, MissingReason::NoAnalyticsGrant))
            }),
            details: json!({
                "videos_unmeasured": snapshot
                    .video_cards
                    .iter()
                    .filter(|card| card.pace == Pace::Unmeasured)
                    .map(|card| card.title.clone())
                    .collect::<Vec<String>>(),
                "remedy": "reconnect the YouTube account with the \
                           yt-analytics.readonly scope — connections → \
                           YouTube → reauthorize.",
            }),
        },
    ]
}
