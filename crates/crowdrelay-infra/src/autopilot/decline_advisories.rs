//! Uncomfortable advice (§4g) — decline detection said plainly.
//!
//! The community engager keeps posting to rooms the audience graph
//! promoted, and the causal question "does r/X produce fans" has a cheap
//! deterministic answer when the data is in: the posts get upvotes and
//! the provenance ledger records zero conversions. A room that engages
//! but produces zero fans is where effort goes to be applauded, not
//! converted — the advice is to stop spending there.
//!
//! Three rules govern the raise, straight from the spec:
//!
//! - **Evidence or silence.** The advisory needs a measured room — enough
//!   posts with real engagement inside the window — or it does not fire.
//!   A quiet room is not a decline, it is a quiet room.
//! - **Always paired with the alternative.** "Stop X" is never raised
//!   without "do Y instead" — the community that converted, or the
//!   strongest other engaged room when nothing converts yet. No
//!   alternative, no advisory.
//! - **The band can disagree and it is recorded.** Cancelling the action
//!   lands the `cancel_autopilot_action` row in `operator_actions` — the
//!   same operator-verdict evidence the preference posterior reads — and
//!   the same subject is not re-raised for a month. Approving parks the
//!   discovery place `not_a_fit`, which is the same switch the console's
//!   block control uses, so the engager leaves the pool on the next
//!   cycle — and a parked room is never flagged again.

use super::*;
use crowdrelay_application::autopilot::AutopilotActionPayload;

/// Both halves of the claim measure the same window: engagement within
/// it, conversions within it.
const WINDOW_DAYS: i64 = 90;

/// A room counts as engaged — not merely posted to — when this many posts
/// carried metrics inside the window. Below it the average is noise.
const MIN_POSTS: i64 = 3;

/// Average score floor for "engages". A room averaging under it is cold,
/// not wrongly-targeted — the uncomfortable claim needs applause to
/// contrast against the zero conversions.
const MIN_AVG_SCORE: f64 = 5.0;

/// How long a cancelled advisory keeps its subject out of the queue. The
/// disagreement is recorded; re-asking inside the month reads as not
/// listening.
const DISAGREE_COOLDOWN_DAYS: i64 = 30;

#[derive(Debug, FromRow)]
struct FlaggedRoom {
    target_id: Uuid,
    place_id: Option<Uuid>,
    subreddit: String,
    posts: i64,
    avg_score: f64,
}

/// Mirrors the SQL `normalize_subreddit` function — lowercase, leading
/// `r/` or `/r/` stripped. The keyed collections below must use the same
/// shape as the SQL comparisons or the lookups silently miss.
fn normalize_subreddit_key(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    lowered
        .strip_prefix("/r/")
        .or_else(|| lowered.strip_prefix("r/"))
        .unwrap_or(lowered.as_str())
        .to_string()
}

