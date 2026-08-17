/// Creates or refreshes the configured administrator and gate-service identities.
pub async fn bootstrap_admission_access(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    admin_email: &str,
    staff_email: &str,
    admin_session_hash: Option<[u8; 32]>,
    staff_session_hash: Option<[u8; 32]>,
) -> Result<(), BootstrapError> {
    validate_database_timeouts(database)?;
    timeout(database.operation_timeout, async {
        let mut transaction = pool.begin().await.map_err(|_| BootstrapError::Database)?;
        acquire_workspace_lock(&mut transaction, workspace_slug).await?;
        let workspace_id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(workspace_slug.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| BootstrapError::Database)?
                .ok_or(BootstrapError::Database)?;
        upsert_service_member(
            &mut transaction,
            workspace_id,
            admin_email,
            "admin",
            admin_session_hash,
        )
        .await?;
        upsert_service_member(
            &mut transaction,
            workspace_id,
            staff_email,
            "staff",
            staff_session_hash,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| BootstrapError::Database)?;
        Ok(())
    })
    .await
    .map_err(|_| BootstrapError::TimedOut)?
}

async fn upsert_service_member(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    email: &str,
    role: &str,
    session_hash: Option<[u8; 32]>,
) -> Result<(), BootstrapError> {
    let member_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
        VALUES ($1, $2, $3, $4, 'active')
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE SET
            display_name = EXCLUDED.display_name, role = EXCLUDED.role, status = 'active'
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(if role == "admin" {
        "CrowdRelay Admin"
    } else {
        "Virya Gate"
    })
    .bind(role)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(session_hash) = session_hash {
        let csrf_hash: [u8; 32] =
            sha2::Sha256::digest([b"csrf:".as_slice(), session_hash.as_slice()].concat()).into();
        sqlx::query(
            r#"
            INSERT INTO workspace_member_sessions (
                workspace_id, member_id, session_token_hash, csrf_token_hash, expires_at
            ) VALUES ($1, $2, $3, $4, now() + interval '10 years')
            ON CONFLICT (session_token_hash) DO UPDATE SET
                workspace_id = EXCLUDED.workspace_id, member_id = EXCLUDED.member_id,
                csrf_token_hash = EXCLUDED.csrf_token_hash, last_seen_at = now(),
                expires_at = EXCLUDED.expires_at, revoked_at = NULL
            "#,
        )
        .bind(workspace_id)
        .bind(member_id)
        .bind(session_hash.as_slice())
        .bind(csrf_hash.as_slice())
        .execute(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    }
    Ok(())
}
