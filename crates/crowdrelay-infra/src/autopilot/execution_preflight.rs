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
        SELECT target.target_kind, target.display_name, target.contact_email
        FROM booking_targets AS target
        WHERE target.workspace_id = $1
          AND target.id = $2
          AND target.city_id = $3
          AND target.version = $4
          AND target.active
          AND target.accepts_booking
          -- A reply recorded after approval retires the pitch before it can
          -- send: the snapshot's `last_reply_disposition` is this same
          -- subquery, and `record_booking_reply` never files an inbound
          -- reply with disposition 'none', so one row existing is the whole
          -- condition.
          AND NOT EXISTS (
              SELECT 1
              FROM booking_interactions AS interaction
              WHERE interaction.workspace_id = target.workspace_id
                AND interaction.target_id = target.id
                AND interaction.direction = 'inbound'
                AND interaction.phase = 'reply'
          )
          -- A venue-kind target IS its room, and a `status` fact does not
          -- bump `version` — so the lock itself must refuse a target whose
          -- room was reported closed after the decision snapshot was taken.
          -- The resolved status decides (a newer 'active' lifts it), and the
          -- room set is the primary link plus the venue edges — the same
          -- linked set `booking_reads` applies at selection time.
          AND NOT (
              target.target_kind = 'venue'
              AND EXISTS (
                  SELECT 1
                  FROM (
                      SELECT target.venue_id AS linked_venue_id
                      UNION
                      SELECT edge.venue_id
                      FROM booking_target_venues AS edge
                      WHERE edge.workspace_id = target.workspace_id
                        AND edge.target_id = target.id
                  ) AS linked
                  WHERE COALESCE((
                      SELECT lower(btrim(status_fact.value))
                      FROM place_venue_facts AS status_fact
                      WHERE status_fact.venue_id = linked.linked_venue_id
                        AND status_fact.attribute = 'status'
                        AND (status_fact.workspace_id IS NULL
                             OR status_fact.workspace_id = $1)
                        AND (status_fact.expires_at IS NULL
                             OR status_fact.expires_at > now())
                      ORDER BY CASE status_fact.provenance
                                   WHEN 'played' THEN 0
                                   WHEN 'researched' THEN 1
                                   WHEN 'event_evidence' THEN 2
                                   WHEN 'open_directory' THEN 3
                                   ELSE 4 END,
                               status_fact.observed_at DESC
                      LIMIT 1
                  ), '') = 'closed'
              )
          )
        FOR UPDATE OF target
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
        FROM promotion_campaign_states
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
        FROM promotion_budget_guardrails
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
        FROM promotion_campaign_states
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
        FROM promotion_budget_reservations
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
        INSERT INTO promotion_budget_reservations (
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
