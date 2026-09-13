/// Refreshes the team-routing identities from deploy-secret contacts.
///
/// Emails remain secret-backed runtime configuration. This function only writes
/// them into CrowdRelay's existing member identity store, while stable member
/// keys and skills stay reviewable in source control.
///
/// It runs inside `setup`, which `scripts/deploy.sh` runs on every release
/// before either long-running service starts. That makes it an unattended
/// periodic writer, so it refreshes facts and never overrides a decision:
/// `workspace_members.status = 'disabled'` and `viryaos_team_profiles.active =
/// false` both survive it. Turning either back on is a deliberate act, not a
/// side effect of deploying.
pub async fn bootstrap_team_operations(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    config: &crowdrelay_infra::config::TeamOperationsConfig,
) -> Result<u64, BootstrapError> {
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
                    -- `disabled` survives. This clause used to set 'active'
                    -- unconditionally, and `setup` runs on every deploy
                    -- (`scripts/deploy.sh` runs it before either long-running
                    -- service starts), so disabling a member lasted until the
                    -- next release and then silently undid itself.
                    --
                    -- That is not cosmetic. `admission/support.rs` requires
                    -- `m.status = 'active'` to operate a gate, and
                    -- `autopilot/{team,control}.rs` require it to route work. A
                    -- revived member regains both.
                    --
                    -- Removing the contact from the deploy secret does keep a
                    -- disablement, because then this loop never reaches the row
                    -- — but `disabled` and "not on the team" are different
                    -- statements, and only one of them is available while
                    -- somebody is still a contact for routing.
                    --
                    -- `invited` is still promoted: a secret-backed contact
                    -- appearing here is what confirms the invitation.
                    status = CASE
                        WHEN workspace_members.status = 'disabled' THEN 'disabled'
                        ELSE 'active'
                    END
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
                    -- `active` is deliberately absent, for the same reason as
                    -- the status above: a profile turned off is somebody's
                    -- decision about capacity, and a deploy is not a decision
                    -- about capacity. `skills` and `member_key` are
                    -- source-controlled facts, so those do refresh.
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
    // Stable slot-to-skill mapping only. Human names stay in runtime member data.
    match member_key {
        "member_1" => Some((
            "Team Member 1",
            ["general", "operations", "booking", "approval", "technical", "people"]
                .into_iter().map(str::to_owned).collect(),
        )),
        "member_2" => Some((
            "Team Member 2",
            ["visual", "video", "photography", "social"]
                .into_iter().map(str::to_owned).collect(),
        )),
        "member_3" => Some((
            "Team Member 3",
            ["english_copy", "polish_copy"].into_iter().map(str::to_owned).collect(),
        )),
        "member_4" => Some((
            "Team Member 4",
            ["operations", "booking", "approval", "people"]
                .into_iter().map(str::to_owned).collect(),
        )),
        "member_5" => Some((
            "Team Member 5",
            ["operations", "approval", "people", "polish_copy"]
                .into_iter().map(str::to_owned).collect(),
        )),
        _ => None,
    }
}
