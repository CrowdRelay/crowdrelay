-- Publication measurement integrity.
--
-- 1) One canonical conversion: `last_tracked_click` is the credit axis —
--    exactly one such row may exist per fan. Other methods
--    (`direct_arrival`, `referral_code`) are complementary axes and stay
--    allowed to coexist; see record_referral_conversion in
--    crates/crowdrelay-infra/src/acquisition/acquisition_events.rs.
-- 2) Archive fan qualification: a mass fan promote requires explicit
--    fan-origin evidence — the sheet typed the row 'fan', the source file
--    was declared a fan list, or an operator qualified the contact. A
--    free-mail domain or a reply to booking mail is no longer sufficient.
-- 3) Pending content 7d measurements re-anchor to the post's real
--    `posted_at`: a draft that waited for a manual publish keeps its full
--    seven-day window. Settled (succeeded/failed) rows are history and are
--    not rewritten.

-- ── 1a. Dedupe: keep the earliest last-click conversion per fan ─────────
-- Duplicates are the same fact restated with different winners; the
-- earliest row is the decision the system actually made at signup time.
DELETE FROM fan_provenance_events AS dup
WHERE dup.event_kind = 'conversion'
  AND dup.attribution_method = 'last_tracked_click'
  AND EXISTS (
      SELECT 1 FROM fan_provenance_events AS earlier
      WHERE earlier.workspace_id = dup.workspace_id
        AND earlier.fan_id = dup.fan_id
        AND earlier.event_kind = 'conversion'
        AND earlier.attribution_method = 'last_tracked_click'
        AND (earlier.occurred_at, earlier.id) < (dup.occurred_at, dup.id)
  );

-- ── 1b. The storage-layer guarantee behind the write-side NOT EXISTS ────
CREATE UNIQUE INDEX IF NOT EXISTS fan_provenance_one_last_click
    ON fan_provenance_events (workspace_id, fan_id)
    WHERE event_kind = 'conversion'
      AND attribution_method = 'last_tracked_click';

-- ── 2. Archive qualification columns ────────────────────────────────────
-- `fan_qualified_at`/`fan_qualified_by`: an operator's per-contact decision
-- that this address is fan material — the individual-confirm path the
-- industry-registry veto forces. `drive_files.audience_role` declares a
-- whole file fan-origin, qualifying every contact it contributes.
ALTER TABLE drive_contacts
    ADD COLUMN IF NOT EXISTS fan_qualified_at timestamptz,
    ADD COLUMN IF NOT EXISTS fan_qualified_by text
        CHECK (fan_qualified_by IS NULL OR char_length(fan_qualified_by) <= 200);

ALTER TABLE drive_contacts
    ADD CONSTRAINT drive_contacts_fan_qualified_check
    CHECK ((fan_qualified_at IS NULL) = (fan_qualified_by IS NULL));

ALTER TABLE drive_files
    ADD COLUMN IF NOT EXISTS audience_role text
        CHECK (audience_role IS NULL OR audience_role IN ('fan_origin'));

-- ── 3. Re-anchor pending content measurements to real publication ───────
-- Mirrors anchor_content_measurements_to_publication, including the same
-- window-length preservation (due_at - action_finished_at is kept) — only
-- the anchor moves. Runs in both directions: a post that went live before
-- the action finished gets the same posted_at start as a late one.
WITH published AS (
    SELECT workspace_id, action_id, MIN(posted_at) AS posted_at
    FROM (
        SELECT workspace_id, action_id, posted_at FROM social_posts
        WHERE posted_at IS NOT NULL
        UNION ALL
        SELECT workspace_id, action_id, posted_at FROM telegram_posts
        WHERE posted_at IS NOT NULL
        UNION ALL
        SELECT workspace_id, action_id, posted_at FROM discord_posts
        WHERE posted_at IS NOT NULL
        UNION ALL
        SELECT workspace_id, action_id, posted_at FROM community_posts
        WHERE posted_at IS NOT NULL
    ) AS posts
    GROUP BY workspace_id, action_id
)
UPDATE autopilot_measurements AS measurement
SET action_finished_at = published.posted_at,
    due_at = published.posted_at + (measurement.due_at - measurement.action_finished_at),
    available_at = published.posted_at + (measurement.due_at - measurement.action_finished_at),
    updated_at = now()
FROM published
WHERE measurement.workspace_id = published.workspace_id
  AND measurement.action_id = published.action_id
  AND measurement.measurement_kind IN ('content_link_clicks_7d', 'content_fan_acquisition_7d')
  AND measurement.status = 'pending'
  AND measurement.action_finished_at <> published.posted_at;