/// Raises one `community.decline.advisory` action per room that engages
/// but converts nobody — at most a handful per sweep so the queue is a
/// statement, not a wall. Returns the number raised.
pub(in crate::autopilot) async fn raise_decline_advisories(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<u32, RepositoryError> {
    let ws = workspace_id.into_uuid();

    let conversions = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT normalize_subreddit(community) AS key, COUNT(*) AS fans
        FROM fan_provenance_events
        WHERE workspace_id = $1 AND event_kind = 'conversion'
          AND channel = 'reddit' AND community IS NOT NULL
          AND occurred_at >= now() - make_interval(days => $2)
        GROUP BY normalize_subreddit(community)
        "#,
    )
    .bind(ws)
    .bind(i32::try_from(WINDOW_DAYS).unwrap_or(i32::MAX))
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let conversions: std::collections::BTreeMap<String, i64> = conversions.into_iter().collect();

    // Flagged = engaged target room with zero conversions. The `::float8`
    // on the AVG is load-bearing: AVG returns numeric, which sqlx will not
    // decode into f64 and the result-type gate refuses outright. A room
    // the band already parked (`not_a_fit`) can still look engaged on old
    // posts — it has left the pool, so flagging it again would be noise.
    let flagged = sqlx::query_as::<_, FlaggedRoom>(
        r#"
        SELECT t.id AS target_id, t.place_id, t.subreddit,
               room.posts, room.avg_score
        FROM agent_outreach_targets t
        LEFT JOIN discovery_places place ON place.id = t.place_id
        JOIN LATERAL (
            SELECT COUNT(*) AS posts, AVG(m.score)::float8 AS avg_score
            FROM (
                SELECT DISTINCT ON (cpm.community_post_id) cpm.score
                FROM community_post_metrics cpm
                JOIN community_posts cp ON cp.id = cpm.community_post_id
                WHERE cp.workspace_id = $1
                  AND cp.status = 'posted'
                  AND normalize_subreddit(cp.subreddit) = normalize_subreddit(t.subreddit)
                  AND cp.posted_at >= now() - make_interval(days => $2)
                ORDER BY cpm.community_post_id, cpm.measured_at DESC
            ) m
        ) room ON room.posts >= $3 AND room.avg_score >= $4
        WHERE t.workspace_id = $1
          AND t.status = 'promoted'
          AND t.target_kind = 'community'
          AND t.subreddit IS NOT NULL
          AND t.screening_verdict IS DISTINCT FROM 'refused'
          AND (place.id IS NULL
               OR (place.status = 'active'
                   AND place.membership_state NOT IN ('not_a_fit', 'rejected')))
          AND NOT EXISTS (
              SELECT 1 FROM fan_provenance_events fpe
              WHERE fpe.workspace_id = $1
                AND fpe.channel = 'reddit'
                AND normalize_subreddit(fpe.community) = normalize_subreddit(t.subreddit)
                AND fpe.event_kind = 'conversion'
                AND fpe.occurred_at >= now() - make_interval(days => $2)
          )
        ORDER BY room.avg_score DESC
        LIMIT 3
        "#,
    )
    .bind(ws)
    .bind(i32::try_from(WINDOW_DAYS).unwrap_or(i32::MAX))
    .bind(MIN_POSTS)
    .bind(MIN_AVG_SCORE)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // Every room flagged this sweep is itself a zero-conversion room —
    // "stop A, do B" where B is flagged in the same breath reads as the
    // brain arguing with itself. Flagged rooms are out of the alternative
    // pool entirely; when only flagged rooms remain, the honest answer is
    // silence.
    let flagged_keys: Vec<String> = flagged
        .iter()
        .map(|room| normalize_subreddit_key(&room.subreddit))
        .collect();

    let mut raised = 0u32;
    for room in flagged {
        // The alternative: a converting room first — fans are the point —
        // else the strongest other engaged room. A target row must back
        // it, so the advice names somewhere the engager may actually go.
        let alternative = alternative_room(
            tx,
            ws,
            &room.subreddit,
            &flagged_keys,
            &conversions,
            i32::try_from(WINDOW_DAYS).unwrap_or(i32::MAX),
        )
        .await?;
        let Some((alternative_label, alternative_detail)) = alternative else {
            continue;
        };

        // Recorded disagreement: a cancelled advisory for the same subject
        // inside the cooldown means the band already answered "keep it".
        let disagreed: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM viryaos_autopilot_actions
                 WHERE workspace_id = $1 AND action_kind = 'community.decline.advisory'
                   AND subject_id = $2 AND status = 'cancelled'
                   AND updated_at >= now() - make_interval(days => $3))",
        )
        .bind(ws)
        .bind(room.target_id)
        .bind(i32::try_from(DISAGREE_COOLDOWN_DAYS).unwrap_or(i32::MAX))
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        if disagreed {
            continue;
        }

        let decision_key = format!("decline-advisory:{}", room.target_id);
        let idempotency_key = format!("decline-advisory:{}:{WINDOW_DAYS}", room.target_id);
        let trace = TraceContext::root(workspace_id);
        let trace_id = trace.trace_id().into_uuid();
        let decision_id = if let Some(id) = sqlx::query_scalar::<_, Uuid>(
            r#"INSERT INTO viryaos_autopilot_decisions (
                   id, workspace_id, decision_key, context, subject_kind, subject_id,
                   decision_kind, confidence_basis_points, disposition, reason,
                   input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
               ) VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                         'community.decline.advisory',9000,'require_approval',
                         'Community engages but produced zero fan conversions in the window',
                         $5,$6,$7,$8,$9)
               ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id"#,
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(&decision_key)
        .bind(room.target_id)
        .bind(json!({
            "subreddit": room.subreddit,
            "posts": room.posts,
            "avg_score": room.avg_score,
            "conversions": 0,
            "window_days": WINDOW_DAYS,
        }))
        .bind(json!({"min_posts": MIN_POSTS, "min_avg_score": MIN_AVG_SCORE}))
        .bind(json!({"advise": "park_community", "alternative": alternative_label}))
        .bind(now)
        .bind(trace_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?
        {
            id
        } else {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM viryaos_autopilot_decisions WHERE workspace_id=$1 AND decision_key=$2",
            )
            .bind(ws)
            .bind(&decision_key)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_sqlx)?
        };

        let action_id = Uuid::now_v7();
        let action_trace =
            TraceContext::for_action(workspace_id, trace.trace_id(), action_id, Some(decision_id));
        let payload = serde_json::to_value(AutopilotActionPayload::RaiseDeclineAdvisory {
            target_id: room.target_id,
            place_id: room.place_id,
            subreddit: room.subreddit.clone(),
            posts_considered: u32::try_from(room.posts.max(0)).unwrap_or(0),
            avg_score_tenths: (room.avg_score * 10.0).round() as i64,
            window_days: u32::try_from(WINDOW_DAYS).unwrap_or(u32::MAX),
            alternative_label: alternative_label.clone(),
            alternative_detail: alternative_detail.clone(),
        })
        .map_err(|_| RepositoryError::Unexpected)?;
        let inserted = sqlx::query(
            r#"INSERT INTO viryaos_autopilot_actions (
                   id, workspace_id, decision_id, context, action_kind, subject_kind,
                   subject_id, idempotency_key, payload, status, approval_expires_at,
                   trace_id, causation_id
               ) VALUES ($1,$2,$3,'growth_intelligence','community.decline.advisory',
                         'target_community',$4,$5,$6,'awaiting_approval',
                         $7 + INTERVAL '72 hours',$8,$9)
               ON CONFLICT (workspace_id, idempotency_key) DO NOTHING"#,
        )
        .bind(action_id)
        .bind(ws)
        .bind(decision_id)
        .bind(room.target_id)
        .bind(&idempotency_key)
        .bind(payload)
        .bind(now)
        .bind(action_trace.trace_id().into_uuid())
        .bind(action_trace.causation_id().map(|c| c.into_uuid()))
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        raised += u32::try_from(inserted.rows_affected()).unwrap_or(0);
    }
    Ok(raised)
}

