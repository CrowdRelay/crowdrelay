impl PostgresAdmissionRepository {
    async fn issue_inner(
        &self,
        command: &IssueAdmissionPassCommand,
    ) -> Result<AdmissionPassIssued, AdmissionStoreError> {
        let request_hash = issue_request_hash(command);
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            ISSUE_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_sensitive_idempotent::<AdmissionPassIssued>(
                &mut transaction,
                workspace_id,
                ISSUE_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }

        let event_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM events
            WHERE workspace_id = $1
              AND slug = $2
              AND status = 'published'
              AND COALESCE(ends_at, starts_at + interval '12 hours') > now()
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.event_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let pool = sqlx::query_as::<_, PoolRow>(
            r#"
            SELECT id, issued_count, reserved_count, capacity
            FROM admission_pools
            WHERE workspace_id = $1 AND event_id = $2 AND slug = $3 AND active
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .bind(command.pool_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let fan = sqlx::query_as::<_, FanRow>(
            r#"
            SELECT id, normalized_email, display_name
            FROM fans
            WHERE workspace_id = $1
              AND normalized_email = $2
              AND status = 'active'
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.fan_email.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let duplicate = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM admission_passes
                WHERE workspace_id = $1
                  AND admission_pool_id = $2
                  AND fan_id = $3
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pool.id)
        .bind(fan.id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        if duplicate || pool.issued_count.saturating_add(pool.reserved_count) >= pool.capacity {
            return Err(AdmissionStoreError::Conflict);
        }

        let admin_member_id = self
            .member_id(&mut transaction, workspace_id, &self.admin_member_email)
            .await?;
        let secret = sqlx::query_as::<_, SecretRow>(
            r#"
            SELECT
                encode(gen_random_bytes(32), 'hex') AS token,
                'VIRYA-' || upper(substr(encode(gen_random_bytes(12), 'hex'), 1, 12))
                    AS public_reference
            "#,
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let claim_token =
            PassClaimToken::parse(&secret.token).map_err(|_| AdmissionStoreError::Unexpected)?;
        let claim_expires_at = OffsetDateTime::now_utc()
            .checked_add(time::Duration::hours(i64::from(
                command.claim_expires_hours,
            )))
            .ok_or(AdmissionStoreError::Unexpected)?;

        let pass_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issued_by_member_id, issuance_method, public_reference,
                claim_token_hash, claim_expires_at, status
            )
            VALUES ($1, $2, $3, $4, $5, $6, 'manual', $7, digest($8, 'sha256'), $9, 'issued')
            "#,
        )
        .bind(pass_id)
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .bind(pool.id)
        .bind(fan.id)
        .bind(admin_member_id)
        .bind(&secret.public_reference)
        .bind(claim_token.as_str())
        .bind(claim_expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        sqlx::query(
            "UPDATE admission_pools SET issued_count = issued_count + 1 \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(pool.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;

        let result = AdmissionPassIssued {
            pass_id: AdmissionPassId::from_uuid(pass_id),
            event_id: EventId::from_uuid(event_id),
            fan_id: FanId::from_uuid(fan.id),
            public_reference: secret.public_reference.clone(),
            claim_token: claim_token.clone(),
            claim_expires_at,
            created: true,
        };
        self.append_outbox(
            &mut transaction,
            workspace_id,
            "admission.pass.issued",
            command.request_id.as_str(),
            json!({
                "pass_id": pass_id,
                "event_id": event_id,
                "fan_id": fan.id,
                "email": &fan.normalized_email,
                "display_name": &fan.display_name,
                "public_reference": &result.public_reference,
                "claim_token": claim_token.as_str(),
                "claim_expires_at": claim_expires_at,
            }),
        )
        .await?;
        self.append_audit(
            &mut transaction,
            AuditEventArgs {
                workspace_id,
                member_id: admin_member_id,
                action: "admission.pass.issued",
                target_type: "admission_pass",
                target_id: pass_id,
                request_id: command.request_id.as_str(),
            },
        )
        .await?;
        self.complete_sensitive_idempotency(
            &mut transaction,
            workspace_id,
            ISSUE_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(result)
    }

    async fn claim_inner(
        &self,
        command: &ClaimAdmissionPassCommand,
    ) -> Result<AdmissionPassClaimed, AdmissionStoreError> {
        let request_hash = Sha256::digest(command.token.as_str().as_bytes()).to_vec();
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            CLAIM_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_sensitive_idempotent::<AdmissionPassClaimed>(
                &mut transaction,
                workspace_id,
                CLAIM_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }
        let pass = sqlx::query_as::<_, ClaimRow>(
            r#"
            SELECT
                pass.id,
                pass.event_id,
                pass.admission_pool_id,
                pass.status,
                pass.claim_expires_at,
                GREATEST(
                    now() + ($3::bigint * interval '1 day'),
                    COALESCE(event.ends_at, event.starts_at) + interval '1 day'
                ) AS session_expires_at
            FROM admission_passes AS pass
            JOIN events AS event
                ON event.workspace_id = pass.workspace_id
                AND event.id = pass.event_id
            WHERE pass.workspace_id = $1
                AND pass.claim_token_hash = digest($2, 'sha256')
            FOR UPDATE OF pass
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.token.as_str())
        .bind(PASS_SESSION_DAYS)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if pass.status != "issued" {
            return Err(AdmissionStoreError::Conflict);
        }
        if pass.claim_expires_at <= OffsetDateTime::now_utc() {
            sqlx::query(
                "UPDATE admission_passes SET status = 'expired', claim_token_hash = NULL \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            sqlx::query(
                "UPDATE admission_pools \
                 SET issued_count = GREATEST(issued_count - 1, 0) \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.admission_pool_id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Err(AdmissionStoreError::Conflict);
        }

        let session_secret =
            sqlx::query_scalar::<_, String>("SELECT encode(gen_random_bytes(32), 'hex')")
                .fetch_one(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
        let session_token =
            PassSessionToken::parse(session_secret).map_err(|_| AdmissionStoreError::Unexpected)?;
        let session_id = Uuid::now_v7();
        let session_expires_at = pass.session_expires_at;
        sqlx::query(
            r#"
            UPDATE admission_passes
            SET status = 'claimed', claim_token_hash = NULL, claim_token_consumed_at = now(), claimed_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pass.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let session_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO pass_sessions (
                id, workspace_id, pass_id, session_token_hash, expires_at
            ) VALUES ($1, $2, $3, digest($4, 'sha256'), $5)
            ON CONFLICT (workspace_id, pass_id) DO UPDATE
            SET session_token_hash = EXCLUDED.session_token_hash,
                created_at = now(),
                last_seen_at = now(),
                expires_at = EXCLUDED.expires_at,
                revoked_at = NULL
            RETURNING id
            "#,
        )
        .bind(session_id)
        .bind(workspace_id.into_uuid())
        .bind(pass.id)
        .bind(session_token.as_str())
        .bind(session_expires_at)
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                pass.id,
                Some(session_id),
                session_expires_at,
            )
            .await?;
        self.append_outbox(
            &mut transaction,
            workspace_id,
            "admission.pass.claimed",
            command.request_id.as_str(),
            json!({
                "pass_id": pass.id,
                "event_id": pass.event_id,
                "public_reference": &view.public_reference,
            }),
        )
        .await?;
        let result = AdmissionPassClaimed {
            pass: view,
            session_token,
        };
        self.complete_sensitive_idempotency(
            &mut transaction,
            workspace_id,
            CLAIM_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(result)
    }

}
