// Persist only after a completed growth evaluation; preview never calls this.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn save_growth_metacognition(
        &self,
        snapshots: &[crowdrelay_brain::GrowthIntelligenceSnapshot],
        now: OffsetDateTime,
        report: &mut AutopilotCycleReport,
    ) {
        let Some(first) = snapshots.first() else {
            return;
        };
        let observation = crowdrelay_brain::self_assessment::checkpoint::MetacognitionObservation {
            metric: first.world_model.north_star.as_str().to_owned(),
            state: first.metacognition.state,
            observed_at_micros: now.unix_timestamp() * 1_000_000 + i64::from(now.microsecond()),
        };
        match serde_json::to_value(observation) {
            Ok(value)
                if self
                    .repository
                    .save_brain_state(self.workspace_id, "metacognition", &value)
                    .await
                    .is_ok() => {}
            _ => report
                .gi_dispatch_log
                .push("metacognition checkpoint failed; continuity did not advance".into()),
        }
    }
}

fn record_growth_assessment(
    snapshots: &[crowdrelay_brain::GrowthIntelligenceSnapshot],
    sizing_multiplier: f64,
    report: &mut AutopilotCycleReport,
) {
    if let Some(first) = snapshots.first() {
        report.gi_dispatch_log.push(format!(
            "growth goal {}: current {}, gained this month {}; assessment {}; \
                 learning streak {}; effective dispatch multiplier {:.2}",
            first.world_model.north_star.as_str(),
            first.world_model.north_star_current,
            first.world_model.north_star_this_month,
            first.metacognition.state.as_str(),
            first.metacognition.learning_cycles,
            sizing_multiplier,
        ));
    }
}
