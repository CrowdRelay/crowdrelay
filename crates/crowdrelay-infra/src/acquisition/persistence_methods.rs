impl PostgresAcquisitionRepository {
    async fn trusted_workspace_id_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<WorkspaceId, StoreError> {
        let id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(self.workspace_slug.as_str())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(StoreError::from_sqlx)?
                .ok_or(StoreError::NotFound)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    async fn ensure_campaign_is_active(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        signup: &FanSignup,
    ) -> Result<(), StoreError> {
        let Some(campaign_id) = signup.campaign_id() else {
            return Ok(());
        };
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM campaigns
                WHERE workspace_id = $1
                    AND id = $2
                    AND active
            )
            "#,
        )
        .bind(signup.workspace_id().into_uuid())
        .bind(campaign_id.into_uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn start_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
    ) -> Result<bool, StoreError> {
        let lease_milliseconds = duration_as_milliseconds(self.operation_timeout)?;
        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        let result = sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id,
                scope,
                key,
                request_hash,
                state,
                lease_owner,
                lease_expires_at,
                expires_at
            )
            VALUES (
                $1,
                $2,
                $3,
                $4,
                'in_progress',
                $5,
                now() + ($6::bigint * interval '1 millisecond'),
                now() + ($7::bigint * interval '1 millisecond')
            )
            ON CONFLICT (workspace_id, scope, key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(command.request_id().as_str())
        .bind(lease_milliseconds)
        .bind(IDEMPOTENCY_RETENTION_MILLISECONDS)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(result.rows_affected() == 1)
    }

    async fn lock_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
    ) -> Result<IdempotencyRow, StoreError> {
        sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT
                request_hash,
                state,
                response_body,
                response_content_type,
                COALESCE(lease_expires_at <= now(), false) AS lease_expired
            FROM idempotency_keys
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)
    }

    async fn reclaim_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
    ) -> Result<(), StoreError> {
        let lease_milliseconds = duration_as_milliseconds(self.operation_timeout)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                lease_owner = $4,
                lease_expires_at =
                    now() + ($5::bigint * interval '1 millisecond')
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND state = 'in_progress'
                AND lease_expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(command.request_id().as_str())
        .bind(lease_milliseconds)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    async fn upsert_fan(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        signup: &FanSignup,
    ) -> Result<FanUpsert, StoreError> {
        let inserted = sqlx::query_as::<_, FanRow>(
            r#"
            INSERT INTO fans (
                workspace_id,
                normalized_email,
                display_name,
                locale,
                status
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, normalized_email) DO NOTHING
            RETURNING id, status
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(signup.email().as_str())
        .bind(signup.display_name())
        .bind(signup.locale())
        .bind(if self.require_double_opt_in {
            "pending"
        } else {
            "active"
        })
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        if let Some(row) = inserted {
            let fan: StoredFan = row.try_into()?;
            return Ok(FanUpsert {
                fan,
                created: true,
                became_active: false,
                already_active: false,
                already_pending: false,
            });
        }

        let existing = sqlx::query_as::<_, FanRow>(
            r#"
            SELECT id, status
            FROM fans
            WHERE workspace_id = $1
                AND normalized_email = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(signup.email().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        let existing: StoredFan = existing.try_into()?;
        if existing.status == FanStatus::Suppressed {
            return Err(StoreError::Conflict);
        }
        if existing.status == FanStatus::Active {
            return Ok(FanUpsert {
                fan: existing,
                created: false,
                became_active: false,
                already_active: true,
                already_pending: false,
            });
        }
        if existing.status == FanStatus::Pending {
            return Ok(FanUpsert {
                fan: existing,
                created: false,
                became_active: false,
                already_active: false,
                already_pending: true,
            });
        }

        let row = sqlx::query_as::<_, FanRow>(
            r#"
            UPDATE fans
            SET
                display_name = COALESCE($3, display_name),
                locale = COALESCE($4, locale),
                status = 'pending'
            WHERE workspace_id = $1
                AND id = $2
            RETURNING id, status
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(existing.id.into_uuid())
        .bind(signup.display_name())
        .bind(signup.locale())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        let fan: StoredFan = row.try_into()?;
        let became_active = existing.status != FanStatus::Active && fan.status == FanStatus::Active;
        Ok(FanUpsert {
            fan,
            created: false,
            became_active,
            already_active: false,
            already_pending: false,
        })
    }

    async fn fan_action_resend_is_in_cooldown(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        purpose: &str,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM fan_action_tokens
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND purpose = $3
                    AND consumed_at IS NULL
                    AND expires_at > now()
                    AND created_at >
                        now() - ($4::bigint * interval '1 minute')
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(purpose)
        .bind(CONFIRMATION_RESEND_COOLDOWN_MINUTES)
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)
    }

    async fn append_consent(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        command: &SignupFanCommand,
    ) -> Result<(), StoreError> {
        let consent = command.signup().consent();
        sqlx::query(
            r#"
            INSERT INTO fan_consents (
                workspace_id,
                fan_id,
                purpose,
                granted,
                policy_version,
                source,
                request_id
            )
            VALUES ($1, $2, 'marketing', $3, $4, $5, $6)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(consent.granted())
        .bind(consent.policy_version())
        .bind(consent.source())
        .bind(command.request_id().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn resolve_city(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        city_slug: &CitySlug,
    ) -> Result<CityId, StoreError> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM cities
            WHERE country_code = $1
                AND slug = $2
            "#,
        )
        .bind(self.default_country_code.as_str())
        .bind(city_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?
        .ok_or(StoreError::NotFound)?;
        Ok(CityId::from_uuid(id))
    }

    async fn insert_city_interest(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        city_id: CityId,
    ) -> Result<bool, StoreError> {
        let inserted = sqlx::query_scalar::<_, i32>(
            r#"
            INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (workspace_id, fan_id, city_id) DO NOTHING
            RETURNING 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(city_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(inserted.is_some())
    }

    async fn increment_city_aggregate(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        city_id: CityId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO city_aggregates (
                workspace_id,
                city_id,
                confirmed_fan_count
            )
            VALUES ($1, $2, 1)
            ON CONFLICT (workspace_id, city_id) DO UPDATE
            SET
                confirmed_fan_count =
                    city_aggregates.confirmed_fan_count + 1,
                updated_at = now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(city_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn increment_city_aggregates_for_fan(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO city_aggregates (
                workspace_id,
                city_id,
                confirmed_fan_count
            )
            SELECT
                workspace_id,
                city_id,
                1
            FROM fan_city_interests
            WHERE workspace_id = $1
                AND fan_id = $2
            ON CONFLICT (workspace_id, city_id) DO UPDATE
            SET
                confirmed_fan_count =
                    city_aggregates.confirmed_fan_count + 1,
                updated_at = now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn load_or_create_referral_code(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
    ) -> Result<ReferralCode, StoreError> {
        let existing = sqlx::query_scalar::<_, String>(
            r#"
            SELECT code
            FROM referral_codes
            WHERE workspace_id = $1
                AND fan_id = $2
                AND active
            ORDER BY created_at, id
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if let Some(code) = existing {
            return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
        }

        for _ in 0..3 {
            let inserted = sqlx::query_scalar::<_, String>(
                r#"
                INSERT INTO referral_codes (
                    workspace_id,
                    fan_id,
                    code
                )
                VALUES ($1, $2, encode(gen_random_bytes(18), 'hex'))
                ON CONFLICT DO NOTHING
                RETURNING code
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if let Some(code) = inserted {
                return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
            }

            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT code
                FROM referral_codes
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND active
                ORDER BY created_at, id
                LIMIT 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if let Some(code) = existing {
                return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
            }
        }

        Err(StoreError::Unavailable)
    }

    async fn resolve_claimed_referral(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        referred_fan_id: FanId,
        signup: &FanSignup,
    ) -> Result<Option<ReferralOwnerRow>, StoreError> {
        let Some(code) = signup.claimed_referral_code() else {
            return Ok(None);
        };
        let referral = sqlx::query_as::<_, ReferralOwnerRow>(
            r#"
            SELECT id, fan_id
            FROM referral_codes
            WHERE workspace_id = $1
                AND code = $2
                AND active
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(code.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        let Some(referral) = referral else {
            return Ok(None);
        };
        if referral.fan_id == referred_fan_id.into_uuid() {
            return Ok(None);
        }
        Ok(Some(referral))
    }

    async fn insert_acquisition_event(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        signup: &FanSignup,
        request_id: &str,
        referral: Option<&ReferralOwnerRow>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO fan_acquisition_events (
                workspace_id,
                fan_id,
                campaign_id,
                anonymous_visitor_id,
                source,
                request_id,
                referral_code_id,
                referrer_fan_id,
                occurred_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(signup.campaign_id().map(Into::<Uuid>::into))
        .bind(signup.visitor_id().map(Into::<Uuid>::into))
        .bind(signup.consent().source())
        .bind(request_id)
        .bind(referral.as_ref().map(|row| row.id))
        .bind(referral.map(|row| row.fan_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        self.record_community_conversion(transaction, workspace_id, fan_id, signup)
            .await
    }

    /// Links a signup back to the community that sent it, when the visitor
    /// arrived through a community-tagged smart link.
    ///
    /// This is the conversion half of the provenance chain the ledger was
    /// built for. Exposure was already being recorded at dispatch; nothing
    /// wrote conversion, so every community-level outcome query matched
    /// nothing — and `COUNT` reports nothing as zero, which reads exactly like
    /// a community that converted no one. The two facts are not the same and
    /// the measurement layer cannot tell them apart on its own.
    ///
    /// Attribution is the visitor's most recent community-tagged click inside
    /// thirty days, recorded as such. Naming the method is the point: this
    /// records an observed click, not a causal claim, and the causal layer is
    /// still the only thing entitled to make one.
    async fn record_community_conversion(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        signup: &FanSignup,
    ) -> Result<(), StoreError> {
        let Some(visitor_id) = signup.visitor_id() else {
            return Ok(());
        };
        sqlx::query(
            r#"
            INSERT INTO fan_provenance_events (
                workspace_id, fan_id, event_kind, channel, source_target,
                community, campaign_id, attribution_method,
                attribution_confidence, occurred_at
            )
            SELECT $1, $2, 'conversion',
                   COALESCE(link.channel_source, 'smart_link'),
                   link.slug, link.channel_community, click.campaign_id,
                   'last_community_click', 1.0, now()
            FROM click_events AS click
            JOIN smart_links AS link
              ON link.workspace_id = click.workspace_id
             AND link.id = click.smart_link_id
            WHERE click.workspace_id = $1
              AND click.anonymous_visitor_id = $3
              AND link.channel_community IS NOT NULL
              AND click.occurred_at >= now() - INTERVAL '30 days'
            ORDER BY click.occurred_at DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(Into::<Uuid>::into(visitor_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn append_fan_active_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        args: FanActiveOutboxArgs<'_>,
    ) -> Result<(), StoreError> {
        let FanActiveOutboxArgs {
            workspace_id,
            command,
            fan_id,
            referral_code,
            unsubscribe_token,
            created,
        } = args;
        let signup = command.signup();
        let payload = json!({
            "workspace_id": workspace_id,
            "fan_id": fan_id,
            "email": signup.email().as_str(),
            "display_name": signup.display_name(),
            "locale": signup.locale(),
            "city_slug": signup.city_slug(),
            "campaign_id": signup.campaign_id(),
            "referral_code": referral_code,
            "unsubscribe_token": unsubscribe_token.as_str(),
            "policy_version": signup.consent().policy_version(),
        });
        let event_type = if created {
            "fan.created"
        } else {
            "fan.reactivated"
        };
        self.append_outbox(transaction, workspace_id, event_type, command, payload)
            .await
    }

    async fn append_confirmation_requested_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        fan_id: FanId,
        confirmation_token: &crowdrelay_domain::FanActionToken,
    ) -> Result<(), StoreError> {
        let signup = command.signup();
        self.append_outbox(
            transaction,
            workspace_id,
            "fan.confirmation_requested",
            command,
            json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": signup.email().as_str(),
                "display_name": signup.display_name(),
                "locale": signup.locale(),
                "city_slug": signup.city_slug(),
                "confirmation_token": confirmation_token.as_str(),
                "policy_version": signup.consent().policy_version(),
            }),
        )
        .await
    }

    async fn append_session_requested_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        fan_id: FanId,
        recovery_token: &FanActionToken,
    ) -> Result<(), StoreError> {
        let signup = command.signup();
        self.append_outbox(
            transaction,
            workspace_id,
            "fan.session_requested",
            command,
            json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": signup.email().as_str(),
                "display_name": signup.display_name(),
                "locale": signup.locale(),
                "session_recovery_token": recovery_token.as_str(),
            }),
        )
        .await
    }

    async fn append_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        event_type: &str,
        command: &SignupFanCommand,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            )
            VALUES ($1, $2, 1, $3, $4)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_type)
        .bind(payload)
        .bind(command.request_id().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn complete_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
        result: &FanSignupResult,
    ) -> Result<(), StoreError> {
        let response = self
            .sensitive_response_codec
            .encrypt(
                workspace_id,
                IDEMPOTENCY_SCOPE,
                command.idempotency_key().as_str(),
                result,
            )
            .map_err(|_| StoreError::Unexpected)?;
        let response_status = if result.confirmation_required {
            202
        } else if result.created {
            201
        } else {
            200
        };
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                state = 'completed',
                lease_owner = NULL,
                lease_expires_at = NULL,
                response_status = $5,
                response_body = $6,
                response_content_type = $7,
                completed_at = now()
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND request_hash = $4
                AND state = 'in_progress'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(response_status)
        .bind(response)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    async fn refresh_completed_idempotency_response(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
        result: &FanSignupResult,
    ) -> Result<(), StoreError> {
        let response = self
            .sensitive_response_codec
            .encrypt(
                workspace_id,
                IDEMPOTENCY_SCOPE,
                command.idempotency_key().as_str(),
                result,
            )
            .map_err(|_| StoreError::Unexpected)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                response_body = $5,
                response_content_type = $6
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND request_hash = $4
                AND state = 'completed'
                AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(response)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
}
