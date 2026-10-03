//! Segment filter resolution for targeted Signal push delivery.
//!
//! When an autopilot `signal.push.request` action includes a `segment` slug,
//! the slug is resolved against the `audience_segments` table. If a matching
//! active segment is found, its JSONB filter is parsed into a typed
//! `SegmentFilter` and applied as additional fan predicates on the push
//! delivery INSERT. A named segment that cannot resolve refuses the send —
//! a push aimed at a subset never widens to everyone consented; only a push
//! with no segment at all broadcasts.
//!
//! This module mirrors the audience panel's `segment_predicate()` semantics
//! (see `crowdrelay-api/src/audience/query_support.rs`) but is self-contained
//! in `crowdrelay-infra` to avoid a cross-crate dependency.
use super::*;

/// A subset of `AudienceFilter` fields applicable to push delivery targeting.
///
/// `marketing_consent` is intentionally excluded: the base push query already
/// enforces the latest `marketing` consent is granted via an EXISTS subquery
/// on `fan_consents`. Push delivery always requires consent regardless of
/// segment filter settings — a segment with `marketing_consent: false` still
/// receives no push (the base guard excludes non-consented fans), and
/// `marketing_consent: true` is redundant. This is the correct privacy/safety
/// behavior for push.
#[derive(Default)]
pub(in crate::autopilot) struct SegmentFilter {
    statuses: Vec<String>,
    city_slugs: Vec<String>,
    min_qualified_referrals: Option<i64>,
    synesthesia_completed: Option<bool>,
    tags_all: Vec<String>,
    /// Campaigns whose delivered-or-claimed sends mark a fan as reached — the
    /// release late waves subtract them so "you might have missed it" only
    /// goes to fans the earlier phases never contacted.
    excluded_campaign_slugs: Vec<String>,
}

impl SegmentFilter {
    /// Whether the filter has any active predicates. When false, the segment
    /// clause is omitted entirely (broadcast behavior, no extra subqueries).
    fn has_conditions(&self) -> bool {
        !self.statuses.is_empty()
            || !self.city_slugs.is_empty()
            || self.min_qualified_referrals.is_some()
            || self.synesthesia_completed.is_some()
            || !self.tags_all.is_empty()
            || !self.excluded_campaign_slugs.is_empty()
    }

    /// Build the SQL fragment and collect bind values for the segment filter.
    ///
    /// Bind positions start at `$start_bind` (the caller's first segment
    /// bind). Each condition is only included when the corresponding field
    /// is present, avoiding unnecessary correlated subqueries for empty
    /// fields. Returns an empty string when the filter has no conditions.
    pub(in crate::autopilot) fn sql_clause(
        &mut self,
        start_bind: usize,
    ) -> (String, Vec<SegmentBind>) {
        if !self.has_conditions() {
            return (String::new(), Vec::new());
        }

        let mut bind_idx = start_bind;
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<SegmentBind> = Vec::new();

        if !self.statuses.is_empty() {
            let b = bind_idx;
            conditions.push(format!("AND fan.status = ANY(${b}::text[])"));
            binds.push(SegmentBind::Statuses(std::mem::take(&mut self.statuses)));
            bind_idx += 1;
        }

        if !self.city_slugs.is_empty() {
            let b = bind_idx;
            conditions.push(format!(
                "AND EXISTS (
                    SELECT 1 FROM fan_city_interests ci
                    JOIN cities city ON city.id = ci.city_id
                    WHERE ci.workspace_id = fan.workspace_id
                      AND ci.fan_id = fan.id
                      AND city.slug = ANY(${b}::text[])
                )"
            ));
            binds.push(SegmentBind::CitySlugs(std::mem::take(&mut self.city_slugs)));
            bind_idx += 1;
        }

