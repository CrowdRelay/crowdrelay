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
                FROM viryaos_growth_autonomy
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
                       weekly_third_party_touches, subject_cooldown_hours,
                       max_recipients_per_step, parked
                FROM viryaos_growth_envelope
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
                subject_cooldown_hours: bounded_u32(i64::from(row.subject_cooldown_hours))
                    .unwrap_or(0),
                max_recipients_per_step: bounded_u32(i64::from(row.max_recipients_per_step))
                    .unwrap_or(1),
                parked: row.parked,
            });

            // Cancelled actions are excluded: an approval that was refused is
            // not a touch anybody received. Everything else counts, including
            // failures, because a send that errored may still have gone out.
            let spend = sqlx::query_as::<_, (String, i64)>(
                r#"
                SELECT action_class, count(*)::bigint
                FROM viryaos_autopilot_actions
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
            for (class, count) in spend {
                let count = bounded_u32(count).unwrap_or(u32::MAX);
                match ActionClass::parse(&class) {
                    Some(ActionClass::OwnedAudience) => usage.owned_audience_touches_7d = count,
                    Some(ActionClass::ThirdParty) => usage.third_party_touches_7d = count,
                    _ => {}
                }
            }
            Ok((envelope, usage))
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
                FROM viryaos_autopilot_actions
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
                SELECT * FROM viryaos_content_suggestions
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
                SELECT * FROM viryaos_arcs
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
