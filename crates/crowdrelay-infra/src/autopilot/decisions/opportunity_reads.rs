macro_rules! decision_opportunity_reads {
    () => {
    async fn load_city_opportunity_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<CityOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, BookingSnapshotRow>(
                r#"
                SELECT
                    city.id AS city_id,
                    COALESCE(city_aggregate.confirmed_fan_count, 0)::bigint AS active_fans,
                    COALESCE(growth.new_fans_30d, 0)::bigint AS new_fans_30d,
                    COALESCE(interests.event_interests, 0)::bigint AS event_interests,
                    COALESCE(area.area_claims, 0)::bigint AS area_claims,
                    CASE
                        WHEN last_show.starts_at IS NULL THEN NULL
                        ELSE GREATEST(
                            0,
                            FLOOR(EXTRACT(EPOCH FROM ($2 - last_show.starts_at)) / 2629800.0)
                        )::bigint
                    END AS months_since_last_show,
                    EXISTS (
                        SELECT 1
                        FROM viryaos_autopilot_actions AS action
                        WHERE action.workspace_id = $1
                          AND action.subject_id = city.id
                          AND action.action_kind = 'booking.outreach.request'
                          AND action.status IN ('awaiting_approval', 'queued', 'processing')
                    ) AS outreach_in_flight,
                    last_outreach.finished_at AS last_outreach_at
                FROM cities AS city
                JOIN city_aggregates AS city_aggregate
                  ON city_aggregate.workspace_id = $1
                 AND city_aggregate.city_id = city.id
                LEFT JOIN LATERAL (
                    SELECT COUNT(DISTINCT interest.fan_id)::bigint AS new_fans_30d
                    FROM fan_city_interests AS interest
                    JOIN fans AS fan
                      ON fan.workspace_id = interest.workspace_id
                     AND fan.id = interest.fan_id
                    WHERE interest.workspace_id = $1
                      AND interest.city_id = city.id
                      AND fan.status = 'active'
                      AND fan.created_at >= $2 - INTERVAL '30 days'
                ) AS growth ON true
                LEFT JOIN LATERAL (
                    SELECT COUNT(*)::bigint AS event_interests
                    FROM event_interests AS event_interest
                    JOIN events AS event
                      ON event.workspace_id = event_interest.workspace_id
                     AND event.id = event_interest.event_id
                    WHERE event_interest.workspace_id = $1
                      AND event.city_id = city.id
                      AND event_interest.created_at >= $2 - INTERVAL '180 days'
                ) AS interests ON true
                LEFT JOIN LATERAL (
                    SELECT COUNT(*)::bigint AS area_claims
                    FROM area_claims AS claim
                    JOIN area_drops AS drop
                      ON drop.workspace_id = claim.workspace_id
                     AND drop.id = claim.drop_id
                    WHERE claim.workspace_id = $1
                      AND drop.city_id = city.id
                      AND claim.claimed_at >= $2 - INTERVAL '365 days'
                ) AS area ON true
                LEFT JOIN LATERAL (
                    SELECT event.starts_at
                    FROM events AS event
                    WHERE event.workspace_id = $1
                      AND event.city_id = city.id
                      AND event.status IN ('published', 'completed')
                      AND event.starts_at < $2
                    ORDER BY event.starts_at DESC, event.id DESC
                    LIMIT 1
                ) AS last_show ON true
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = $1
                      AND action.subject_id = city.id
                      AND action.action_kind = 'booking.outreach.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_outreach ON true
                WHERE city_aggregate.workspace_id = $1
                ORDER BY city_aggregate.confirmed_fan_count DESC, city.name, city.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let city_ids = rows.iter().map(|row| row.city_id).collect::<Vec<_>>();
            let market_rows = if city_ids.is_empty() {
                Vec::new()
            } else {
                sqlx::query_as::<_, MarketSignalRow>(
                    r#"
                    SELECT city_id, signal_kind, score_basis_points, confidence_basis_points,
                           observed_at, expires_at
                    FROM viryaos_city_market_signals
                    WHERE workspace_id = $1
                      AND city_id = ANY($2)
                      AND observed_at <= $3
                      AND expires_at > $3
                    ORDER BY city_id, signal_kind, source, id
                    LIMIT 5000
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&city_ids)
                .bind(now)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?
            };
            let mut signals_by_city: HashMap<Uuid, Vec<CityMarketSignal>> = HashMap::new();
            for row in market_rows {
                signals_by_city
                    .entry(row.city_id)
                    .or_default()
                    .push(market_signal(row)?);
            }
            rows.into_iter()
                .map(|row| {
                    let market_evidence = aggregate_city_market_evidence(
                        signals_by_city.remove(&row.city_id).unwrap_or_default(),
                        now,
                    );
                    booking_snapshot(row, market_evidence)
                })
                .collect()
        })
        .await
    }

    async fn load_booking_target_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError> {
        self.bounded(async {
            sqlx::query_as::<_, BookingTargetRow>(
                r#"
                SELECT target.id AS target_id, target.city_id, target.target_kind,
                       target.display_name, target.capacity, target.version, target.active, target.accepts_booking, target.priority,
                       target.relationship_score,
                       EXISTS (
                           SELECT 1
                           FROM viryaos_autopilot_actions AS action
                           WHERE action.workspace_id = target.workspace_id
                             AND action.action_kind = 'booking.outreach.request'
                             AND action.status IN ('awaiting_approval', 'queued', 'processing')
                             AND action.payload ->> 'target_id' = target.id::text
                       ) AS outreach_in_flight,
                       target.last_outreach_at,
                       COALESCE((SELECT count(*)::integer FROM viryaos_booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='outbound' AND interaction.phase='followup'),0) AS followup_count,
                       COALESCE((SELECT interaction.disposition FROM viryaos_booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='inbound' AND interaction.phase='reply'
                         ORDER BY interaction.occurred_at DESC,interaction.id DESC LIMIT 1),'none') AS last_reply_disposition
                FROM viryaos_booking_targets AS target
                WHERE target.workspace_id = $1
                ORDER BY target.city_id, target.priority DESC,
                         target.relationship_score DESC, target.id
                LIMIT 2000
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(booking_target_snapshot)
            .collect()
        })
        .await
    }

    async fn load_outreach_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
        self.bounded(operations::load_outreach_snapshots(self, workspace_id, now))
            .await
    }

    async fn load_content_supply_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError> {
        self.bounded(operations::load_content_supply_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_experiment_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ExperimentSnapshot>, RepositoryError> {
        self.bounded(operations::load_experiment_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_show_task_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowTaskSnapshot>, RepositoryError> {
        self.bounded(operations::load_show_task_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_promotion_performance_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PromotionPerformanceSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PromotionSnapshotRow>(
                r#"
                SELECT
                    state.id AS campaign_id,
                    state.current_daily_budget_minor,
                    state.minimum_daily_budget_minor,
                    state.maximum_daily_budget_minor,
                    state.spend_last_7d_minor,
                    state.attributed_revenue_last_7d_minor,
                    SUM(state.current_daily_budget_minor) OVER (
                        PARTITION BY state.workspace_id, state.currency
                    ) AS workspace_daily_budget_minor,
                    SUM(state.spend_month_to_date_minor) OVER (
                        PARTITION BY state.workspace_id, state.currency
                    ) AS workspace_spend_month_to_date_minor,
                    guardrail.maximum_total_daily_budget_minor AS workspace_maximum_daily_budget_minor,
                    guardrail.maximum_monthly_spend_minor AS workspace_maximum_monthly_spend_minor,
                    CASE
                        WHEN event.starts_at IS NULL THEN 365::bigint
                        ELSE GREATEST(0, CEIL(EXTRACT(EPOCH FROM (event.starts_at - $2)) / 86400.0))::bigint
                    END AS days_to_event,
                    state.active,
                    COALESCE(last_change.finished_at, state.last_budget_change_at) AS last_budget_change_at,
                    state.observed_at,
                    state.expires_at
                FROM viryaos_promotion_campaign_states AS state
                LEFT JOIN events AS event
                  ON event.workspace_id = state.workspace_id
                 AND event.id = state.event_id
                LEFT JOIN viryaos_promotion_budget_guardrails AS guardrail
                  ON guardrail.workspace_id = state.workspace_id
                 AND guardrail.currency = state.currency
                LEFT JOIN LATERAL (
                    SELECT action.finished_at
                    FROM viryaos_autopilot_actions AS action
                    WHERE action.workspace_id = state.workspace_id
                      AND action.subject_id = state.id
                      AND action.action_kind = 'promotion.budget_change.request'
                      AND action.status = 'succeeded'
                    ORDER BY action.finished_at DESC, action.id DESC
                    LIMIT 1
                ) AS last_change ON true
                WHERE state.workspace_id = $1
                  AND state.active
                  AND state.expires_at > $2
                  AND (event.id IS NULL OR event.starts_at > $2)
                ORDER BY state.observed_at DESC, state.id
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter().map(promotion_snapshot).collect()
        })
        .await
    }

    async fn load_release_plan_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ReleasePlanSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, ReleaseSnapshotRow>(r#"
                SELECT plan.id AS release_id, plan.title, plan.release_at, plan.active,
                       plan.assets_ready, plan.communication_enabled, plan.press_enabled,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='seed_calendar') calendar_seeded,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='announcement') announcement_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='start_press') press_started,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='fan_warmup') fan_warmup_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='countdown') countdown_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='release_day') release_day_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='sustain') sustain_sent,
                       EXISTS(SELECT 1 FROM viryaos_release_milestones m WHERE m.workspace_id=plan.workspace_id AND m.release_id=plan.id AND m.milestone='wrap') wrap_sent
                FROM viryaos_release_plans plan
                WHERE plan.workspace_id=$1 AND plan.active
                  AND plan.release_at BETWEEN $2 - INTERVAL '30 days' AND $2 + INTERVAL '180 days'
                ORDER BY plan.release_at, plan.id
                LIMIT $3
            "#)
            .bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            Ok(rows.into_iter().map(|row| ReleasePlanSnapshot {
                release_id: ReleasePlanId::from_uuid(row.release_id), title: row.title,
                release_at: row.release_at, active: row.active, assets_ready: row.assets_ready,
                communication_enabled: row.communication_enabled, press_enabled: row.press_enabled,
                history: ReleaseMilestoneHistory {
                    calendar_seeded: row.calendar_seeded, announcement_sent: row.announcement_sent,
                    press_started: row.press_started, fan_warmup_sent: row.fan_warmup_sent,
                    countdown_sent: row.countdown_sent, release_day_sent: row.release_day_sent,
                    sustain_sent: row.sustain_sent, wrap_sent: row.wrap_sent,
                },
            }).collect())
        }).await
    }

    async fn load_live_opportunity_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows=sqlx::query_as::<_,LiveOpportunityRow>(r#"
                SELECT opportunity.id opportunity_id, opportunity.opportunity_kind,
                       opportunity.status NOT IN ('submitted','replied','won','lost','dismissed') active,
                       opportunity.verified_destination, opportunity.contact_email, opportunity.metadata,
                       opportunity.fit_basis_points, opportunity.reputation_basis_points,
                       opportunity.confidence_basis_points, opportunity.expected_fee_minor,
                       opportunity.estimated_cost_minor, opportunity.application_fee_minor,
                       opportunity.requires_contract, opportunity.exclusive, opportunity.deadline,
                       opportunity.status, opportunity.event_starts_at, opportunity.travel_band,
                       (
                           SELECT COUNT(*)
                           FROM events event
                           WHERE event.workspace_id=opportunity.workspace_id
                             AND event.status IN ('published','completed')
                             AND EXTRACT(YEAR FROM event.starts_at)=EXTRACT(
                                 YEAR FROM COALESCE(opportunity.event_starts_at,$2)
                             )
                       ) committed_shows_year,
                       COALESCE((manager.value->>'annual_target')::integer,15) annual_target,
                       COALESCE((manager.value->>'annual_stretch')::integer,20) annual_stretch,
                       COALESCE((manager.value->>'stretch_minimum_score_basis_points')::integer,9000)
                           stretch_minimum_score_basis_points,
                       COALESCE((manager.value->>'far_shot_minimum_score_basis_points')::integer,9000)
                           far_shot_minimum_score_basis_points,
                       COALESCE((manager.value->>'prefer_weekend_one_shots')::boolean,true)
                           prefer_weekend_one_shots
                FROM viryaos_team_opportunities opportunity
                LEFT JOIN viryaos_manager_config manager
                  ON manager.workspace_id=opportunity.workspace_id
                 AND manager.config_key='booking_policy'
                WHERE opportunity.workspace_id=$1
                  AND opportunity.opportunity_kind IN ('festival','showcase','review_contest','support_slot')
                  AND opportunity.eligible
                  AND opportunity.status IN ('new','prepared','awaiting_approval')
                  AND (opportunity.deadline IS NULL OR opportunity.deadline>$2)
                ORDER BY opportunity.deadline NULLS LAST,
                         opportunity.fit_basis_points DESC, opportunity.id
                LIMIT $3
            "#).bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
              .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            rows.into_iter().map(|row| {
                let kind=match row.opportunity_kind.as_str(){
                    "festival"=>LiveOpportunityKind::Festival,"showcase"=>LiveOpportunityKind::Showcase,
                    "review_contest"=>LiveOpportunityKind::ReviewContest,"support_slot"=>LiveOpportunityKind::SupportSlot,
                    _=>return Err(RepositoryError::Unexpected),
                };
                let travel_band=match row.travel_band.as_deref(){
                    Some("poland")=>Some(LiveTravelBand::Poland),
                    Some("east_germany")=>Some(LiveTravelBand::EastGermany),
                    Some("czechia_slovakia")=>Some(LiveTravelBand::CzechiaSlovakia),
                    Some("far_shot")=>Some(LiveTravelBand::FarShot),
                    None=>None,
                    Some(_)=>return Err(RepositoryError::Unexpected),
                };
                Ok(LiveOpportunitySnapshot{
                    opportunity_id:TeamOpportunityId::from_uuid(row.opportunity_id),kind,active:row.active,
                    verified_destination:row.verified_destination,
                    auto_submission_capable: (row.contact_email.as_ref().is_some_and(|email| !email.trim().is_empty())
                        || row.metadata.get("submission_adapter").and_then(serde_json::Value::as_str).is_some_and(|value| value=="email"))
                        && !row.metadata.get("discovery").and_then(|value|value.get("fee_unverified")).and_then(serde_json::Value::as_bool).unwrap_or(false)
                        && !row.metadata.get("discovery").and_then(|value|value.get("terms_unverified")).and_then(serde_json::Value::as_bool).unwrap_or(false),
                    fit_basis_points:u16::try_from(row.fit_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    reputation_basis_points:u16::try_from(row.reputation_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    evidence_confidence:parse_confidence(row.confidence_basis_points)?,
                    expected_fee_minor:row.expected_fee_minor, estimated_cost_minor:row.estimated_cost_minor,
                    application_fee_minor:row.application_fee_minor, requires_contract:row.requires_contract,
                    exclusive:row.exclusive, deadline:row.deadline, event_starts_at:row.event_starts_at,
                    travel_band, committed_shows_year:u16::try_from(row.committed_shows_year).unwrap_or(u16::MAX),
                    annual_target:u16::try_from(row.annual_target).map_err(|_|RepositoryError::Unexpected)?,
                    annual_stretch:u16::try_from(row.annual_stretch).map_err(|_|RepositoryError::Unexpected)?,
                    stretch_minimum_score_basis_points:u16::try_from(row.stretch_minimum_score_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    far_shot_minimum_score_basis_points:u16::try_from(row.far_shot_minimum_score_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                    prefer_weekend_one_shots:row.prefer_weekend_one_shots,
                    already_applied:matches!(row.status.as_str(),"submitted"|"replied"|"won"|"lost"),
                })
            }).collect()
        }).await
    }

    async fn load_funding_opportunity_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FundingOpportunitySnapshot>, RepositoryError> {
        self.bounded(async {
            let rows=sqlx::query_as::<_,TeamOpportunityRow>(r#"
                SELECT id opportunity_id, opportunity_kind, status NOT IN ('submitted','won','lost','dismissed') active,
                       verified_destination,contact_email,metadata,fit_basis_points,reputation_basis_points,confidence_basis_points,
                       expected_fee_minor,estimated_cost_minor,application_fee_minor,requires_contract,exclusive,eligible,
                       funding_amount_minor,own_contribution_minor,deadline,package_status,status
                FROM viryaos_team_opportunities
                WHERE workspace_id=$1 AND opportunity_kind='funding' AND eligible
                  AND status IN ('new','prepared','awaiting_approval') AND deadline>$2
                ORDER BY deadline, funding_amount_minor DESC, id LIMIT $3
            "#).bind(workspace_id.into_uuid()).bind(now).bind(MAX_SNAPSHOTS_PER_CONTEXT)
              .fetch_all(&self.pool).await.map_err(map_sqlx)?;
            rows.into_iter().map(|row| Ok(FundingOpportunitySnapshot{
                opportunity_id:TeamOpportunityId::from_uuid(row.opportunity_id),active:row.active,eligible:row.eligible,
                evidence_confidence:parse_confidence(row.confidence_basis_points)?,
                fit_basis_points:u16::try_from(row.fit_basis_points).map_err(|_|RepositoryError::Unexpected)?,
                amount_minor:row.funding_amount_minor,own_contribution_minor:row.own_contribution_minor,
                deadline:row.deadline.ok_or(RepositoryError::Unexpected)?,package_prepared:row.package_status=="ready",
                submitted:matches!(row.status.as_str(),"submitted"|"won"|"lost"),
            })).collect()
        }).await
    }

    async fn load_beacon_discovery_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError> {
        self.bounded(operations::load_beacon_discovery_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_beacon_campaign_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError> {
        self.bounded(operations::load_beacon_campaign_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

    async fn load_show_growth_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError> {
        self.bounded(operations::load_show_growth_snapshots(
            self,
            workspace_id,
            now,
        ))
        .await
    }

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
    };
}
