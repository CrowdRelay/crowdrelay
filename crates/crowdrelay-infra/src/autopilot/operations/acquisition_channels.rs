//! Which acquisition channels produced people who stayed.
//!
//! Grouped in SQL and classified in the domain. Grouping first keeps the result
//! bounded by the number of distinct channels rather than by the number of
//! fans; classifying after keeps the rule that a signup with no click is
//! unattributed rather than "direct" in exactly one place.

use super::*;
use crate::autopilot::bounded_u32;
use crowdrelay_application::autopilot::{
    AcquisitionChannels, ChannelPerformance, MIN_CHANNEL_EVIDENCE_FANS, UnattributedGroup,
};
use crowdrelay_domain::acquisition_channel::{
    AttributionEvidence, ChannelAttribution, ChannelIdentity, attribute_channel,
};
use crowdrelay_domain::fan_activation::MeaningfulAction;

#[derive(Debug, FromRow)]
struct ChannelPerformanceRow {
    had_visitor: bool,
    had_click: bool,
    channel_source: Option<String>,
    channel_community: Option<String>,
    channel_creative: Option<String>,
    signups: i64,
    activated_30d: i64,
    departed: i64,
    /// The strongest action any fan from this channel took, as a string
    /// matching MeaningfulAction::as_str(). NULL when nobody acted.
    best_action: Option<String>,
}

