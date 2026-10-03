//! The SQL statements behind the play anchor and audience reads.
//!
//! Split out of `plays.rs` so the repository code that runs them stays
//! readable; the statements and their doc comments moved unchanged.

use super::*;

/// Shows with no play of this kind yet.
pub(super) const PLAY_EVENT_ANCHORS_SQL: &str = r#"
SELECT
    event.id AS anchor_id,
    event.starts_at AS anchor_at,
    (event.status = 'published') AS active,
    FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $3)) / 3600)::bigint AS hours_until
FROM events AS event
WHERE event.workspace_id = $1
  AND event.status = 'published'
  AND event.starts_at > $3
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = event.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'event'
        AND play.anchor_id = event.id
  )
ORDER BY event.starts_at
LIMIT $4
"#;

/// Engaged fans with no play of this kind yet.
///
/// Engaged means they have done something, not that a score said so: a paid
/// ticket or a registered interest inside the last year. The alternative — every
/// consented fan — is a mailing list with an ask attached, which is the thing
/// this play exists instead of.
///
/// The anchor moment is now, because the anchor is the fan qualifying rather
/// than a date in the future, so every rung is scheduled forward from here.
///
/// The ladder is only offered where the operator has already published the
/// tracked link it points at. A follow ask with nowhere to send people is the
/// one message in the system that is worse than silence.
pub(super) const PLAY_FAN_ANCHORS_SQL: &str = r#"
SELECT
    fan.id AS anchor_id,
    $3::timestamptz AS anchor_at,
    true AS active,
    0::bigint AS hours_until
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
  AND fan.status = 'active'
  AND EXISTS (
      SELECT 1
      FROM smart_links AS link
      WHERE link.workspace_id = fan.workspace_id
        AND link.slug = $5
        AND link.active
  )
  AND (
      EXISTS (
          SELECT 1
          FROM ticket_orders AS ticket_order
          WHERE ticket_order.workspace_id = fan.workspace_id
            AND ticket_order.buyer_email = fan.normalized_email
            AND ticket_order.status IN ('paid', 'partially_refunded')
            AND ticket_order.created_at > $3 - INTERVAL '1 year'
      )
      OR EXISTS (
          SELECT 1
          FROM event_interests AS interest
          WHERE interest.workspace_id = fan.workspace_id
            AND interest.fan_id = fan.id
            AND interest.created_at > $3 - INTERVAL '1 year'
      )
  )
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = fan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'fan'
        AND play.anchor_id = fan.id
  )
ORDER BY fan.id
LIMIT $4
"#;

/// Fans who were here and stopped being here.
///
/// The exact complement of the ladder's audience at the one-year line, so a fan
/// is never both at once: engaged at some point, nothing at all inside the last
/// year. Anybody who never did anything is not dormant, they are a name on a
/// list, and writing to them is the mailing-machine failure this whole context
/// exists to avoid.
///
/// Two further refusals, both of which are the difference between a revival and
/// a pestering:
///
/// * **Nothing to revive them with.** A revival message in a workspace with no
///   upcoming show is "hello, remember us". The gate is a published date, and it
///   is the same shape as the follow-ask ladder's tracked-link gate.
/// * **We already talked at them.** The weekly envelope bounds contact; it does
///   not know that this fan has just had a whole three-rung ladder. Six months
///   without any play step reaching them is what makes this a second chance
///   rather than a continuation.
pub(super) const PLAY_DORMANT_ANCHORS_SQL: &str = r#"
SELECT
    fan.id AS anchor_id,
    $3::timestamptz AS anchor_at,
    true AS active,
    0::bigint AS hours_until
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
  AND fan.status = 'active'
  AND EXISTS (
      SELECT 1
      FROM events AS event
      WHERE event.workspace_id = fan.workspace_id
        AND event.status = 'published'
        AND event.starts_at > $3
  )
  AND (
      EXISTS (
          SELECT 1
          FROM ticket_orders AS ticket_order
          WHERE ticket_order.workspace_id = fan.workspace_id
            AND ticket_order.buyer_email = fan.normalized_email
            AND ticket_order.status IN ('paid', 'partially_refunded')
      )
      OR EXISTS (
          SELECT 1
          FROM event_interests AS interest
          WHERE interest.workspace_id = fan.workspace_id
            AND interest.fan_id = fan.id
      )
  )
  AND NOT EXISTS (
      SELECT 1
      FROM ticket_orders AS ticket_order
      WHERE ticket_order.workspace_id = fan.workspace_id
        AND ticket_order.buyer_email = fan.normalized_email
        AND ticket_order.status IN ('paid', 'partially_refunded')
        AND ticket_order.created_at > $3 - INTERVAL '1 year'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM event_interests AS interest
      WHERE interest.workspace_id = fan.workspace_id
        AND interest.fan_id = fan.id
        AND interest.created_at > $3 - INTERVAL '1 year'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM play_step_recipients AS reached
      WHERE reached.workspace_id = fan.workspace_id
        AND reached.fan_id = fan.id
        AND reached.created_at > $3 - INTERVAL '6 months'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = fan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'fan'
        AND play.anchor_id = fan.id
  )
