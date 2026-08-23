//! Which acquisition channels produced people who stayed.
//!
//! Grouped in SQL and classified in the domain. Grouping first keeps the result
//! bounded by the number of distinct channels rather than by the number of
//! fans; classifying after keeps the rule that a signup with no click is
//! unattributed rather than "direct" in exactly one place.

use super::*;
use crate::autopilot::bounded_u32;
use crowdrelay_application::autopilot::{
    AcquisitionChannels, ChannelPerformance, UnattributedGroup,
};
use crowdrelay_domain::acquisition_channel::{
    AttributionEvidence, ChannelAttribution, ChannelIdentity, attribute_channel,
};

#[derive(Debug, FromRow)]
struct ChannelPerformanceRow {
    had_visitor: bool,
    had_click: bool,
    channel_source: Option<String>,
    channel_community: Option<String>,
    channel_creative: Option<String>,
    signups: i64,
    activated_30d: i64,
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
                    acquisition.anonymous_visitor_id,
                    acquisition.occurred_at AS signed_up_at,
                    fan_last_meaningful_action(
                        fan.workspace_id, fan.id, fan.normalized_email
                    ) AS last_action_at,
                    EXISTS (
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
                WHERE fan.workspace_id = $1
                  AND fan.status = 'active'
            ), attributed AS (
                SELECT
                    arrival.fan_id,
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
                count(*)::bigint AS signups,
                count(*) FILTER (WHERE activated)::bigint AS activated_30d
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

    for row in rows {
        let signups = bounded_u32(row.signups)?;
        let activated = bounded_u32(row.activated_30d)?;
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

        match attribution {
            ChannelAttribution::Unattributed { reason } => {
                // Merge rather than append: two groups can share a
                // reason for different underlying shapes, and an
                // operator wants one line per fix.
                if let Some(existing) = unattributed.iter_mut().find(|group| group.reason == reason)
                {
                    existing.signups = existing.signups.saturating_add(signups);
                    existing.activated_30d = existing.activated_30d.saturating_add(activated);
                } else {
                    unattributed.push(UnattributedGroup {
                        reason,
                        remedy: reason.remedy(),
                        signups,
                        activated_30d: activated,
                    });
                }
            }
            attributed => channels.push(ChannelPerformance {
                attribution: attributed,
                signups,
                activated_30d: activated,
                // A rate from an empty denominator is not a zero.
                activation_basis_points: (signups > 0).then(|| {
                    u32::try_from(u64::from(activated).saturating_mul(10_000) / u64::from(signups))
                        .unwrap_or(u32::MAX)
                }),
                best_action: None,
            }),
        }
    }

    Ok(AcquisitionChannels {
        channels,
        total_signups,
        total_activated_30d: total_activated,
        unattributed,
    })
}
