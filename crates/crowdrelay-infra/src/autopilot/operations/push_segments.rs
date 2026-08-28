//! Segment filter resolution for targeted Signal push delivery.
//!
//! When an autopilot `signal.push.request` action includes a `segment` slug,
//! the slug is resolved against the `audience_segments` table. If a matching
//! active segment is found, its JSONB filter is parsed into a typed
//! `SegmentFilter` and applied as additional fan predicates on the push
//! delivery INSERT. If the slug is not found, inactive, or the filter is
//! invalid, the push falls back to broadcasting to all consented fans with
//! active endpoints (the original behavior).
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
                "AND (
                    SELECT count(*)::bigint FROM referral_attributions ref
                    WHERE ref.workspace_id = fan.workspace_id
                      AND ref.referrer_fan_id = fan.id
                      AND ref.status = 'qualified'
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
}

/// Load a segment's JSONB filter from `audience_segments` and parse it into a
/// validated `SegmentFilter`. Returns `None` (→ broadcast) if:
/// - the slug is not provided or empty
/// - the segment is not found or inactive
/// - the filter is not a valid JSON object
/// - any filter field has an invalid type (e.g. `statuses` is a string, not
///   an array; `min_qualified_referrals` is not a number)
///
/// Logs a warning on every fallback path so the operator can see when the
/// brain referenced a non-existent or malformed segment.
pub(in crate::autopilot) async fn resolve_segment_filter(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    segment: Option<&str>,
) -> Option<SegmentFilter> {
    let slug = segment?;
    if slug.is_empty() {
        return None;
    }

    let result = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        r#"SELECT filter FROM audience_segments
           WHERE workspace_id = $1 AND slug = $2 AND active"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(slug)
    .fetch_optional(&mut **tx)
    .await;

    let filter_json = match result {
        Ok(Some(Some(value))) if value.is_object() => value,
        Ok(Some(Some(_))) => {
            tracing::warn!(
                segment = slug,
                "segment filter is not a JSON object, broadcasting"
            );
            return None;
        }
        Ok(Some(None)) => {
            tracing::warn!(segment = slug, "segment has null filter, broadcasting");
            return None;
        }
        Ok(None) => {
            tracing::warn!(segment = slug, "segment slug not found, broadcasting");
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, segment = slug, "failed to load segment filter, broadcasting");
            return None;
        }
    };

    match parse_segment_filter(&filter_json) {
        Some(filter) => Some(filter),
        None => {
            tracing::warn!(
                segment = slug,
                filter = %filter_json,
                "segment filter has invalid field types, broadcasting"
            );
            None
        }
    }
}

/// Parse a JSONB filter object into a validated `SegmentFilter`.
///
/// Validates that:
/// - `statuses`, `city_slugs`, `tags_all` are JSON arrays of strings
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
    })
}
