impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// Walks one running play as far as it will go this cycle.
    ///
    /// A settle can make the step behind it immediately actionable: a withdrawn
    /// anchor settles every remaining step in turn, and an expired step settles
    /// one that is already due. So this loops rather than deciding once — but
    /// only ever forward, and never more times than there are steps, because a
    /// state machine that stopped shrinking would otherwise spin against the
    /// database for the rest of the cycle.
    async fn advance_play(
        &self,
        snapshot: &mut PlayRunSnapshot,
        policy: &AutopilotPolicy,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let bound = snapshot.steps.len().saturating_add(1);
        for _ in 0..bound {
            let Some(decision) = play_decision(snapshot, policy, now) else {
                return Ok(());
            };
            match decision {
                PlayDecision::Hold(_) => return Ok(()),
                PlayDecision::RunStep { .. } => {
                    if let Some(candidate) = play_step_candidate(snapshot, decision, policy)? {
                        self.persist(&candidate, limits, report).await?;
                    }
                    // One send per play per cycle. The recipient came from a
                    // read taken before this action existed, and deciding again
                    // against it would either offer the same fan twice or skip
                    // the next one; the following cycle reads a fresh audience.
                    return Ok(());
                }
                PlayDecision::SkipStep { index, reason, .. } => {
                    self.repository
                        .settle_play_step(
                            self.workspace_id,
                            &PlayStepSettlement {
                                play_id: snapshot.play_id,
                                step_index: index,
                                reason,
                            },
                            now,
                        )
                        .await?;
                    report.play_steps_skipped = report.play_steps_skipped.saturating_add(1);
                    // The same settle applied to the copy in hand. Without it
                    // the next pass reads the step as still open and settles it
                    // again, which double-counts an omission that happened once.
                    if let Some(step) = snapshot
                        .steps
                        .iter_mut()
                        .find(|step| step.index == index && !step.settled)
                    {
                        step.settled = true;
                    }
                }
                PlayDecision::Complete => {
                    self.repository
                        .complete_play(self.workspace_id, snapshot.play_id, now)
                        .await?;
                    report.plays_completed = report.plays_completed.saturating_add(1);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