pub(in crate::autopilot) async fn load_acquisition_channels(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<AcquisitionChannels, RepositoryError> {
    // Grouped in SQL and classified in the domain. Grouping first keeps
    // the result bounded by the number of distinct channels rather than
    // by the number of fans, and classifying after keeps the "a signup
    // with no click is not direct traffic" rule in exactly one place.
    let rows = sqlx::query_as::<_, ChannelPerformanceRow>(
            r#"
            WITH arrival AS (
                SELECT
                    fan.id AS fan_id,
                    fan.workspace_id,
                    fan.normalized_email,
                    acquisition.anonymous_visitor_id,
                    acquisition.occurred_at AS signed_up_at,
                    fan.status = 'active' AS stayed,
                    -- Only for people still here: nobody who left can be
                    -- activated, and the function is the expensive part of
                    -- this read, so it is not spent on them.
                    CASE WHEN fan.status = 'active' THEN fan_last_meaningful_action(
                        fan.workspace_id, fan.id, fan.normalized_email
                    ) END AS last_action_at,
                    fan.status = 'active' AND EXISTS (
                        SELECT 1 FROM fan_consents AS consent
                        WHERE consent.workspace_id = fan.workspace_id
                          AND consent.fan_id = fan.id
                          AND consent.purpose = 'marketing'
                          AND consent.granted
                          AND consent.recorded_at = (
                              SELECT max(latest.recorded_at) FROM fan_consents AS latest
                              WHERE latest.workspace_id = fan.workspace_id
                                AND latest.fan_id = fan.id
                                AND latest.purpose = 'marketing'
                          )
                    ) AS consented
                FROM fans AS fan
                -- The *first* acquisition event is the acquisition. A later
                -- one describes a return visit, and crediting the channel
                -- somebody came back through would rob the one that found
                -- them.
                LEFT JOIN LATERAL (
                    SELECT event.anonymous_visitor_id, event.occurred_at
                    FROM fan_acquisition_events AS event
                    WHERE event.workspace_id = fan.workspace_id
                      AND event.fan_id = fan.id
                    ORDER BY event.occurred_at ASC, event.id ASC
                    LIMIT 1
                ) AS acquisition ON true
                -- Everyone who became a fan, including those who have since
                -- gone, so a channel's departures can be counted against it.
                -- `merged` is excluded because that person is already counted
                -- under the identity they merged into; `pending` because they
                -- never became a fan to leave.
                WHERE fan.workspace_id = $1
                  AND fan.status IN ('active', 'unsubscribed', 'suppressed')
            ), attributed AS (
                SELECT
                    arrival.fan_id,
                    arrival.workspace_id,
                    arrival.normalized_email,
                    arrival.stayed,
                    arrival.anonymous_visitor_id IS NOT NULL AS had_visitor,
                    click.smart_link_id IS NOT NULL AS had_click,
                    link.channel_source,
                    link.channel_community,
                    link.channel_creative,
                    (
                        arrival.consented
                        AND arrival.last_action_at IS NOT NULL
                        AND arrival.last_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
                    ) AS activated
                FROM arrival
                -- The newest click at or before the signup. Later clicks are
                -- the person coming back, not the route that brought them.
                LEFT JOIN LATERAL (
                    SELECT click.smart_link_id
                    FROM click_events AS click
                    WHERE click.workspace_id = $1
                      AND arrival.anonymous_visitor_id IS NOT NULL
                      AND click.anonymous_visitor_id = arrival.anonymous_visitor_id
                      AND (arrival.signed_up_at IS NULL OR click.occurred_at <= arrival.signed_up_at)
                    ORDER BY click.occurred_at DESC, click.id DESC
                    LIMIT 1
                ) AS click ON true
                LEFT JOIN smart_links AS link
                  ON link.workspace_id = $1
                 AND link.id = click.smart_link_id
            )
            SELECT
                had_visitor,
                had_click,
                channel_source,
                channel_community,
                channel_creative,
                -- `signups` keeps its meaning — people who arrived and are
                -- still here — so every existing reader of it is unchanged.
                count(*) FILTER (WHERE stayed)::bigint AS signups,
                count(*) FILTER (WHERE activated)::bigint AS activated_30d,
                count(*) FILTER (WHERE NOT stayed)::bigint AS departed,
                -- The strongest action any fan from this channel took.
                -- Priority: ticket_purchase > merch_purchase > qualified_referral
                -- > attendance > event_interest > synesthesia_run > signal_session.
                -- Uses the fan_last_meaningful_action function to find the
                -- timestamp, then maps it to the action kind.
                CASE
                    WHEN bool_or(attributed.stayed AND EXISTS (
                        SELECT 1 FROM ticket_orders orders
                        WHERE orders.workspace_id = attributed.workspace_id
                          AND orders.buyer_email = attributed.normalized_email
                          AND orders.status IN ('paid', 'partially_refunded')
                    )) THEN 'ticket_purchase'
                    WHEN bool_or(attributed.stayed AND EXISTS (
                        SELECT 1 FROM merch_order_facts merch
                        WHERE merch.workspace_id = attributed.workspace_id
                          AND merch.fan_id = attributed.fan_id
                          AND merch.confirmed_at IS NOT NULL
                    )) THEN 'merch_purchase'
                    WHEN bool_or(
                        attributed.stayed
                        AND canonical_qualified_referral_count(
                            attributed.workspace_id, attributed.fan_id, NULL
                        ) > 0
                    ) THEN 'qualified_referral'
                    WHEN bool_or(attributed.stayed AND (
                        EXISTS (
                            SELECT 1 FROM concert_checkins checkin
                            WHERE checkin.workspace_id = attributed.workspace_id
                              AND checkin.fan_id = attributed.fan_id
                        )
                        OR EXISTS (
                            SELECT 1 FROM admission_passes pass
                            WHERE pass.workspace_id = attributed.workspace_id
                              AND pass.fan_id = attributed.fan_id
                              AND pass.status = 'redeemed'
                              AND pass.redeemed_at IS NOT NULL
                        )
                    )) THEN 'attendance'
                    WHEN bool_or(attributed.stayed AND EXISTS (
                        SELECT 1 FROM event_interests interest
                        WHERE interest.workspace_id = attributed.workspace_id
                          AND interest.fan_id = attributed.fan_id
                    )) THEN 'event_interest'
                    WHEN bool_or(attributed.stayed AND EXISTS (
                        SELECT 1 FROM synesthesia_reward_entries entry
                        JOIN synesthesia_runs run
                          ON run.workspace_id = entry.workspace_id
                         AND run.id = entry.run_id
                        WHERE entry.workspace_id = attributed.workspace_id
                          AND entry.fan_id = attributed.fan_id
                          AND NOT run.synthetic
                          AND run.completed_at IS NOT NULL
                    )) THEN 'synesthesia_run'
                    WHEN bool_or(attributed.stayed AND EXISTS (
                        SELECT 1 FROM fan_sessions session
                        WHERE session.workspace_id = attributed.workspace_id
                          AND session.fan_id = attributed.fan_id
                          AND session.revoked_at IS NULL
                          AND session.last_seen_at IS NOT NULL
                    )) THEN 'signal_session'
                    ELSE NULL
                END AS best_action
            FROM attributed
            GROUP BY had_visitor, had_click, channel_source,
                     channel_community, channel_creative
            ORDER BY activated_30d DESC, signups DESC
            LIMIT $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .bind(MAX_SNAPSHOTS_PER_CONTEXT)
        .fetch_all(&repo.pool)
        .await
        .map_err(map_sqlx)?;

    let mut channels = Vec::new();
    let mut unattributed: Vec<UnattributedGroup> = Vec::new();
    let mut total_signups: u32 = 0;
    let mut total_activated: u32 = 0;

    // The workspace-level funnel, from the derived view. One row by
    // construction; a missing row means zero fans, which reads as zeros
    // rather than as an error.
    let kpi: Option<(i64, i64, i64)> = sqlx::query_as(
        r#"
        SELECT active_30d, reachable_consented, retained_30d
        FROM fan_activation_kpi
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let (active_30d, reachable_consented, retained_30d) = match kpi {
        Some((active, consented, retained)) => (
            bounded_u32(active)?,
            bounded_u32(consented)?,
            bounded_u32(retained)?,
        ),
        None => (0, 0, 0),
    };

    for row in rows {
        let signups = bounded_u32(row.signups)?;
        let activated = bounded_u32(row.activated_30d)?;
        let departed = bounded_u32(row.departed)?;
        total_signups = total_signups.saturating_add(signups);
        total_activated = total_activated.saturating_add(activated);

        let attribution = attribute_channel(&AttributionEvidence {
            had_visitor: row.had_visitor,
            had_click_before_signup: row.had_click,
            identity: row.channel_source.map(|source| ChannelIdentity {
                source,
                community: row.channel_community,
                creative: row.channel_creative,
            }),
        });

        // Each claim is floored on its own denominator. The channel-level
        // "did it produce fans" flag counts everyone it produced — stayed or
        // departed — while the rate's denominator is only those who stayed.
        let produced = signups.saturating_add(departed);
        match attribution {
            ChannelAttribution::Unattributed { reason } => {
                // Merge rather than append: two groups can share a
                // reason for different underlying shapes, and an
                // operator wants one line per fix.
                if let Some(existing) = unattributed.iter_mut().find(|group| group.reason == reason)
                {
                    existing.signups = existing.signups.saturating_add(signups);
                    existing.activated_30d = existing.activated_30d.saturating_add(activated);
                    existing.departed = existing.departed.saturating_add(departed);
                    existing.sufficient_evidence =
                        existing.departed.saturating_add(existing.signups)
                            >= MIN_CHANNEL_EVIDENCE_FANS;
                } else {
                    unattributed.push(UnattributedGroup {
                        reason,
                        remedy: reason.remedy(),
                        signups,
                        activated_30d: activated,
                        departed,
                        sufficient_evidence: produced >= MIN_CHANNEL_EVIDENCE_FANS,
                    });
                }
            }
            attributed => channels.push(ChannelPerformance {
                attribution: attributed,
                signups,
                activated_30d: activated,
                // A rate from an empty denominator is not a zero, and a rate
                // over fewer than four who stayed is luck, not signal.
                activation_basis_points: (signups >= MIN_CHANNEL_EVIDENCE_FANS).then(|| {
                    u32::try_from(u64::from(activated).saturating_mul(10_000) / u64::from(signups))
                        .unwrap_or(u32::MAX)
                }),
                sufficient_evidence: produced >= MIN_CHANNEL_EVIDENCE_FANS,
                best_action: row.best_action.as_deref().and_then(MeaningfulAction::parse),
                departed,
            }),
        }
    }

    Ok(AcquisitionChannels {
        channels,
        total_signups,
        total_activated_30d: total_activated,
        active_30d,
        reachable_consented,
        retained_30d,
        unattributed,
    })
}