/// The room the advice points at instead. A community with measured fan
/// conversions outranks raw applause — conversions are the thing the
/// flagged room failed to produce. Returns `(label, detail)` or None —
/// and None means the advisory does not fire at all.
async fn alternative_room(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    flagged_subreddit: &str,
    flagged_keys: &[String],
    conversions: &std::collections::BTreeMap<String, i64>,
    window_days: i32,
) -> Result<Option<(String, String)>, RepositoryError> {
    // First preference: a promoted community room that actually converted
    // fans inside the window and is still somewhere the engager may go —
    // a room the band already parked is not a destination.
    let converted = sqlx::query_as::<_, (String,)>(
        r#"
        SELECT t.subreddit
        FROM fan_provenance_events fpe
        JOIN agent_outreach_targets t
          ON t.workspace_id = fpe.workspace_id
         AND normalize_subreddit(t.subreddit) = normalize_subreddit(fpe.community)
         AND t.status = 'promoted'
         AND t.target_kind = 'community'
         AND t.screening_verdict IS DISTINCT FROM 'refused'
        LEFT JOIN discovery_places place ON place.id = t.place_id
        WHERE fpe.workspace_id = $1
          AND fpe.event_kind = 'conversion'
          AND fpe.occurred_at >= now() - make_interval(days => $2)
          AND normalize_subreddit(fpe.community) <> normalize_subreddit($3)
          AND NOT EXISTS (
              SELECT 1 FROM viryaos_autopilot_actions a
              WHERE a.workspace_id = $1
                AND a.action_kind = 'community.decline.advisory'
                AND a.subject_id = t.id
                AND a.status IN ('awaiting_approval', 'queued', 'processing')
          )
          AND (place.id IS NULL
               OR (place.status = 'active'
                   AND place.membership_state NOT IN ('not_a_fit', 'rejected')))
        GROUP BY t.subreddit
        ORDER BY COUNT(*) DESC, t.subreddit
        LIMIT 1
        "#,
    )
    .bind(ws)
    .bind(window_days)
    .bind(flagged_subreddit)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if let Some((subreddit,)) = converted {
        let fans = conversions
            .get(&normalize_subreddit_key(&subreddit))
            .copied()
            .unwrap_or(0);
        return Ok(Some((
            subreddit.clone(),
            format!("{fans} fan conversions in {window_days} days"),
        )));
    }

    // Fallback: the strongest other engaged room the engager may still
    // spend on — never a room flagged in this same sweep, since "stop A,
    // do B" where B is also being stopped is the brain arguing with
    // itself. The detail stays honest: applause, not fans, is what it
    // has going for it.
    let other = sqlx::query_as::<_, (String, i64, f64)>(
        r#"
        SELECT t.subreddit, room.posts, room.avg_score
        FROM agent_outreach_targets t
        LEFT JOIN discovery_places place ON place.id = t.place_id
        JOIN LATERAL (
            SELECT COUNT(*) AS posts, AVG(m.score)::float8 AS avg_score
            FROM (
                SELECT DISTINCT ON (cpm.community_post_id) cpm.score
                FROM community_post_metrics cpm
                JOIN community_posts cp ON cp.id = cpm.community_post_id
                WHERE cp.workspace_id = $1
                  AND cp.status = 'posted'
                  AND normalize_subreddit(cp.subreddit) = normalize_subreddit(t.subreddit)
                  AND cp.posted_at >= now() - make_interval(days => $2)
                ORDER BY cpm.community_post_id, cpm.measured_at DESC
            ) m
        ) room ON room.posts >= $3
        WHERE t.workspace_id = $1
          AND t.status = 'promoted'
          AND t.target_kind = 'community'
          AND t.subreddit IS NOT NULL
          AND normalize_subreddit(t.subreddit) <> normalize_subreddit($4)
          AND normalize_subreddit(t.subreddit) <> ALL($5)
          AND t.screening_verdict IS DISTINCT FROM 'refused'
          AND NOT EXISTS (
              SELECT 1 FROM viryaos_autopilot_actions a
              WHERE a.workspace_id = $1
                AND a.action_kind = 'community.decline.advisory'
                AND a.subject_id = t.id
                AND a.status IN ('awaiting_approval', 'queued', 'processing')
          )
          AND (place.id IS NULL
               OR (place.status = 'active'
                   AND place.membership_state NOT IN ('not_a_fit', 'rejected')))
        ORDER BY room.avg_score DESC
        LIMIT 1
        "#,
    )
    .bind(ws)
    .bind(window_days)
    .bind(MIN_POSTS)
    .bind(flagged_subreddit)
    .bind(flagged_keys)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(other.map(|(subreddit, posts, avg_score)| {
        (
            subreddit,
            format!("avg score {avg_score:.1} across {posts} posts in {window_days} days"),
        )
    }))
}
