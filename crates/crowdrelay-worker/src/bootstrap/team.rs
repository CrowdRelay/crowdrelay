/// Refreshes the team-routing identities from deploy-secret contacts.
///
/// Emails remain secret-backed runtime configuration. This function only writes
/// them into CrowdRelay's existing member identity store, while stable member
/// keys and skills stay reviewable in source control.
pub async fn bootstrap_team_operations(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    config: &crowdrelay_infra::config::TeamOperationsConfig,
) -> Result<u64, BootstrapError> {
    validate_database_timeouts(database)?;
    timeout(database.operation_timeout, async {
        let mut transaction = pool.begin().await.map_err(|_| BootstrapError::Database)?;
        configure_transaction(&mut transaction, database).await?;
        acquire_workspace_lock(&mut transaction, workspace_slug).await?;
        let workspace_id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(workspace_slug.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| BootstrapError::Database)?
                .ok_or(BootstrapError::Database)?;

        let mut changed = 0_u64;
        for (member_key, email) in config.configured_members() {
            let (display_name, skills) = team_member_profile(member_key)
                .ok_or(BootstrapError::Database)?;
            let member_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO workspace_members (
                    workspace_id, normalized_email, display_name, role, status
                ) VALUES ($1, $2, $3, 'staff', 'active')
                ON CONFLICT (workspace_id, normalized_email) DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    status = 'active'
                RETURNING id
                "#,
            )
            .bind(workspace_id)
            .bind(email)
            .bind(display_name)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;

            let result = sqlx::query(
                r#"
                INSERT INTO viryaos_team_profiles (
                    workspace_id, member_id, member_key, active, skills, capacity_basis_points
                ) VALUES ($1, $2, $3, true, $4, 10000)
                ON CONFLICT (workspace_id, member_id) DO UPDATE SET
                    member_key = EXCLUDED.member_key,
                    active = true,
                    skills = EXCLUDED.skills
                "#,
            )
            .bind(workspace_id)
            .bind(member_id)
            .bind(member_key)
            .bind(skills)
            .execute(&mut *transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;
            changed = changed.saturating_add(result.rows_affected());
        }

        transaction
            .commit()
            .await
            .map_err(|_| BootstrapError::Database)?;
        Ok(changed)
    })
    .await
    .map_err(|_| BootstrapError::TimedOut)?
}

fn team_member_profile(member_key: &str) -> Option<(&'static str, Vec<String>)> {
    match member_key {
        "wojtek" => Some((
            "Wojtek",
            ["general", "operations", "booking", "approval", "technical", "people"].into_iter().map(str::to_owned).collect(),
        )),
        "lubek" => Some((
            "Lubek",
            ["visual", "video", "photography", "social"].into_iter().map(str::to_owned).collect(),
        )),
        "kuba" => Some(("Kuba", ["english_copy", "polish_copy"].into_iter().map(str::to_owned).collect())),
        "marcin" => Some((
            "Marcin",
            ["operations", "booking", "approval", "people"].into_iter().map(str::to_owned).collect(),
        )),
        "marek" => Some((
            "Marek",
            ["operations", "approval", "people", "polish_copy"].into_iter().map(str::to_owned).collect(),
        )),
        _ => None,
    }
}
