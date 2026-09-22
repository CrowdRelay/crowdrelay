macro_rules! decision_cycle_reads {
    () => {
    /// Reads the operator's ceilings.
    ///
    /// A row whose class or level this build does not recognise is skipped, so
    /// the class falls back to its safest ceiling in the caller. Guessing at an
    /// unreadable authority row in the permissive direction is the one mistake
    /// this whole mechanism exists to prevent.
    async fn load_autonomy_ceilings_impl(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(ActionClass, AutonomyLevel)>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, (String, String)>(
                r#"
                SELECT action_class, ceiling
                FROM growth_autonomy
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            Ok(rows
                .into_iter()
                .filter_map(|(class, ceiling)| {
                    Some((
                        ActionClass::parse(&class)?,
                        parse_autonomy_level(&ceiling).ok()?,
                    ))
                })
                .collect())
        })
        .await
    }

    /// The envelope and what has already been spent against it.
    ///
    /// Spend is counted from the durable action rows rather than a separate
    /// ledger, and only from rows the agent itself created — `action_class` is
    /// NULL for everything that predates the envelope, and work done before the
    /// agent existed was not the agent's to be charged for.
    async fn load_growth_envelope_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<(GrowthEnvelope, EnvelopeUsage), RepositoryError> {
        self.bounded(async {
            let envelope = sqlx::query_as::<_, GrowthEnvelopeRow>(
                r#"
                SELECT agent_enabled, dry_run, weekly_owned_audience_touches,
                       weekly_third_party_touches, daily_third_party_touches,
                       subject_cooldown_hours,
                       max_recipients_per_step, weekly_approval_requests,
                       weekly_bootstrap_actions, parked
                FROM growth_envelope
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;

            // A missing row is the timid default, which has the agent switched
            // off. An absent envelope must never read as an absent limit.
            let envelope = envelope.map_or_else(GrowthEnvelope::default, |row| GrowthEnvelope {
                agent_enabled: row.agent_enabled,
                dry_run: row.dry_run,
                weekly_owned_audience_touches: bounded_u32(i64::from(
                    row.weekly_owned_audience_touches,
                ))
                .unwrap_or(0),
                weekly_third_party_touches: bounded_u32(i64::from(row.weekly_third_party_touches))
                    .unwrap_or(0),
                daily_third_party_touches: bounded_u32(i64::from(row.daily_third_party_touches))
                    .unwrap_or(0),
                subject_cooldown_hours: bounded_u32(i64::from(row.subject_cooldown_hours))
                    .unwrap_or(0),
                max_recipients_per_step: bounded_u32(i64::from(row.max_recipients_per_step))
                    .unwrap_or(1),
                // An unreadable budget is no asking, never unbounded asking.
                weekly_approval_requests: bounded_u32(i64::from(row.weekly_approval_requests))
                    .unwrap_or(0),
                // An unreadable cap is no warm-up, never an unbounded one.
                weekly_bootstrap_actions: bounded_u32(i64::from(row.weekly_bootstrap_actions))
                    .unwrap_or(0),
                parked: row.parked,
            });

            // Cancelled actions are excluded: an approval that was refused is
            // not a touch anybody received. Everything else counts, including
            // failures, because a send that errored may still have gone out.
            let spend = sqlx::query_as::<_, (String, i64, i64)>(
                r#"
                SELECT action_class, count(*)::bigint,
                       count(*) FILTER (WHERE created_at >= $2 - INTERVAL '24 hours')::bigint
                FROM autopilot_actions
                WHERE workspace_id = $1
                  AND action_class IN ('owned_audience', 'third_party')
                  AND status <> 'cancelled'
                  AND created_at >= $2 - INTERVAL '7 days'
                GROUP BY action_class
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let mut usage = EnvelopeUsage::default();
            for (class, count_7d, count_24h) in spend {
                let count = bounded_u32(count_7d).unwrap_or(u32::MAX);
                match ActionClass::parse(&class) {
                    Some(ActionClass::OwnedAudience) => usage.owned_audience_touches_7d = count,
                    Some(ActionClass::ThirdParty) => {
                        usage.third_party_touches_7d = count;
                        usage.third_party_touches_24h =
                            bounded_u32(count_24h).unwrap_or(u32::MAX);
                    }
                    _ => {}
                }
            }

            // Asks made, not asks still waiting. `approval_expires_at` is set
            // when an action is parked for a person and survives the approval,
            // so an operator who answers quickly has still been asked — and a
            // budget that forgot them the moment they answered would bound
            // nothing. Cancelled rows count too: the interruption happened.
            //
            // Counted for every class. An approval request about a
            // first-party action costs a person exactly what one about a
            // third-party action costs them, which is why this is not part of
            // the class-keyed spend above.
            usage.approval_requests_7d = bounded_u32(
                sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT count(*)
                    FROM autopilot_actions
                    WHERE workspace_id = $1
                      AND approval_expires_at IS NOT NULL
                      AND created_at >= $2::timestamptz - INTERVAL '7 days'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?,
            )
            .unwrap_or(u32::MAX);

            Ok((envelope, usage))
        })
        .await
    }

    /// Unattended actions each context took in the trailing seven days.
    ///
    /// `approved_by = 'policy:bounded_auto'` is what the persist path stamps
    /// on an action nobody approved, so it is exactly the set the warm-up
    /// allowance is bounding. Counted from the durable rows for the same
    /// reason the envelope counts its touches there: a second ledger can
    /// disagree with the actions, and the one that is wrong is the one nobody
    /// reads.
    ///
    /// Cancelled rows still count. The allowance bounds what the agent *did*
    /// unattended, and an action an operator pulled back was still an action
    /// that went out of the gate without them.
    ///
    /// A context whose name this build cannot parse is skipped rather than
    /// bucketed somewhere: a spend attributed to the wrong context would widen
    /// one allowance while narrowing another.
    async fn load_bootstrap_spend_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::BTreeMap<AutopilotContext, i64>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, (String, i64)>(
                r#"
                SELECT context, count(*)
                FROM autopilot_actions
                WHERE workspace_id = $1
                  AND approved_by = 'policy:bounded_auto'
                  -- Cast so the statement can be PREPAREd standalone, which is
                  -- what `sql-result-types.py` needs to check it at all. Without
                  -- it Postgres infers `interval` for $2 and the query silently
                  -- drops out of the gate's count instead of being verified.
                  AND created_at >= $2::timestamptz - INTERVAL '7 days'
                GROUP BY context
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            Ok(rows
                .into_iter()
                .filter_map(|(context, count)| {
                    Some((AutopilotContext::from_storage(&context)?, count))
                })
                .collect())
        })
        .await
    }

    async fn load_outward_touch_ages_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::HashMap<Uuid, u32>, RepositoryError> {
        self.bounded(async {
            // Bounded by the longest cooldown the schema allows (a year), so a
            // workspace with years of history does not scan all of it.
            let rows = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
                r#"
                SELECT subject_id, max(created_at)
                FROM autopilot_actions
                WHERE workspace_id = $1
                  AND action_class IN ('owned_audience', 'third_party')
                  AND status <> 'cancelled'
                  AND created_at >= $2 - INTERVAL '365 days'
                GROUP BY subject_id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            Ok(rows
                .into_iter()
                .map(|(subject_id, touched_at)| {
                    (
                        subject_id,
                        u32::try_from((now - touched_at).whole_hours().max(0)).unwrap_or(u32::MAX),
                    )
                })
                .collect())
        })
        .await
    }

    async fn load_growth_debt_observations_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthDebtObservation>, RepositoryError> {
        self.bounded(operations::load_growth_debt_observations(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_outreach_supply_snapshot_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OutreachSupplySnapshot, RepositoryError> {
        self.bounded(operations::load_outreach_supply_snapshot(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_growth_intelligence_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthIntelligenceSnapshot>, RepositoryError> {
        self.bounded(operations::load_growth_intelligence_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_open_content_suggestions_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::content_engine::ContentSuggestion>, RepositoryError> {
        self.bounded(async {
            // `raised` only, and still inside its window: an approved row is
            // committed work the queue already asked about, and an expired
            // one is a dead question.
            let rows = sqlx::query_as::<_, crate::content_engine::SuggestionRow>(
                r#"
                SELECT * FROM content_suggestions
                WHERE workspace_id = $1 AND status = 'raised'
                  AND (expires_at IS NULL OR expires_at > $2)
                ORDER BY created_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter()
                .map(|row| {
                    crowdrelay_domain::content_engine::ContentSuggestion::try_from(row)
                        .map_err(|_| RepositoryError::Unexpected)
                })
                .collect()
        })
        .await
    }

    async fn load_proposed_content_arcs_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::content_engine::Arc>, RepositoryError> {
        self.bounded(async {
            // `proposed` only, and still inside its window: approved and
            // active arcs are the season already chosen, retired ones are
            // answers, and a proposal whose horizon closed between sweeps is
            // a dead question — asking it now would only produce an approval
            // that conflicts against a retired row.
            let rows = sqlx::query_as::<_, crate::content_engine::ArcRow>(
                r#"
                SELECT * FROM arcs
                WHERE workspace_id = $1 AND status = 'proposed'
                  AND (horizon_end IS NULL OR horizon_end >= $2::date)
                ORDER BY created_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now.date())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter()
                .map(|row| {
                    crowdrelay_domain::content_engine::Arc::try_from(row)
                        .map_err(|_| RepositoryError::Unexpected)
                })
                .collect()
        })
        .await
    }
    };
}
