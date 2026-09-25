//! The two growth-pulse lines of the daily briefing: replies still waiting
//! on the band, and the 28-day outward funnel.
//!
//! Both render only when there is something to say — a briefing that lists
//! zeros is noise, and this briefing exists to be the opposite of noise.
//! The unanswered count is the same predicate the attention board reads,
//! and the funnel is the compact version of `/ops/funnel` — outward event
//! types only, so internal bookkeeping can never pass for growth.

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::BriefingLocale;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::autopilot::map_sqlx;

/// People who answered the band's outreach and have not heard back —
/// the warmest leads the workspace has.
const fn replies_label(locale: BriefingLocale) -> &'static str {
    match locale {
        BriefingLocale::Pl => "Odpowiedzi bez odpowiedzi",
        BriefingLocale::En => "Replies still unanswered",
    }
}

/// The outward funnel line — what the last 28 days of proposals became once
/// they left the queue.
const fn funnel_label(locale: BriefingLocale) -> &'static str {
    match locale {
        BriefingLocale::Pl => "Zewnętrzne dotarcie (28 dni)",
        BriefingLocale::En => "Outward reach (28d)",
    }
}

/// Appends the unanswered-replies and funnel lines to `body` and records
/// their numbers in `sections_map` — one section per fact, both skipped
/// entirely on a quiet day.
pub(super) async fn growth_pulse_lines(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    locale: BriefingLocale,
    sections_map: &mut serde_json::Map<String, serde_json::Value>,
    body: &mut String,
) -> Result<(), RepositoryError> {
    // ── Replies still unanswered ──────────────────────────────────────
    // The same predicate the attention board's `unanswered_replies`
    // section reads: an inbound reply at `received`/`positive` with no
    // later outbound and no newer inbound superseding it, across both the
    // outreach and booking interaction tables. This is the line the whole
    // review gap was about — sixteen people answered the band and heard
    // nothing — so it renders whenever the count is nonzero.
    let (unanswered_total, unanswered_positive, unanswered_oldest_days): (i64, i64, Option<i64>) =
        sqlx::query_as(
            r#"
            SELECT COUNT(*)::bigint,
                   COUNT(*) FILTER (WHERE disposition = 'positive')::bigint,
                   MAX(GREATEST(0, EXTRACT(EPOCH FROM (now() - replied_at))::bigint / 86400))
            FROM (
                SELECT i.target_id, i.disposition, i.occurred_at AS replied_at
                FROM outreach_interactions i
                WHERE i.workspace_id = $1
                  AND i.direction = 'inbound' AND i.phase = 'reply'
                  AND i.disposition IN ('positive', 'received')
                  AND NOT EXISTS (
                      SELECT 1 FROM outreach_interactions later
                      WHERE later.workspace_id = i.workspace_id
                        AND later.target_id = i.target_id
                        AND later.direction = 'outbound'
                        AND later.occurred_at > i.occurred_at
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM outreach_interactions newer
                      WHERE newer.workspace_id = i.workspace_id
                        AND newer.target_id = i.target_id
                        AND newer.direction = 'inbound'
                        AND newer.occurred_at > i.occurred_at
                  )
                UNION ALL
                SELECT i.target_id, i.disposition, i.occurred_at
                FROM booking_interactions i
                WHERE i.workspace_id = $1
                  AND i.direction = 'inbound' AND i.phase = 'reply'
                  AND i.disposition IN ('positive', 'received')
                  AND NOT EXISTS (
                      SELECT 1 FROM booking_interactions later
                      WHERE later.workspace_id = i.workspace_id
                        AND later.target_id = i.target_id
                        AND later.direction = 'outbound'
                        AND later.occurred_at > i.occurred_at
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM booking_interactions newer
                      WHERE newer.workspace_id = i.workspace_id
                        AND newer.target_id = i.target_id
                        AND newer.direction = 'inbound'
                        AND newer.occurred_at > i.occurred_at
                  )
            ) unanswered
            "#,
        )
        .bind(ws)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    if unanswered_total > 0 {
        sections_map.insert(
            "unanswered_replies".to_owned(),
            serde_json::json!({
                "total": unanswered_total,
                "positive": unanswered_positive,
                "oldest_days": unanswered_oldest_days,
            }),
        );
        let positive_part = if unanswered_positive > 0 {
            match locale {
                BriefingLocale::Pl => format!(" — w tym {unanswered_positive} na tak"),
                BriefingLocale::En => format!(" — {unanswered_positive} of them said yes"),
            }
        } else {
            String::new()
        };
        let age_part = match (locale, unanswered_oldest_days) {
            (_, None) => String::new(),
            (BriefingLocale::Pl, Some(days)) => format!(" — najstarsza {days} dni"),
            (BriefingLocale::En, Some(days)) => format!(" — oldest {days}d"),
        };
        body.push_str(&format!(
            "{}: {}{}{}\n",
            replies_label(locale),
            unanswered_total,
            positive_part,
            age_part
        ));
    }

    // ── Outward funnel, last 28 days ──────────────────────────────────
    // The compact version of `/ops/funnel`: outward proposals minted,
    // sends that actually delivered, replies that came back, how many of
    // them were positive. Internal bookkeeping never enters — the count is
    // outward event types and outward contexts only, so motion cannot pass
    // for growth. Renders whenever anything outward happened; silence here
    // is itself the finding.
    let (funnel_proposed, funnel_sent, funnel_replies, funnel_positive): (i64, i64, i64, i64) = {
        let (proposed, sent) = sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT
                (SELECT COUNT(*) FROM autopilot_actions
                  WHERE workspace_id = $1
                    AND context IN ('outreach','booking_opportunity','booking_agent',
                                    'beacon','representation','live_opportunity')
                    AND created_at >= now() - interval '28 days'),
                (SELECT COUNT(*) FROM outbox_events
                  WHERE workspace_id = $1
                    AND event_type IN ('crowdrelay.outreach.requested',
                                       'crowdrelay.outreach.reply_requested',
                                       'crowdrelay.booking.outreach_requested',
                                       'crowdrelay.booking_agent.approach_requested',
                                       'crowdrelay.beacon.outreach_requested',
                                       'crowdrelay.representation.approach_requested',
                                       'crowdrelay.opportunity.application_requested')
                    AND status = 'delivered'
                    AND delivered_at >= now() - interval '28 days')
            "#,
        )
        .bind(ws)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        let (replies, positive) = sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT COUNT(*)::bigint,
                   COUNT(*) FILTER (WHERE disposition IN
                      ('positive','interested','partner','signed','booked'))::bigint
            FROM (
                SELECT disposition FROM outreach_interactions
                WHERE workspace_id = $1 AND direction = 'inbound'
                  AND occurred_at >= now() - interval '28 days'
                UNION ALL
                SELECT disposition FROM booking_interactions
                WHERE workspace_id = $1 AND direction = 'inbound'
                  AND occurred_at >= now() - interval '28 days'
                UNION ALL
                SELECT disposition FROM booking_agent_interactions
                WHERE workspace_id = $1 AND direction = 'inbound'
                  AND occurred_at >= now() - interval '28 days'
            ) inbound
            "#,
        )
        .bind(ws)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        (proposed, sent, replies, positive)
    };
    if funnel_proposed > 0 || funnel_sent > 0 || funnel_replies > 0 {
        sections_map.insert(
            "funnel_28d".to_owned(),
            serde_json::json!({
                "proposed": funnel_proposed,
                "delivered": funnel_sent,
                "replies": funnel_replies,
                "positive": funnel_positive,
            }),
        );
        let detail = match locale {
            BriefingLocale::Pl => format!(
                "{funnel_proposed} propozycji · {funnel_sent} wysłanych · {funnel_replies} odpowiedzi · {funnel_positive} na tak"
            ),
            BriefingLocale::En => format!(
                "{funnel_proposed} proposed · {funnel_sent} delivered · {funnel_replies} replies · {funnel_positive} positive"
            ),
        };
        body.push_str(&format!("{}: {}\n", funnel_label(locale), detail));
    }

    Ok(())
}