ORDER BY fan.id
LIMIT $4
"#;

/// Active release plans with no release runway play yet.
///
/// A release is the anchor when it is ahead and active. The anchor moment is
/// `release_at`, so steps schedule relative to the release date: pre-save
/// four weeks before, announce two weeks before, curator wave one week
/// before, release-day push at zero, sustain ask two weeks after.
pub(super) const PLAY_RELEASE_ANCHORS_SQL: &str = r#"
SELECT
    plan.id AS anchor_id,
    plan.release_at AS anchor_at,
    plan.active AS active,
    FLOOR(EXTRACT(EPOCH FROM (plan.release_at - $3)) / 3600)::bigint AS hours_until
FROM release_plans AS plan
WHERE plan.workspace_id = $1
  AND plan.active
  -- A filler release is posted into a quiet week, not run through the
  -- runway — the tier is the band's call, not a play that failed to open.
  AND plan.tier <> 'filler'
  AND plan.release_at > $3
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = plan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'release'
        AND play.anchor_id = plan.id
  )
ORDER BY plan.release_at
LIMIT $4
"#;

/// The statement that finds anchors for one play, and whether it reads the
/// follow-ask link slug.
///
/// Keyed on the play kind rather than the anchor kind: two plays now share the
/// fan anchor and ask completely different questions of it. A parameter the
/// statement does not mention is a protocol error, so the flag travels with the
/// statement rather than being inferred at the call site.
pub(super) const fn anchor_statement(kind: PlayKind) -> (&'static str, bool) {
    match kind {
        PlayKind::TrackUsAsk | PlayKind::ListingCompletenessSweep => {
            (PLAY_EVENT_ANCHORS_SQL, false)
        }
        PlayKind::FollowAskLadder => (PLAY_FAN_ANCHORS_SQL, true),
        PlayKind::DormantRevival => (PLAY_DORMANT_ANCHORS_SQL, false),
        PlayKind::ReleaseRunway => (PLAY_RELEASE_ANCHORS_SQL, false),
    }
}