        if let Some(min_refs) = self.min_qualified_referrals {
            let b = bind_idx;
            conditions.push(format!(
                "AND canonical_qualified_referral_count(
                    fan.workspace_id, fan.id, NULL
                ) >= ${b}"
            ));
            binds.push(SegmentBind::MinReferrals(min_refs));
            bind_idx += 1;
        }

        if let Some(syn) = self.synesthesia_completed {
            let b = bind_idx;
            conditions.push(format!(
                "AND EXISTS (
                    SELECT 1 FROM synesthesia_reward_entries se
                    WHERE se.workspace_id = fan.workspace_id
                      AND se.fan_id = fan.id
                ) = ${b}"
            ));
            binds.push(SegmentBind::Synesthesia(syn));
            bind_idx += 1;
        }

        if !self.tags_all.is_empty() {
            // Use the unnest + NOT EXISTS anti-join pattern (same as
            // segment_predicate() in query_support.rs) to verify the fan
            // has ALL required tags. `= ALL(...)` is wrong because it
            // requires every tag row to equal every array element.
            let b = bind_idx;
            conditions.push(format!(
                "AND NOT EXISTS (
                    SELECT 1
                    FROM unnest(${b}::text[]) required(tag)
                    WHERE NOT EXISTS (
                        SELECT 1
                        FROM fan_audience_tags assigned
                        WHERE assigned.workspace_id = fan.workspace_id
                          AND assigned.fan_id = fan.id
                          AND assigned.tag = required.tag
                    )
                )"
            ));
            binds.push(SegmentBind::TagsAll(std::mem::take(&mut self.tags_all)));
            bind_idx += 1;
        }

        if !self.excluded_campaign_slugs.is_empty() {
            let b = bind_idx;
            conditions.push(format!(
                "AND NOT EXISTS (
                    SELECT 1
                    FROM communication_campaign_deliveries reached
                    JOIN communication_campaigns reached_campaign
                      ON reached_campaign.workspace_id = reached.workspace_id
                     AND reached_campaign.id = reached.campaign_id
                    WHERE reached.workspace_id = fan.workspace_id
                      AND reached.fan_id = fan.id
                      AND reached.status IN ('delivered', 'claimed')
                      AND reached_campaign.slug = ANY(${b}::text[])
                )"
            ));
            binds.push(SegmentBind::ExcludedCampaignSlugs(std::mem::take(
                &mut self.excluded_campaign_slugs,
            )));
        }

        (conditions.join("\n          "), binds)
    }
}

/// A typed bind value for a segment filter condition. The caller matches
/// on the variant and applies the appropriate `.bind()` call.
pub(in crate::autopilot) enum SegmentBind {
    Statuses(Vec<String>),
    CitySlugs(Vec<String>),
    MinReferrals(i64),
    Synesthesia(bool),
    TagsAll(Vec<String>),
    ExcludedCampaignSlugs(Vec<String>),
}

/// Load a segment's JSONB filter from `audience_segments` and parse it into a
/// validated `SegmentFilter`.
///
/// Two different answers for two different requests: *no segment named* is a
/// broadcast — the operator asked for everyone, so `Ok(None)` applies no
/// clause. *A segment named but unresolvable* is a refusal — the send was
/// aimed at a subset, and delivering it to everyone consented is precisely
/// the misfire the recipient ceiling exists to bound. `Err` parks the action
/// instead of widening the audience.
pub(in crate::autopilot) async fn resolve_segment_filter<'e, E>(
    executor: E,
    workspace_id: WorkspaceId,
    segment: Option<&str>,
) -> Result<Option<SegmentFilter>, RepositoryError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let Some(slug) = segment else {
        return Ok(None);
    };
    if slug.is_empty() {
        return Ok(None);
    }

    let result = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        r#"SELECT filter FROM audience_segments
           WHERE workspace_id = $1 AND slug = $2 AND active"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(slug)
    .fetch_optional(executor)
    .await;

    let filter_json = match result {
        Ok(Some(Some(value))) if value.is_object() => value,
        Ok(Some(Some(_))) => {
            tracing::warn!(
                segment = slug,
                "segment filter is not a JSON object, refusing to widen to broadcast"
            );
            return Err(RepositoryError::ConflictBecause(
                "segment's filter is not a JSON object — refusing to widen the send to broadcast",
            ));
        }
        Ok(Some(None)) => {
            tracing::warn!(
                segment = slug,
                "segment has null filter, refusing to widen to broadcast"
            );
            return Err(RepositoryError::ConflictBecause(
                "segment's filter is empty — refusing to widen the send to broadcast",
            ));
        }
        Ok(None) => {
            tracing::warn!(
                segment = slug,
                "segment slug not found or inactive, refusing to widen to broadcast"
            );
            return Err(RepositoryError::ConflictBecause(
                "segment not found or inactive — refusing to widen the send to broadcast",
            ));
        }
        Err(error) => {
            tracing::warn!(%error, segment = slug, "failed to load segment filter");
            return Err(map_sqlx(error));
        }
    };

    match parse_segment_filter(&filter_json) {
        Some(filter) => Ok(Some(filter)),
        None => {
            tracing::warn!(
                segment = slug,
                filter = %filter_json,
                "segment filter has invalid field types, refusing to widen to broadcast"
            );
            Err(RepositoryError::ConflictBecause(
                "segment's filter has invalid fields — refusing to widen the send to broadcast",
            ))
        }
    }
}

