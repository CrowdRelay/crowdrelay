// Preflight checks an action must pass before it is allowed to have an effect.
//
// Three guards that each answer one question — is the booking target still the
// one the decision was made against, is the promotion state fresh enough to act
// on, may this workspace send at all — and nothing else. They live apart from
// `execution.rs` because that file is where an action's *effect* and its
// *measurement* are decided, and the policy scripts read it as exactly that.
//
// `include!`d into `autopilot.rs` like its siblings, so no `mod` and no
// imports of its own.

async fn lock_booking_target_for_execution(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    city_id: CityId,
    target_id: BookingTargetId,
    expected_version: i64,
) -> Result<(String, String, String), RepositoryError> {
    sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT target_kind, display_name, contact_email
        FROM viryaos_booking_targets
        WHERE workspace_id = $1
          AND id = $2
          AND city_id = $3
          AND version = $4
          AND active
          AND accepts_booking
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(city_id.into_uuid())
    .bind(expected_version)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)
}

async fn ensure_promotion_state_current(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    campaign_id: PromotionCampaignId,
    expected_budget_minor: i64,
    proposed_budget_minor: i64,
) -> Result<(), RepositoryError> {
    let current = sqlx::query_as::<_, (i64, String)>(
        r#"
        SELECT current_daily_budget_minor, currency
        FROM viryaos_promotion_campaign_states
        WHERE workspace_id = $1 AND id = $2 AND active AND expires_at > now()
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)?;
    if current.0 != expected_budget_minor {
        return Err(RepositoryError::Conflict);
    }
    if proposed_budget_minor <= expected_budget_minor {
        return Ok(());
    }

    let guardrail = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT maximum_total_daily_budget_minor, maximum_monthly_spend_minor
        FROM viryaos_promotion_budget_guardrails
        WHERE workspace_id = $1 AND currency = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    let (daily_budget_minor, month_to_date_minor) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT
            COALESCE(SUM(current_daily_budget_minor), 0)::bigint,
            COALESCE(SUM(spend_month_to_date_minor), 0)::bigint
        FROM viryaos_promotion_campaign_states
        WHERE workspace_id = $1
          AND currency = $2
          AND active
          AND expires_at > now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let reserved_delta_minor = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(daily_delta_minor), 0)::bigint
        FROM viryaos_promotion_budget_reservations
        WHERE workspace_id = $1 AND currency = $2 AND expires_at > now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&current.1)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let delta = proposed_budget_minor
        .checked_sub(expected_budget_minor)
        .ok_or(RepositoryError::Unexpected)?;
    let projected_daily = daily_budget_minor
        .checked_add(reserved_delta_minor)
        .and_then(|value| value.checked_add(delta))
        .ok_or(RepositoryError::Unexpected)?;
    if projected_daily > guardrail.0 || month_to_date_minor >= guardrail.1 {
        return Err(RepositoryError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO viryaos_promotion_budget_reservations (
            workspace_id, action_id, campaign_id, currency, daily_delta_minor, expires_at
        ) VALUES ($1,$2,$3,$4,$5,now() + interval '24 hours')
        ON CONFLICT (workspace_id, action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .bind(&current.1)
    .bind(delta)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

async fn ensure_marketing_eligible(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<(), RepositoryError> {
    let eligible = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM fans AS fan
            JOIN LATERAL (
                SELECT consent.granted
                FROM fan_consents AS consent
                WHERE consent.workspace_id = fan.workspace_id
                  AND consent.fan_id = fan.id
                  AND consent.purpose = 'marketing'
                ORDER BY consent.recorded_at DESC, consent.id DESC
                LIMIT 1
            ) AS latest_consent ON latest_consent.granted
            WHERE fan.workspace_id = $1
              AND fan.id = $2
              AND fan.status = 'active'
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if eligible {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}
