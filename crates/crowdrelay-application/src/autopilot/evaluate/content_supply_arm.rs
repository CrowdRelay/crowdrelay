// ContentSupply arm — synced-post relay fan-out and drop surge, gated by
// the organic funnel's directive. Extracted from `execute`'s match under the
// modularity contract's 1000-line parent ceiling.

impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// The mature visitor→signup leak can downgrade or veto fan-out; the arm
    /// otherwise scores supply exactly as before.
    async fn evaluate_content_supply_arm(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        evidence: &EvidenceLedger,
        organic_funnel_control: Option<OrganicFunnelControl>,
    ) -> Result<(), AutopilotError> {
        let snapshots = self
            .repository
            .load_content_supply_snapshots(self.workspace_id, now)
            .await?;
        // The stop rule: no live material is not a quiet
        // portfolio, it is the band having done nothing public
        // lately — `supply_quiet_reason` names which quiet the
        // cycle is in rather than reporting a generic "nothing
        // scored". A non-supply config still skips the loop.
        if !matches!(policy.config, AutopilotPolicyConfig::ContentSupply(_)) {
            return Ok(());
        }
        // Load relay targets only when fresh material can actually fan out.
        let domain_policy = match &policy.config {
            AutopilotPolicyConfig::ContentSupply(config) => Some(*config),
            _ => None,
        };
        let has_relay_material = snapshots.iter().any(|snapshot| {
            snapshot.source_kind == ContentSourceKind::SocialPost
                || domain_policy.is_some_and(|config| {
                    crowdrelay_domain::content_supply::drop_surge_eligible(
                        snapshot, &config, now,
                    )
                })
        });
        let communities = if has_relay_material {
            self.repository
                .load_relay_community_targets(self.workspace_id)
                .await?
        } else {
            Vec::new()
        };
        let push_audience = if has_relay_material {
            Some(
                self.repository
                    .load_signal_push_audience(self.workspace_id, None)
                    .await?,
            )
        } else {
            None
        };
        // Relay pushes are paced across cycles and within one:
        // see `relay_push_verdict`. Newest post first, so a held
        // backlog relays the freshest news, not the oldest.
        let mut recent_relays = if has_relay_material {
            self.repository
                .load_recent_relay_pushes(
                    self.workspace_id,
                    now - time::Duration::days(
                        crowdrelay_domain::content_supply::RELAY_PUSH_DEDUPE_DAYS,
                    ),
                )
                .await?
        } else {
            Vec::new()
        };
        let mut ordered: Vec<&ContentSupplySnapshot> = snapshots.iter().collect();
        ordered.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.occurred_at));
        let mut produced = 0usize;
        for snapshot in ordered {
            for mut candidate in content_candidates(
                snapshot,
                policy,
                &communities,
                push_audience,
                evidence.for_context(policy.context),
                now,
            )? {
                if !Self::prepare_content_candidate_for_funnel(
                    &mut candidate,
                    organic_funnel_control,
                    report,
                ) {
                    return Ok(());
                }
                let relay_push = match &candidate.action {
                    AutopilotActionPayload::RequestSignalPush {
                        title, body, ..
                    } if candidate.decision_kind == "relay_owned_post" => {
                        Some((title.clone(), body.clone()))
                    }
                    _ => None,
                };
                if let Some((title, body)) = &relay_push
                    && crowdrelay_domain::content_supply::relay_push_verdict(
                        title,
                        body,
                        &recent_relays,
                        now,
                    ) != crowdrelay_domain::content_supply::RelayPushVerdict::Send
                {
                    return Ok(());
                }
                produced += 1;
                let action = self.persist(&candidate, limits, report).await?;
                if let (Some(_), Some((title, body))) = (action, relay_push) {
                    recent_relays.push(
                        crowdrelay_domain::content_supply::RecentRelayPush {
                            at: now,
                            title,
                            body,
                        },
                    );
                }
            }
        }
        report.supply_wait_reason =
            supply_quiet_reason(&snapshots, policy, produced, now);
        Ok(())
    }
}
