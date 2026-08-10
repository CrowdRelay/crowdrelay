impl PostgresAcquisitionRepository {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_slug: WorkspaceSlug,
        default_country_code: CountryCode,
        database: &DatabaseConfig,
        require_double_opt_in: bool,
        sensitive_response_codec: SensitiveResponseCodec,
    ) -> Self {
        Self {
            pool,
            workspace_slug,
            default_country_code,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
            require_double_opt_in,
            sensitive_response_codec,
        }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| StoreError::Unavailable)?
    }

    async fn trusted_workspace_id_inner(&self) -> Result<WorkspaceId, StoreError> {
        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(self.workspace_slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?
            .ok_or(StoreError::NotFound)?;

        Ok(WorkspaceId::from_uuid(id))
    }

    async fn resolve_workspace_inner(
        &self,
        slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, StoreError> {
        if slug != &self.workspace_slug {
            return Ok(None);
        }

        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?;

        Ok(id.map(WorkspaceId::from_uuid))
    }

    async fn load_active_smart_links_inner(&self) -> Result<Vec<ResolvedSmartLink>, StoreError> {
        let rows = sqlx::query_as::<_, SmartLinkRow>(
            r#"
            SELECT
                smart_links.id,
                smart_links.workspace_id,
                smart_links.campaign_id,
                smart_links.slug,
                smart_links.destination_url,
                smart_links.version
            FROM smart_links
            INNER JOIN workspaces
                ON workspaces.id = smart_links.workspace_id
            LEFT JOIN campaigns
                ON campaigns.workspace_id = smart_links.workspace_id
                AND campaigns.id = smart_links.campaign_id
            WHERE workspaces.slug = $1
                AND smart_links.active
                AND (
                    smart_links.campaign_id IS NULL
                    OR campaigns.active
                )
            ORDER BY smart_links.slug, smart_links.id
            "#,
        )
        .bind(self.workspace_slug.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        rows.into_iter()
            .map(|row| ResolvedSmartLink::try_from(row).map_err(|_| StoreError::Unexpected))
            .collect()
    }

    async fn persist_click_batch_inner(&self, clicks: &[ClickEvent]) -> Result<(), StoreError> {
        if clicks.is_empty() {
            return Ok(());
        }
        if clicks.len() > MAX_CLICK_BATCH_ROWS {
            return Err(StoreError::Conflict);
        }

        let workspace_id = self.trusted_workspace_id_inner().await?;
        if clicks
            .iter()
            .any(|click| click.workspace_id() != workspace_id)
        {
            return Err(StoreError::NotFound);
        }

        let workspace_ids = vec![workspace_id.into_uuid(); clicks.len()];
        let smart_link_ids: Vec<Uuid> = clicks
            .iter()
            .map(|click| click.smart_link_id().into_uuid())
            .collect();
        let campaign_ids: Vec<Option<Uuid>> = clicks
            .iter()
            .map(|click| click.campaign_id().map(Into::into))
            .collect();
        let visitor_ids: Vec<Option<Uuid>> = clicks
            .iter()
            .map(|click| click.visitor_id().map(Into::into))
            .collect();
        let referrer_hosts: Vec<Option<String>> = clicks
            .iter()
            .map(|click| click.referrer_host().map(str::to_owned))
            .collect();
        let occurred_at: Vec<OffsetDateTime> = clicks.iter().map(ClickEvent::occurred_at).collect();

        let result = sqlx::query(
            r#"
            WITH candidates (
                workspace_id,
                smart_link_id,
                campaign_id,
                anonymous_visitor_id,
                referrer_host,
                occurred_at
            ) AS (
                SELECT *
                FROM UNNEST(
                    $1::uuid[],
                    $2::uuid[],
                    $3::uuid[],
                    $4::uuid[],
                    $5::text[],
                    $6::timestamptz[]
                )
            ),
            valid_candidates AS (
                SELECT candidates.*
                FROM candidates
                INNER JOIN smart_links
                    ON smart_links.workspace_id = candidates.workspace_id
                    AND smart_links.id = candidates.smart_link_id
                    AND candidates.campaign_id
                        IS NOT DISTINCT FROM smart_links.campaign_id
            )
            INSERT INTO click_events (
                workspace_id,
                smart_link_id,
                campaign_id,
                anonymous_visitor_id,
                referrer_host,
                occurred_at
            )
            SELECT valid_candidates.*
            FROM valid_candidates
            WHERE
                (SELECT count(*) FROM valid_candidates)
                = (SELECT count(*) FROM candidates)
            "#,
        )
        .bind(&workspace_ids)
        .bind(&smart_link_ids)
        .bind(&campaign_ids)
        .bind(&visitor_ids)
        .bind(&referrer_hosts)
        .bind(&occurred_at)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        if result.rows_affected() != u64::try_from(clicks.len()).unwrap_or(u64::MAX) {
            return Err(StoreError::Conflict);
        }

        Ok(())
    }

    async fn list_city_signals_inner(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, StoreError> {
        if !(1..=MAX_CITY_SIGNAL_ROWS).contains(&limit) {
            return Err(StoreError::Conflict);
        }
        if self.trusted_workspace_id_inner().await? != workspace_id {
            return Err(StoreError::NotFound);
        }

        let rows = sqlx::query_as::<_, CitySignalRow>(
            r#"
            SELECT
                cities.id AS city_id,
                cities.slug,
                cities.name,
                cities.country_code::text AS country_code,
                city_aggregates.confirmed_fan_count AS fan_count
            FROM city_aggregates
            INNER JOIN cities
                ON cities.id = city_aggregates.city_id
            WHERE city_aggregates.workspace_id = $1
                AND cities.country_code = $2
                AND cities.moderation_status = 'approved'
            ORDER BY
                city_aggregates.confirmed_fan_count DESC,
                cities.name,
                cities.id
            LIMIT $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(self.default_country_code.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        rows.into_iter()
            .map(|row| CitySignal::try_from(row).map_err(|_| StoreError::Unexpected))
            .collect()
    }

    async fn persist_fan_signup_inner(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, StoreError> {
        let signup = command.signup();
        signup.validate().map_err(|_| StoreError::Conflict)?;
        if !signup.consent().granted() {
            return Err(StoreError::Conflict);
        }

        let request_bytes = serde_json::to_vec(signup).map_err(|_| StoreError::Unexpected)?;
        let request_hash = Sha256::digest(request_bytes).to_vec();
        let mut transaction = self.pool.begin().await.map_err(StoreError::from_sqlx)?;
        self.configure_transaction(&mut transaction).await?;

        let workspace_id = self
            .trusted_workspace_id_in_transaction(&mut transaction)
            .await?;
        if signup.workspace_id() != workspace_id {
            return Err(StoreError::NotFound);
        }

        let inserted_idempotency = self
            .start_idempotency(&mut transaction, workspace_id, command, &request_hash)
            .await?;
        let idempotency = self
            .lock_idempotency(&mut transaction, workspace_id, command)
            .await?;
        if idempotency.request_hash != request_hash {
            return Err(StoreError::Conflict);
        }
        if idempotency.state == "completed" {
            let response = idempotency.response_body.ok_or(StoreError::Unexpected)?;
            let result: FanSignupResult = match idempotency.response_content_type.as_deref() {
                Some(ENCRYPTED_JSON_CONTENT_TYPE) => self
                    .sensitive_response_codec
                    .decrypt(
                        workspace_id,
                        IDEMPOTENCY_SCOPE,
                        command.idempotency_key().as_str(),
                        response,
                    )
                    .map_err(|_| StoreError::Unexpected)?,
                Some(JSON_CONTENT_TYPE) | None => {
                    serde_json::from_value(response).map_err(|_| StoreError::Unexpected)?
                }
                Some(_) => return Err(StoreError::Unexpected),
            };
            self.refresh_completed_idempotency_response(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }
        if idempotency.state != "in_progress" {
            return Err(StoreError::Unexpected);
        }
        if !inserted_idempotency {
            if !idempotency.lease_expired {
                return Err(StoreError::Conflict);
            }
            self.reclaim_idempotency(&mut transaction, workspace_id, command)
                .await?;
        }

        let fan_upsert = self
            .upsert_fan(&mut transaction, workspace_id, signup)
            .await?;
        if fan_upsert.already_active {
            let resend_is_too_soon = self
                .fan_action_resend_is_in_cooldown(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "session",
                )
                .await?;
            if !resend_is_too_soon {
                let recovery_token = issue_fan_action_token(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "session",
                    2,
                )
                .await
                .map_err(map_lifecycle_error)?;
                self.append_session_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &recovery_token,
                )
                .await?;
            }
            let result = FanSignupResult {
                fan_id: fan_upsert.fan.id,
                status: FanStatus::Active,
                referral_code: None,
                fan_session_token: None,
                confirmation_required: true,
                created: false,
                email_kind: Some(FanSignupEmailKind::SessionRecovery),
                email_queued: !resend_is_too_soon,
                retry_after_seconds: resend_is_too_soon
                    .then_some(CONFIRMATION_RESEND_COOLDOWN_SECONDS),
            };
            self.complete_idempotency(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }
        if fan_upsert.already_pending {
            let resend_is_too_soon = self
                .fan_action_resend_is_in_cooldown(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "confirm",
                )
                .await?;
            if !resend_is_too_soon {
                let confirmation_token =
                    issue_confirmation_token(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_lifecycle_error)?;
                self.append_confirmation_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &confirmation_token,
                )
                .await?;
            }
            let result = FanSignupResult {
                fan_id: fan_upsert.fan.id,
                status: FanStatus::Pending,
                referral_code: None,
                fan_session_token: None,
                confirmation_required: true,
                created: false,
                email_kind: Some(FanSignupEmailKind::Confirmation),
                email_queued: !resend_is_too_soon,
                retry_after_seconds: resend_is_too_soon
                    .then_some(CONFIRMATION_RESEND_COOLDOWN_SECONDS),
            };
            self.complete_idempotency(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }

        self.ensure_campaign_is_active(&mut transaction, signup)
            .await?;
        self.append_consent(&mut transaction, workspace_id, fan_upsert.fan.id, command)
            .await?;
        let city_id = self
            .resolve_city(&mut transaction, signup.city_slug())
            .await?;
        let city_interest_created = self
            .insert_city_interest(&mut transaction, workspace_id, fan_upsert.fan.id, city_id)
            .await?;
        if fan_upsert.became_active {
            self.increment_city_aggregates_for_fan(
                &mut transaction,
                workspace_id,
                fan_upsert.fan.id,
            )
            .await?;
        } else if fan_upsert.fan.status == FanStatus::Active && city_interest_created {
            self.increment_city_aggregate(&mut transaction, workspace_id, city_id)
                .await?;
        }

        let claimed_referral = self
            .resolve_claimed_referral(&mut transaction, workspace_id, fan_upsert.fan.id, signup)
            .await?;
        self.insert_acquisition_event(
            &mut transaction,
            workspace_id,
            fan_upsert.fan.id,
            signup,
            command.request_id().as_str(),
            claimed_referral.as_ref(),
        )
        .await?;
        record_pending_signup_referral(
            &mut transaction,
            workspace_id,
            fan_upsert.fan.id,
            claimed_referral.as_ref().map(|row| row.id),
            claimed_referral.as_ref().map(|row| row.fan_id),
        )
        .await
        .map_err(map_referral_error)?;

        let (referral_code, fan_session_token, confirmation_required, email_kind, email_queued) =
            if fan_upsert.fan.status == FanStatus::Active {
                qualify_signup_referral_and_rewards(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    claimed_referral.as_ref().map(|row| row.id),
                    claimed_referral.as_ref().map(|row| row.fan_id),
                    command.request_id().as_str(),
                )
                .await
                .map_err(map_referral_error)?;
                let referral_code = self
                    .load_or_create_referral_code(&mut transaction, workspace_id, fan_upsert.fan.id)
                    .await?;
                let fan_session_token =
                    issue_fan_session(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_referral_error)?;
                let unsubscribe_token = issue_fan_action_token(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "unsubscribe",
                    730,
                )
                .await
                .map_err(map_lifecycle_error)?;
                if fan_upsert.created || fan_upsert.became_active {
                    self.append_fan_active_outbox(
                        &mut transaction,
                        FanActiveOutboxArgs {
                            workspace_id,
                            command,
                            fan_id: fan_upsert.fan.id,
                            referral_code: &referral_code,
                            unsubscribe_token: &unsubscribe_token,
                            created: fan_upsert.created,
                        },
                    )
                    .await?;
                }
                (
                    Some(referral_code),
                    Some(fan_session_token),
                    false,
                    None,
                    false,
                )
            } else {
                let confirmation_token =
                    issue_confirmation_token(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_lifecycle_error)?;
                self.append_confirmation_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &confirmation_token,
                )
                .await?;
                (
                    None,
                    None,
                    true,
                    Some(FanSignupEmailKind::Confirmation),
                    true,
                )
            };

        let result = FanSignupResult {
            fan_id: fan_upsert.fan.id,
            status: fan_upsert.fan.status,
            referral_code,
            fan_session_token,
            confirmation_required,
            created: fan_upsert.created,
            email_kind,
            email_queued,
            retry_after_seconds: None,
        };
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            &result,
        )
        .await?;

        transaction.commit().await.map_err(StoreError::from_sqlx)?;
        Ok(result)
    }
}