/// The eligible audience for the earliest unsettled step of one play.
///
/// Written as one statement so the recipient and the count can never come from
/// two different reads: a count taken separately would be a different moment's
/// answer, and the play would claim work it no longer had.
pub(super) const PLAY_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index, step.step_kind
    FROM play_steps AS step
    WHERE step.workspace_id = $1
      AND step.play_id = $2
      AND step.settled_at IS NULL
    ORDER BY step.step_index
    LIMIT 1
),
eligible AS (
    SELECT fan.id AS fan_id
    FROM open_step
    CROSS JOIN fans AS fan
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
      AND fan.status = 'active'
      AND (
          (
              open_step.step_kind = 'announce_ask'
              AND (
                  EXISTS (
                      SELECT 1
                      FROM ticket_orders AS ticket_order
                      JOIN ticket_sales AS sale
                        ON sale.workspace_id = ticket_order.workspace_id
                       AND sale.id = ticket_order.ticket_sale_id
                      WHERE ticket_order.workspace_id = fan.workspace_id
                        AND ticket_order.buyer_email = fan.normalized_email
                        AND ticket_order.status IN ('paid', 'partially_refunded')
                        AND sale.event_id = $3
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM event_interests AS interest
                      WHERE interest.workspace_id = fan.workspace_id
                        AND interest.fan_id = fan.id
                        AND interest.event_id = $3
                  )
              )
          )
          OR (
              -- Compatibility only: new TrackUsAsk plays no longer create this
              -- rung. If an old persisted play reaches it, attendance must be
              -- observed rather than inferred from a purchase.
              open_step.step_kind = 'post_show_ask'
              AND (
                  EXISTS (
                      SELECT 1
                      FROM concert_checkins AS checkin
                      WHERE checkin.workspace_id = fan.workspace_id
                        AND checkin.fan_id = fan.id
                        AND checkin.event_id = $3
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM admission_passes AS pass
                      WHERE pass.workspace_id = fan.workspace_id
                        AND pass.fan_id = fan.id
                        AND pass.event_id = $3
                        AND pass.status = 'redeemed'
                  )
              )
              -- Show Growth owns the observed room whenever it is enabled:
              -- recap, merch and follow ask share one cadence there. Leaving
              -- this legacy rung live in parallel would create a second owner
              -- of the same T+ moment.
              AND NOT EXISTS (
                  SELECT 1
                  FROM autopilot_policies AS policy
                  WHERE policy.workspace_id = fan.workspace_id
                    AND policy.context = 'show_growth'
                    AND policy.enabled
              )
          )
      )
      -- Already committed to, whether or not it has been delivered. Reading
      -- only the delivered table here is what makes a play re-offer the same
      -- fan every cycle and never finish.
      AND NOT EXISTS (
          SELECT 1
          FROM autopilot_actions AS action
          WHERE action.workspace_id = fan.workspace_id
            AND action.context = 'plays'
            AND action.action_kind = 'play.step.run'
            AND action.status <> 'cancelled'
            AND action.subject_id = fan.id
            AND action.payload->>'play_id' = $2::text
            AND (action.payload->>'step_index')::integer = open_step.step_index
      )
)
SELECT fan_id, count(*) OVER ()::bigint AS remaining
FROM eligible
ORDER BY fan_id
LIMIT 1
"#;

/// The audience of a fan-anchored play: the anchor, and nobody else.
///
/// Separate from the show query rather than a branch inside it. The show
/// version answers "which of our fans has a reason to hear about this date";
/// this one answers "is the person this campaign is about still someone we may
/// write to", and merging them would make one statement that means neither.
pub(super) const PLAY_FAN_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index
    FROM play_steps AS step
    WHERE step.workspace_id = $1
      AND step.play_id = $2
      AND step.settled_at IS NULL
    ORDER BY step.step_index
    LIMIT 1
),
eligible AS (
    SELECT fan.id AS fan_id
    FROM open_step
    CROSS JOIN fans AS fan
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
      AND fan.id = $3
      AND fan.status = 'active'
      -- Committed to, whether or not it has been delivered. Without this the
      -- rung re-offers the same fan every cycle and the ladder never climbs.
      AND NOT EXISTS (
          SELECT 1
          FROM autopilot_actions AS action
          WHERE action.workspace_id = fan.workspace_id
            AND action.context = 'plays'
            AND action.action_kind = 'play.step.run'
            AND action.status <> 'cancelled'
            AND action.subject_id = fan.id
            AND action.payload->>'play_id' = $2::text
            AND (action.payload->>'step_index')::integer = open_step.step_index
      )
)
SELECT fan_id, count(*) OVER ()::bigint AS remaining
FROM eligible
LIMIT 1
"#;

/// The audience of a release-anchored play: every consented fan, because a
/// release is the one thing the whole list should hear about.
///
/// Unlike a fan-anchored play (one person) or an event-anchored play (fans
/// near one show), a release has no geographic or per-fan filter. The only
/// gate is consent and the step's own eligibility — the same action-ledger
/// check that stops any other play re-offering the same fan.
pub(super) const PLAY_RELEASE_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index
    FROM play_steps AS step
    WHERE step.workspace_id = $1
      AND step.play_id = $2
      AND step.settled_at IS NULL
    ORDER BY step.step_index
    LIMIT 1
),
eligible AS (
    SELECT fan.id AS fan_id
    FROM open_step
    CROSS JOIN fans AS fan
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
      AND fan.status = 'active'
      AND NOT EXISTS (
          SELECT 1
          FROM autopilot_actions AS action
          WHERE action.workspace_id = fan.workspace_id
            AND action.context = 'plays'
            AND action.action_kind = 'play.step.run'
            AND action.status <> 'cancelled'
            AND action.subject_id = fan.id
            AND action.payload->>'play_id' = $2::text
            AND (action.payload->>'step_index')::integer = open_step.step_index
      )
)
SELECT fan_id, count(*) OVER ()::bigint AS remaining
FROM eligible
ORDER BY fan_id
LIMIT 1
"#;