/// Parse a JSONB filter object into a validated `SegmentFilter`.
///
/// Validates that:
/// - `statuses`, `city_slugs`, `tags_all`, `excluded_campaign_slugs` are JSON
///   arrays of strings
/// - `min_qualified_referrals` is a JSON number (or absent)
/// - `synesthesia_completed` is a JSON boolean (or absent)
///
/// Returns `None` if any field has an invalid type.
fn parse_segment_filter(filter: &serde_json::Value) -> Option<SegmentFilter> {
    let obj = filter.as_object()?;

    let parse_string_array = |key: &str| -> Option<Vec<String>> {
        match obj.get(key) {
            None => Some(Vec::new()),
            Some(serde_json::Value::Array(arr)) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(item.as_str()?.to_owned());
                }
                Some(out)
            }
            Some(_) => None, // wrong type
        }
    };

    Some(SegmentFilter {
        statuses: parse_string_array("statuses")?,
        city_slugs: parse_string_array("city_slugs")?,
        min_qualified_referrals: obj.get("min_qualified_referrals").and_then(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::Number(_) => v.as_i64(),
            _ => None,
        }),
        synesthesia_completed: obj.get("synesthesia_completed").and_then(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::Bool(_) => v.as_bool(),
            _ => None,
        }),
        tags_all: parse_string_array("tags_all")?,
        excluded_campaign_slugs: parse_string_array("excluded_campaign_slugs")?,
    })
}

