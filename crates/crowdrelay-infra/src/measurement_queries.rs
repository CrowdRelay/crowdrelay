//! The measurement ledger's queries, kept here so both the API read model
//! (`crowdrelay-api`'s `autopilot/measurement.rs`) and the disposable-database
//! test (`tests/tests/postgres/measurement.rs`) run the same string.
//!
//! These are the sprint-M spec's queries verbatim — every one is
//! workspace-scoped and read-only, `$1 = workspace_id uuid`,
//! `$2 = now timestamptz`. Do not restate or "improve" one: they were
//! PREPARE-checked against the dev schema, and a query that errors is a
//! finding to report, not a shape to fix.

/// Claim 1 — "Nothing the band does goes unshared": actions ingested ÷ happened.
pub const UNSHARED_SQL: &str = r#"
WITH happened AS (
    SELECT s.id
    FROM content_sources s
    WHERE s.workspace_id = $1
      AND s.created_at >= $2 - interval '90 days' AND s.created_at <= $2
)
SELECT count(*) AS happened,
       count(*) FILTER (WHERE EXISTS (
           SELECT 1 FROM autopilot_actions a
           WHERE a.workspace_id = $1
             AND a.subject_kind = 'content_source' AND a.subject_id = h.id
             AND a.status = 'succeeded')) AS shared
FROM happened h
"#;

/// Claim 2 — "Amplification is fast": median minutes action → first artifact.
pub const AMPLIFICATION_SPEED_SQL: &str = r#"
WITH first_artifact AS (
    SELECT s.id, s.created_at, min(a.finished_at) AS first_at
    FROM content_sources s
    JOIN autopilot_actions a
      ON a.workspace_id = s.workspace_id
     AND a.subject_kind = 'content_source' AND a.subject_id = s.id
     AND a.status = 'succeeded' AND a.finished_at IS NOT NULL
    WHERE s.workspace_id = $1
      AND s.created_at >= $2 - interval '90 days' AND s.created_at <= $2
    GROUP BY s.id, s.created_at
)
SELECT count(*) AS n,
       percentile_cont(0.5) WITHIN GROUP (
           ORDER BY EXTRACT(EPOCH FROM (first_at - created_at))::double precision / 60.0
       ) AS median_minutes
FROM first_artifact
WHERE first_at >= created_at
"#;

/// Claim 3 — "Suggestions worth reading": acted on ÷ shown.
pub const SUGGESTIONS_READ_SQL: &str = r#"
SELECT count(*) AS shown,
       count(*) FILTER (WHERE s.status IN ('approved','done') OR EXISTS (
           SELECT 1 FROM suggestion_outcomes o
           WHERE o.workspace_id = $1 AND o.suggestion_id = s.id
             AND o.outcome IN ('done','done_differently'))) AS acted
FROM content_suggestions s
WHERE s.workspace_id = $1
  AND s.created_at >= $2 - interval '90 days' AND s.created_at <= $2
"#;

/// Claim 5 — "Outreach converts": replies ÷ sent, per channel.
pub const OUTREACH_CONVERTS_SQL: &str = r#"
SELECT channel,
       count(*) AS sent,
       count(*) FILTER (WHERE status IN ('replied','positive_reply','declined','converted')) AS replied
FROM reach_events
WHERE workspace_id = $1
  AND recipient_kind IN ('outreach_target','community','subreddit_audience')
  AND status <> 'failed'
  AND created_at >= $2 - interval '90 days' AND created_at <= $2
GROUP BY channel
ORDER BY channel
"#;

/// Claim 6 — "Time saved": approvals/week × minutes by hand. This reports the
/// measured factor only — the minutes-by-hand multiplier is the chief's model.
pub const TIME_SAVED_SQL: &str = r#"
SELECT count(*) AS approvals
FROM autopilot_actions
WHERE workspace_id = $1
  AND approved_at IS NOT NULL
  AND approved_at >= $2 - interval '90 days' AND approved_at <= $2
"#;

