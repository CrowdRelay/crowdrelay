impl PostgresAdmissionRepository {
    async fn load_inner(
        &self,
        workspace_id: WorkspaceId,
        session: &PassSessionToken,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let trusted_workspace = self.workspace_id(&mut transaction).await?;
        if trusted_workspace != workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            SELECT id, pass_id, expires_at
            FROM pass_sessions
            WHERE workspace_id = $1
              AND session_token_hash = digest($2, 'sha256')
              AND revoked_at IS NULL
              AND expires_at > now()
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(session.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        sqlx::query(
            "UPDATE pass_sessions SET last_seen_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(row.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                row.pass_id,
                Some(row.id),
                row.expires_at,
            )
            .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(view)
    }

    async fn redeem_inner(
        &self,
        command: &RedeemAdmissionPassCommand,
    ) -> Result<AdmissionRedemptionResult, AdmissionStoreError> {
        let request_hash = redeem_request_hash(command);
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            REDEEM_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_idempotent::<AdmissionRedemptionResult>(
                &mut transaction,
                workspace_id,
                REDEEM_SCOPE,
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

        let staff = self.staff_actor(&mut transaction, workspace_id).await?;
        let pass = sqlx::query_as::<_, RedemptionRow>(
            r#"
            SELECT p.id, p.event_id, p.status, p.public_reference, p.redeemed_at,
                   f.display_name, f.normalized_email
            FROM admission_passes p
            JOIN fans f ON f.workspace_id = p.workspace_id AND f.id = p.fan_id
            JOIN events e ON e.workspace_id = p.workspace_id AND e.id = p.event_id
            WHERE p.workspace_id = $1
                AND p.public_reference = $2
                AND e.slug = $3
                AND e.status = 'published'
                AND now() >= COALESCE(e.doors_at, e.starts_at) - interval '1 hour'
                AND now() <= COALESCE(e.ends_at, e.starts_at + interval '12 hours')
                    + interval '2 hours'
            FOR UPDATE OF p
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&command.public_reference)
        .bind(command.event_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if command.pass_id.is_some_and(|id| id.into_uuid() != pass.id)
            || command
                .event_id
                .is_some_and(|id| id.into_uuid() != pass.event_id)
        {
            return Err(AdmissionStoreError::Conflict);
        }

        let now = OffsetDateTime::now_utc();
        let (status, redeemed_at) = match pass.status.as_str() {
            "redeemed" => (AdmissionRedemptionStatus::AlreadyRedeemed, pass.redeemed_at),
            "revoked" => (AdmissionRedemptionStatus::Revoked, None),
            "expired" => (AdmissionRedemptionStatus::Expired, None),
            "issued" => (AdmissionRedemptionStatus::NotClaimed, None),
            "claimed" => {
                sqlx::query(
                    r#"
                    INSERT INTO pass_redemptions (
                        workspace_id, pass_id, staff_member_id, staff_session_id, request_id,
                        result_metadata
                    ) VALUES ($1, $2, $3, $4, $5, jsonb_build_object('source', 'gate_api'))
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(pass.id)
                .bind(staff.member_id)
                .bind(staff.session_id)
                .bind(command.request_id.as_str())
                .execute(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
                sqlx::query(
                    "UPDATE admission_passes SET status = 'redeemed', redeemed_at = $3 \
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(pass.id)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
                self.append_outbox(
                    &mut transaction,
                    workspace_id,
                    "admission.pass.redeemed",
                    command.request_id.as_str(),
                    json!({
                        "pass_id": pass.id,
                        "event_id": pass.event_id,
                        "public_reference": &pass.public_reference,
                        "redeemed_at": now,
                    }),
                )
                .await?;
                (AdmissionRedemptionStatus::Redeemed, Some(now))
            }
            _ => return Err(AdmissionStoreError::Unexpected),
        };
        let result = AdmissionRedemptionResult {
            pass_id: AdmissionPassId::from_uuid(pass.id),
            event_id: EventId::from_uuid(pass.event_id),
            public_reference: pass.public_reference,
            holder_name: pass.display_name,
            holder_email_masked: mask_email(&pass.normalized_email),
            status,
            redeemed_at,
        };
        self.append_audit(
            &mut transaction,
            AuditEventArgs {
                workspace_id,
                member_id: staff.member_id,
                action: "admission.pass.redemption_checked",
                target_type: "admission_pass",
                target_id: pass.id,
                request_id: command.request_id.as_str(),
            },
        )
        .await?;
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            REDEEM_SCOPE,
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

    async fn revoke_inner(
        &self,
        command: &RevokeAdmissionPassCommand,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let request_hash = Sha256::digest(command.public_reference.as_bytes()).to_vec();
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            REVOKE_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_idempotent::<AdmissionPassView>(
                &mut transaction,
                workspace_id,
                REVOKE_SCOPE,
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
        let admin_member_id = self
            .member_id(&mut transaction, workspace_id, &self.admin_member_email)
            .await?;
        let pass = sqlx::query_as::<_, RevokeRow>(
            r#"
            SELECT id, admission_pool_id, status
            FROM admission_passes
            WHERE workspace_id = $1 AND public_reference = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&command.public_reference)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if pass.status == "redeemed" {
            return Err(AdmissionStoreError::Conflict);
        }
        let changed = pass.status != "revoked";
        if changed {
            sqlx::query(
                "UPDATE admission_passes SET status = 'revoked', claim_token_hash = NULL \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            sqlx::query(
                "UPDATE pass_sessions SET revoked_at = now() \
                 WHERE workspace_id = $1 AND pass_id = $2 AND revoked_at IS NULL",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            if releases_pool_capacity(&pass.status) {
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
            }
        }
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                pass.id,
                None,
                OffsetDateTime::now_utc(),
            )
            .await?;
        if changed {
            self.append_audit(
                &mut transaction,
                AuditEventArgs {
                    workspace_id,
                    member_id: admin_member_id,
                    action: "admission.pass.revoked",
                    target_type: "admission_pass",
                    target_id: pass.id,
                    request_id: command.request_id.as_str(),
                },
            )
            .await?;
            self.append_outbox(
                &mut transaction,
                workspace_id,
                "admission.pass.revoked",
                command.request_id.as_str(),
                json!({
                    "pass_id": pass.id,
                    "public_reference": &command.public_reference,
                }),
            )
            .await?;
        }
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            REVOKE_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &view,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(view)
    }

}