/// Materializes an approved Signal push as `fan_push_deliveries` rows for all
/// consented fans with active push endpoints. The PushDeliveryWorker then
/// sends them via FCM/Web Push.
///
/// Idempotency: `UNIQUE (workspace_id, source_kind, source_id, endpoint_id)`
/// on `fan_push_deliveries` means a retry of the same action + endpoint is a
/// no-op. `source_id` is the autopilot action id.
///
/// When `segment` is `Some(slug)`, [`resolve_segment_filter`] applies the
/// segment's parsed predicates — and refuses the action rather than widening
/// to broadcast when the slug cannot resolve.
#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn execute_signal_push(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    title: &str,
    body: &str,
    target_path: Option<&str>,
    _event_id: Option<&uuid::Uuid>,
    segment: Option<&str>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let action_uuid = action_id.into_uuid();
    let collapse_key = format!("agent:{action_uuid}");
    let target = target_path.unwrap_or("/my-signal/");

    // No segment named means a broadcast the operator asked for. A segment
    // named but unresolvable refuses the action rather than widening the
    // send to everyone consented.
    let mut segment_filter = resolve_segment_filter(&mut **tx, workspace_id, segment).await?;

    // Build the segment clause + typed bind values. Only fields present
    // in the filter generate SQL conditions, avoiding unnecessary
    // correlated subqueries for absent fields.
    let (segment_clause, segment_binds) = segment_filter
        .as_mut()
        .map(|f| f.sql_clause(7))
        .unwrap_or_default();

    // The envelope's per-step recipient bound is an operator safety dial: it
    // clamps the fan set, not just the reported reach. An absent envelope row
    // reads as the domain default — never as "no bound". The bound applies to
    // fans (recipients), not deliveries: a fan with three endpoints is still
    // one person reached.
    let recipient_bound = sqlx::query_scalar::<_, i32>(
        "SELECT max_recipients_per_step FROM growth_envelope WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .map_or_else(
        || {
            i64::from(
                crowdrelay_domain::growth_envelope::GrowthEnvelope::default()
                    .max_recipients_per_step,
            )
        },
        |bound| i64::from(bound.max(1)),
    );
    let limit_bind = 7 + segment_binds.len();

    let sql = format!(
        r#"
        INSERT INTO fan_push_deliveries
            (workspace_id, fan_id, endpoint_id, source_kind, source_id,
             category, title, body, target_path, collapse_key)
        SELECT endpoint.workspace_id, endpoint.fan_id, endpoint.id,
               'agent_signal_push', $2,
               'community', $3, $4, $5, $6
        FROM fan_push_endpoints endpoint
        JOIN (
            -- The envelope's blast-radius bound applies to the fan set, not
            -- to deliveries: a fan with three endpoints is still one person
            -- reached. An absent envelope row reads as the domain default,
            -- never as "no bound".
            SELECT fan.id
            FROM fans fan
            WHERE fan.workspace_id = $1
              AND fan.status = 'active'
              AND EXISTS (
                  SELECT 1 FROM fan_consents consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
                    AND consent.id = (
                        SELECT newest.id FROM fan_consents newest
                        WHERE newest.workspace_id = consent.workspace_id
                          AND newest.fan_id = consent.fan_id
                          AND newest.purpose = consent.purpose
                        ORDER BY newest.recorded_at DESC, newest.id DESC LIMIT 1
                    )
              )
              {segment_clause}
            ORDER BY fan.id
            LIMIT ${limit_bind}
        ) fan ON fan.id = endpoint.fan_id
        WHERE endpoint.workspace_id = $1
          AND endpoint.active
          AND endpoint.invalidated_at IS NULL
          -- The gap is enforced where the push is sent, not only where it
          -- is raised. Relay pacing (`relay_push_verdict`) spaces pushes by
          -- when they were *raised*; four raised twelve hours apart, held
          -- for approval and released together, still reached the same two
          -- phones in one second on 2026-09-26. A fan who got an autopilot
          -- push inside the gap is skipped for this one.
          AND NOT EXISTS (
              SELECT 1 FROM fan_push_deliveries AS recent
              WHERE recent.workspace_id = endpoint.workspace_id
                AND recent.fan_id = endpoint.fan_id
                AND recent.source_kind = 'agent_signal_push'
                AND recent.source_id <> $2
                AND recent.created_at > ${now_bind} - make_interval(hours => ${gap_bind})
          )
        ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
        "#,
        now_bind = limit_bind + 1,
        gap_bind = limit_bind + 2,
    );

    let mut query = sqlx::query(&sql)
        .bind(workspace_id.into_uuid())
        .bind(action_uuid)
        .bind(title)
        .bind(body)
        .bind(target)
        .bind(&collapse_key);

    query = apply_segment_binds(query, segment_binds);
    query = query.bind(recipient_bound).bind(now).bind(
        i32::try_from(crowdrelay_domain::content_supply::RELAY_PUSH_MIN_GAP_HOURS)
            .unwrap_or(i32::MAX),
    );

    let inserted = query.execute(&mut **tx).await.map_err(map_sqlx)?;

    // Record a reach event for the unified reach ledger. Signal pushes are
    // broadcast reaches — one action reaches many fans. The estimated_reach
    // is the number of eligible endpoints that received the push (from the
    // INSERT ... ON CONFLICT row count above).
    let estimated_reach = inserted.rows_affected() as i32;
    // Zero endpoints inserted means zero reach — the credit allocator divides
    // fan outcomes by reach, so a fabricated denominator of 1 would invent
    // credit from nothing. The honest report is no reach event at all: the
    // column refuses 0 (`estimated_reach >= 1`), and a row that reached
    // nobody is not a reach. The action still completes — the gap guard, not
    // a fault, is what reached zero.
    if estimated_reach > 0 {
        sqlx::query(r#"INSERT INTO reach_events (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata) VALUES ($1, $2, 'platform_audience', 'signal_fans', 'signal_push', 'signal-inviter', $4, 'sent', jsonb_build_object('title', $3)) ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#)
        .bind(workspace_id.into_uuid())
        .bind(action_uuid)
        .bind(title)
        .bind(estimated_reach)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }

    Ok(())
}

