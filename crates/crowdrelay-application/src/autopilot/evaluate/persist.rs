// The one gate every candidate passes through: class ceiling, volume
// envelope and attention budget, in that order.
//
// Split out of `evaluate.rs` because it is the file's one piece of policy —
// everything else there dispatches to a detector — and because the parent
// has a 1000-line contract that this pushed past. `include!`d rather than a
// module so it stays a method on the same type, exactly as the candidate
// chunks beside it do.

impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    /// The one place every candidate passes through, and therefore the only
    /// place the class ceiling has to be applied.
    ///
    /// Doing it here rather than inside each of the twenty candidate functions
    /// means a new detector cannot forget it, and a detector author cannot
    /// choose to skip it.
    ///
    /// Returns the action_id if one was created or already existed. Callers
    /// that don't need it can ignore the return value.
    async fn persist(
        &self,
        candidate: &DecisionCandidate,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
    ) -> Result<Option<Uuid>, AutopilotError> {
        let stats = report.context_stats(candidate.context);
        stats.candidates = stats.candidates.saturating_add(1);
        let class = candidate.action.action_class();
        let ceiling = limits
            .ceilings
            .iter()
            .find_map(|(known, level)| (*known == class).then_some(*level))
            // An absent row is the safest ceiling, never an absent limit.
            .unwrap_or_else(|| class.safest_ceiling());
        // What held this decision below the disposition its detector asked
        // for, in the order the limits apply. Recorded on the decision's
        // policy snapshot, so "why did it not act" is one read of the row
        // rather than a reconstruction of seven interacting budgets — every
        // starved lever found on 2026-09-25/26 took an afternoon to trace.
        let mut held_by: Vec<String> = Vec::new();
        let clamped = clamp_disposition(candidate.disposition, ceiling);
        if clamped != candidate.disposition {
            report.actions_gated = report.actions_gated.saturating_add(1);
            held_by.push(format!("class_ceiling:{}", class.as_str()));
        }

        // The volume limits apply after the class ceiling, never instead of it:
        // a full budget must not be able to let a third-party action through,
        // and an empty one must not promote anything.
        let subject_usage = EnvelopeUsage {
            // Only contacts have a cooldown. An event is a topic, not a person:
            // keying it there would let one show run a single growth lever a
            // week and quietly starve the other nine.
            hours_since_subject_touched: candidate
                .subject
                .is_contactable_person()
                .then(|| {
                    // Somebody this cycle already reached is touched now, not
                    // whenever the cycle's snapshot says. Without this, two
                    // contexts — or two plays around two different shows — can
                    // each pass the cooldown against the same stale reading and
                    // between them message one person twice in a minute.
                    if limits
                        .touched_this_cycle
                        .contains(&candidate.subject.uuid())
                    {
                        return Some(0);
                    }
                    limits.touch_ages.get(&candidate.subject.uuid()).copied()
                })
                .flatten(),
            ..*limits.usage
        };
        // A candidate already denied by policy (the §4i-2 show-week hold is
        // one) owes the envelope nothing — counting it as a held action on top
        // of the hold decision would report the same refusal twice.
        let clamped = if matches!(clamped, PolicyDisposition::Deny) {
            clamped
        } else {
            match check_envelope(class, limits.envelope, &subject_usage) {
                EnvelopeVerdict::Allow => clamped,
                EnvelopeVerdict::Hold(block) => {
                    report.actions_held = report.actions_held.saturating_add(1);
                    held_by.push(format!("envelope:{}", block.as_str()));
                    // A rehearsal produces the decision and its evidence but
                    // nothing anybody can press send on. Every other block still
                    // offers the work to a human, because "the budget is spent" is
                    // not the same as "this should not happen".
                    if block.may_offer_for_approval() {
                        clamp_disposition(clamped, AutonomyLevel::RequireApproval)
                    } else {
                        clamp_disposition(clamped, AutonomyLevel::Recommend)
                    }
                }
            }
        };

        // Last, and only for the one disposition that costs a person anything.
        //
        // The agent's ladder makes "ask a person" its cheapest move in almost
        // every context, and nothing charged it for that move — so an agent
        // behaving correctly filled a queue nobody could empty and the
        // approvals expired unread. The budget makes asking cost something.
        //
        // It can only ever narrow. A budget with room left promotes nothing;
        // it declines to park one more decision, and the finding still
        // surfaces as a recommendation with all of its evidence. Nothing is
        // lost except the interruption.
        let clamped = if matches!(clamped, PolicyDisposition::RequireApproval)
            && !check_attention(limits.envelope, limits.usage).may_ask()
        {
            report.asks_withheld = report.asks_withheld.saturating_add(1);
            held_by.push("attention_budget".to_owned());
            clamp_disposition(clamped, AutonomyLevel::Recommend)
        } else {
            clamped
        };

        let mut policy_snapshot = candidate.policy_snapshot.clone();
        if !held_by.is_empty()
            && let Some(object) = policy_snapshot.as_object_mut()
        {
            object.insert("held_by".to_owned(), serde_json::json!(held_by));
        }
        let candidate = &DecisionCandidate {
            disposition: clamped,
            policy_snapshot,
            ..candidate.clone()
        };
        let persisted = match self
            .repository
            .persist_candidate(
                self.workspace_id,
                candidate,
                &TraceContext::root(self.workspace_id),
            )
            .await
        {
            Ok(p) => p,
            // A conflict means the candidate already exists or is in-flight.
            // Skip it and continue the cycle rather than aborting all remaining
            // candidates. This is the same semantics as ActionConflict in the
            // infra layer, but catches any remaining uncovered index conflicts.
            Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if persisted.decision_created {
            report.decisions = report.decisions.saturating_add(1);
            let stats = report.context_stats(candidate.context);
            stats.decisions = stats.decisions.saturating_add(1);
        }
        if persisted.action_created {
            report.actions_enqueued = report.actions_enqueued.saturating_add(1);
            report.context_stats(candidate.context).actions = report
                .context_stats(candidate.context)
                .actions
                .saturating_add(1);
            if candidate.subject.is_contactable_person() {
                limits.touched_this_cycle.insert(candidate.subject.uuid());
            }
            // Spend the budget as it is used, not once at the start of the
            // cycle. Without this the weekly cap is read from a snapshot that
            // never moves, and a single cycle with fifty findings enqueues all
            // fifty against a budget of five.
            match class {
                ActionClass::OwnedAudience => {
                    limits.usage.owned_audience_touches_7d =
                        limits.usage.owned_audience_touches_7d.saturating_add(1);
                }
                ActionClass::ThirdParty => {
                    limits.usage.third_party_touches_7d =
                        limits.usage.third_party_touches_7d.saturating_add(1);
                    // The daily wall spends from the same send — leaving it
                    // stale would let one cycle enqueue a week of third-party
                    // touches against a day-sized ceiling.
                    limits.usage.third_party_touches_24h =
                        limits.usage.third_party_touches_24h.saturating_add(1);
                }
                ActionClass::FirstPartyReversible | ActionClass::Paid => {}
            }
            // Spent as it is used, like the touch budgets above. Without this
            // one cycle with forty findings parks all forty against a budget
            // of twenty, because the count was read before the cycle began.
            if matches!(candidate.disposition, PolicyDisposition::RequireApproval) {
                limits.usage.approval_requests_7d =
                    limits.usage.approval_requests_7d.saturating_add(1);
            }
        }
        if persisted.quota_throttled {
            report.actions_throttled = report.actions_throttled.saturating_add(1);
            let stats = report.context_stats(candidate.context);
            stats.throttled = stats.throttled.saturating_add(1);
        }
        Ok(persisted.action_id)
    }
}
