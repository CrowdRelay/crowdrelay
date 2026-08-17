impl PostgresAdmissionRepository {
    async fn workspace_id(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<WorkspaceId, AdmissionStoreError> {
        let id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(self.workspace_slug.as_str())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?
                .ok_or(AdmissionStoreError::NotFound)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    async fn member_id(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        email: &str,
    ) -> Result<Uuid, AdmissionStoreError> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM workspace_members \
             WHERE workspace_id = $1 AND normalized_email = $2 AND status = 'active' \
             FOR SHARE",
        )
        .bind(workspace_id.into_uuid())
        .bind(email)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)
    }

    async fn staff_actor(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<StaffActor, AdmissionStoreError> {
        sqlx::query_as::<_, StaffActor>(
            r#"
            SELECT m.id AS member_id, s.id AS session_id
            FROM workspace_members m
            JOIN workspace_member_sessions s
              ON s.workspace_id = m.workspace_id AND s.member_id = m.id
            WHERE m.workspace_id = $1
              AND m.normalized_email = $2
              AND m.status = 'active'
              AND s.session_token_hash = $3
              AND s.revoked_at IS NULL
              AND s.expires_at > now()
            FOR SHARE OF m, s
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&self.staff_member_email)
        .bind(self.staff_session_token_hash.as_slice())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)
    }

    async fn lock_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
    ) -> Result<(), AdmissionStoreError> {
        let lock_key = format!("{}:{scope}:{key}", workspace_id);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(&mut **transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn load_idempotent<T: serde::de::DeserializeOwned>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
    ) -> Result<Option<T>, AdmissionStoreError> {
        let Some(row) = self
            .load_idempotency_row(transaction, workspace_id, scope, key)
            .await?
        else {
            return Ok(None);
        };
        if row.request_hash != request_hash {
            return Err(AdmissionStoreError::Conflict);
        }
        let body = row.response_body.ok_or(AdmissionStoreError::Unexpected)?;
        serde_json::from_value(body)
            .map(Some)
            .map_err(|_| AdmissionStoreError::Unexpected)
    }

    async fn load_sensitive_idempotent<T: serde::de::DeserializeOwned + serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
    ) -> Result<Option<T>, AdmissionStoreError> {
        let Some(row) = self
            .load_idempotency_row(transaction, workspace_id, scope, key)
            .await?
        else {
            return Ok(None);
        };
        if row.request_hash != request_hash {
            return Err(AdmissionStoreError::Conflict);
        }
        let body = row.response_body.ok_or(AdmissionStoreError::Unexpected)?;
        let result = match row.response_content_type.as_deref() {
            Some(ENCRYPTED_JSON_CONTENT_TYPE) => self
                .sensitive_response_codec
                .decrypt(workspace_id, scope, key, body)
                .map_err(|_| AdmissionStoreError::Unexpected)?,
            Some(JSON_CONTENT_TYPE) | None => {
                serde_json::from_value(body).map_err(|_| AdmissionStoreError::Unexpected)?
            }
            Some(_) => return Err(AdmissionStoreError::Unexpected),
        };

        // Re-encrypt every successful replay with the current key. This lazily
        // migrates both legacy plaintext rows and ciphertext created with the
        // configured previous key without extending the retention window.
        let encrypted = self
            .sensitive_response_codec
            .encrypt(workspace_id, scope, key, &result)
            .map_err(|_| AdmissionStoreError::Unexpected)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET response_body = $5, response_content_type = $6
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND request_hash = $4
              AND state = 'completed' AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .bind(encrypted)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(AdmissionStoreError::Conflict);
        }
        Ok(Some(result))
    }

    async fn load_idempotency_row(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRow>, AdmissionStoreError> {
        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;

        sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT request_hash, response_body, response_content_type
            FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND state = 'completed' AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)
    }

    async fn complete_idempotency<T: serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        result: &T,
    ) -> Result<(), AdmissionStoreError> {
        let body = serde_json::to_value(result).map_err(|_| AdmissionStoreError::Unexpected)?;
        self.complete_idempotency_body(
            transaction,
            workspace_id,
            scope,
            key,
            request_hash,
            body,
            JSON_CONTENT_TYPE,
        )
        .await
    }

    async fn complete_sensitive_idempotency<T: serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        result: &T,
    ) -> Result<(), AdmissionStoreError> {
        let body = self
            .sensitive_response_codec
            .encrypt(workspace_id, scope, key, result)
            .map_err(|_| AdmissionStoreError::Unexpected)?;
        self.complete_idempotency_body(
            transaction,
            workspace_id,
            scope,
            key,
            request_hash,
            body,
            ENCRYPTED_JSON_CONTENT_TYPE,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_idempotency_body(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        body: Value,
        content_type: &str,
    ) -> Result<(), AdmissionStoreError> {
        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, completed_at, expires_at
            ) VALUES (
                $1, $2, $3, $4, 'completed', 200, $5, $6,
                now(), now() + ($7::bigint * interval '1 day')
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .bind(body)
        .bind(content_type)
        .bind(IDEMPOTENCY_RETENTION_DAYS)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn load_view_by_pass(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        pass_id: Uuid,
        session_id: Option<Uuid>,
        session_expires_at: OffsetDateTime,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let row = sqlx::query_as::<_, PassViewRow>(
            r#"
            SELECT p.id, p.event_id, p.public_reference, p.status, p.redeemed_at,
                   e.slug AS event_slug, e.title AS event_title, e.venue, e.starts_at,
                   f.display_name, f.normalized_email
            FROM admission_passes p
            JOIN events e ON e.workspace_id = p.workspace_id AND e.id = p.event_id
            JOIN fans f ON f.workspace_id = p.workspace_id AND f.id = p.fan_id
            WHERE p.workspace_id = $1 AND p.id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pass_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        row.into_view(session_id, session_expires_at)
    }

    async fn append_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        event_type: &str,
        request_id: &str,
        payload: Value,
    ) -> Result<(), AdmissionStoreError> {
        sqlx::query(
            "INSERT INTO outbox_events \
             (workspace_id, event_type, event_version, payload, request_id) \
             VALUES ($1, $2, 1, $3, $4)",
        )
        .bind(workspace_id.into_uuid())
        .bind(event_type)
        .bind(payload)
        .bind(request_id)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn append_audit(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        args: AuditEventArgs<'_>,
    ) -> Result<(), AdmissionStoreError> {
        sqlx::query(
            "INSERT INTO audit_events \
             (workspace_id, actor_kind, actor_member_id, action, target_type, target_id, request_id) \
             VALUES ($1, 'member', $2, $3, $4, $5, $6)",
        )
        .bind(args.workspace_id.into_uuid())
        .bind(args.member_id)
        .bind(args.action)
        .bind(args.target_type)
        .bind(args.target_id.to_string())
        .bind(args.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }}