fn apply_segment_binds<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    binds: Vec<SegmentBind>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for bind in binds {
        query = match bind {
            SegmentBind::Statuses(v) => query.bind(v),
            SegmentBind::CitySlugs(v) => query.bind(v),
            SegmentBind::MinReferrals(v) => query.bind(v),
            SegmentBind::Synesthesia(v) => query.bind(v),
            SegmentBind::TagsAll(v) => query.bind(v),
            SegmentBind::ExcludedCampaignSlugs(v) => query.bind(v),
        };
    }
    query
}

/// How many fans a signal push to `segment` reaches right now — the same
/// eligibility `execute_signal_push` enforces at send time (active fan,
/// newest marketing consent granted, the segment's predicates, at least one
/// live push endpoint), then clamped by the workspace's per-step recipient
/// bound. `reached` is what the send would deliver; `eligible` is the set
/// before the bound. Both are counts, not estimates — an approval screen
/// that prints `reached` is describing the send it is approving.
///
/// Runs on the pool rather than inside a transaction: an audience count is
/// a read, and the raise paths that need it (the relay candidate builder,
/// the signal-inviter's outcome mapper) hold no open transaction.
///
/// A segment that cannot resolve propagates the same refusal the send path
/// returns — counting a bad segment as "everyone" would lie about the push
/// the operator is approving.
pub async fn signal_push_audience(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    segment: Option<&str>,
) -> Result<crowdrelay_domain::content_supply::SignalPushAudience, RepositoryError> {
    let mut segment_filter = resolve_segment_filter(pool, workspace_id, segment).await?;
    let (segment_clause, segment_binds) = segment_filter
        .as_mut()
        .map(|filter| filter.sql_clause(2))
        .unwrap_or_default();

    let sql = format!(
        r#"
        SELECT COUNT(*)::bigint
        FROM fans fan
        WHERE fan.workspace_id = $1
          AND fan.status = 'active'
          AND EXISTS (
              SELECT 1 FROM fan_consents consent
              WHERE consent.workspace_id = fan.workspace_id
                AND consent.fan_id = fan.id
                AND consent.purpose = 'marketing'
                AND consent.granted
                AND consent.id = (
                    SELECT newest.id FROM fan_consents newest
                    WHERE newest.workspace_id = consent.workspace_id
                      AND newest.fan_id = consent.fan_id
                      AND newest.purpose = consent.purpose
                    ORDER BY newest.recorded_at DESC, newest.id DESC LIMIT 1
                )
          )
          AND EXISTS (
              SELECT 1 FROM fan_push_endpoints endpoint
              WHERE endpoint.workspace_id = fan.workspace_id
                AND endpoint.fan_id = fan.id
                AND endpoint.active
                AND endpoint.invalidated_at IS NULL
          )
          {segment_clause}
        "#
    );
    let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(workspace_id.into_uuid());
    // `QueryAs` (query_scalar) is a different builder type than `Query`, so
    // the segment binds are applied by hand here rather than through
    // `apply_segment_binds`.
    for bind in segment_binds {
        query = match bind {
            SegmentBind::Statuses(v) => query.bind(v),
            SegmentBind::CitySlugs(v) => query.bind(v),
            SegmentBind::MinReferrals(v) => query.bind(v),
            SegmentBind::Synesthesia(v) => query.bind(v),
            SegmentBind::TagsAll(v) => query.bind(v),
            SegmentBind::ExcludedCampaignSlugs(v) => query.bind(v),
        };
    }
    let eligible = query.fetch_one(pool).await.map_err(map_sqlx)?.max(0) as u32;

    // The envelope's per-step bound clamps the fan set, not just the report:
    // an eligible audience of five thousand with a bound of fifty is a push
    // to fifty people, and the approval must say fifty.
    let send_cap = sqlx::query_scalar::<_, i32>(
        "SELECT max_recipients_per_step FROM growth_envelope WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?
    .map_or_else(
        || {
            i64::from(
                crowdrelay_domain::growth_envelope::GrowthEnvelope::default()
                    .max_recipients_per_step,
            )
        },
        |bound| i64::from(bound.max(1)),
    );

    Ok(crowdrelay_domain::content_supply::SignalPushAudience {
        eligible,
        reached: i64::from(eligible).min(send_cap) as u32,
    })
}