/// Claim 7 — "Plan followed": arc beats delivered ÷ planned. `delivered` is
/// capped per arc at that arc's due beats so over-delivery on one arc cannot
/// cover a slip on another.
pub const PLAN_FOLLOWED_SQL: &str = r#"
WITH beats AS (
    SELECT a.id AS arc_id, (b ->> 'at')::date AS at
    FROM arcs a,
         jsonb_array_elements(CASE WHEN jsonb_typeof(a.spine) = 'array' THEN a.spine ELSE '[]'::jsonb END) b
    WHERE a.workspace_id = $1
      AND a.status IN ('approved','active','completed')
      AND (b ->> 'at') ~ '^\d{4}-\d{2}-\d{2}$'
),
due AS (
    SELECT arc_id, at FROM beats
    WHERE at <= ($2)::date AND at >= (($2)::date - 90)
),
delivered AS (
    SELECT s.arc_id, count(*) AS n
    FROM content_suggestions s
    WHERE s.workspace_id = $1 AND s.arc_id IS NOT NULL
      AND (s.status = 'done' OR EXISTS (
          SELECT 1 FROM suggestion_outcomes o
          WHERE o.workspace_id = $1 AND o.suggestion_id = s.id
            AND o.outcome IN ('done','done_differently')))
      AND s.created_at >= $2 - interval '90 days' AND s.created_at <= $2
    GROUP BY s.arc_id
)
SELECT (SELECT count(*) FROM due) AS planned,
       (SELECT COALESCE(sum(LEAST(d.n, (SELECT count(*) FROM due WHERE due.arc_id = d.arc_id))), 0)::bigint FROM delivered d) AS delivered
"#;

/// Claim 8 — "Drift caught early": median days between a slipped due date and
/// the first reminder that told someone. `slipped_untold` is the companion
/// count — open, past due, and nobody has been reminded yet.
pub const DRIFT_CAUGHT_SQL: &str = r#"
SELECT count(*) AS n,
       percentile_cont(0.5) WITHIN GROUP (
           ORDER BY EXTRACT(EPOCH FROM (first_overdue_reminder_at - due_at))::double precision / 86400.0
       ) AS median_days,
       (SELECT count(*) FROM team_assignments late
         WHERE late.workspace_id = $1 AND late.status = 'open'
           AND late.due_at IS NOT NULL AND late.due_at < $2
           AND late.first_overdue_reminder_at IS NULL) AS slipped_untold
FROM team_assignments
WHERE workspace_id = $1
  AND due_at IS NOT NULL AND first_overdue_reminder_at IS NOT NULL
  AND due_at >= $2 - interval '90 days' AND due_at <= $2
"#;

/// Claim 9 — "The room stops leaking": scans ÷ room size, per show. The master
/// variable. `room_size` is nullable — a show with no room capacity on record
/// is unmeasured, not zero.
///
/// `room_size` is `events.room_capacity` and deliberately not the pass-pool
/// total it used to be. `sum(admission_pools.capacity)` is how many passes the
/// night issued — guestlist, winners, comps — so a sold-out three-hundred-cap
/// room that issued twelve passes reported a room of twelve, and the leak rate
/// came out near perfect on exactly the nights that leaked most. How many
/// people the room holds is a fact about the venue, known when the show is
/// booked and unchanged by anything ticketing does later.
pub const ROOM_LEAK_SQL: &str = r#"
SELECT e.slug, e.title, e.starts_at,
       (SELECT count(DISTINCT c.fan_id) FROM concert_checkins c
         WHERE c.workspace_id = $1 AND c.event_id = e.id) AS scans,
       e.room_capacity::bigint AS room_size
FROM events e
WHERE e.workspace_id = $1 AND e.status = 'completed'
  AND e.starts_at >= $2 - interval '90 days' AND e.starts_at <= $2
ORDER BY e.starts_at DESC
"#;

/// Claim 10 — "Fans gathered, not just retained": new consented reachable fans
/// per channel per month. The control plane divides by 3 for "per month".
pub const FANS_GATHERED_SQL: &str = r#"
WITH latest_marketing AS (
    SELECT DISTINCT ON (fan_id) fan_id, granted
    FROM fan_consents
    WHERE workspace_id = $1 AND purpose = 'marketing'
    ORDER BY fan_id, recorded_at DESC, id DESC
),
first_touch AS (
    SELECT DISTINCT ON (fan_id) fan_id, source
    FROM fan_acquisition_events
    WHERE workspace_id = $1
    ORDER BY fan_id, occurred_at, id
)
SELECT COALESCE(split_part(ft.source, ':', 1), 'unattributed') AS channel,
       count(*) AS fans
FROM fans f
JOIN latest_marketing lm ON lm.fan_id = f.id AND lm.granted
LEFT JOIN first_touch ft ON ft.fan_id = f.id
WHERE f.workspace_id = $1 AND f.status = 'active'
  AND f.created_at >= $2 - interval '90 days' AND f.created_at <= $2
  AND (ft.source IS NULL OR ft.source NOT LIKE 'fan_import:%')
