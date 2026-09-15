//! The R+3 release outcome report — the honest read on "who arrived because
//! of this release," three days out.
//!
//! Same discipline as the T+7 show report: `observed` is first-party evidence
//! (acquisition events and clicks bound to the release's campaign, delivery
//! receipts for the phases that actually ran), `inferred` is what the window
//! suggests against the tenant's own trailing baseline, and `evidence_gaps`
//! names what cannot be claimed — streams above all, since no platform play
//! count reaches this system. A recipient can repeat every figure.

use super::*;

/// Baseline comparison window and the honesty thresholds, kept as constants so
/// the payload's stated formula matches the code exactly.
const BASELINE_DAYS: i64 = 28;
const REPORT_WINDOW_DAYS: i64 = 3;
const MIN_BASELINE_DAYS: i64 = 7;

/// Emits `crowdrelay.release.r3_report_due` inside the sustain-milestone
/// transaction. The caller's row lock already proved the plan is live and the
/// milestone due; the report reads the campaign binding lazily so a plan whose
/// listen_url arrived late still names the gap rather than skipping it.
pub(in crate::autopilot) async fn issue_release_r3_report(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    release_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let campaign_ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM campaigns WHERE workspace_id = $1 AND release_plan_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(release_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // First-party evidence: fans and clicks that arrived through the release's
    // tracked link, and the receipts of every phase send that ran.
    let (bound_acquisitions, release_clicks, release_clickers) =
        sqlx::query_as::<_, (i64, i64, i64)>(
            r#"
        SELECT
            (SELECT count(*) FROM fan_acquisition_events a
             WHERE a.workspace_id = $1 AND a.campaign_id = ANY($2)),
            (SELECT count(*) FROM click_events c
             WHERE c.workspace_id = $1 AND c.campaign_id = ANY($2)),
            (SELECT count(DISTINCT c.anonymous_visitor_id) FROM click_events c
             WHERE c.workspace_id = $1 AND c.campaign_id = ANY($2))
        "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&campaign_ids)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;

    let campaigns = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<OffsetDateTime>,
            Option<i32>,
            Option<i32>,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT slug, template_key, status, scheduled_at,
               recipient_count, delivered_count, completed_at
        FROM communication_campaigns
        WHERE workspace_id = $1 AND slug LIKE $2
        ORDER BY scheduled_at NULLS LAST, slug
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("viryaos-release-{release_id}-%"))
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // The ambient-growth read: every acquisition in the release window, and the
    // tenant's own trailing rate over the days it has actually existed for.
    let (window_acquisitions, baseline_acquisitions, baseline_days) =
        sqlx::query_as::<_, (i64, i64, i64)>(
            r#"
        SELECT
            (SELECT count(*) FROM fan_acquisition_events a
             WHERE a.workspace_id = $1
               AND a.occurred_at >= $2
               AND a.occurred_at < $2 + make_interval(days => $3::int)),
            (SELECT count(*) FROM fan_acquisition_events a
             WHERE a.workspace_id = $1
               AND a.occurred_at >= $2 - make_interval(days => $4::int)
               AND a.occurred_at < $2),
            GREATEST(1, LEAST($4::int, EXTRACT(DAY FROM $2 - w.created_at)::bigint))
        FROM workspaces w WHERE w.id = $1
        "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(release_at)
        .bind(REPORT_WINDOW_DAYS as i32)
        .bind(BASELINE_DAYS as i32)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;

    let window_cities = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT city.name, count(*) AS arrivals
        FROM fan_acquisition_events a
        JOIN fan_city_interests fci
          ON fci.workspace_id = a.workspace_id AND fci.fan_id = a.fan_id
        JOIN cities AS city ON city.id = fci.city_id
        WHERE a.workspace_id = $1
          AND a.occurred_at >= $2
          AND a.occurred_at < $2 + make_interval(days => $3::int)
        GROUP BY city.name
        ORDER BY arrivals DESC, city.name
        LIMIT 5
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(release_at)
    .bind(REPORT_WINDOW_DAYS as i32)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let band = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT normalized_email, COALESCE(display_name, normalized_email)
        FROM workspace_members
        WHERE workspace_id = $1 AND status = 'active'
        ORDER BY display_name
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // The honest verdict: a window is only "above trend" against a baseline
    // long enough to be a trend, and only when the count clears both a real
    // number and twice what the baseline would have produced anyway.
    let expected = baseline_acquisitions as f64 / baseline_days as f64 * REPORT_WINDOW_DAYS as f64;
    let verdict = if baseline_days < MIN_BASELINE_DAYS {
        "insufficient_evidence"
    } else if window_acquisitions >= 3 && (window_acquisitions as f64) >= expected * 2.0 {
        "above_trend"
    } else {
        "within_noise"
    };

    let mut evidence_gaps: Vec<&str> = vec!["streams_not_measured"];
    if campaign_ids.is_empty() {
        // No listen_url at bind time means no tracked link and no campaign —
        // bound attribution could not exist; the gap names the record, not
        // the intent.
        evidence_gaps.push("no_release_campaign");
    }
    if baseline_days < MIN_BASELINE_DAYS {
        evidence_gaps.push("baseline_window_thin");
    }
    if band.is_empty() {
        evidence_gaps.push("no_active_band_recipient");
    }

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.release.r3_report_due",
        json!({
            "action_id": action_id,
            "release_id": release_id,
            "release": {
                "title": title,
                "release_at": release_at,
            },
            "report": {
                "kind": "release_r3",
                "generated_at": now,
                "window": {
                    "from": release_at,
                    "to": release_at + time::Duration::days(REPORT_WINDOW_DAYS),
                },
                "observed": {
                    "fans_acquired_via_release_campaign": bound_acquisitions,
                    "release_link_clicks": release_clicks,
                    "release_link_clickers": release_clickers,
                },
                "inferred": {
                    "window_acquisitions": window_acquisitions,
                    "baseline_acquisitions_28d": baseline_acquisitions,
                    "baseline_days": baseline_days,
                    "expected_window_acquisitions": expected,
                    "verdict": verdict,
                    "verdict_formula": format!(
                        "above_trend when window >= 3 and >= 2 * (baseline/{BASELINE_DAYS}d * {REPORT_WINDOW_DAYS}d); insufficient_evidence under {MIN_BASELINE_DAYS} baseline days"
                    ),
                    "cities_of_window_arrivals": window_cities
                        .iter()
                        .map(|(name, arrivals)| json!({"city": name, "arrivals": arrivals}))
                        .collect::<Vec<_>>(),
                },
                "campaigns": campaigns
                    .iter()
                    .map(|row| json!({
                        "slug": row.0,
                        "template_key": row.1,
                        "status": row.2,
                        "scheduled_at": row.3,
                        "recipients": row.4,
                        "delivered": row.5,
                        "completed_at": row.6,
                    }))
                    .collect::<Vec<_>>(),
                "evidence_gaps": evidence_gaps,
            },
            "recipients": {
                "band": band
                    .iter()
                    .map(|(email, name)| json!({"email": email, "name": name}))
                    .collect::<Vec<_>>(),
            },
            "honesty_contract": {
                "observed": "first-party attribution only — acquisitions and clicks bound to the release campaign, plus delivery receipts",
                "inferred": "the whole window against the tenant's own trailing baseline — ambient growth is not credited to the release",
                "rules": [
                    "never_sum_numbers_across_evidence_classes",
                    "state_evidence_gaps_explicitly_do_not_zero_them",
                    "do_not_claim_listens_the_records_do_not_support",
                    "ambient_growth_is_not_release_growth",
                    "the_artifact_is_the_whole_report_no_account_required"
                ]
            },
        }),
    )
    .await?;
    Ok(())
}
