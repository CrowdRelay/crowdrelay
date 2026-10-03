// §5 weekly join-ask evaluation behind the organic-funnel gate — extracted
// from evaluate.rs to keep the orchestrator under the modularity contract
// line limit.

impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// §5: the weekly join-ask rides this context — a grounded post on the
    /// tenant's own pages is the strategy surface's work. Explicit wording
    /// wins; otherwise one fresh tenant-owned source may seed a deterministic
    /// starter. A tenant with neither still lands on `NoVariants`, so a truly
    /// empty workspace reports what it is waiting on instead of producing a
    /// cycle that reads as healthy and empty.
    async fn evaluate_join_ask_week(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        funnel_control: Option<OrganicFunnelControl>,
    ) -> Result<(), AutopilotError> {
        let snapshot = self
            .repository
            .load_join_ask_snapshot(self.workspace_id, now)
            .await?;
        if funnel_control.is_none_or(|control| control.directive.permits_join_ask()) {
            let mut evaluation =
                evaluate_join_ask_candidates(&snapshot, policy, self.workspace_id, now)?;
            if let Some(control) = funnel_control {
                for candidate in &mut evaluation.candidates {
                    attach_organic_funnel_control(candidate, control);
                }
            }
            report.join_ask_held.extend(evaluation.held);
            for candidate in &evaluation.candidates {
                self.persist(candidate, limits, report).await?;
            }
        } else if let Some(control) = funnel_control {
            report.gi_dispatch_log.push(format!(
                "organic funnel control: held join-ask while directive={} — do not add signups ahead of the current downstream leak",
                control.directive.as_str()
            ));
        }
        Ok(())
    }
}
