/// Refreshes the team-routing identities from the configured roster.
///
/// Emails remain secret-backed runtime configuration. The roster itself is
/// elastic — `CROWDRELAY_TEAM_MEMBERS_JSON` carries however many members the
/// tenant onboarded with, while the legacy `VIRYA_TEAM_MEMBER_N_EMAIL` slots
/// keep their source-controlled member keys and skills. This function only
/// writes resolved specs into CrowdRelay's existing member identity store.
///
/// It runs inside `setup`, which `scripts/deploy.sh` runs on every release
/// before either long-running service starts. That makes it an unattended
/// periodic writer, so it refreshes facts and never overrides a decision:
/// `workspace_members.status = 'disabled'` and `team_profiles.active =
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
        for member in config.configured_members() {
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
            .bind(member.email.as_str())
            .bind(member.display_name.as_str())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;

            // The member_key is the routing identity, so it owns the profile
            // row: re-pointing a key at a new email re-binds the profile to the
            // new member row. A single INSERT ... ON CONFLICT cannot cover both
            // UNIQUE constraints ((workspace_id, member_id) and
            // (workspace_id, member_key)), so the re-bind runs first and the
            // member-keyed upsert only fires for a key nobody holds.
            let result = sqlx::query(
                r#"
                UPDATE team_profiles SET
                    member_id = $2,
                    -- `active` is deliberately absent here and below: a profile
                    -- turned off is somebody's decision about capacity, and a
                    -- deploy is not a decision about capacity. `skills` and the
                    -- member binding are source-controlled facts, so those do
                    -- refresh.
                    skills = $4
                WHERE workspace_id = $1 AND member_key = $3
                "#,
            )
            .bind(workspace_id)
            .bind(member_id)
            .bind(member.member_key.as_str())
            .bind(member.skills.as_slice())
            .execute(&mut *transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;
            changed = changed.saturating_add(result.rows_affected());
            if result.rows_affected() == 0 {
                let result = sqlx::query(
                    r#"
                    INSERT INTO team_profiles (
                        workspace_id, member_id, member_key, active, skills, capacity_basis_points
                    ) VALUES ($1, $2, $3, true, $4, 10000)
                    ON CONFLICT (workspace_id, member_id) DO UPDATE SET
                        member_key = EXCLUDED.member_key,
                        skills = EXCLUDED.skills
                    "#,
                )
                .bind(workspace_id)
                .bind(member_id)
                .bind(member.member_key.as_str())
                .bind(member.skills.as_slice())
                .execute(&mut *transaction)
                .await
                .map_err(|_| BootstrapError::Database)?;
                changed = changed.saturating_add(result.rows_affected());
            }
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