GROUP BY 1
ORDER BY fans DESC, channel
"#;

/// Claim 11 — "Recovery is not growth": archive confirmations on their own
/// line. Same CTEs as `FANS_GATHERED_SQL`, the `fan_import:` side of the split.
pub const RECOVERY_NOT_GROWTH_SQL: &str = r#"
WITH latest_marketing AS (
    SELECT DISTINCT ON (fan_id) fan_id, granted
    FROM fan_consents
    WHERE workspace_id = $1 AND purpose = 'marketing'
    ORDER BY fan_id, recorded_at DESC, id DESC
),
first_touch AS (
    SELECT DISTINCT ON (fan_id) fan_id, source
    FROM fan_acquisition_events
    WHERE workspace_id = $1
    ORDER BY fan_id, occurred_at, id
)
SELECT count(*) AS recovered
FROM fans f
JOIN latest_marketing lm ON lm.fan_id = f.id AND lm.granted
JOIN first_touch ft ON ft.fan_id = f.id
WHERE f.workspace_id = $1 AND f.status = 'active'
  AND f.created_at >= $2 - interval '90 days' AND f.created_at <= $2
  AND ft.source LIKE 'fan_import:%'
"#;

/// Claim 12 — "The archive was worth mining": confirmations ÷ contacts
/// imported. Not windowed — the archive is a stock, not a flow.
pub const ARCHIVE_WORTH_MINING_SQL: &str = r#"
WITH latest_marketing AS (
    SELECT DISTINCT ON (fan_id) fan_id, granted
    FROM fan_consents
    WHERE workspace_id = $1 AND purpose = 'marketing'
    ORDER BY fan_id, recorded_at DESC, id DESC
)
SELECT count(*) AS imported,
       count(*) FILTER (WHERE EXISTS (
           SELECT 1 FROM fans f
           JOIN latest_marketing lm ON lm.fan_id = f.id AND lm.granted
           WHERE f.workspace_id = $1 AND f.normalized_email = d.normalized_email)) AS confirmed
FROM drive_contacts d
WHERE d.workspace_id = $1 AND d.fan_outcome = 'promoted'
"#;

/// Claim 13 — "Source ROI honest": fans still engaged at 30d ÷ acquired, per
/// channel. Only fans old enough to have had 30 days are in the denominator.
pub const SOURCE_ROI_SQL: &str = r#"
WITH first_touch AS (
    SELECT DISTINCT ON (fan_id) fan_id, source
    FROM fan_acquisition_events
    WHERE workspace_id = $1
    ORDER BY fan_id, occurred_at, id
)
SELECT COALESCE(split_part(ft.source, ':', 1), 'unattributed') AS channel,
       count(*) AS acquired,
       count(*) FILTER (WHERE f.status = 'active'
                          AND f.last_activity_at >= f.created_at + interval '30 days') AS engaged_30d
FROM fans f
LEFT JOIN first_touch ft ON ft.fan_id = f.id
WHERE f.workspace_id = $1
  AND f.created_at >= $2 - interval '90 days'
  AND f.created_at <= $2 - interval '30 days'
GROUP BY 1
ORDER BY acquired DESC, channel
"#;

/// Claim 15 — "Counterparty pull": delivered T+7 counterparty reports that
/// became a follow-up conversation — an inbound message from that address
/// after the report left. `delivered_at` (not `created_at`): the claim says
/// delivered, and an outbox row that never left is not an artifact anyone
/// could answer.
pub const COUNTERPARTY_PULL_SQL: &str = r#"
WITH reports AS (
    SELECT o.delivered_at AS sent_at,
           lower(btrim(o.payload -> 'report' -> 'counterparty' ->> 'email')) AS email
    FROM outbox_events o
    WHERE o.workspace_id = $1
      AND o.event_type = 'crowdrelay.show.post_show_report_due'
      AND o.delivered_at IS NOT NULL
      AND o.payload -> 'report' -> 'counterparty' ->> 'email' IS NOT NULL
      AND o.delivered_at >= $2 - interval '90 days' AND o.delivered_at <= $2
)
SELECT count(*) AS delivered,
       count(*) FILTER (WHERE EXISTS (
           SELECT 1 FROM drive_contacts d
           WHERE d.workspace_id = $1
             AND d.normalized_email = r.email
             AND d.last_inbound_at > r.sent_at)) AS conversations
FROM reports r
"#;
